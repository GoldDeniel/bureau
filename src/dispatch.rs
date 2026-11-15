//! The company loop: managers triage new tickets, workers execute delegated ones.
use crate::adapters::{self, JobCtx, JobKind};
use crate::db::{self, Agent, Db, Division, Ticket};
use crate::{failure, rotation};
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

/// Drain the company: start work until nothing is left to start, then wait for
/// the last jobs to finish. Newly freed workers are picked up immediately.
pub async fn tick(db: Db, max_parallel: usize, max_subtasks: usize) -> Result<TickReport> {
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

    // 2. Keep the fleet busy: fill the fleet, then refill after every finished
    //    job. Collecting again only once a job has actually settled avoids
    //    spawning a duplicate for an agent whose status flip has not landed yet.
    let mut set = tokio::task::JoinSet::new();
    for job in collect_jobs(&db)?.into_iter().take(max_parallel.max(1)) {
        spawn_job(&mut set, &mut report, db.clone(), job, max_subtasks);
    }
    loop {
        if set.is_empty() {
            let more = collect_jobs(&db)?;
            if more.is_empty() {
                break;
            }
            for job in more.into_iter().take(max_parallel.max(1)) {
                spawn_job(&mut set, &mut report, db.clone(), job, max_subtasks);
            }
            if set.is_empty() {
                break;
            }
            continue;
        }
        match set.join_next().await {
            Some(Ok(Ok(()))) => {}
            Some(Ok(Err(e))) => report.errors.push(e.to_string()),
            Some(Err(e)) => report.errors.push(format!("job panicked: {e}")),
            None => break,
        }
        let free = max_parallel.max(1).saturating_sub(set.len());
        if free > 0 {
            for job in collect_jobs(&db)?.into_iter().take(free) {
                spawn_job(&mut set, &mut report, db.clone(), job, max_subtasks);
            }
        }
    }
    report.ms = started.elapsed().as_millis();
    Ok(report)
}


fn spawn_job(
    set: &mut tokio::task::JoinSet<Result<()>>,
    report: &mut TickReport,
    db: Db,
    job: Job,
    max_subtasks: usize,
) {
    match &job {
        Job::Manager { .. } => report.managers_started += 1,
        Job::Worker { .. } => report.workers_started += 1,
    }
    set.spawn(async move { run_job(db, job, max_subtasks).await });
}

fn collect_jobs(db: &Db) -> Result<Vec<Job>> {
    let conn = db.lock().unwrap();
    // Self-heal: a queued ticket assigned to a manager would never be picked
    // up by either loop, so hand it back to triage instead of stranding it.
    db::requeue_manager_assignments(&conn)?;
    db::repair_parent_rollups(&conn)?;
    let mut jobs = Vec::new();

    let paused = db::paused_division_ids(&conn)?;

    for division in db::divisions(&conn)? {
        // A paused division takes no new work. Anything already running finishes
        // because its job was spawned before the pause; nothing new starts here.
        if paused.contains(&division.id) {
            continue;
        }
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
            && !paused.contains(&ticket.division_id)
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

async fn run_job(db: Db, job: Job, max_subtasks: usize) -> Result<()> {
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

            let prompt = manager_prompt(&ticket, &division, &manager, &workers, max_subtasks);
            let ctx = JobCtx {
                kind: JobKind::Manager,
                workers: workers.clone(),
                project_path: division.project_path.clone(),
                ticket_ref: ticket.ref_.clone(),
                ticket_title: ticket.title.clone(),
                log_path: log_file(&db, &ticket.ref_),
            };
            let result = run_with_ladder(&db, &ticket, &manager, &prompt, &ctx).await;

            {
                let conn = db.lock().unwrap();
                db::set_agent_status(&conn, manager.id, "idle")?;
            }

            match result {
                Ok(output) => {
                    apply_plan(&db, &ticket, &division, &manager, &workers, &output, max_subtasks).await
                }
                Err(failure) => handled_failure(&db, &ticket, &manager, failure).await,
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
            let result = run_with_ladder(&db, &ticket, &agent, &prompt, &ctx).await;

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
                Err(failure) => handled_failure(&db, &ticket, &agent, failure).await,
            }
        }
    }
}

