//! Agent adapters: how Bureau actually runs a manager or worker.
use crate::db::{Agent, AgentToggles};
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Manager,
    Worker,
}

#[derive(Debug, Clone)]
pub struct JobCtx {
    pub kind: JobKind,
    pub workers: Vec<Agent>,
    pub project_path: String,
    pub ticket_ref: String,
    pub ticket_title: String,
    /// Where to mirror the agent's stdout/stderr while it runs ("" = no file).
    pub log_path: String,
    /// This agent's skill/plugin selection; drives the child's config dir.
    pub toggles: AgentToggles,
}

/// Hard cap for agent output kept in the database. Managers answer with a JSON
/// plan, so the *head* must survive: truncating the tail (or the head) destroys
/// the JSON and the ticket gets marked "not valid JSON".
const MAX_OUTPUT: usize = 262_144;

fn trim_output(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= MAX_OUTPUT {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX_OUTPUT).collect();
    format!("{head}\n…(truncated)")
}

/// Job budgets. Workers do real builds and captures, and a 30 min cap kept
/// killing productive runs mid-file; two hours is the new default. Overridable
/// because a slow model on a slow day should not need a rebuild.
fn timeout_for(kind: JobKind) -> Duration {
    let (var, default) = match kind {
        JobKind::Manager => ("BUREAU_MANAGER_TIMEOUT_S", 420u64),
        JobKind::Worker => ("BUREAU_WORKER_TIMEOUT_S", 7200u64),
    };
    let secs = std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(default);
    Duration::from_secs(secs)
}

pub async fn run_agent(agent: &Agent, prompt: &str, ctx: &JobCtx) -> Result<String> {
    let timeout = timeout_for(ctx.kind);
    let fut = dispatch(agent, prompt, ctx);
    match tokio::time::timeout(timeout, fut).await {
        Ok(res) => res,
        Err(_) => bail!(
            "adapter '{}' timed out after {}s",
            agent.adapter,
            timeout.as_secs()
        ),
    }
}

