//! Skill distribution: copy named skill trees into the global opencode skills
//! directory and keep an exact manifest for later sync/uninstall.
//!
//! Source trees are read-only inputs. Installs are ordinary file copies, never
//! symlinks, so a worker cannot reach back into the originals.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const MANIFEST_FILE: &str = ".bureau-manifest.json";
pub const MANIFEST_VERSION: u32 = 1;

// ---------------------------------------------------------------- manifest

/// Written to `<target>/.bureau-manifest.json`. Keys are sorted (BTreeMap) so
/// an unchanged set serialises byte-identically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub tool_version: String,
    #[serde(default)]
    pub skills: BTreeMap<String, InstalledSkill>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledSkill {
    /// Absolute path of the skill directory that was copied.
    pub source: String,
    pub source_hash: String,
    pub installed_hash: String,
    /// Sorted, `/`-separated relative paths of every copied file.
    pub files: Vec<String>,
    pub timestamp: String,
}

impl Manifest {
    fn load(target: &Path) -> Result<Self> {
        let path = target.join(MANIFEST_FILE);
        let mut manifest: Manifest = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("cannot parse manifest {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Manifest {
                version: MANIFEST_VERSION,
                tool_version: String::new(),
                skills: BTreeMap::new(),
            },
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        manifest.version = MANIFEST_VERSION;
        manifest.tool_version = env!("CARGO_PKG_VERSION").into();
        Ok(manifest)
    }

