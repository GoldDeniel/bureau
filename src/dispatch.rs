//! The company loop: managers triage new tickets, workers execute delegated ones.
use crate::adapters::{self, JobCtx, JobKind};
use crate::db::{self, Agent, Db, Division, Ticket};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Debug, Default, Serialize)]
pub struct TickReport {
    pub managers_started: usize,
    pub workers_started: usize,
    pub recovered: usize,
    pub errors: Vec<String>,
    pub ms: u128,
}

enum Job {
    Manager {
        ticket: Ticket,
        division: Division,
        manager: Agent,
        workers: Vec<Agent>,
    },
    Worker {
        ticket: Ticket,
        division: Division,
        agent: Agent,
    },
}

/// One full pass over the company. Returns after every job it started has finished.
pub async fn tick(db: Db, max_parallel: usize) -> Result<TickReport> {
    let started = Instant::now();
    let mut report = TickReport::default();

    // 1. Recover tickets whose worker vanished.
    {
        let conn = db.lock().unwrap();
        match db::recover_leases(&conn) {
            Ok(v) => report.recovered = v.len(),
            Err(e) => report.errors.push(format!("lease recovery: {e}")),
        }
    }

    // 2. Collect work.
    let jobs = collect_jobs(&db)?;

    // 3. Run a bounded batch, one job at a time per agent.
    let mut set = tokio::task::JoinSet::new();
    for job in jobs.into_iter().take(max_parallel.max(1)) {
        let db = db.clone();
        match &job {
            Job::Manager { .. } => report.managers_started += 1,
            Job::Worker { .. } => report.workers_started += 1,
        }
        set.spawn(async move { run_job(db, job).await });
    }
    while let Some(res) = set.join_next().await {
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => report.errors.push(e.to_string()),
            Err(e) => report.errors.push(format!("job panicked: {e}")),
        }
    }
    report.ms = started.elapsed().as_millis();
    Ok(report)
}

fn collect_jobs(db: &Db) -> Result<Vec<Job>> {
    let conn = db.lock().unwrap();
    // Self-heal: a queued ticket assigned to a manager would never be picked
    // up by either loop, so hand it back to triage instead of stranding it.
    db::requeue_manager_assignments(&conn)?;
    let mut jobs = Vec::new();

    for division in db::divisions(&conn)? {
        let Some(manager) = db::manager_for(&conn, division.id)? else {
            continue;
        };
        if !manager.enabled || manager.status != "idle" {
            continue;
        }
        let Some(ticket) = db::new_unassigned(&conn, division.id)? else {
            continue;
        };
        let workers = db::workers_for(&conn, division.id)?;
        jobs.push(Job::Manager {
            ticket,
            division,
            manager,
            workers,
        });
    }

    for agent_id in db::idle_workers_with_work(&conn)? {
        if let Some(agent) = db::agents(&conn, None)?
            .into_iter()
            .find(|a| a.id == agent_id)
            && let Some(ticket) = db::queued_for_agent(&conn, agent_id)?
            && let Some(division) = db::division_by_id(&conn, ticket.division_id)?
        {
            jobs.push(Job::Worker {
                ticket,
                division,
                agent,
            });
        }
    }
    Ok(jobs)
}

