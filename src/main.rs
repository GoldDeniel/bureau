//! Bureau — a ticket system for a company of AI agents.
use bureau::{api, db, dispatch, seed};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "bureau",
    version,
    about = "Bureau — divisions, managers and workers. A ticket system for AI agents.",
    long_about = None
)]
struct Cli {
    /// Path to the SQLite database.
    #[arg(long, global = true, env = "BUREAU_DB", default_value = "bureau.db")]
    db: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the database and seed the divisions.
    Init,
    /// Run the HTTP server + board UI (and the dispatcher loop).
    Serve {
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: String,
        #[arg(long)]
        seed: bool,
        #[arg(long)]
        no_dispatch: bool,
        /// Seconds between dispatch passes.
        #[arg(long, default_value_t = 20)]
        interval: u64,
        #[arg(long, default_value_t = 2)]
        max_parallel: usize,
    },
    /// Run exactly one dispatch pass (managers triage, workers execute).
    Tick {
        #[arg(long, default_value_t = 2)]
        max_parallel: usize,
    },
    /// Add the default divisions and agents (idempotent).
    Seed,
    /// Tickets.
    Ticket {
        #[command(subcommand)]
        cmd: TicketCmd,
    },
    /// Agents.
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// Divisions.
    Division {
        #[command(subcommand)]
        cmd: DivisionCmd,
    },
    /// Recent events.
    Events {
        #[arg(short = 'n', long, default_value_t = 40)]
        limit: i64,
    },
}