/// A failed run, already classified and already recorded.
#[derive(Debug)]
struct Failure {
    kind: failure::FailureKind,
    detail: String,
    /// The full agent output, for the ticket comment and the expandable view.
    raw: String,
}

impl Failure {
    /// The ticket-facing message: one readable line, not 14 KB of narration.
    fn summary(&self, agent: &Agent) -> String {
        format!(
            "{} {} failed: [{}] {}",
            if agent.role == "manager" { "manager" } else { "worker" },
            agent.slug,
            self.kind.as_str(),
            self.detail
        )
    }
}

/// Run one job, walking the agent's model ladder when the failure is the
/// provider's fault.
///
/// The ladder is the agent's primary `adapter`/`model` pair followed by every
/// rung in `agent_models`. A rung that fails for an infrastructure reason is
/// put on cooldown so the next ticket does not rediscover the same dead
/// provider, and the call advances to the next usable rung.
///
/// A failure that might have left the repository half-changed never rotates:
/// see `rotation::decide`, which parks it for a human instead.
async fn run_with_ladder(
    db: &Db,
    ticket: &Ticket,
    agent: &Agent,
    prompt: &str,
    ctx: &JobCtx,
) -> Result<String, Failure> {
    let rungs = {
        let conn = db.lock().unwrap();
        db::agent_models(&conn, agent.id).unwrap_or_default()
    };
    let now = db::now();
    let usable: Vec<db::AgentModel> = rungs
        .into_iter()
        .filter(|r| db::rung_available(r, &now))
        .collect();

    let mut attempt: u32 = 0;
    let mut index: usize = 0;

    loop {
        attempt += 1;
        // Rung 0 is the agent's own adapter/model; later rungs override both.
        let mut tried = agent.clone();
        if let Some(rung) = usable.get(index) {
            if !rung.adapter.is_empty() {
                tried.adapter = rung.adapter.clone();
            }
            if !rung.model.is_empty() {
                tried.model = rung.model.clone();
            }
        }
        let provider = rotation::provider_of(&tried.model);

        match adapters::run_agent(&tried, prompt, ctx).await {
            Ok(output) => {
                // The rung works again: clear any stale cooldown.
                if let Some(rung) = usable.get(index) {
                    let conn = db.lock().unwrap();
                    let _ = db::set_model_cooldown(&conn, rung.id, 0);
                }
                return Ok(output);
            }
            Err(e) => {
                let raw = e.to_string();
                let classified = failure::classify(&raw, None);
                let has_next = index + 1 < usable.len();
                let action = rotation::decide(classified.kind, attempt, has_next);

                {
                    let conn = db.lock().unwrap();
                    let _ = db::record_attempt(
                        &conn,
                        ticket.id,
                        &tried,
                        provider,
                        classified.kind.as_str(),
                        &classified.detail,
                        &raw,
                    );
                    if let Some(rung) = usable.get(index) {
                        let secs = rotation::cooldown_secs(classified.kind);
                        if secs > 0 {
                            let _ = db::set_model_cooldown(&conn, rung.id, secs);
                        }
                    }
                }

                let current = Failure {
                    kind: classified.kind,
                    detail: classified.detail,
                    raw,
                };

                match action {
                    // Same rung again: a rate-limit blip is often gone by the
                    // time the process has torn down.
                    rotation::Action::RetrySame => continue,
                    rotation::Action::Rotate => {
                        // Say which model is being skipped and why, on the
                        // ticket, so the board reads as a chain of attempts.
                        {
                            let conn = db.lock().unwrap();
                            let _ = db::add_event(
                                &conn,
                                "rotate",
                                Some(ticket.id),
                                Some(agent.id),
                                &format!(
                                    "{}: {} on {} — rotating to the next model",
                                    agent.slug,
                                    current.kind.as_str(),
                                    if tried.model.is_empty() { "adapter default" } else { &tried.model }
                                ),
                            );
                        }
                        index += 1;
                        attempt = 0; // a fresh rung gets its own single retry
                        continue;
                    }
                    rotation::Action::Park => {
                        return Err(Failure {
                            detail: format!(
                                "{} — stopped without retrying: the run may have left changes in the repo",
                                current.detail
                            ),
                            ..current
                        });
                    }
                    rotation::Action::Fail => return Err(current),
                }
            }
        }
    }
}

