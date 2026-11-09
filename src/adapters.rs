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
}

const MAX_OUTPUT: usize = 8000;

fn tail(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= MAX_OUTPUT {
        return s.to_string();
    }
    let cut: String = s
        .chars()
        .rev()
        .take(MAX_OUTPUT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…(truncated)\n{cut}")
}

pub async fn run_agent(agent: &Agent, prompt: &str, ctx: &JobCtx) -> Result<String> {
    let timeout = match ctx.kind {
        JobKind::Manager => Duration::from_secs(420),
        JobKind::Worker => Duration::from_secs(1800),
    };
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

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

async fn run_cmd(
    bin: &str,
    args: &[String],
    cwd: Option<&str>,
    prompt: &str,
    ctx: &JobCtx,
    agent: &Agent,
) -> Result<String> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
    let out = child.wait_with_output().await?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        bail!(
            "'{bin}' exited {}: {}",
            out.status.code().unwrap_or(-1),
            tail(&format!("{stdout}\n{stderr}"))
        );
    }
    let text = if stdout.trim().is_empty() {
        stderr
    } else {
        stdout
    };
    Ok(tail(&text))
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
    let v: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("ollama said: {}", tail(&raw)))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        bail!("ollama error: {err}");
    }
    let content = v
        .pointer("/message/content")
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow!("ollama response had no message.content: {}", tail(&raw)))?;
    Ok(tail(content))
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
            enabled: true,
            status: "idle".into(),
            created_at: String::new(),
        }
    }

    #[tokio::test]
    async fn demo_manager_returns_valid_plan() {
        let ctx = JobCtx {
            kind: JobKind::Manager,
            workers: Vec::new(),
            project_path: "/tmp".into(),
            ticket_ref: "BUR-1".into(),
            ticket_title: "Add grass".into(),
        };
        let out = run_agent(&agent("demo"), "prompt", &ctx).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["action"], "delegate");
        assert_eq!(v["subtasks"].as_array().unwrap().len(), 1);
    }
}