async fn run_job(db: Db, job: Job) -> Result<()> {
    match job {
        Job::Manager {
            ticket,
            division,
            manager,
            workers,
        } => {
            {
                let conn = db.lock().unwrap();
                if !db::try_start_agent(&conn, manager.id)? {
                    return Ok(());
                }
                db::add_event(
                    &conn,
                    "triage",
                    Some(ticket.id),
                    Some(manager.id),
                    &format!("{} picked up {}", manager.slug, ticket.ref_),
                )?;
            }

            let prompt = manager_prompt(&ticket, &division, &manager, &workers);
            let ctx = JobCtx {
                kind: JobKind::Manager,
                workers: workers.clone(),
                project_path: division.project_path.clone(),
                ticket_ref: ticket.ref_.clone(),
                ticket_title: ticket.title.clone(),
                log_path: log_file(&db, &ticket.ref_),
            };
            let result = adapters::run_agent(&manager, &prompt, &ctx).await;

            {
                let conn = db.lock().unwrap();
                db::set_agent_status(&conn, manager.id, "idle")?;
            }

            match result {
                Ok(output) => {
                    apply_plan(&db, &ticket, &division, &manager, &workers, &output).await
                }
                Err(e) => {
                    fail_ticket(
                        &db,
                        &ticket,
                        &format!("manager {} failed: {e}", manager.slug),
                    )
                    .await
                }
            }
        }
        Job::Worker {
            ticket,
            division,
            agent,
        } => {
            {
                let conn = db.lock().unwrap();
                if !db::try_start_agent(&conn, agent.id)? {
                    return Ok(());
                }
                if !db::claim(&conn, ticket.id, agent.id, 3600)? {
                    db::set_agent_status(&conn, agent.id, "idle")?;
                    return Ok(());
                }
                db::add_event(
                    &conn,
                    "claim",
                    Some(ticket.id),
                    Some(agent.id),
                    &format!("{} started {}", agent.slug, ticket.ref_),
                )?;
            }

            let prompt = worker_prompt(&ticket, &division, &agent);
            let ctx = JobCtx {
                kind: JobKind::Worker,
                workers: Vec::new(),
                project_path: division.project_path.clone(),
                ticket_ref: ticket.ref_.clone(),
                ticket_title: ticket.title.clone(),
                log_path: log_file(&db, &ticket.ref_),
            };
            let result = adapters::run_agent(&agent, &prompt, &ctx).await;

            {
                let conn = db.lock().unwrap();
                db::set_agent_status(&conn, agent.id, "idle")?;
            }

            match result {
                Ok(output) => {
                    let conn = db.lock().unwrap();
                    db::set_result(&conn, ticket.id, &output, "")?;
                    db::add_comment(&conn, ticket.id, &format!("agent:{}", agent.slug), &output)?;
                    db::set_status(&conn, ticket.id, "review")?;
                    db::add_event(
                        &conn,
                        "review",
                        Some(ticket.id),
                        Some(agent.id),
                        &format!("{} finished {}", agent.slug, ticket.ref_),
                    )?;
                    db::rollup_parent(&conn, ticket.id)?;
                    Ok(())
                }
                Err(e) => {
                    fail_ticket(&db, &ticket, &format!("worker {} failed: {e}", agent.slug)).await
                }
            }
        }
    }
}

async fn fail_ticket(db: &Db, ticket: &Ticket, message: &str) -> Result<()> {
    let conn = db.lock().unwrap();
    db::add_comment(&conn, ticket.id, "bureau", message)?;
    let attempts: i64 = conn.query_row(
        "SELECT attempts FROM tickets WHERE id=?1",
        [ticket.id],
        |r| r.get(0),
    )?;
    let status = if attempts >= 3 { "failed" } else { "queued" };
    db::set_status(&conn, ticket.id, status)?;
    db::add_event(&conn, "error", Some(ticket.id), None, message)?;
    Ok(())
}