fn mark_failed(
    conn: &rusqlite::Connection,
    ticket: &Ticket,
    message: &str,
    force: bool,
) -> Result<()> {
    db::add_comment(conn, ticket.id, "bureau", message)?;
    let attempts: i64 = conn.query_row(
        "SELECT attempts FROM tickets WHERE id=?1",
        [ticket.id],
        |r| r.get(0),
    )?;
    let status = if force || attempts >= 3 {
        "failed"
    } else {
        "queued"
    };
    db::set_status(conn, ticket.id, status)?;
    db::add_event(conn, "error", Some(ticket.id), None, message)?;
    // A dead child must move its parent into the attention column, exactly like
    // a child that finished: without this, parents sat in triaged forever.
    if status == "failed" {
        db::rollup_parent(conn, ticket.id)?;
    }
    Ok(())
}

/// Finish a failed run: record the classified reason on the ticket, keep the
/// full output in a comment, and move the ticket on.
///
/// `Park` (timeout / bad model output) always goes straight to `failed`: the
/// ticket may correspond to half-finished work, and the 3-ticket comment budget
/// below must not be spent silently re-running something that already edited
/// files. Infrastructure failures keep the old budget so a transient blip does
/// not need a human.
async fn handled_failure(db: &Db, ticket: &Ticket, agent: &Agent, failure: Failure) -> Result<()> {
    let message = failure.summary(agent);
    let park = !failure.kind.is_infrastructure()
        && matches!(
            failure.kind,
            failure::FailureKind::Timeout | failure::FailureKind::ModelOutput
        );
    let conn = db.lock().unwrap();
    // The full output still goes somewhere a person can read it — but as a
    // comment, not as the one-line reason on the card.
    if !failure.raw.is_empty() {
        db::add_comment(
            &conn,
            ticket.id,
            &format!("bureau:output:{}", agent.slug),
            &failure.raw,
        )?;
    }
    mark_failed(&conn, ticket, &message, park)
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

fn manager_prompt(
    t: &Ticket,
    d: &Division,
    m: &Agent,
    workers: &[Agent],
    max_subtasks: usize,
) -> String {
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
- Split real work into 1–{max_subtasks} self-contained sub-tickets — never more than that. Prefer fewer, larger sub-tickets over a long list; if the work does not need splitting, use \"answer\" with the plan instead. A worker cannot see this ticket, so every sub-ticket needs enough context to stand alone (what to build, where, acceptance criteria).
- Assign each sub-ticket to one worker slug from the list above. Never invent a slug.
- Spread the work: never hand one worker more than two sub-tickets while another worker has none. Match the job to the speciality (QA and regression checks to the reviewer, research to the researcher, narrative to the storyteller, implementation to the builder).
- If a title starts with a worker slug and a colon ("lens: ..."), assign it to that worker.
- If the ticket is just a question or something you can settle in a paragraph, use "answer".
- If it cannot proceed without a human decision, use "blocked" and say exactly what you need.

Reply with ONLY a JSON object, no prose, no code fences:

{{"action":"delegate","summary":"one sentence","subtasks":[{{"title":"...","body":"...","priority":2,"agent":"{first_worker}"}}]}}
or {{"action":"answer","body":"your answer or the plan you already made"}}
or {{"action":"blocked","reason":"what you need from the human"}}
"#,
        max_subtasks = max_subtasks,
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

/// Who does this sub-ticket belong to? A slug written into the title ("lens: …")
/// is the manager's most specific signal, so it wins over the JSON `agent`
/// field — otherwise QA lands on the implementer and the queue serialises.
fn pick_assignee<'a>(workers: &'a [Agent], title: &str, wanted: &str) -> Option<&'a Agent> {
    let from_title = title
        .split_once(':')
        .map(|(head, _)| head.trim().to_lowercase())
        .filter(|head| !head.contains(' '))
        .and_then(|slug| workers.iter().find(|w| w.slug == slug));
    from_title
        .or_else(|| workers.iter().find(|w| w.slug == wanted))
        .or_else(|| workers.first())
}