async fn dispatch(agent: &Agent, prompt: &str, ctx: &JobCtx) -> Result<String> {
    match agent.adapter.as_str() {
        "demo" => Ok(demo(agent, ctx)),
        "echo" => Ok(prompt.to_string()),
        "ollama" => ollama(agent, prompt, ctx).await,
        "hermes" => {
            let mut args = vec!["-z".to_string(), prompt.to_string()];
            if !ctx.project_path.is_empty() {
                args.push("--in".to_string());
                args.push(ctx.project_path.clone());
            }
            if !agent.model.is_empty() {
                args.push("-m".to_string());
                args.push(agent.model.clone());
            }
            if agent.system_hint.contains("yolo") {
                args.push("--yolo".to_string());
            }
            run_cmd("hermes", &args, None, prompt, ctx, agent).await
        }
        "opencode" => {
            let args = opencode_args(agent, prompt, &ctx.toggles);
            run_cmd(
                "opencode",
                &args,
                Some(&ctx.project_path),
                prompt,
                ctx,
                agent,
            )
            .await
        }
        "shell" => {
            if agent.model.trim().is_empty() {
                bail!("shell adapter needs a command in the agent's model field");
            }
            let cmd = agent.model.replace("{{prompt}}", &shell_quote(prompt));
            run_cmd(
                "sh",
                &["-c".to_string(), cmd],
                Some(&ctx.project_path),
                prompt,
                ctx,
                agent,
            )
            .await
        }
        other => bail!("unknown adapter '{other}' (demo|echo|ollama|hermes|opencode|shell)"),
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

/// opencode args. Inherit stays byte-identical to the pre-toggles command;
/// explicit adds `--standalone` because the shared background service loads
/// its config once, ignoring this run's XDG_CONFIG_HOME.
fn opencode_args(agent: &Agent, prompt: &str, toggles: &AgentToggles) -> Vec<String> {
    let mut args = vec!["run".to_string(), prompt.to_string(), "--auto".to_string()];
    if toggles.mode == "explicit" {
        args.push("--standalone".to_string());
    }
    if !agent.model.is_empty() {
        args.push("--model".to_string());
        args.push(agent.model.clone());
    }
    args
}

/// Resolve an adapter binary even when the service PATH is minimal.
///
/// A systemd unit does not inherit the login shell PATH, and CLIs installed by
/// their own installers (Hermes) live under `~/.hermes/installs/*/venv/bin`.
/// Look there as a fallback so services and CLI runs behave the same.
fn resolve_bin(name: &str) -> String {
    if name.contains('/') {
        return name.to_string();
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':').filter(|d| !d.is_empty()) {
            let candidate = std::path::Path::new(dir).join(name);
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let installs = std::path::PathBuf::from(home).join(".hermes/installs");
        if let Ok(entries) = std::fs::read_dir(installs) {
            let mut found: Vec<std::path::PathBuf> = Vec::new();
            for entry in entries.flatten() {
                let envs = entry.path().join("environments");
                if let Ok(envs) = std::fs::read_dir(envs) {
                    for env in envs.flatten() {
                        let candidate = env.path().join("venv/bin").join(name);
                        if candidate.is_file() {
                            found.push(candidate);
                        }
                    }
                }
            }
            found.sort_by_key(|p| {
                std::fs::metadata(p)
                    .and_then(|m| m.modified())
                    .ok()
                    .map(std::cmp::Reverse)
            });
            if let Some(p) = found.into_iter().next() {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    name.to_string()
}

/// Extra child env for the opencode adapter. A profile replaces opencode's whole
/// XDG config dir, so only that profile's opencode.json is read.
fn plugin_env(
    profile: Option<&str>,
    root: &std::path::Path,
) -> Option<(&'static str, std::path::PathBuf)> {
    let name = profile?.trim();
    (!name.is_empty()).then(|| ("XDG_CONFIG_HOME", root.join(name)))
}

/// Where the child's opencode config comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigHome {
    /// Leave XDG_CONFIG_HOME alone: the global config, exactly as before toggles.
    Inherit,
    /// A complete, user-authored profile directory.
    Profile(PathBuf),
    /// A directory generated for this agent's explicit selection.
    Generated {
        dir: PathBuf,
        skills: Vec<String>,
        plugins: Vec<String>,
    },
}

/// Decide the config dir without touching disk. Explicit wins over the legacy
/// plugin_profile; an opencode child with neither keeps today's environment.
pub fn config_home(agent: &Agent, toggles: &AgentToggles, root: &Path) -> Result<ConfigHome> {
    if agent.adapter != "opencode" {
        return Ok(ConfigHome::Inherit);
    }
    if toggles.mode == "explicit" {
        let slug = agent.slug.trim();
        if slug.is_empty() || slug.contains('/') || slug.contains("..") {
            bail!("agent slug '{slug}' cannot name a generated profile");
        }
        return Ok(ConfigHome::Generated {
            dir: root.join("generated").join(slug),
            skills: toggles.skills.clone(),
            plugins: toggles.plugins.clone(),
        });
    }
    Ok(match plugin_env(agent.plugin_profile.as_deref(), root) {
        Some((_, dir)) => ConfigHome::Profile(dir),
        None => ConfigHome::Inherit,
    })
}

fn apply_config_home(cmd: &mut std::process::Command, home: &ConfigHome) {
    match home {
        ConfigHome::Inherit => {}
        ConfigHome::Profile(dir) | ConfigHome::Generated { dir, .. } => {
            cmd.env("XDG_CONFIG_HOME", dir);
        }
    }
}

/// The generator script: an env override, then the working tree, then the
/// directory next to the binary. Missing means explicit runs fail loudly.
fn profiles_script() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BUREAU_PROFILES_SCRIPT") {
        return Some(PathBuf::from(path));
    }
    let mut candidates = vec![
        PathBuf::from("scripts/opencode-profiles.sh"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/opencode-profiles.sh"),
    ];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("scripts/opencode-profiles.sh"));
        if let Some(up) = dir.parent() {
            candidates.push(up.join("scripts/opencode-profiles.sh"));
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn csv(names: &[String]) -> String {
    names.join(",")
}

/// A generated dir is current when its marker names this agent and exactly
/// this selection. Anything else (missing, stale, hand-edited) regenerates.
fn selection_is_current(dir: &Path, slug: &str, skills: &[String], plugins: &[String]) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join("generated.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let names = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|n| n.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let sorted = |mut names: Vec<String>| {
        names.sort();
        names
    };
    value.get("generated_for").and_then(|v| v.as_str()) == Some(slug)
        && sorted(names("skills")) == sorted(skills.to_vec())
        && sorted(names("plugins")) == sorted(plugins.to_vec())
}

/// Regenerate the agent's directory when the selection changed. Never writes
/// into a user-authored profile: the script refuses a dir without its marker.
async fn ensure_generated(
    root: &Path,
    slug: &str,
    skills: &[String],
    plugins: &[String],
    dir: &Path,
) -> Result<()> {
    if selection_is_current(dir, slug, skills, plugins) {
        return Ok(());
    }
    let script = profiles_script().ok_or_else(|| {
        anyhow!(
            "cannot generate opencode profile for '{slug}': \
             scripts/opencode-profiles.sh not found (set BUREAU_PROFILES_SCRIPT)"
        )
    })?;
    let out = Command::new("bash")
        .arg(&script)
        .arg("--agent")
        .arg(slug)
        .arg("--skills")
        .arg(csv(skills))
        .arg("--plugins")
        .arg(csv(plugins))
        .env("PROFILE_ROOT", root)
        .output()
        .await
        .with_context(|| format!("running {}", script.display()))?;
    if !out.status.success() {
        let detail = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        let first = detail
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("no output");
        bail!("cannot generate opencode profile for '{slug}': {first}");
    }
    if !selection_is_current(dir, slug, skills, plugins) {
        bail!(
            "generator for '{slug}' exited 0 but wrote no matching {}",
            dir.join("generated.json").display()
        );
    }
    Ok(())
}

async fn run_cmd(
    bin: &str,
    args: &[String],
    cwd: Option<&str>,
    prompt: &str,
    ctx: &JobCtx,
    agent: &Agent,
) -> Result<String> {
    let mut cmd = Command::new(resolve_bin(bin));
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("BUREAU_TICKET", &ctx.ticket_ref)
        .env("BUREAU_TITLE", &ctx.ticket_title)
        .env("BUREAU_AGENT", &agent.slug)
        .env(
            "BUREAU_DIVISION",
            agent.division_slug.clone().unwrap_or_default(),
        );
    // None leaves XDG_CONFIG_HOME alone: the child keeps today's global config.
    // Explicit toggles need a generated dir, and a generation failure aborts the
    // run instead of silently running with the wrong skill/plugin set.
    let config = match crate::db::profiles_dir() {
        Some(root) => {
            let home = config_home(agent, &ctx.toggles, &root)?;
            if let ConfigHome::Generated {
                dir,
                skills,
                plugins,
            } = &home
            {
                ensure_generated(&root, &agent.slug, skills, plugins, dir).await?;
            }
            home
        }
        None if agent.adapter == "opencode" && ctx.toggles.mode == "explicit" => bail!(
            "cannot apply explicit toggles for '{}': no opencode profiles directory \
             (set HOME or BUREAU_PROFILES_DIR)",
            agent.slug
        ),
        None => ConfigHome::Inherit,
    };
    apply_config_home(cmd.as_std_mut(), &config);
    if let Some(dir) = cwd
        && !dir.is_empty()
    {
        cmd.current_dir(dir);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("cannot spawn '{bin}': {e} — is it installed and on PATH?"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // Everything gets the prompt on stdin too; argv-only CLIs simply ignore it,
        // while `shell` can read it when the command has no {{prompt}} placeholder.
        let _ = stdin.write_all(prompt.as_bytes()).await;
        drop(stdin);
    }
    let out_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let err_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    if !ctx.log_path.is_empty() {
        if let Some(dir) = std::path::Path::new(&ctx.log_path).parent() {
            let _ = tokio::fs::create_dir_all(dir).await;
        }
        let _ = tokio::fs::write(
            &ctx.log_path,
            format!(
                "== {} · {} · {} ==\n",
                ctx.ticket_ref, agent.slug, agent.adapter
            ),
        )
        .await;
    }
    let mut h_out = child
        .stdout
        .take()
        .map(|s| tokio::spawn(tee(s, ctx.log_path.clone(), out_buf.clone(), "»")));
    let mut h_err = child
        .stderr
        .take()
        .map(|s| tokio::spawn(tee(s, ctx.log_path.clone(), err_buf.clone(), "!")));

    let status = child.wait().await?;
    if let Some(h) = h_out.take() {
        let _ = h.await;
    }
    if let Some(h) = h_err.take() {
        let _ = h.await;
    }
    let stdout = out_buf.lock().unwrap().clone();
    let stderr = err_buf.lock().unwrap().clone();
    if !status.success() {
        bail!(
            "'{bin}' exited {}: {}",
            status.code().unwrap_or(-1),
            trim_output(&format!("{stdout}\n{stderr}"))
        );
    }
    let text = if stdout.trim().is_empty() {
        stderr
    } else {
        stdout
    };
    Ok(trim_output(&text))
}

/// Mirror one stream to the job log and to an in-memory buffer.
///
/// Line-buffered on purpose: the log is what the board shows live, so partial
/// lines must not sit in a pipe buffer until the agent finishes.
async fn tee<R>(
    reader: R,
    path: String,
    sink: std::sync::Arc<std::sync::Mutex<String>>,
    tag: &'static str,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut lines = BufReader::new(reader).lines();
    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        Ok(f) if !path.is_empty() => Some(f),
        _ => None,
    };
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(f) = file.as_mut() {
            let stamp = chrono::Local::now().format("%H:%M:%S");
            let _ = f
                .write_all(format!("{stamp} {tag} {line}\n").as_bytes())
                .await;
            let _ = f.flush().await;
        }
        let mut buf = sink.lock().unwrap();
        if buf.len() < MAX_OUTPUT {
            buf.push_str(&line);
            buf.push('\n');
        }
    }
}

// ---------------------------------------------------------------- evidence

/// What opencode's own log says the run loaded. Never guessed: absent log or a
/// non-opencode adapter yields `honoured: None` plus the reason.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunEvidence {
    pub honoured: Option<bool>,
    pub evidence: Vec<String>,
    pub reason: Option<String>,
}

/// opencode writes one global log; XDG_DATA_HOME moves it.
pub fn opencode_log_path() -> Option<PathBuf> {
    if let Some(base) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(base).join("opencode/log/opencode.log"));
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local/share/opencode/log/opencode.log"))
}

