//! Read-only discovery of the skills and plugins available to an agent.
//!
//! Nothing here writes to the user's opencode config. Results are cached in
//! memory so a dashboard load never waits for a subprocess; `refresh` is the
//! explicit way to re-read the disk. Any missing path degrades to an empty
//! list plus a reason instead of an error.
use crate::db;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

/// How long a cached answer stays fresh. Older answers are still served; they
/// are only marked `stale` until someone calls `refresh`.
pub const TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Discovery {
    pub skills: Vec<Item>,
    pub plugins: Vec<Item>,
    pub skills_at: String,
    pub plugins_at: String,
    pub skills_source: String,
    pub reason: Option<String>,
    #[serde(skip)]
    fetched_at: Instant,
}

impl Discovery {
    pub fn stale(&self) -> bool {
        self.fetched_at.elapsed() > TTL
    }

    pub fn to_json(&self) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        if let Value::Object(map) = &mut value {
            map.insert("stale".into(), json!(self.stale()));
        }
        value
    }
}

fn cache() -> &'static RwLock<Option<Discovery>> {
    static CACHE: OnceLock<RwLock<Option<Discovery>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(None))
}

/// The cached answer, even when it is stale. `None` before the first refresh.
pub fn cached() -> Option<Discovery> {
    cache().read().unwrap().clone()
}

/// Fast path for the API: never runs a subprocess once the cache is warm.
/// Before it is warm this returns an empty set and refreshes in the background,
/// so a page load never waits for `opencode`.
pub fn get() -> Discovery {
    if let Some(found) = cached() {
        return found;
    }
    static WARMING: AtomicBool = AtomicBool::new(false);
    if !WARMING.swap(true, Ordering::SeqCst) {
        std::thread::spawn(|| {
            refresh();
        });
    }
    Discovery {
        skills: Vec::new(),
        plugins: Vec::new(),
        skills_at: db::now(),
        plugins_at: db::now(),
        skills_source: "pending".into(),
        reason: Some("discovery is still warming up".into()),
        fetched_at: Instant::now(),
    }
}

pub fn refresh() -> Discovery {
    let fresh = discover();
    *cache().write().unwrap() = Some(fresh.clone());
    fresh
}

fn discover() -> Discovery {
    let mut reasons = Vec::new();
    let (skills, skills_source) = discover_skills(&mut reasons);
    let plugins = discover_plugins(&mut reasons);
    Discovery {
        skills,
        plugins,
        skills_at: db::now(),
        plugins_at: db::now(),
        skills_source,
        reason: (!reasons.is_empty()).then(|| reasons.join("; ")),
        fetched_at: Instant::now(),
    }
}

fn sorted(mut items: Vec<Item>) -> Vec<Item> {
    items.sort_by(|a, b| a.name.cmp(&b.name));
    items.dedup_by(|a, b| a.name == b.name);
    items
}

// ---------------------------------------------------------------- skills

fn discover_skills(reasons: &mut Vec<String>) -> (Vec<Item>, String) {
    match skills_from_opencode() {
        Ok(items) => (sorted(items), "opencode debug skill".into()),
        Err(reason) => {
            reasons.push(format!("`opencode debug skill` unavailable ({reason})"));
            (sorted(skills_from_dirs(reasons)), "filesystem".into())
        }
    }
}

/// The installed opencode 2.2.7 has no `skill` subcommand, so this normally
/// fails and the filesystem scan takes over. Newer builds may return JSON.
fn skills_from_opencode() -> Result<Vec<Item>, String> {
    let output = std::process::Command::new("opencode")
        .args(["debug", "skill"])
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let line = stderr.lines().next().unwrap_or("").trim();
        return Err(if line.is_empty() {
            format!("exit {}", output.status)
        } else {
            line.to_string()
        });
    }
    let value: Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("bad JSON: {e}"))?;
    let entries = value
        .get("skills")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .ok_or_else(|| "expected an array of skills".to_string())?;
    let mut items = Vec::new();
    for entry in entries {
        let (name, description) = match entry {
            Value::String(name) => (name.clone(), None),
            Value::Object(fields) => (
                fields
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                fields
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            ),
            _ => continue,
        };
        if !name.is_empty() {
            items.push(Item { name, description });
        }
    }
    Ok(items)
}

fn skill_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".config/opencode/skills"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        for rel in [".opencode/skill", ".opencode/skills"] {
            roots.push(cwd.join(rel));
        }
    }
    roots
}