/// Job log path next to the database: `<db dir>/logs/<ref>.log`.
/// The board tails this file while the agent runs.
fn log_file(db: &Db, ticket_ref: &str) -> String {
    let conn = db.lock().unwrap();
    conn.path()
        .map(|p| {
            std::path::Path::new(p)
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join("logs")
                .join(format!("{ticket_ref}.log"))
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- prompts

fn manager_prompt(t: &Ticket, d: &Division, m: &Agent, workers: &[Agent]) -> String {
    let team = if workers.is_empty() {
        "(no workers configured — you must handle it yourself with action \"answer\")".to_string()
    } else {
        workers
            .iter()
            .map(|w| {
                format!(
                    "- {slug} — {name} (adapter: {adapter}{model}){hint}",
                    slug = w.slug,
                    name = w.name,
                    adapter = w.adapter,
                    model = if w.model.is_empty() {
                        String::new()
                    } else {
                        format!(", model: {}", w.model)
                    },
                    hint = if w.system_hint.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", one_line(&w.system_hint, 160))
                    },
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        r#"You are {mname}, division manager at Bureau — an agent-run software company.
Division: {dname} ({dslug})
Repository / working directory: {path}
Division charter: {charter}

A new ticket has landed in your queue and only you can triage it.

TICKET {tref}
Title: {title}
Priority: P{prio}
Opened by: {by}

{body}

Your team (workers in this division):
{team}

Decide what happens to this ticket. Rules:
- Split real work into 1–6 self-contained sub-tickets. A worker cannot see this ticket, so every sub-ticket needs enough context to stand alone (what to build, where, acceptance criteria).
- Assign each sub-ticket to one worker slug from the list above. Never invent a slug.
- If the ticket is just a question or something you can settle in a paragraph, use "answer".
- If it cannot proceed without a human decision, use "blocked" and say exactly what you need.

Reply with ONLY a JSON object, no prose, no code fences:

{{"action":"delegate","summary":"one sentence","subtasks":[{{"title":"...","body":"...","priority":2,"agent":"{first_worker}"}}]}}
or {{"action":"answer","body":"your answer or the plan you already made"}}
or {{"action":"blocked","reason":"what you need from the human"}}
"#,
        mname = m.name,
        dname = d.name,
        dslug = d.slug,
        path = if d.project_path.is_empty() {
            "(none)"
        } else {
            &d.project_path
        },
        charter = if d.description.is_empty() {
            "(none)"
        } else {
            &d.description
        },
        tref = t.ref_,
        title = t.title,
        prio = t.priority,
        by = t.created_by,
        body = if t.body.is_empty() {
            "(no description)"
        } else {
            &t.body
        },
        team = team,
        first_worker = workers.first().map(|w| w.slug.as_str()).unwrap_or("worker"),
    )
}

fn worker_prompt(t: &Ticket, d: &Division, a: &Agent) -> String {
    format!(
        r#"You are {aname} ({aslug}), a worker in the "{dname}" division of Bureau.
Working directory: {path}
Division charter: {charter}

You own ticket {tref} — "{title}". Priority P{prio}.
{parent}

{body}

Do the work now, in the working directory above. Prefer real edits and real commands over proposals.
Do not ask questions; make the best decision you can and state the assumptions you made.
When you are finished, reply with a short changelog: what you changed (files), how you verified it, and any follow-up tickets you recommend.
"#,
        aname = a.name,
        aslug = a.slug,
        dname = d.name,
        path = if d.project_path.is_empty() {
            "(none)"
        } else {
            &d.project_path
        },
        charter = if d.description.is_empty() {
            "(none)"
        } else {
            &d.description
        },
        tref = t.ref_,
        title = t.title,
        prio = t.priority,
        parent = match &t.parent_ref {
            Some(p) => format!("This is a sub-ticket of {p}; stay inside its scope."),
            None => String::new(),
        },
        body = if t.body.is_empty() {
            "(no description)"
        } else {
            &t.body
        },
    )
}

fn one_line(s: &str, n: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= n {
        flat
    } else {
        flat.chars().take(n).collect::<String>() + "…"
    }
}

// ---------------------------------------------------------------- plan handling

#[derive(Debug, Deserialize)]
struct Plan {
    #[serde(default)]
    action: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    subtasks: Vec<SubTask>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Deserialize)]
struct SubTask {
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    agent: String,
}

fn extract_json(raw: &str) -> Option<&str> {
    let raw = raw.trim();
    let raw = raw
        .strip_prefix("```json")
        .or_else(|| raw.strip_prefix("```"))
        .unwrap_or(raw);
    let raw = raw.trim_end_matches("```").trim();
    let start = raw.find('{')?;
    // Return the first *balanced* object: plans contain nested objects, and
    // models sometimes trail prose after the JSON.
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in raw.as_bytes().iter().enumerate().skip(start) {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&raw[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

async fn apply_plan(
    db: &Db,
    ticket: &Ticket,
    division: &Division,
    manager: &Agent,
    workers: &[Agent],
    raw: &str,
) -> Result<()> {
    let plan: Option<Plan> = extract_json(raw).and_then(|j| serde_json::from_str(j).ok());

    let Some(plan) = plan else {
        let conn = db.lock().unwrap();
        db::set_result(&conn, ticket.id, "", raw)?;
        db::set_status(&conn, ticket.id, "blocked")?;
        db::add_comment(
            &conn,
            ticket.id,
            &format!("agent:{}", manager.slug),
            &format!("I could not produce a valid plan. Raw output:\n\n{}", raw),
        )?;
        db::add_event(
            &conn,
            "unparsed",
            Some(ticket.id),
            Some(manager.id),
            "manager output was not valid JSON",
        )?;
        return Ok(());
    };

    match plan.action.as_str() {
        "delegate" if !plan.subtasks.is_empty() => {
            let mut created = Vec::new();
            for sub in &plan.subtasks {
                let title = if sub.title.trim().is_empty() {
                    "Sub-task".to_string()
                } else {
                    sub.title.trim().to_string()
                };
                let assignee = workers
                    .iter()
                    .find(|w| w.slug == sub.agent)
                    .or_else(|| workers.first());
                let body = if sub.body.trim().is_empty() {
                    format!(
                        "Part of {}: {}\n\n{}",
                        ticket.ref_, ticket.title, ticket.body
                    )
                } else {
                    sub.body.trim().to_string()
                };
                let child = {
                    let conn = db.lock().unwrap();
                    db::create_ticket(
                        &conn,
                        division.id,
                        Some(ticket.id),
                        &title,
                        &body,
                        sub.priority.unwrap_or(ticket.priority),
                        assignee.map(|a| a.id),
                        &format!("agent:{}", manager.slug),
                        "queued",
                    )?
                };
                {
                    let conn = db.lock().unwrap();
                    db::add_event(
                        &conn,
                        "delegate",
                        Some(child.id),
                        Some(manager.id),
                        &format!(
                            "{} → {} ({})",
                            ticket.ref_,
                            child.ref_,
                            assignee.map(|a| a.slug.as_str()).unwrap_or("unassigned")
                        ),
                    )?;
                }
                created.push(format!(
                    "{} → {} ({})",
                    child.ref_,
                    child.title,
                    assignee.map(|a| a.slug.as_str()).unwrap_or("unassigned")
                ));
            }
            let conn = db.lock().unwrap();
            db::set_result(&conn, ticket.id, &plan.summary, "")?;
            db::set_status(&conn, ticket.id, "triaged")?;
            db::add_comment(
                &conn,
                ticket.id,
                &format!("agent:{}", manager.slug),
                &format!(
                    "{}\n\nDelegated {} sub-ticket(s):\n{}",
                    if plan.summary.is_empty() {
                        "Triaged."
                    } else {
                        &plan.summary
                    },
                    created.len(),
                    created
                        .iter()
                        .map(|c| format!("- {c}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            )?;
            db::add_event(
                &conn,
                "triaged",
                Some(ticket.id),
                Some(manager.id),
                &format!("{} delegated {} sub-tickets", manager.slug, created.len()),
            )?;
            Ok(())
        }
        "answer" => {
            let text = if plan.body.trim().is_empty() {
                &plan.summary
            } else {
                &plan.body
            };
            let conn = db.lock().unwrap();
            db::set_result(&conn, ticket.id, text, "")?;
            db::set_status(&conn, ticket.id, "review")?;
            db::add_comment(&conn, ticket.id, &format!("agent:{}", manager.slug), text)?;
            db::add_event(
                &conn,
                "answered",
                Some(ticket.id),
                Some(manager.id),
                &format!("{} answered {}", manager.slug, ticket.ref_),
            )?;
            Ok(())
        }
        "blocked" => {
            let reason = if plan.reason.trim().is_empty() {
                "Manager marked this blocked without a reason."
            } else {
                &plan.reason
            };
            let conn = db.lock().unwrap();
            db::set_status(&conn, ticket.id, "blocked")?;
            db::add_comment(&conn, ticket.id, &format!("agent:{}", manager.slug), reason)?;
            db::add_event(&conn, "blocked", Some(ticket.id), Some(manager.id), reason)?;
            Ok(())
        }
        other => {
            let conn = db.lock().unwrap();
            db::set_status(&conn, ticket.id, "blocked")?;
            db::add_comment(
                &conn,
                ticket.id,
                &format!("agent:{}", manager.slug),
                &format!("Unusable plan (action '{other}'). Raw:\n\n{raw}"),
            )?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_json_from_chatty_output() {
        let raw = "Sure! Here is the plan:\n```json\n{\"action\":\"blocked\",\"reason\":\"x\"}\n```\nThanks";
        assert_eq!(
            extract_json(raw).unwrap(),
            "{\"action\":\"blocked\",\"reason\":\"x\"}"
        );
    }

    #[test]
    fn extracts_json_with_nested_objects_and_trailing_prose() {
        let raw = "Here you go:\n{\"action\":\"delegate\",\"summary\":\"x\",\"subtasks\":[{\"title\":\"a\",\"body\":\"b\",\"priority\":2,\"agent\":\"w\"}]}\nHope that helps!";
        let j = extract_json(raw).unwrap();
        let v: serde_json::Value = serde_json::from_str(j).unwrap();
        assert_eq!(v["action"], "delegate");
        assert_eq!(v["subtasks"][0]["agent"], "w");
    }

    #[test]
    fn rejects_output_without_json() {
        assert!(extract_json("no json here").is_none());
    }
}