#[derive(Subcommand)]
enum TicketCmd {
    /// Open a ticket. Without --agent it waits for its division manager.
    New {
        title: String,
        #[arg(short, long, default_value = "bureau")]
        division: String,
        #[arg(short, long, default_value = "")]
        body: String,
        #[arg(short, long, default_value_t = 2)]
        priority: i64,
        #[arg(short, long)]
        agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    List {
        #[arg(short, long)]
        division: Option<String>,
        #[arg(short, long)]
        status: Option<String>,
        #[arg(short, long, default_value_t = 200)]
        limit: i64,
        #[arg(long)]
        json: bool,
    },
    Show {
        key: String,
        #[arg(long)]
        json: bool,
    },
    /// Force a status (new|triaged|queued|in_progress|review|done|blocked|failed).
    Status {
        key: String,
        status: String,
    },
    Assign {
        key: String,
        agent: String,
    },
    Comment {
        key: String,
        body: String,
    },
}

#[derive(Subcommand)]
enum AgentCmd {
    List {
        #[arg(short, long)]
        division: Option<String>,
    },
    /// Add (or update) an agent.
    Add {
        slug: String,
        name: String,
        #[arg(short, long, default_value = "worker")]
        role: String,
        #[arg(short = 'D', long)]
        division: String,
        #[arg(long, default_value = "demo")]
        adapter: String,
        #[arg(long, default_value = "")]
        model: String,
        #[arg(long, default_value = "")]
        hint: String,
    },
    /// Configure an agent: adapter, model, enable/disable.
    Set {
        slug: String,
        #[arg(long)]
        adapter: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        hint: Option<String>,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
    },
}

#[derive(Subcommand)]
enum DivisionCmd {
    List,
    Add {
        slug: String,
        name: String,
        #[arg(short, long, default_value = "")]
        description: String,
        #[arg(short, long, default_value = "")]
        path: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init => {
            let db = db::open(&cli.db)?;
            seed::seed(&db)?;
            println!("database: {}", cli.db);
            print!("{}", seed::describe(&db)?);
            println!("ok ✨");
        }
        Cmd::Seed => {
            let db = db::open(&cli.db)?;
            seed::seed(&db)?;
            print!("{}", seed::describe(&db)?);
        }
        Cmd::Serve {
            bind,
            seed: do_seed,
            no_dispatch,
            interval,
            max_parallel,
        } => {
            let db = db::open(&cli.db)?;
            if do_seed {
                seed::seed(&db)?;
            }
            let flag = Arc::new(AtomicBool::new(false));
            let state = api::AppState {
                db: db.clone(),
                dispatching: flag.clone(),
                max_parallel,
            };

            if !no_dispatch {
                let db = db.clone();
                let flag = flag.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(interval.max(5))).await;
                        if flag.swap(true, Ordering::SeqCst) {
                            continue;
                        }
                        match dispatch::tick(db.clone(), max_parallel).await {
                            Ok(rep) => {
                                if rep.managers_started + rep.workers_started > 0 {
                                    let conn = db.lock().unwrap();
                                    let _ = db::add_event(
                                        &conn,
                                        "tick",
                                        None,
                                        None,
                                        &format!(
                                            "dispatcher: {} triage(s), {} worker(s) in {}ms",
                                            rep.managers_started, rep.workers_started, rep.ms
                                        ),
                                    );
                                    for e in &rep.errors {
                                        let _ = db::add_event(
                                            &conn,
                                            "error",
                                            None,
                                            None,
                                            &format!("dispatcher: {e}"),
                                        );
                                    }
                                }
                            }
                            Err(e) => eprintln!("dispatch error: {e:#}"),
                        }
                        flag.store(false, Ordering::SeqCst);
                    }
                });
            }

            let listener = tokio::net::TcpListener::bind(&bind)
                .await
                .with_context(|| format!("cannot bind {bind}"))?;
            let actual = listener.local_addr()?;
            println!("Bureau v{}  →  http://{actual}", api::VERSION);
            println!(
                "database: {}   dispatcher: {}",
                cli.db,
                if no_dispatch { "off" } else { "on" }
            );
            axum::serve(listener, api::router(state)).await?;
        }
        Cmd::Tick { max_parallel } => {
            let db = db::open(&cli.db)?;
            let rep = dispatch::tick(db, max_parallel).await?;
            println!(
                "tick: {} manager job(s), {} worker job(s), {} recovered, {}ms",
                rep.managers_started, rep.workers_started, rep.recovered, rep.ms
            );
            for e in &rep.errors {
                eprintln!("  error: {e}");
            }
        }
        Cmd::Events { limit } => {
            let db = db::open(&cli.db)?;
            let conn = db.lock().unwrap();
            for e in db::events(&conn, limit)? {
                println!(
                    "{}  {:<9} {:<8} {}",
                    e.created_at,
                    e.kind,
                    e.ticket_ref.unwrap_or_default(),
                    e.message
                );
            }
        }
        Cmd::Ticket { cmd } => {
            let db = db::open(&cli.db)?;
            let conn = db.lock().unwrap();
            match cmd {
                TicketCmd::New {
                    title,
                    division,
                    body,
                    priority,
                    agent,
                    json,
                } => {
                    let div = db::division_by_slug(&conn, &division)?
                        .with_context(|| format!("division '{division}' not found"))?;
                    let assignee = match agent {
                        Some(slug) => Some(
                            db::agent_by_slug(&conn, &slug)?
                                .with_context(|| format!("agent '{slug}' not found"))?
                                .id,
                        ),
                        None => None,
                    };
                    let t = db::create_ticket(
                        &conn,
                        div.id,
                        None,
                        &title,
                        &body,
                        priority,
                        assignee,
                        "human",
                        if assignee.is_some() { "queued" } else { "new" },
                    )?;
                    db::add_event(&conn, "created", Some(t.id), None, "human opened ticket")?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&t)?);
                    } else {
                        println!("{}  {}", t.ref_, t.title);
                        println!(
                            "division: {}   status: {}   priority: P{}",
                            div.slug, t.status, t.priority
                        );
                    }
                }
                TicketCmd::List {
                    division,
                    status,
                    limit,
                    json,
                } => {
                    let division_id = match division {
                        Some(slug) => Some(
                            db::division_by_slug(&conn, &slug)?
                                .with_context(|| format!("division '{slug}' not found"))?
                                .id,
                        ),
                        None => None,
                    };
                    let list = db::tickets(&conn, division_id, status.as_deref(), limit)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&list)?);
                    } else if list.is_empty() {
                        println!("(no tickets)");
                    } else {
                        println!(
                            "{:<8} {:<3} {:<12} {:<16} {:<8} {}",
                            "REF", "P", "STATUS", "DIVISION", "AGENT", "TITLE"
                        );
                        for t in list {
                            println!(
                                "{:<8} {:<3} {:<12} {:<16} {:<8} {}",
                                t.ref_,
                                t.priority,
                                t.status,
                                t.division_slug,
                                t.assignee_slug.unwrap_or_else(|| "-".into()),
                                ellipsis(&t.title, 60)
                            );
                        }
                    }
                }
                TicketCmd::Show { key, json } => {
                    let t = db::ticket_by_ref(&conn, &key)?
                        .with_context(|| format!("ticket '{key}' not found"))?;
                    if json {
                        let detail = serde_json::json!({
                            "ticket": t,
                            "comments": db::comments(&conn, t.id)?,
                        });
                        println!("{}", serde_json::to_string_pretty(&detail)?);
                    } else {
                        println!("{}  {}   [{}]  P{}", t.ref_, t.title, t.status, t.priority);
                        println!(
                            "division: {}   assignee: {}   created by: {}",
                            t.division_slug,
                            t.assignee_slug.as_deref().unwrap_or("-"),
                            t.created_by
                        );
                        if !t.body.is_empty() {
                            println!("\n{}", t.body);
                        }
                        if !t.result.is_empty() {
                            println!("\n--- result ---\n{}", t.result);
                        }
                        let kids = db::children_of(&conn, t.id)?;
                        if !kids.is_empty() {
                            println!("\nsub-tickets:");
                            for k in kids {
                                println!(
                                    "  {:<8} {:<10} {:<8} {}",
                                    k.ref_,
                                    k.status,
                                    k.assignee.unwrap_or_else(|| "-".into()),
                                    k.title
                                );
                            }
                        }
                        let cs = db::comments(&conn, t.id)?;
                        if !cs.is_empty() {
                            println!("\ncomments:");
                            for c in cs {
                                println!("  [{}] {}: {}", c.created_at, c.author, c.body);
                            }
                        }
                    }
                }
                TicketCmd::Status { key, status } => {
                    let t = db::ticket_by_ref(&conn, &key)?
                        .with_context(|| format!("ticket '{key}' not found"))?;
                    if !db::STATUSES.contains(&status.as_str()) {
                        anyhow::bail!("invalid status '{status}' (use one of {:?})", db::STATUSES);
                    }
                    db::set_status(&conn, t.id, &status)?;
                    db::add_event(&conn, "status", Some(t.id), None, &format!("→ {status}"))?;
                    println!("{} → {}", t.ref_, status);
                }
                TicketCmd::Assign { key, agent } => {
                    let t = db::ticket_by_ref(&conn, &key)?
                        .with_context(|| format!("ticket '{key}' not found"))?;
                    let a = db::agent_by_slug(&conn, &agent)?
                        .with_context(|| format!("agent '{agent}' not found"))?;
                    db::set_assignee(&conn, t.id, Some(a.id))?;
                    if matches!(t.status.as_str(), "new" | "triaged") {
                        db::set_status(&conn, t.id, "queued")?;
                    }
                    println!("{} → {} ({})", t.ref_, a.slug, a.name);
                }
                TicketCmd::Comment { key, body } => {
                    let t = db::ticket_by_ref(&conn, &key)?
                        .with_context(|| format!("ticket '{key}' not found"))?;
                    db::add_comment(&conn, t.id, "human", &body)?;
                    println!("commented on {}", t.ref_);
                }
            }
        }
        Cmd::Agent { cmd } => {
            let db = db::open(&cli.db)?;
            let conn = db.lock().unwrap();
            match cmd {
                AgentCmd::List { division } => {
                    let division_id = match division {
                        Some(slug) => Some(
                            db::division_by_slug(&conn, &slug)?
                                .with_context(|| format!("division '{slug}' not found"))?
                                .id,
                        ),
                        None => None,
                    };
                    println!(
                        "{:<10} {:<8} {:<14} {:<10} {:<24} {:<8} {}",
                        "SLUG", "ROLE", "DIVISION", "ADAPTER", "MODEL", "ENABLED", "STATUS"
                    );
                    for a in db::agents(&conn, division_id)? {
                        println!(
                            "{:<10} {:<8} {:<14} {:<10} {:<24} {:<8} {}",
                            a.slug,
                            a.role,
                            a.division_slug.unwrap_or_else(|| "-".into()),
                            a.adapter,
                            if a.model.is_empty() { "-" } else { &a.model },
                            if a.enabled { "yes" } else { "no" },
                            a.status
                        );
                    }
                }
                AgentCmd::Add {
                    slug,
                    name,
                    role,
                    division,
                    adapter,
                    model,
                    hint,
                } => {
                    let div = db::division_by_slug(&conn, &division)?
                        .with_context(|| format!("division '{division}' not found"))?;
                    db::create_agent(
                        &conn,
                        &slug,
                        &name,
                        if role == "manager" {
                            "manager"
                        } else {
                            "worker"
                        },
                        Some(div.id),
                        &adapter,
                        &model,
                        &hint,
                    )?;
                    let a = db::agent_by_slug(&conn, &slug)?.unwrap();
                    println!(
                        "{} ({}) in {}: adapter={} model={}",
                        a.slug,
                        a.role,
                        division,
                        a.adapter,
                        if a.model.is_empty() { "-" } else { &a.model }
                    );
                }
                AgentCmd::Set {
                    slug,
                    adapter,
                    model,
                    hint,
                    enable,
                    disable,
                } => {
                    let patch = db::AgentPatch {
                        adapter,
                        model,
                        system_hint: hint,
                        enabled: if enable {
                            Some(true)
                        } else if disable {
                            Some(false)
                        } else {
                            None
                        },
                        role: None,
                    };
                    db::agent_by_slug(&conn, &slug)?
                        .with_context(|| format!("agent '{slug}' not found"))?;
                    db::update_agent(&conn, &slug, &patch)?;
                    let a = db::agent_by_slug(&conn, &slug)?.unwrap();
                    println!(
                        "{}: adapter={} model={} enabled={}",
                        a.slug,
                        a.adapter,
                        if a.model.is_empty() { "-" } else { &a.model },
                        a.enabled
                    );
                }
            }
        }
        Cmd::Division { cmd } => {
            let db = db::open(&cli.db)?;
            let conn = db.lock().unwrap();
            match cmd {
                DivisionCmd::List => {
                    print!("{}", seed::describe(&db)?);
                }
                DivisionCmd::Add {
                    slug,
                    name,
                    description,
                    path,
                } => {
                    let id = db::create_division(&conn, &slug, &name, &description, &path)?;
                    println!("created division {slug} (#{id})");
                }
            }
        }
    }
    Ok(())
}

fn ellipsis(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}