/// Read the lines written since the run started and judge them against the
/// selection. Overlapping runs share this window; the log is global.
pub fn collect_evidence(
    agent: &Agent,
    toggles: &AgentToggles,
    since: DateTime<Utc>,
) -> RunEvidence {
    if agent.adapter != "opencode" {
        return RunEvidence {
            honoured: None,
            evidence: Vec::new(),
            reason: Some(format!(
                "adapter '{}' is not opencode; its log cannot prove the selection",
                agent.adapter
            )),
        };
    }
    let Some(path) = opencode_log_path() else {
        return RunEvidence {
            honoured: None,
            evidence: Vec::new(),
            reason: Some("opencode log path is unknown (HOME and XDG_DATA_HOME unset)".into()),
        };
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return RunEvidence {
            honoured: None,
            evidence: Vec::new(),
            reason: Some(format!("opencode log not found at {}", path.display())),
        };
    };
    let evidence = evidence_since(&text, since);
    let (honoured, reason) = judge(&toggles.mode, &toggles.skills, &toggles.plugins, &evidence);
    RunEvidence {
        honoured,
        evidence,
        reason,
    }
}

fn line_time(line: &str) -> Option<DateTime<Utc>> {
    let raw = line
        .split_whitespace()
        .find_map(|token| token.strip_prefix("timestamp="))?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// `id=...` / `name=...` from a logfmt line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace().find_map(|token| {
        token
            .strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('='))
            .map(|v| v.trim_matches('"'))
            .filter(|v| !v.is_empty())
    })
}