fn skills_from_dirs(reasons: &mut Vec<String>) -> Vec<Item> {
    let mut items = Vec::new();
    let mut seen_root = false;
    for root in skill_roots() {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        seen_root = true;
        for entry in entries.flatten() {
            let file = entry.path().join("SKILL.md");
            if !file.is_file() {
                continue;
            }
            let fallback = entry.file_name().to_string_lossy().into_owned();
            let (name, description) = frontmatter(&file).unwrap_or((fallback, None));
            items.push(Item {
                name,
                description,
            });
        }
    }
    if !seen_root {
        reasons.push("no skills directory under ~/.config/opencode/skills or .opencode".into());
    }
    items
}

/// Minimal YAML frontmatter reader: just the `name:` and `description:` lines.
fn frontmatter(path: &std::path::Path) -> Option<(String, Option<String>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut name = None;
    let mut description = None;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        match key.trim() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            _ => {}
        }
    }
    let name = name.filter(|n| !n.is_empty()).or_else(|| {
        path.parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    })?;
    Some((name, description))
}

// ---------------------------------------------------------------- catalog

/// One skill or plugin the board can show, with where it came from.
/// `default_enabled` is the state an agent gets when it has no override.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CatalogItem {
    pub name: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub default_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Catalog {
    pub skills: Vec<CatalogItem>,
    pub plugins: Vec<CatalogItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Filesystem-only catalog. It never spawns opencode: the board can call it on
/// every drawer open without paying for a subprocess.
pub fn catalog() -> Catalog {
    let mut reasons = Vec::new();
    Catalog {
        skills: catalog_skills(&mut reasons),
        plugins: catalog_plugins(&mut reasons),
        reason: (!reasons.is_empty()).then(|| reasons.join("; ")),
    }
}

fn catalog_skills(reasons: &mut Vec<String>) -> Vec<CatalogItem> {
    let mut items = Vec::new();
    let mut seen_root = false;
    for (root, source) in catalog_skill_roots(reasons) {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        seen_root = true;
        for entry in entries.flatten() {
            let file = entry.path().join("SKILL.md");
            if !file.is_file() {
                continue;
            }
            let fallback = entry.file_name().to_string_lossy().into_owned();
            let (name, description) = frontmatter(&file).unwrap_or((fallback, None));
            items.push(CatalogItem {
                name,
                source: source.clone(),
                description,
                default_enabled: true,
            });
        }
    }
    if !seen_root {
        reasons.push(
            "no skills directory under ~/.agents/skills, ~/.config/opencode/skills or profiles"
                .into(),
        );
    }
    dedup_catalog(items)
}

fn catalog_skill_roots(reasons: &mut Vec<String>) -> Vec<(PathBuf, String)> {
    let mut roots = Vec::new();
    match std::env::var_os("HOME") {
        Some(home) => {
            let home = PathBuf::from(home);
            roots.push((home.join(".agents/skills"), "~/.agents/skills".into()));
            roots.push((
                home.join(".config/opencode/skills"),
                "~/.config/opencode/skills".into(),
            ));
        }
        None => reasons.push("HOME is not set".into()),
    }
    if let Ok(cwd) = std::env::current_dir() {
        for rel in [".opencode/skill", ".opencode/skills"] {
            roots.push((cwd.join(rel), rel.to_string()));
        }
    }
    if let Some(root) = db::profiles_dir() {
        roots.extend(profile_skill_roots(&root));
    }
    roots
}

/// User profiles keep their skills under `<profile>/skills`; generated ones
/// under `<profile>/generated/<slug>/skills` (symlinks to the originals).
fn profile_skill_roots(root: &std::path::Path) -> Vec<(PathBuf, String)> {
    let mut roots = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return roots;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let label = entry.file_name().to_string_lossy().into_owned();
        if label == "generated" {
            for generated in std::fs::read_dir(entry.path()).into_iter().flatten().flatten() {
                let slug = generated.file_name().to_string_lossy().into_owned();
                let dir = generated.path().join("skills");
                if dir.is_dir() {
                    roots.push((dir, format!("generated:{slug}")));
                }
            }
        } else {
            let dir = entry.path().join("skills");
            if dir.is_dir() {
                roots.push((dir, format!("profile:{label}")));
            }
        }
    }
    roots
}

