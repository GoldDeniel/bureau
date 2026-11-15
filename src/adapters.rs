//! Agent adapters: how Bureau actually runs a manager or worker.
use crate::db::Agent;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::json;
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
            // Continue one stable, named session per agent instead of a bare
            // one-shot, which creates a fresh untitled session for every job
            // and floods the app's session list with unknowns. `chat -q` with
            // --oneshot keeps the scripted stdout contract; -c <name> plus
            // --create-if-missing makes the first job create the session and
            // later jobs resume it.
            //
            // Safe only because Bureau runs at most one job per agent (see
            // db::try_start_agent, which flips idle -> running atomically). A
            // resumed session is not concurrency-safe: do not run two jobs for
            // one agent, and do not rename a session under a live job.
            let mut args = vec![
                "chat".to_string(),
                "-q".to_string(),
                prompt.to_string(),
                "--oneshot".to_string(),
                "-Q".to_string(),
                "-c".to_string(),
                agent_session_name(agent),
                "--create-if-missing".to_string(),
            ];
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
            // Skills and plugins are typed per agent, but only the hermes CLI
            // has a way to accept them. Other adapters get a warning rather
            // than a silent lie about what was applied.
            for skill in parse_list(&agent.skills) {
                args.push("--skill".to_string());
                args.push(skill);
            }
            for plugin in parse_list(&agent.plugins) {
                args.push("--plugin".to_string());
                args.push(plugin);
            }
            run_cmd("hermes", &args, None, prompt, ctx, agent).await
        }
        "opencode" => {
            warn_unapplied(agent);
            let mut args = vec!["run".to_string(), prompt.to_string(), "--auto".to_string()];
            if !agent.model.is_empty() {
                args.push("--model".to_string());
                args.push(agent.model.clone());
            }
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

/// Read a JSON array of strings, tolerating anything malformed. An agent with
/// a corrupt list must still run — losing a skill is annoying, refusing to
/// start is worse.
fn parse_list(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect()
}

/// Say loudly when an agent carries settings its CLI cannot honour, so nobody
/// debugs a "missing skill" that was never passed to anything.
fn warn_unapplied(agent: &Agent) {
    let skills = parse_list(&agent.skills);
    let plugins = parse_list(&agent.plugins);
    if !skills.is_empty() || !plugins.is_empty() {
        eprintln!(
            "bureau: agent '{}' has {} skill(s) and {} plugin(s), but the '{}' adapter \
             cannot pass them to its CLI; they are stored and ignored",
            agent.slug,
            skills.len(),
            plugins.len(),
            agent.adapter
        );
    }
}

/// The stable Hermes session name for an agent: `bureau:<division>:<agent>`.
///
/// One session per agent keeps the agent's history in one place and keeps
/// concurrent jobs apart — a worker and its division's manager never share a
/// conversation. A division cannot be the unit because a division runs several
/// agents in parallel.
fn agent_session_name(agent: &Agent) -> String {
    let division = agent.division_slug.as_deref().unwrap_or("none");
    format!("bureau:{division}:{}", agent.slug)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
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

async fn ollama(agent: &Agent, prompt: &str, _ctx: &JobCtx) -> Result<String> {
    let model = if agent.model.trim().is_empty() {
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
            skills: "[]".into(),
            plugins: "[]".into(),
            enabled: true,
            status: "idle".into(),
            created_at: String::new(),
        }
    }

    #[test]
    fn skill_and_plugin_lists_survive_a_bad_value() {
        assert_eq!(parse_list(r#"["a","b"]"#), vec!["a", "b"]);
        assert_eq!(parse_list("[]"), Vec::<String>::new());
        // A corrupt list must not stop the agent from running...
        assert_eq!(parse_list("not json"), Vec::<String>::new());
        assert_eq!(parse_list(""), Vec::<String>::new());
        // ...and blank entries are dropped rather than passed as empty flags.
        assert_eq!(parse_list(r#"["a","  ",""]"#), vec!["a"]);
        // A nested structure is not a flat list; refuse rather than guess.
        assert_eq!(parse_list(r#"[{"name":"x"}]"#), Vec::<String>::new());
    }

    #[test]
    fn agent_session_name_is_stable_per_agent() {
        let a = agent("hermes");
        assert_eq!(agent_session_name(&a), "bureau:hexa:ada");
        // Two jobs for the same agent must land in the same session...
        assert_eq!(agent_session_name(&a), agent_session_name(&agent("hermes")));
        // ...but a worker must never share the manager's session.
        let mut w = agent("hermes");
        w.slug = "bob".into();
        assert_ne!(agent_session_name(&a), agent_session_name(&w));
        // An agent with no division still gets a nameable session.
        let mut orphan = agent("hermes");
        orphan.division_slug = None;
        assert_eq!(agent_session_name(&orphan), "bureau:none:ada");
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
        };
        let out = run_agent(&agent("demo"), "prompt", &ctx).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["action"], "delegate");
        assert_eq!(v["subtasks"].as_array().unwrap().len(), 1);
    }
}