    /// Rewrite only when the bytes differ, so an unchanged run keeps the
    /// file's timestamp. An empty manifest removes the file.
    fn save(&self, target: &Path, apply: bool) -> Result<()> {
        let path = target.join(MANIFEST_FILE);
        if self.skills.is_empty() {
            if apply && fs::symlink_metadata(&path).is_ok() {
                fs::remove_file(&path)
                    .with_context(|| format!("cannot remove {}", path.display()))?;
            }
            return Ok(());
        }
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        if fs::read(&path).ok().as_deref() == Some(bytes.as_slice()) {
            return Ok(());
        }
        if !apply {
            return Ok(());
        }
        fs::create_dir_all(target).with_context(|| format!("cannot create {}", target.display()))?;
        let tmp = target.join(format!(".{MANIFEST_FILE}.tmp"));
        fs::write(&tmp, &bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("cannot replace {}", path.display()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------- trees

#[derive(Debug, Clone)]
pub struct Tree {
    pub hash: String,
    pub files: Vec<String>,
}

/// Walk a skill tree without following symlinks. Returns the sorted file list
/// and a sha256 over (relative path, mode, file sha256) triples.
pub fn tree(root: &Path) -> Result<Tree, String> {
    let meta = fs::symlink_metadata(root).map_err(|e| format!("{}: {e}", root.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink", root.display()));
    }
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let mut rels = Vec::new();
    walk(root, Path::new(""), &mut rels)?;
    rels.sort();
    let mut hasher = Sha256::new();
    let mut files = Vec::new();
    for rel in rels {
        let abs = root.join(&rel);
        let bytes = fs::read(&abs).map_err(|e| format!("{}: {e}", abs.display()))?;
        let rel_str = rel.to_string_lossy().into_owned();
        hasher.update(rel_str.as_bytes());
        hasher.update([0u8]);
        hasher.update(file_mode(&abs)?.to_le_bytes());
        hasher.update(Sha256::digest(&bytes));
        files.push(rel_str);
    }
    Ok(Tree {
        hash: hex(&hasher.finalize()),
        files,
    })
}

fn walk(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let dir = root.join(rel);
    let mut entries: Vec<_> = fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let child = rel.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", entry.path().display()))?;
        if kind.is_symlink() {
            return Err(format!("symlink {} (copies only)", child.display()));
        } else if kind.is_dir() {
            walk(root, &child, out)?;
        } else if kind.is_file() {
            out.push(child);
        } else {
            return Err(format!("unsupported file type {}", child.display()));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn file_mode(path: &Path) -> Result<u32, String> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o777)
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> Result<u32, String> {
    Ok(0)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

// ---------------------------------------------------------------- locations

pub fn default_sources() -> Vec<PathBuf> {
    match std::env::var_os("HOME") {
        Some(home) => {
            let home = PathBuf::from(home);
            vec![
                home.join(".config/opencode/skills"),
                home.join(".agents/skills"),
            ]
        }
        None => Vec::new(),
    }
}

pub fn default_target() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set; pass --target")?;
    Ok(PathBuf::from(home).join(".agents/skills"))
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Candidate source roots, highest priority first.
    pub sources: Vec<PathBuf>,
    pub target: PathBuf,
    /// False means dry run: nothing is written.
    pub apply: bool,
}

// ---------------------------------------------------------------- report

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Create,
    Update,
    Remove,
    UpToDate,
    Skip,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub name: String,
    pub action: Action,
    pub path: PathBuf,
    pub files: usize,
    pub detail: String,
}

impl Outcome {
    fn action(name: &str, action: Action, path: &Path, files: usize, detail: &str) -> Self {
        Self {
            name: name.into(),
            action,
            path: path.to_path_buf(),
            files,
            detail: detail.into(),
        }
    }

    fn skip(name: &str, reason: String) -> Self {
        Self::action(name, Action::Skip, Path::new(""), 0, &reason)
    }

    pub fn changed(&self) -> bool {
        matches!(self.action, Action::Create | Action::Update | Action::Remove)
    }

    pub fn line(&self) -> String {
        match self.action {
            Action::Create => format!("create {} ({} files)", self.path.display(), self.files),
            Action::Update if self.detail.is_empty() => {
                format!("update {} ({} files)", self.path.display(), self.files)
            }
            Action::Update => format!("update {} ({})", self.path.display(), self.detail),
            Action::Remove => format!("remove {}", self.path.display()),
            Action::UpToDate => format!("up-to-date {}", self.name),
            Action::Skip => format!("skip {}: {}", self.name, self.detail),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ListRow {
    pub name: String,
    pub state: String,
    pub detail: String,
}

// ---------------------------------------------------------------- distribute

#[derive(Debug, Clone)]
struct Candidate {
    path: PathBuf,
    tree: Tree,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn entry(source: &Candidate, installed_hash: &str, timestamp: String) -> InstalledSkill {
    InstalledSkill {
        source: source.path.to_string_lossy().into_owned(),
        source_hash: source.tree.hash.clone(),
        installed_hash: installed_hash.to_string(),
        files: source.tree.files.clone(),
        timestamp,
    }
}

/// Copy the named skills. Without `apply` this only reports the plan.
pub fn distribute(config: &Config, names: &[String]) -> Result<Vec<Outcome>> {
    let mut manifest = Manifest::load(&config.target)?;
    let mut names = names.to_vec();
    names.sort();
    names.dedup();
    let mut outcomes = Vec::new();
    for name in &names {
        outcomes.push(plan_one(
            name,
            &config.sources,
            &config.target,
            &mut manifest,
            config.apply,
        )?);
    }
    if config.apply {
        manifest.save(&config.target, true)?;
    }
    Ok(outcomes)
}

/// Re-copy every manifest entry from the source path it was installed from.
pub fn sync(config: &Config) -> Result<Vec<Outcome>> {
    let mut manifest = Manifest::load(&config.target)?;
    let mut outcomes = Vec::new();
    let names: Vec<String> = manifest.skills.keys().cloned().collect();
    for name in names {
        let Some(installed) = manifest.skills.get(&name) else {
            continue;
        };
        let source = PathBuf::from(&installed.source);
        if fs::symlink_metadata(&source).is_err() {
            outcomes.push(Outcome::skip(
                &name,
                format!("recorded source missing: {}", source.display()),
            ));
            continue;
        }
        let root = source.parent().unwrap_or(Path::new("")).to_path_buf();
        outcomes.push(plan_one(
            &name,
            &[root],
            &config.target,
            &mut manifest,
            config.apply,
        )?);
    }
    if config.apply {
        manifest.save(&config.target, true)?;
    }
    Ok(outcomes)
}

fn plan_one(
    name: &str,
    sources: &[PathBuf],
    target: &Path,
    manifest: &mut Manifest,
    apply: bool,
) -> Result<Outcome> {
    let mut valid = Vec::new();
    let mut rejected = Vec::new();
    for root in sources {
        let path = root.join(name);
        // The target is not a source: an installed copy must never win over
        // the tree it was installed from.
        if path == target.join(name) {
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                rejected.push(format!("{} is a symlink", path.display()));
                continue;
            }
            Ok(meta) if !meta.is_dir() => {
                rejected.push(format!("{} is not a directory", path.display()));
                continue;
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", path.display())),
        }
        match tree(&path) {
            Ok(tree) if tree.files.iter().any(|file| file == "SKILL.md") => {
                valid.push(Candidate { path, tree });
            }
            Ok(_) => rejected.push(format!("{} has no SKILL.md (empty skill)", path.display())),
            Err(reason) => rejected.push(reason),
        }
    }

    if valid.is_empty() {
        let reason = if rejected.is_empty() {
            let roots: Vec<String> = sources.iter().map(|p| p.display().to_string()).collect();
            format!("not found under {}", roots.join(", "))
        } else {
            rejected.join("; ")
        };
        return Ok(Outcome::skip(name, reason));
    }

    let source = &valid[0];
    if valid
        .iter()
        .skip(1)
        .any(|candidate| candidate.tree.hash != source.tree.hash)
    {
        let paths: Vec<String> = valid
            .iter()
            .map(|candidate| {
                format!(
                    "{} ({})",
                    candidate.path.display(),
                    &candidate.tree.hash[..12]
                )
            })
            .collect();
        return Ok(Outcome::skip(
            name,
            format!("name collision between sources: {}", paths.join(", ")),
        ));
    }

    let dest = target.join(name);
    let dest_meta = match fs::symlink_metadata(&dest) {
        Ok(meta) => Some(meta),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", dest.display())),
    };
    let Some(meta) = dest_meta else {
        if apply {
            copy_tree(&source.path, &dest, &source.tree)?;
        }
        manifest.skills.insert(
            name.into(),
            entry(source, &source.tree.hash, now()),
        );
        return Ok(Outcome::action(
            name,
            Action::Create,
            &dest,
            source.tree.files.len(),
            "",
        ));
    };
    if meta.file_type().is_symlink() {
        return Ok(Outcome::skip(
            name,
            format!("target {} is a symlink", dest.display()),
        ));
    }
    if !meta.is_dir() {
        return Ok(Outcome::skip(
            name,
            format!("target {} exists and is not a directory", dest.display()),
        ));
    }

    let installed = match tree(&dest) {
        Ok(tree) => tree,
        Err(reason) => {
            return Ok(Outcome::skip(
                name,
                format!("target {}: {reason}", dest.display()),
            ));
        }
    };
    let Some(recorded) = manifest.skills.get(name).cloned() else {
        return Ok(Outcome::skip(
            name,
            format!(
                "unmanaged directory at target {} (not in manifest; move it aside or uninstall it)",
                dest.display()
            ),
        ));
    };

    if installed.hash == source.tree.hash {
        if recorded.installed_hash == installed.hash
            && recorded.source == source.path.to_string_lossy()
            && recorded.source_hash == source.tree.hash
            && recorded.files == source.tree.files
        {
            return Ok(Outcome::action(
                name,
                Action::UpToDate,
                &dest,
                installed.files.len(),
                "",
            ));
        }
        // Content is already exact; only the manifest needs refreshing, so
        // keep the original timestamp.
        let timestamp = recorded.timestamp;
        manifest
            .skills
            .insert(name.into(), entry(source, &installed.hash, timestamp));
        return Ok(Outcome::action(
            name,
            Action::Update,
            &dest,
            installed.files.len(),
            "manifest refreshed",
        ));
    }

    if installed.hash != recorded.installed_hash {
        return Ok(Outcome::skip(
            name,
            format!(
                "target {} changed outside Bureau (refusing to overwrite)",
                dest.display()
            ),
        ));
    }

    if apply {
        copy_tree(&source.path, &dest, &source.tree)?;
    }
    manifest
        .skills
        .insert(name.into(), entry(source, &source.tree.hash, now()));
    Ok(Outcome::action(
        name,
        Action::Update,
        &dest,
        source.tree.files.len(),
        "",
    ))
}

fn copy_tree(source: &Path, dest: &Path, expected: &Tree) -> Result<()> {
    if fs::symlink_metadata(dest).is_ok() {
        fs::remove_dir_all(dest)
            .with_context(|| format!("cannot replace {}", dest.display()))?;
    }
    fs::create_dir_all(dest).with_context(|| format!("cannot create {}", dest.display()))?;
    for rel in &expected.files {
        let from = source.join(rel);
        let to = dest.join(rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        fs::copy(&from, &to)
            .with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
    }
    let installed = tree(dest)
        .map_err(|reason| anyhow::anyhow!("verify {}: {reason}", dest.display()))?;
    if installed.hash != expected.hash {
        let _ = fs::remove_dir_all(dest);
        bail!(
            "copy of {} did not match the source tree hash",
            source.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- uninstall

pub fn uninstall(config: &Config, names: &[String]) -> Result<Vec<Outcome>> {
    let mut manifest = Manifest::load(&config.target)?;
    let mut names = names.to_vec();
    names.sort();
    names.dedup();
    let mut outcomes = Vec::new();
    for name in &names {
        let Some(recorded) = manifest.skills.get(name).cloned() else {
            outcomes.push(Outcome::skip(
                name,
                "not managed by Bureau (no manifest entry)".into(),
            ));
            continue;
        };
        let dest = config.target.join(name);
        match fs::symlink_metadata(&dest) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if config.apply {
                    manifest.skills.remove(name);
                }
                outcomes.push(Outcome::action(
                    name,
                    Action::Remove,
                    &dest,
                    recorded.files.len(),
                    "already absent",
                ));
            }
            Err(e) => {
                return Err(e).with_context(|| format!("cannot inspect {}", dest.display()));
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                outcomes.push(Outcome::skip(
                    name,
                    format!("target {} is a symlink", dest.display()),
                ));
            }
            Ok(_) => match tree(&dest) {
                Err(reason) => outcomes.push(Outcome::skip(
                    name,
                    format!("target {}: {reason}", dest.display()),
                )),
                Ok(installed) if installed.hash == recorded.installed_hash => {
                    if config.apply {
                        fs::remove_dir_all(&dest)
                            .with_context(|| format!("cannot remove {}", dest.display()))?;
                        manifest.skills.remove(name);
                    }
                    outcomes.push(Outcome::action(
                        name,
                        Action::Remove,
                        &dest,
                        installed.files.len(),
                        "",
                    ));
                }
                Ok(_) => outcomes.push(Outcome::skip(
                    name,
                    format!(
                        "target {} changed outside Bureau (refusing to delete)",
                        dest.display()
                    ),
                )),
            },
        }
    }
    if config.apply {
        manifest.save(&config.target, true)?;
    }
    Ok(outcomes)
}

// ---------------------------------------------------------------- list

pub fn list(config: &Config) -> Result<Vec<ListRow>> {
    let manifest = Manifest::load(&config.target)?;
    let mut rows: BTreeMap<String, ListRow> = BTreeMap::new();
    for (name, installed) in &manifest.skills {
        rows.insert(
            name.clone(),
            ListRow {
                name: name.clone(),
                state: "managed".into(),
                detail: installed.source.clone(),
            },
        );
    }
    if let Ok(entries) = fs::read_dir(&config.target) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !entry.path().is_dir() {
                continue;
            }
            rows.entry(name.clone()).or_insert(ListRow {
                name,
                state: "unmanaged".into(),
                detail: entry.path().display().to_string(),
            });
        }
    }
    let mut available: BTreeMap<String, Vec<(PathBuf, String)>> = BTreeMap::new();
    for root in &config.sources {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path == config.target.join(entry.file_name()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let hash = tree(&path).map(|tree| tree.hash).unwrap_or_default();
            available.entry(name).or_default().push((path, hash));
        }
    }
    for (name, candidates) in available {
        if rows.contains_key(&name) {
            continue;
        }
        let collision = candidates.iter().any(|(_, hash)| *hash != candidates[0].1);
        rows.insert(
            name.clone(),
            ListRow {
                name,
                state: if collision { "collision" } else { "available" }.into(),
                detail: candidates
                    .iter()
                    .map(|(path, _)| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            },
        );
    }
    Ok(rows.into_values().collect())
}