fn evidence_since(text: &str, since: DateTime<Utc>) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let recent = match line_time(line) {
            Some(at) => at >= since,
            None => false,
        };
        if !recent {
            continue;
        }
        let kind = if line.contains("loading plugin") {
            "loading plugin"
        } else if line.contains("loading skill") || line.contains("skill loaded") {
            "loading skill"
        } else {
            continue;
        };
        let name = field(line, "id")
            .or_else(|| field(line, "name"))
            .unwrap_or("");
        out.push(if name.is_empty() {
            kind.to_string()
        } else {
            format!("{kind} {name}")
        });
    }
    out
}

/// Explicit selection is honoured when the log loads nothing outside it and no
/// selected plugin is missing. Skills are judged only when the log reports them
/// (opencode 2.2.7 is silent about them, so silence is not evidence of failure).
fn judge(
    mode: &str,
    skills: &[String],
    plugins: &[String],
    evidence: &[String],
) -> (Option<bool>, Option<String>) {
    if evidence.is_empty() {
        return (
            None,
            Some("no plugin or skill loading lines in opencode.log since the run started".into()),
        );
    }
    if mode != "explicit" {
        return (Some(true), None);
    }
    let loaded = |prefix: &str| -> Vec<&str> {
        evidence
            .iter()
            .filter_map(|line| line.strip_prefix(prefix))
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect()
    };
    let loaded_plugins = loaded("loading plugin ");
    let loaded_skills = loaded("loading skill ");

    if let Some(extra) = loaded_plugins
        .iter()
        .find(|name| !plugins.iter().any(|p| p == *name))
    {
        return (
            Some(false),
            Some(format!(
                "selection excludes plugin '{extra}' but opencode loaded it"
            )),
        );
    }
    if loaded_plugins.is_empty()
        && let Some(missing) = plugins.first()
    {
        return (
            Some(false),
            Some(format!("selected plugin '{missing}' was not loaded")),
        );
    }
    if let Some(missing) = plugins
        .iter()
        .find(|name| !loaded_plugins.iter().any(|l| l == name))
    {
        return (
            Some(false),
            Some(format!("selected plugin '{missing}' was not loaded")),
        );
    }
    if let Some(extra) = loaded_skills
        .iter()
        .find(|name| !skills.iter().any(|s| s == *name))
    {
        return (
            Some(false),
            Some(format!(
                "selection excludes skill '{extra}' but opencode loaded it"
            )),
        );
    }
    if !loaded_skills.is_empty()
        && let Some(missing) = skills
            .iter()
            .find(|name| !loaded_skills.iter().any(|l| l == name))
    {
        return (
            Some(false),
            Some(format!("selected skill '{missing}' was not loaded")),
        );
    }
    (Some(true), None)
}