async fn apply_plan(
    db: &Db,
    ticket: &Ticket,
    division: &Division,
    manager: &Agent,
    workers: &[Agent],
    raw: &str,
    max_subtasks: usize,
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
            let cap = max_subtasks.max(1);
            if plan.subtasks.len() > cap {
                let conn = db.lock().unwrap();
                db::add_event(
                    &conn,
                    "delegate",
                    Some(ticket.id),
                    Some(manager.id),
                    &format!(
                        "manager proposed {} sub-tickets, capped to {}",
                        plan.subtasks.len(),
                        cap
                    ),
                )?;
            }
            for sub in plan.subtasks.iter().take(cap) {
                let title = if sub.title.trim().is_empty() {
                    "Sub-task".to_string()
                } else {
                    sub.title.trim().to_string()
                };
                let assignee = pick_assignee(workers, &title, &sub.agent);
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

    #[test]
    fn manager_prompt_states_the_subtask_cap() {
        let ticket = Ticket {
            id: 1,
            ref_: "BUR-1".into(),
            division_id: 1,
            division_slug: "d".into(),
            parent_id: None,
            parent_ref: None,
            title: "Big ticket".into(),
            body: "do things".into(),
            status: "new".into(),
            priority: 2,
            assignee_id: None,
            assignee_slug: None,
            assignee_name: None,
            created_by: "human".into(),
            result: String::new(),
            agent_notes: String::new(),
            attempts: 0,
            created_at: String::new(),
            updated_at: String::new(),
            children: Vec::new(),
            comment_count: 0,
        };
        let division = Division {
            id: 1,
            slug: "d".into(),
            name: "D".into(),
            description: String::new(),
            project_path: "/tmp".into(),
            created_at: String::new(),
            manager: None,
            counts: Default::default(),
            total: 0,
            paused: false,
        };
        let mgr = agent("mgr");
        let p = manager_prompt(&ticket, &division, &mgr, &[], 2);
        assert!(p.contains("1–2 self-contained sub-tickets"), "{p}");
        assert!(!p.contains("1–6"), "the old hard-coded range must be gone");
    }

    fn plan_with(n: usize) -> String {
        let subs: Vec<String> = (0..n)
            .map(|i| format!("{{\"title\":\"t{i}\",\"body\":\"b{i}\",\"priority\":2,\"agent\":\"w1\"}}"))
            .collect();
        format!("{{\"action\":\"delegate\",\"summary\":\"s\",\"subtasks\":[{}]}}", subs.join(","))
    }

    #[tokio::test]
    async fn delegation_is_capped_at_max_subtasks() {
        let db = db::open(":memory:").unwrap();
        let (ticket, division, manager, workers) = {
            let conn = db.lock().unwrap();
            let d = db::create_division(&conn, "d", "D", "", "/tmp").unwrap();
            let mgr = db::create_agent(&conn, "mgr", "Mgr", "manager", Some(d), "demo", "", "").unwrap();
            let w1 = db::create_agent(&conn, "w1", "W1", "worker", Some(d), "demo", "", "").unwrap();
            let t = db::create_ticket(&conn, d, None, "Parent", "body", 2, None, "human", "new").unwrap();
            let all = db::agents(&conn, None).unwrap();
            let by = |id: i64| all.iter().find(|a| a.id == id).unwrap().clone();
            (t, db::division_by_id(&conn, d).unwrap().unwrap(), by(mgr), vec![by(w1)])
        };

        apply_plan(&db, &ticket, &division, &manager, &workers, &plan_with(7), 2)
            .await
            .unwrap();

        let kids = {
            let conn = db.lock().unwrap();
            db::children_of(&conn, ticket.id).unwrap()
        };
        assert_eq!(kids.len(), 2, "manager asked for 7, cap is 2");
    }

    #[tokio::test]
    async fn delegation_below_the_cap_is_untouched() {
        let db = db::open(":memory:").unwrap();
        let (ticket, division, manager, workers) = {
            let conn = db.lock().unwrap();
            let d = db::create_division(&conn, "d", "D", "", "/tmp").unwrap();
            let mgr = db::create_agent(&conn, "mgr", "Mgr", "manager", Some(d), "demo", "", "").unwrap();
            let w1 = db::create_agent(&conn, "w1", "W1", "worker", Some(d), "demo", "", "").unwrap();
            let t = db::create_ticket(&conn, d, None, "Parent", "body", 2, None, "human", "new").unwrap();
            let all = db::agents(&conn, None).unwrap();
            let by = |id: i64| all.iter().find(|a| a.id == id).unwrap().clone();
            (t, db::division_by_id(&conn, d).unwrap().unwrap(), by(mgr), vec![by(w1)])
        };

        apply_plan(&db, &ticket, &division, &manager, &workers, &plan_with(2), 3)
            .await
            .unwrap();

        let kids = {
            let conn = db.lock().unwrap();
            db::children_of(&conn, ticket.id).unwrap()
        };
        assert_eq!(kids.len(), 2);
    }

    #[test]
    fn paused_division_takes_no_new_jobs() {
        let db = db::open(":memory:").unwrap();
        {
            let conn = db.lock().unwrap();
            let d = db::create_division(&conn, "p", "Paused", "", "/tmp").unwrap();
            db::create_agent(&conn, "mgr", "Mgr", "manager", Some(d), "demo", "", "").unwrap();
            db::create_agent(&conn, "w1", "W1", "worker", Some(d), "demo", "", "").unwrap();
            // one new ticket for the manager and one already queued for the worker
            db::create_ticket(&conn, d, None, "New work", "b", 2, None, "human", "new").unwrap();
            db::create_ticket(&conn, d, None, "Queued work", "b", 2, None, "human", "queued").unwrap();
        }

        // Unpaused: both loops have something to do. (collect_jobs takes the
        // mutex itself, so it must be called with no live guard.)
        assert_eq!(
            db.lock().unwrap().query_row("SELECT COUNT(*) FROM divisions WHERE paused=1", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(
            !collect_jobs(&db).unwrap().is_empty(),
            "an unpaused division must be collected"
        );

        {
            let conn = db.lock().unwrap();
            assert_eq!(db::set_division_paused(&conn, "p", true).unwrap(), 1);
        }
        assert!(
            collect_jobs(&db).unwrap().is_empty(),
            "a paused division must start neither triage nor worker claims"
        );

        {
            let conn = db.lock().unwrap();
            db::set_division_paused(&conn, "p", false).unwrap();
        }
        assert!(
            !collect_jobs(&db).unwrap().is_empty(),
            "resuming must make the queued work collectable again"
        );
    }

    #[test]
    fn pausing_unknown_division_changes_nothing() {
        let db = db::open(":memory:").unwrap();
        let conn = db.lock().unwrap();
        assert_eq!(db::set_division_paused(&conn, "nope", true).unwrap(), 0);
    }

    fn agent(slug: &str) -> Agent {
        Agent {
            id: 0,
            slug: slug.into(),
            name: slug.into(),
            role: "worker".into(),
            division_id: None,
            division_slug: None,
            adapter: "demo".into(),
            model: String::new(),
            system_hint: String::new(),
            skills: "[]".into(),
            plugins: "[]".into(),
            created_at: String::new(),
            enabled: true,
            status: "idle".into(),
        }
    }

    #[test]
    fn title_slug_beats_the_json_agent_field() {
        let team = vec![agent("forge"), agent("lens")];
        assert_eq!(
            pick_assignee(&team, "lens: capture frames", "forge")
                .unwrap()
                .slug,
            "lens"
        );
        assert_eq!(
            pick_assignee(&team, "Rebuild trees", "lens").unwrap().slug,
            "lens"
        );
        assert_eq!(
            pick_assignee(&team, "Rebuild trees", "nobody")
                .unwrap()
                .slug,
            "forge"
        );
        // A prose title with a colon must not hijack the assignment.
        assert_eq!(
            pick_assignee(&team, "Audio: charge loop", "lens")
                .unwrap()
                .slug,
            "lens"
        );
    }

    #[test]
    fn a_failure_is_classified_and_summarised_in_one_line() {
        // The old fatal_error() only answered yes/no; the tag has to survive
        // into the ticket message or the board cannot group failures.
        let f = Failure {
            kind: failure::FailureKind::Quota,
            detail: "Error: Free usage limit reached.".into(),
            raw: "x".repeat(5000),
        };
        let msg = f.summary(&agent("forge"));
        assert!(msg.contains("[quota]"), "message was {msg:?}");
        assert!(msg.contains("forge"));
        assert!(
            msg.len() < 200,
            "the summary must stay short even when the raw output is huge"
        );
    }

    #[test]
    fn a_timeout_is_summarised_as_a_park_not_a_plain_failure() {
        let f = Failure {
            kind: failure::FailureKind::Timeout,
            detail: "timed out after 7200s".into(),
            raw: String::new(),
        };
        assert_eq!(
            rotation::decide(f.kind, 1, true),
            rotation::Action::Park,
            "a run that may have committed must not rotate"
        );
    }

    #[test]
    fn a_failed_child_moves_its_parent_to_blocked() {
        let dir = std::env::temp_dir().join(format!("bureau-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fail.db");
        let _ = std::fs::remove_file(&path);
        let db = db::open(path.to_str().unwrap()).unwrap();
        let conn = db.lock().unwrap();

        let hexa = db::create_division(&conn, "hexa", "Hexa", "", "/tmp").unwrap();
        let worker =
            db::create_agent(&conn, "w", "W", "worker", Some(hexa), "demo", "", "").unwrap();
        let parent =
            db::create_ticket(&conn, hexa, None, "Parent", "", 2, None, "human", "new").unwrap();
        let child = db::create_ticket(
            &conn,
            hexa,
            Some(parent.id),
            "Child",
            "",
            2,
            Some(worker),
            "manager",
            "queued",
        )
        .unwrap();
        // Three attempts is the retry ceiling: the next failure is terminal.
        conn.execute("UPDATE tickets SET attempts=3 WHERE id=?1", [child.id])
            .unwrap();
        let child_row = db::ticket_by_id(&conn, child.id).unwrap().unwrap();

        mark_failed(&conn, &child_row, "worker w failed: usage limit", true).unwrap();

        assert_eq!(
            db::ticket_by_id(&conn, child.id).unwrap().unwrap().status,
            "failed"
        );
        assert_eq!(
            db::ticket_by_id(&conn, parent.id).unwrap().unwrap().status,
            "blocked",
            "a dead child must pull its parent into the attention column"
        );
    }
}
