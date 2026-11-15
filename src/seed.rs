//! Seed the company with example divisions.
use crate::db::{self, Db};
use anyhow::Result;

pub const PROJECTS_ROOT: &str = "/home/you/Projects";

struct Spec {
    slug: &'static str,
    name: &'static str,
    repo: &'static str,
    charter: &'static str,
    manager: (&'static str, &'static str),
    workers: [(&'static str, &'static str, &'static str); 2],
}

const SEED: &[Spec] = &[
    Spec {
        slug: "example-game",
        name: "Example Game",
        repo: "example-game",
        charter: "Godot 4 VRMMO RPG. GDScript. Work on branch dev/agent. \
Run with `godot --path .`, tests are res://tests/*.tscn scenes. The world streams terrain \
and grass (GrassField), so keep an eye on frame time. Prefer clean, DRY scene/script structure.",
        manager: ("mira", "Mira"),
        workers: [
            (
                "forge",
                "Forge",
                "Gameplay & systems engineer. GDScript, scenes, shaders, performance.",
            ),
            (
                "lens",
                "Lens",
                "QA and reviewer. Runs the Godot test scenes, looks for regressions, writes bug reports.",
            ),
        ],
    },
    Spec {
        slug: "example-web-app",
        name: "Example Web App",
        repo: "example-web-app",
        charter: "Example web app at http://localhost/example-web-app. Inspect the repo's README before \
assigning work; keep changes small and verifiable.",
        manager: ("sol", "Sol"),
        workers: [
            (
                "orbit",
                "Orbit",
                "Full-stack builder. Reads the repo first, then implements.",
            ),
            (
                "kepler",
                "Kepler",
                "Reviewer and tester. Verifies every change actually runs.",
            ),
        ],
    },
    Spec {
        slug: "example-site",
        name: "Example Marketing Site",
        repo: "example-site",
        charter: "Public-facing website for the studio. Front-end polish, content and deployment. \
Inspect the repo's README/package.json before starting.",
        manager: ("vera", "Vera"),
        workers: [
            ("press", "Press", "Front-end and content builder."),
            ("proof", "Proof", "Accessibility, copy and QA reviewer."),
        ],
    },
    Spec {
        slug: "example-portfolio",
        name: "Example Portfolio Site",
        repo: "example-portfolio",
        charter: "The studio's production site. Inspect the repo first; keep the visual \
language consistent and the build green.",
        manager: ("kai", "Kai"),
        workers: [
            ("canvas", "Canvas", "Front-end builder."),
            ("pixel", "Pixel", "Visual QA and responsive reviewer."),
        ],
    },
    Spec {
        slug: "bureau",
        name: "Bureau (self)",
        repo: "bureau",
        charter: "Bureau itself — the agent ticketing company. Rust (axum + SQLite), embedded \
vanilla-JS board. `cargo test`, then `cargo run -- serve`. Improve the product you run on.",
        manager: ("nova", "Nova"),
        workers: [
            (
                "spark",
                "Spark",
                "Rust builder for the Bureau server and CLI.",
            ),
            (
                "sentry",
                "Sentry",
                "Reviewer: API tests, edge cases, data integrity.",
            ),
        ],
    },
];

pub fn projects_root() -> String {
    std::env::var("BUREAU_PROJECTS_ROOT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| PROJECTS_ROOT.to_string())
}

pub fn seed(db: &Db) -> Result<Vec<String>> {
    let conn = db.lock().unwrap();
    let existing: Vec<String> = db::divisions(&conn)?.into_iter().map(|d| d.slug).collect();
    let mut created = Vec::new();

    for spec in SEED {
        let div_id = if existing.iter().any(|s| s == spec.slug) {
            db::division_by_slug(&conn, spec.slug)?.unwrap().id
        } else {
            let path = format!("{}/{}", projects_root(), spec.repo);
            let id = db::create_division(&conn, spec.slug, spec.name, spec.charter, &path)?;
            created.push(spec.slug.to_string());
            id
        };

        db::create_agent(
            &conn,
            spec.manager.0,
            spec.manager.1,
            "manager",
            Some(div_id),
            "demo",
            "",
            "You are a pragmatic engineering manager. Read the ticket, split it into small \
self-contained sub-tickets, and hand each to the right specialist. Never invent work.",
        )?;
        for (slug, name, hint) in spec.workers {
            db::create_agent(&conn, slug, name, "worker", Some(div_id), "demo", "", hint)?;
        }
    }

    let total: i64 = conn.query_row("SELECT COUNT(*) FROM tickets", [], |r| r.get(0))?;
    if total == 0 {
        let div = db::division_by_slug(&conn, "bureau")?.unwrap();
        db::create_ticket(
            &conn,
            div.id,
            None,
            "Welcome to Bureau — watch me triage this ticket",
            "This is a self-test ticket. Nova (manager) will read it, split it into sub-tickets \
and hand them to Spark and Sentry. Everything here runs on the `demo` adapter, so no model is \
called — you are watching the workflow, not the intelligence.\n\nWhen you're ready for real \
work, open Agents and switch an adapter to `ollama`, `hermes` or `opencode`.",
            2,
            None,
            "human:you",
            "new",
        )?;
    }
    Ok(created)
}

/// Print the seeded company as a compact tree (used by `bureau init` and
/// `bureau division list`). An empty database yields an empty string, so
/// callers that would otherwise go silent must check for that themselves.
pub fn describe(db: &Db) -> Result<String> {
    let conn = db.lock().unwrap();
    let mut out = String::new();
    for d in db::divisions(&conn)? {
        let agents = db::agents(&conn, Some(d.id))?;
        let mgr = agents
            .iter()
            .find(|a| a.role == "manager")
            .map(|a| a.slug.clone())
            .unwrap_or_else(|| "—".into());
        out.push_str(&format!(
            "{:<16} {:<32} manager={:<7} workers={} tickets={}\n",
            d.slug,
            d.name,
            mgr,
            agents.iter().filter(|a| a.role == "worker").count(),
            d.total
        ));
    }
    Ok(out)
}