async fn ollama(agent: &Agent, prompt: &str, _ctx: &JobCtx) -> Result<String> {    let model = if agent.model.trim().is_empty() {
        std::env::var("BUREAU_OLLAMA_MODEL").unwrap_or_else(|_| "qwen3.8:27b-ud-gguf".into())
    } else {
        agent.model.clone()
    };
    let system = if agent.system_hint.trim().is_empty() {
        format!(
            "You are {}, a {} agent in the '{}' division of Bureau. Be concise and concrete.",
            agent.name,
            agent.role,
            agent.division_slug.clone().unwrap_or_default()
        )
    } else {
        agent.system_hint.clone()
    };
    let body = json!({
        "model": model,
        "stream": false,
        "options": { "temperature": 0.2 },
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": prompt }
        ]
    })
    .to_string();

    let mut child = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "900",
            "-X",
            "POST",
            "http://127.0.0.1:11434/api/chat",
            "-H",
            "content-type: application/json",
            "--data-binary",
            "@-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn curl (needed for the ollama adapter)")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(body.as_bytes()).await?;
        drop(stdin);
    }
    let out = child.wait_with_output().await?;
    let raw = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        bail!("curl failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let v: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("ollama said: {}", trim_output(&raw)))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        bail!("ollama error: {err}");
    }
    let content = v
        .pointer("/message/content")
        .and_then(|c| c.as_str())
        .ok_or_else(|| {
            anyhow!(
                "ollama response had no message.content: {}",
                trim_output(&raw)
            )
        })?;
    Ok(trim_output(content))
}