fn catalog_plugins(reasons: &mut Vec<String>) -> Vec<CatalogItem> {
    let mut items = Vec::new();
    if let Some(root) = db::profiles_dir() {
        let mut dirs: Vec<(PathBuf, String)> = Vec::new();
        for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != "generated" {
                dirs.push((entry.path(), format!("profile:{name}")));
            }
        }
        for entry in std::fs::read_dir(root.join("generated"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let slug = entry.file_name().to_string_lossy().into_owned();
            dirs.push((entry.path(), format!("generated:{slug}")));
        }
        for (dir, source) in dirs {
            for rel in ["opencode/opencode.json", "opencode.json"] {
                let file = dir.join(rel);
                let Ok(text) = std::fs::read_to_string(&file) else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    reasons.push(format!("cannot parse {}", file.display()));
                    continue;
                };
                for key in ["plugins", "plugin"] {
                    let Some(list) = value.get(key).and_then(Value::as_array) else {
                        continue;
                    };
                    for name in list.iter().filter_map(Value::as_str) {
                        if !name.trim().is_empty() {
                            items.push(CatalogItem {
                                name: name.trim().to_string(),
                                source: source.clone(),
                                description: None,
                                default_enabled: true,
                            });
                        }
                    }
                }
            }
        }
    } else {
        reasons.push("no profile directory".into());
    }
    for item in installed_plugins(reasons) {
        items.push(CatalogItem {
            name: item.name,
            source: "installed".into(),
            description: item.description,
            default_enabled: true,
        });
    }
    dedup_catalog(items)
}

/// One entry per name; the first source in sorted order wins.
fn dedup_catalog(mut items: Vec<CatalogItem>) -> Vec<CatalogItem> {
    items.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.source.cmp(&b.source)));
    items.dedup_by(|a, b| a.name == b.name);
    items
}

// ---------------------------------------------------------------- plugins

fn discover_plugins(reasons: &mut Vec<String>) -> Vec<Item> {
    let mut items = Vec::new();
    let Some(root) = db::profiles_dir() else {
        reasons.push("no profile directory".into());
        items.extend(installed_plugins(reasons));
        return sorted(items);
    };
    let profiles = db::plugin_profiles();
    if profiles.is_empty() {
        reasons.push(format!("no profiles under {}", root.display()));
    }
    for profile in profiles {
        for rel in ["opencode/opencode.json", "opencode.json"] {
            let file = root.join(&profile).join(rel);
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                reasons.push(format!("cannot parse {}", file.display()));
                continue;
            };
            for key in ["plugins", "plugin"] {
                let Some(list) = value.get(key).and_then(Value::as_array) else {
                    continue;
                };
                for entry in list.iter().filter_map(Value::as_str) {
                    if !entry.trim().is_empty() {
                        items.push(Item {
                            name: entry.trim().to_string(),
                            description: Some(format!("profile {profile}")),
                        });
                    }
                }
            }
        }
    }
    items.extend(installed_plugins(reasons));
    sorted(items)
}

fn installed_plugins(reasons: &mut Vec<String>) -> Vec<Item> {
    let mut items = Vec::new();
    let Some(home) = std::env::var_os("HOME") else {
        reasons.push("HOME is not set".into());
        return items;
    };
    let config = PathBuf::from(home).join(".config/opencode");
    let mut seen = false;

    let plugin_dir = config.join("plugin");
    if let Ok(entries) = std::fs::read_dir(&plugin_dir) {
        seen = true;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = if path.is_dir() {
                entry.file_name().to_string_lossy().into_owned()
            } else {
                path.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned())
            };
            if !name.is_empty() {
                items.push(Item {
                    name,
                    description: None,
                });
            }
        }
    }

    let modules = config.join("node_modules");
    if let Ok(entries) = std::fs::read_dir(&modules) {
        seen = true;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if name.starts_with('@') {
                if let Ok(scoped) = std::fs::read_dir(entry.path()) {
                    for child in scoped.flatten() {
                        let child = child.file_name().to_string_lossy().into_owned();
                        if !child.starts_with('.') {
                            items.push(Item {
                                name: format!("{name}/{child}"),
                                description: None,
                            });
                        }
                    }
                }
            } else {
                items.push(Item {
                    name,
                    description: None,
                });
            }
        }
    }

    if !seen {
        reasons.push("no installed plugins under ~/.config/opencode".into());
    }
    items
}