/// Deterministic adapter used for demos and tests: no model, no tokens, full loop.
fn demo(agent: &Agent, ctx: &JobCtx) -> String {
    match ctx.kind {
        JobKind::Manager => {
            let first = ctx
                .workers
                .first()
                .map(|w| w.slug.clone())
                .unwrap_or_else(|| "worker".into());
            let second = ctx.workers.get(1).map(|w| w.slug.clone());
            let mut subtasks = vec![json!({
                "title": format!("Break down: {}", truncate(&ctx.ticket_title, 70)),
                "body": format!(
                    "Demo delegation for {}. Read the parent ticket in the Bureau API, produce a concrete implementation plan, and report back.\n\nParent title: {}",
                    ctx.ticket_ref, ctx.ticket_title
                ),
                "priority": 2,
                "agent": first
            })];
            if let Some(second) = second {
                subtasks.push(json!({
                    "title": format!("Review: {}", truncate(&ctx.ticket_title, 62)),
                    "body": format!("Review the work produced for {}. List blockers or approve.", ctx.ticket_ref),
                    "priority": 3,
                    "agent": second
                }));
            }
            json!({
                "action": "delegate",
                "summary": format!("{} triaged {} and split it into {} sub-tickets.", agent.name, ctx.ticket_ref, subtasks.len()),
                "subtasks": subtasks
            })
            .to_string()
        }
        JobKind::Worker => format!(
            "Demo run by {} for {}.\n\nNo model was called. This ticket travelled the whole loop: \ncreate → manager triage → delegation → worker claim → result.\n\nNext step: set this agent's adapter to ollama, hermes or opencode and the same ticket will do real work.",
            agent.name, ctx.ticket_ref
        ),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(adapter: &str) -> Agent {
        Agent {
            id: 1,
            slug: "ada".into(),
            name: "Ada".into(),
            role: "manager".into(),
            division_id: Some(1),
            division_slug: Some("hexa".into()),
            adapter: adapter.into(),
            model: String::new(),
            system_hint: String::new(),
            plugin_profile: None,
            enabled: true,
            status: "idle".into(),
            created_at: String::new(),
        }
    }

    #[test]
    fn opencode_plugin_profile_sets_xdg_config_home() {
        let root = std::path::Path::new("/home/ada/.config/opencode/profiles");
        assert_eq!(plugin_env(None, root), None, "NULL profile must not touch XDG_CONFIG_HOME");
        assert_eq!(plugin_env(Some(""), root), None);
        assert_eq!(
            plugin_env(Some("worker-lite"), root).unwrap().1,
            std::path::PathBuf::from("/home/ada/.config/opencode/profiles/worker-lite")
        );
    }

    fn envs(cmd: &std::process::Command) -> Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> {
        cmd.get_envs().map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned()))).collect()
    }

    fn explicit(skills: &[&str], plugins: &[&str]) -> AgentToggles {
        AgentToggles {
            mode: "explicit".into(),
            skills: skills.iter().map(|s| s.to_string()).collect(),
            plugins: plugins.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// BUR-29 acceptance: an inherit agent with no profile must spawn with
    /// exactly the environment it had before toggles existed.
    #[test]
    fn inherit_without_profile_leaves_the_environment_untouched() {
        let mut a = agent("opencode");
        a.plugin_profile = None;
        let home = config_home(&a, &AgentToggles::default(), std::path::Path::new("/p")).unwrap();
        assert_eq!(home, ConfigHome::Inherit);

        let mut cmd = std::process::Command::new("opencode");
        cmd.env("BUREAU_TICKET", "BUR-1")
            .env("BUREAU_AGENT", "ada")
            .env("PATH", "/usr/bin");
        let before = envs(&cmd);
        apply_config_home(&mut cmd, &home);
        assert_eq!(envs(&cmd), before, "inherit must not add or drop env");
        assert!(
            envs(&cmd).iter().all(|(key, _)| key != "XDG_CONFIG_HOME"),
            "inherit must leave XDG_CONFIG_HOME alone"
        );
        println!("inherit spawn env unchanged: {:?}", envs(&cmd));
    }

    #[test]
    fn inherit_with_profile_still_uses_that_profile() {
        let mut a = agent("opencode");
        a.plugin_profile = Some("worker-lite".into());
        assert_eq!(
            config_home(&a, &AgentToggles::default(), std::path::Path::new("/p")).unwrap(),
            ConfigHome::Profile(std::path::PathBuf::from("/p/worker-lite"))
        );
    }

    #[test]
    fn inherit_opencode_args_are_unchanged_but_explicit_runs_standalone() {
        let mut a = agent("opencode");
        a.model = "some/model".into();
        assert_eq!(
            opencode_args(&a, "do it", &AgentToggles::default()),
            vec!["run", "do it", "--auto", "--model", "some/model"],
            "inherit must keep the exact pre-toggles argv"
        );
        assert_eq!(
            opencode_args(&a, "do it", &explicit(&[], &[])),
            vec![
                "run",
                "do it",
                "--auto",
                "--standalone",
                "--model",
                "some/model"
            ]
        );
    }

    #[test]
    fn explicit_toggles_point_at_the_generated_dir() {
        let mut a = agent("opencode");
        a.plugin_profile = Some("worker-lite".into());
        let toggles = explicit(&["gamma"], &["@dietrichgebert/ponytail"]);
        let home = config_home(&a, &toggles, std::path::Path::new("/p")).unwrap();
        assert_eq!(
            home,
            ConfigHome::Generated {
                dir: std::path::PathBuf::from("/p/generated/ada"),
                skills: vec!["gamma".into()],
                plugins: vec!["@dietrichgebert/ponytail".into()],
            }
        );
        let mut cmd = std::process::Command::new("opencode");
        apply_config_home(&mut cmd, &home);
        assert!(envs(&cmd).iter().any(|(key, value)| {
            key == "XDG_CONFIG_HOME" && value.as_deref() == Some(std::ffi::OsStr::new("/p/generated/ada"))
        }));
        // Non-opencode adapters never get a config override.
        assert_eq!(
            config_home(&agent("demo"), &toggles, std::path::Path::new("/p")).unwrap(),
            ConfigHome::Inherit
        );
    }

    #[test]
    fn a_changed_selection_makes_the_generated_dir_stale() {
        let dir = std::env::temp_dir().join(format!(
            "bureau-generated-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("generated.json"),
            r#"{"generated_for":"ada","skills":["gamma"],"plugins":["p-one"]}"#,
        )
        .unwrap();
        let dir_ref = dir.as_path();
        assert!(selection_is_current(dir_ref, "ada", &["gamma".into()], &["p-one".into()]));
        assert!(!selection_is_current(dir_ref, "ada", &[], &["p-one".into()]));
        assert!(!selection_is_current(dir_ref, "ada", &["gamma".into()], &[]));
        assert!(!selection_is_current(dir_ref, "forge", &["gamma".into()], &["p-one".into()]));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn evidence_takes_only_lines_after_the_run_started() {
        let log = concat!(
            "timestamp=2026-11-13T10:00:00.000Z level=INFO msg=\"loading plugin\" id=old role=cli\n",
            "timestamp=2026-11-13T10:00:10.000Z level=INFO msg=\"loading plugin\" id=@dietrichgebert/ponytail role=cli\n",
            "timestamp=2026-11-13T10:00:11.000Z level=INFO message=\"loading plugin\" id=superpowers@git+https://github.com/obra/superpowers.git role=cli\n",
            "timestamp=2026-11-13T10:00:12.000Z level=INFO msg=\"cli starting\" role=cli\n",
        );
        let since = DateTime::parse_from_rfc3339("2026-11-13T10:00:05Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            evidence_since(log, since),
            vec![
                "loading plugin @dietrichgebert/ponytail",
                "loading plugin superpowers@git+https://github.com/obra/superpowers.git",
            ]
        );
    }

    #[test]
    fn explicit_judgement_flags_an_excluded_plugin() {
        let evidence = vec![
            "loading plugin superpowers@git+https://github.com/obra/superpowers.git".to_string(),
        ];
        let (honoured, reason) = judge(
            "explicit",
            &[],
            &["@dietrichgebert/ponytail".to_string()],
            &evidence,
        );
        assert_eq!(honoured, Some(false));
        assert!(reason.unwrap().contains("superpowers"));
    }

    #[test]
    fn explicit_judgement_accepts_the_selected_plugin() {
        let evidence = vec!["loading plugin @dietrichgebert/ponytail".to_string()];
        assert_eq!(
            judge(
                "explicit",
                &[],
                &["@dietrichgebert/ponytail".to_string()],
                &evidence
            )
            .0,
            Some(true)
        );
    }

    #[test]
    fn absent_evidence_is_unknown_not_a_pass() {
        assert_eq!(judge("explicit", &[], &[], &[]).0, None);
        assert_eq!(judge("inherit", &[], &[], &[]).0, None);
    }

    #[test]
    fn long_output_keeps_the_head() {
        let raw = format!(
            "{{\"action\":\"answer\",\"body\":\"{}\"}}",
            "x".repeat(300_000)
        );
        let out = trim_output(&raw);
        assert!(
            out.starts_with("{\"action\":\"answer\""),
            "head must survive"
        );
        assert!(out.ends_with("…(truncated)"));
    }

    #[tokio::test]
    async fn demo_manager_returns_valid_plan() {
        let ctx = JobCtx {
            kind: JobKind::Manager,
            workers: Vec::new(),
            project_path: "/tmp".into(),
            ticket_ref: "BUR-1".into(),
            ticket_title: "Add grass".into(),
            log_path: String::new(),
            toggles: AgentToggles::default(),
        };
        let out = run_agent(&agent("demo"), "prompt", &ctx).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["action"], "delegate");
        assert_eq!(v["subtasks"].as_array().unwrap().len(), 1);
    }
}
