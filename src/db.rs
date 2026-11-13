//! Storage layer: SQLite schema, models and queries.
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub type Db = Arc<Mutex<Connection>>;

pub const STATUSES: [&str; 8] = [
    "new",
    "triaged",
    "queued",
    "in_progress",
    "review",
    "done",
    "blocked",
    "failed",
];

pub fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS divisions (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    slug         TEXT NOT NULL UNIQUE,
    name         TEXT NOT NULL,
    description  TEXT NOT NULL DEFAULT '',
    project_path TEXT NOT NULL DEFAULT '',
    created_at   TEXT NOT NULL,
    paused       INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS agents (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    slug        TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    role        TEXT NOT NULL DEFAULT 'worker' CHECK (role IN ('manager','worker')),
    division_id INTEGER REFERENCES divisions(id) ON DELETE CASCADE,
    adapter     TEXT NOT NULL DEFAULT 'demo',
    model       TEXT NOT NULL DEFAULT '',
    system_hint TEXT NOT NULL DEFAULT '',
    plugin_profile TEXT,
    enabled     INTEGER NOT NULL DEFAULT 1,
    status      TEXT NOT NULL DEFAULT 'idle',
    created_at  TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS one_manager_per_division
    ON agents(division_id) WHERE role = 'manager';

-- Skills and plugins selected for an agent. No row means inherit: today's
-- behaviour, where opencode's own defaults apply. CREATE IF NOT EXISTS is
-- the migration for databases opened before this table existed.
CREATE TABLE IF NOT EXISTS agent_toggles (
    agent_id   INTEGER PRIMARY KEY REFERENCES agents(id) ON DELETE CASCADE,
    mode       TEXT NOT NULL DEFAULT 'inherit',
    skills     TEXT NOT NULL DEFAULT '[]',
    plugins    TEXT NOT NULL DEFAULT '[]',
    updated_at TEXT NOT NULL
);

-- Last run per agent: the opencode log evidence that the selection was
-- honoured. honoured is NULL when the log cannot prove it.
CREATE TABLE IF NOT EXISTS agent_runs (
    agent_id INTEGER PRIMARY KEY REFERENCES agents(id) ON DELETE CASCADE,
    at       TEXT NOT NULL,
    mode     TEXT NOT NULL DEFAULT 'inherit',
    honoured INTEGER,
    evidence TEXT NOT NULL DEFAULT '[]',
    reason   TEXT
);

CREATE TABLE IF NOT EXISTS tickets (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ref         TEXT UNIQUE,
    division_id INTEGER NOT NULL REFERENCES divisions(id) ON DELETE CASCADE,
    parent_id   INTEGER REFERENCES tickets(id) ON DELETE SET NULL,
    title       TEXT NOT NULL,
    body        TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'new'
                CHECK (status IN ('new','triaged','queued','in_progress','review','done','blocked','failed')),
    priority    INTEGER NOT NULL DEFAULT 2,
    assignee_id INTEGER REFERENCES agents(id) ON DELETE SET NULL,
    created_by  TEXT NOT NULL DEFAULT 'human',
    result      TEXT NOT NULL DEFAULT '',
    agent_notes TEXT NOT NULL DEFAULT '',
    attempts    INTEGER NOT NULL DEFAULT 0,
    lease_until TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tickets_by_division ON tickets(division_id, status);
CREATE INDEX IF NOT EXISTS tickets_by_parent   ON tickets(parent_id);

CREATE TABLE IF NOT EXISTS comments (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    ticket_id  INTEGER NOT NULL REFERENCES tickets(id) ON DELETE CASCADE,
    author     TEXT NOT NULL,
    body       TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS comments_by_ticket ON comments(ticket_id);

CREATE TABLE IF NOT EXISTS events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    kind       TEXT NOT NULL,
    ticket_id  INTEGER REFERENCES tickets(id) ON DELETE SET NULL,
    agent_id   INTEGER REFERENCES agents(id) ON DELETE SET NULL,
    message    TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_recent ON events(id DESC);
"#;

pub fn open(path: &str) -> Result<Db> {
    if let Some(dir) = Path::new(path).parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).ok();
    }
    let conn = Connection::open(path).with_context(|| format!("opening {path}"))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(SCHEMA)?;
    // Databases created before the column existed; SQLite has no ADD COLUMN IF NOT EXISTS.
    // Added after the first schema: CREATE TABLE IF NOT EXISTS does not touch an
    // existing table, so older databases need the column added explicitly.
    let has_paused: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('divisions') WHERE name='paused'",
        [],
        |r| r.get(0),
    )?;
    if has_paused == 0 {
        conn.execute_batch(
            "ALTER TABLE divisions ADD COLUMN paused INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    if !has_column(&conn, "agents", "plugin_profile")? {
        conn.execute("ALTER TABLE agents ADD COLUMN plugin_profile TEXT", [])?;
    }
    // A crash leaves agents marked running; nothing can actually be running at boot.
    conn.execute("UPDATE agents SET status='idle' WHERE status='running'", [])?;
    conn.execute(
        "UPDATE tickets SET status='queued', lease_until=NULL
         WHERE status='in_progress' AND assignee_id IS NOT NULL",
        [],
    )?;
    Ok(Arc::new(Mutex::new(conn)))
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------- models

#[derive(Debug, Clone, Serialize)]
pub struct AgentLite {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Agent {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub role: String,
    pub division_id: Option<i64>,
    pub division_slug: Option<String>,
    pub adapter: String,
    pub model: String,
    #[serde(skip_serializing)]
    pub system_hint: String,
    pub plugin_profile: Option<String>,
    pub enabled: bool,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Counts {
    pub new: i64,
    pub triaged: i64,
    pub queued: i64,
    pub in_progress: i64,
    pub review: i64,
    pub done: i64,
    pub blocked: i64,
    pub failed: i64,
}

impl Counts {
    fn add(&mut self, status: &str, n: i64) {
        match status {
            "new" => self.new = n,
            "triaged" => self.triaged = n,
            "queued" => self.queued = n,
            "in_progress" => self.in_progress = n,
            "review" => self.review = n,
            "done" => self.done = n,
            "blocked" => self.blocked = n,
            "failed" => self.failed = n,
            _ => {}
        }
    }
    pub fn total(&self) -> i64 {
        self.new
            + self.triaged
            + self.queued
            + self.in_progress
            + self.review
            + self.done
            + self.blocked
            + self.failed
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Division {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub description: String,
    pub project_path: String,
    pub manager: Option<AgentLite>,
    pub counts: Counts,
    pub total: i64,
    pub created_at: String,
    #[serde(default)]
    pub paused: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChildRef {
    pub id: i64,
    #[serde(rename = "ref")]
    pub ref_: String,
    pub title: String,
    pub status: String,
    pub assignee: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ticket {
    pub id: i64,
    #[serde(rename = "ref")]
    pub ref_: String,
    pub division_id: i64,
    pub division_slug: String,
    pub parent_id: Option<i64>,
    pub parent_ref: Option<String>,
    pub title: String,
    pub body: String,
    pub status: String,
    pub priority: i64,
    pub assignee_id: Option<i64>,
    pub assignee_slug: Option<String>,
    pub assignee_name: Option<String>,
    pub created_by: String,
    pub result: String,
    pub agent_notes: String,
    pub attempts: i64,
    pub created_at: String,
    pub updated_at: String,
    pub children: Vec<ChildRef>,
    pub comment_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    pub id: i64,
    pub ticket_id: i64,
    pub author: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub id: i64,
    pub kind: String,
    pub ticket_id: Option<i64>,
    pub ticket_ref: Option<String>,
    pub agent_id: Option<i64>,
    pub message: String,
    pub created_at: String,
}

const TICKET_SELECT: &str = r#"
SELECT t.id, COALESCE(t.ref,''), t.division_id, d.slug, t.parent_id, p.ref,
       t.title, t.body, t.status, t.priority, t.assignee_id, a.slug, a.name,
       t.created_by, t.result, t.agent_notes, t.attempts, t.created_at, t.updated_at,
       (SELECT COUNT(*) FROM comments c WHERE c.ticket_id = t.id)
FROM tickets t
JOIN divisions d ON d.id = t.division_id
LEFT JOIN tickets p ON p.id = t.parent_id
LEFT JOIN agents a  ON a.id = t.assignee_id
"#;

fn map_ticket(row: &Row) -> rusqlite::Result<Ticket> {
    Ok(Ticket {
        id: row.get(0)?,
        ref_: row.get(1)?,
        division_id: row.get(2)?,
        division_slug: row.get(3)?,
        parent_id: row.get(4)?,
        parent_ref: row.get(5)?,
        title: row.get(6)?,
        body: row.get(7)?,
        status: row.get(8)?,
        priority: row.get(9)?,
        assignee_id: row.get(10)?,
        assignee_slug: row.get(11)?,
        assignee_name: row.get(12)?,
        created_by: row.get(13)?,
        result: row.get(14)?,
        agent_notes: row.get(15)?,
        attempts: row.get(16)?,
        created_at: row.get(17)?,
        updated_at: row.get(18)?,
        children: Vec::new(),
        comment_count: row.get(19)?,
    })
}

const AGENT_SELECT: &str = r#"
SELECT a.id, a.slug, a.name, a.role, a.division_id, d.slug, a.adapter, a.model,
       a.system_hint, a.enabled, a.status, a.created_at, a.plugin_profile
FROM agents a LEFT JOIN divisions d ON d.id = a.division_id
"#;

fn map_agent(row: &Row) -> rusqlite::Result<Agent> {
    Ok(Agent {
        id: row.get(0)?,
        slug: row.get(1)?,
        name: row.get(2)?,
        role: row.get(3)?,
        division_id: row.get(4)?,
        division_slug: row.get(5)?,
        adapter: row.get(6)?,
        model: row.get(7)?,
        system_hint: row.get(8)?,
        enabled: row.get::<_, i64>(9)? != 0,
        status: row.get(10)?,
        created_at: row.get(11)?,
        plugin_profile: row.get(12)?,
    })
}

// ---------------------------------------------------------------- divisions

pub fn divisions(conn: &Connection) -> Result<Vec<Division>> {
    let mut stmt = conn.prepare(
        "SELECT id, slug, name, description, project_path, created_at, paused FROM divisions ORDER BY id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Division {
                id: r.get(0)?,
                slug: r.get(1)?,
                name: r.get(2)?,
                description: r.get(3)?,
                project_path: r.get(4)?,
                created_at: r.get(5)?,
                paused: r.get::<_, i64>(6)? != 0,
                manager: None,
                counts: Counts::default(),
                total: 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut out = Vec::with_capacity(rows.len());
    let mut count_stmt = conn
        .prepare("SELECT status, COUNT(*) FROM tickets WHERE division_id = ?1 GROUP BY status")?;
    let mut mgr_stmt = conn.prepare(
        "SELECT id, slug, name, role FROM agents WHERE division_id = ?1 AND role = 'manager'",
    )?;
    for mut d in rows {
        let mut counts = Counts::default();
        let iter = count_stmt.query_map([d.id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for item in iter {
            let (status, n) = item?;
            counts.add(&status, n);
        }
        d.total = counts.total();
        d.manager = mgr_stmt
            .query_row([d.id], |r| {
                Ok(AgentLite {
                    id: r.get(0)?,
                    slug: r.get(1)?,
                    name: r.get(2)?,
                    role: r.get(3)?,
                })
            })
            .optional()?;
        d.counts = counts;
        out.push(d);
    }
    Ok(out)
}

pub fn division_by_slug(conn: &Connection, slug: &str) -> Result<Option<Division>> {
    Ok(divisions(conn)?.into_iter().find(|d| d.slug == slug))
}

pub fn division_by_id(conn: &Connection, id: i64) -> Result<Option<Division>> {
    Ok(divisions(conn)?.into_iter().find(|d| d.id == id))
}

/// Pause or resume a division. A paused division starts no new triage and no new
/// worker claims; tickets already `in_progress` keep running to completion.
/// Returns rows changed (0 = unknown slug).
pub fn set_division_paused(conn: &Connection, slug: &str, paused: bool) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE divisions SET paused = ?1 WHERE slug = ?2",
        rusqlite::params![i64::from(paused), slug],
    )?)
}

pub fn paused_division_ids(conn: &Connection) -> Result<std::collections::HashSet<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM divisions WHERE paused = 1")?;
    let ids = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<std::collections::HashSet<i64>>>()?;
    Ok(ids)
}

pub fn create_division(
    conn: &Connection,
    slug: &str,
    name: &str,
    description: &str,
    project_path: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO divisions (slug, name, description, project_path, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![slug, name, description, project_path, now()],
    )?;
    Ok(conn.last_insert_rowid())
}

// ---------------------------------------------------------------- agents

pub fn agents(conn: &Connection, division_id: Option<i64>) -> Result<Vec<Agent>> {
    let sql = match division_id {
        Some(_) => format!("{AGENT_SELECT} WHERE a.division_id = ?1 ORDER BY a.role DESC, a.id"),
        None => format!("{AGENT_SELECT} ORDER BY a.division_id, a.role DESC, a.id"),
    };
    let mut stmt = conn.prepare(&sql)?;
    let map = |r: &Row| map_agent(r);
    let out = match division_id {
        Some(id) => stmt
            .query_map([id], map)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        None => stmt
            .query_map([], map)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    Ok(out)
}

pub fn agent_by_slug(conn: &Connection, slug: &str) -> Result<Option<Agent>> {
    let sql = format!("{AGENT_SELECT} WHERE a.slug = ?1");
    Ok(conn.query_row(&sql, [slug], map_agent).optional()?)
}

pub fn workers_for(conn: &Connection, division_id: i64) -> Result<Vec<Agent>> {
    let sql = format!(
        "{AGENT_SELECT} WHERE a.division_id = ?1 AND a.role = 'worker' AND a.enabled = 1 ORDER BY a.id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let out = stmt
        .query_map([division_id], map_agent)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

pub fn manager_for(conn: &Connection, division_id: i64) -> Result<Option<Agent>> {
    let sql = format!("{AGENT_SELECT} WHERE a.division_id = ?1 AND a.role = 'manager'");
    Ok(conn.query_row(&sql, [division_id], map_agent).optional()?)
}

#[allow(clippy::too_many_arguments)]
pub fn create_agent(
    conn: &Connection,
    slug: &str,
    name: &str,
    role: &str,
    division_id: Option<i64>,
    adapter: &str,
    model: &str,
    system_hint: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO agents (slug, name, role, division_id, adapter, model, system_hint, enabled, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, 'idle', ?8)
         ON CONFLICT(slug) DO UPDATE SET
            name=excluded.name, role=excluded.role, division_id=excluded.division_id,
            adapter=excluded.adapter, model=excluded.model, system_hint=excluded.system_hint",
        params![slug, name, role, division_id, adapter, model, system_hint, now()],
    )?;
    let id: i64 = conn.query_row("SELECT id FROM agents WHERE slug = ?1", [slug], |r| {
        r.get(0)
    })?;
    Ok(id)
}

#[derive(Debug, Default, Deserialize)]
pub struct AgentPatch {
    pub name: Option<String>,
    pub adapter: Option<String>,
    pub model: Option<String>,
    pub system_hint: Option<String>,
    /// A present `null` clears the profile. A missing key must leave it alone,
    /// and serde calls this only when the key is there.
    #[serde(default, deserialize_with = "null_clears")]
    pub plugin_profile: Option<String>,
    pub enabled: Option<bool>,
    pub role: Option<String>,
    pub division: Option<String>,
}

fn null_clears<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(de)?.or_else(|| Some("none".into())))
}

/// Profile names an agent may use: directories under $BUREAU_PROFILES_DIR (or
/// ~/.config/opencode/profiles) that hold an opencode.json.
pub fn profiles_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("BUREAU_PROFILES_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| std::path::PathBuf::from(home).join(".config/opencode/profiles"))
        })
}

pub fn plugin_profiles() -> Vec<String> {
    let Some(dir) = profiles_dir() else {
        return Vec::new();
    };
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            let dir = entry.path();
            dir.join("opencode.json").is_file() || dir.join("opencode/opencode.json").is_file()
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// `Ok(None)` clears the profile. Anything else must name a real profile.
pub fn validate_plugin_profile(name: &str) -> Result<Option<String>> {
    let name = match name.trim() {
        "" | "none" => return Ok(None),
        name => name,
    };
    let valid = plugin_profiles();
    // A profile is a directory name, never a path.
    if name.contains('/') || name.contains("..") || !valid.iter().any(|v| v == name) {
        let expected = if valid.is_empty() {
            "none found".to_string()
        } else {
            valid.join(", ")
        };
        anyhow::bail!("unknown plugin profile '{name}': expected one of {expected} (or 'none' to clear)");
    }
    Ok(Some(name.to_string()))
}

/// One UPDATE for every optional field, so a role change and a division move
/// cannot half-apply. The partial unique index on managers is the only
/// constraint a patch can hit; it is reported as a plain API error.
pub fn update_agent(conn: &Connection, slug: &str, p: &AgentPatch) -> Result<()> {
    if let Some(role) = &p.role
        && !matches!(role.as_str(), "manager" | "worker")
    {
        anyhow::bail!("invalid role '{}': expected 'manager' or 'worker'", role);
    }
    let division_id = match &p.division {
        Some(div) => Some(
            division_by_slug(conn, div)?
                .ok_or_else(|| anyhow::anyhow!("division {} not found", div))?
                .id,
        ),
        None => None,
    };
    let profile = match &p.plugin_profile {
        Some(name) => Some(validate_plugin_profile(name)?),
        None => None,
    };
    let changed = conn
        .execute(
            "UPDATE agents SET
                name        = COALESCE(?1, name),
                adapter     = COALESCE(?2, adapter),
                model       = COALESCE(?3, model),
                system_hint = COALESCE(?4, system_hint),
                enabled     = COALESCE(?5, enabled),
                role        = COALESCE(?6, role),
                division_id = COALESCE(?7, division_id),
                plugin_profile = CASE WHEN ?9 THEN ?10 ELSE plugin_profile END
             WHERE slug = ?8",
            params![
                &p.name,
                &p.adapter,
                &p.model,
                &p.system_hint,
                p.enabled.map(|b| b as i64),
                &p.role,
                division_id,
                slug,
                profile.is_some(),
                profile.flatten(),
            ],
        )
        .map_err(|e| {
            if e.to_string()
                .contains("UNIQUE constraint failed: agents.division_id")
            {
                anyhow::anyhow!(
                    "division already has a manager; demote it before promoting {slug}"
                )
            } else {
                e.into()
            }
        })?;
    if changed == 0 {
        anyhow::bail!("agent {} not found", slug);
    }
    Ok(())
}

/// Per-agent skill/plugin selection. `inherit` means no row and today's
/// behaviour: opencode's own defaults. Names are validated against discovery
/// by the API; this layer rejects invalid modes and blank names.
#[derive(Debug, Clone, Serialize)]
pub struct AgentToggles {
    pub mode: String,
    pub skills: Vec<String>,
    pub plugins: Vec<String>,
}

impl Default for AgentToggles {
    fn default() -> Self {
        Self {
            mode: "inherit".into(),
            skills: Vec::new(),
            plugins: Vec::new(),
        }
    }
}

/// What a run actually loaded, from opencode's own log. `honoured` is NULL
/// when the log cannot prove either way.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LastRun {
    pub honoured: Option<bool>,
    pub evidence: Vec<String>,
    pub at: String,
    pub reason: Option<String>,
}

pub fn record_agent_run(conn: &Connection, slug: &str, run: &LastRun, mode: &str) -> Result<()> {
    let agent_id: i64 = conn
        .query_row("SELECT id FROM agents WHERE slug = ?1", [slug], |r| r.get(0))
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("agent {} not found", slug))?;
    conn.execute(
        "INSERT INTO agent_runs (agent_id, at, mode, honoured, evidence, reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(agent_id) DO UPDATE SET
            at=excluded.at, mode=excluded.mode, honoured=excluded.honoured,
            evidence=excluded.evidence, reason=excluded.reason",
        params![
            agent_id,
            run.at,
            mode,
            run.honoured,
            serde_json::to_string(&run.evidence)?,
            run.reason,
        ],
    )?;
    Ok(())
}

pub fn last_agent_run(conn: &Connection, slug: &str) -> Result<Option<LastRun>> {
    let row: Option<(String, Option<i64>, String, Option<String>)> = conn
        .query_row(
            "SELECT r.at, r.honoured, r.evidence, r.reason
             FROM agent_runs r JOIN agents a ON a.id = r.agent_id
             WHERE a.slug = ?1",
            [slug],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    Ok(row.map(|(at, honoured, evidence, reason)| LastRun {
        honoured: honoured.map(|v| v != 0),
        evidence: serde_json::from_str(&evidence).unwrap_or_default(),
        at,
        reason,
    }))
}

pub fn agent_toggles(conn: &Connection, slug: &str) -> Result<AgentToggles> {
    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT t.mode, t.skills, t.plugins
             FROM agent_toggles t JOIN agents a ON a.id = t.agent_id
             WHERE a.slug = ?1",
            [slug],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((mode, skills, plugins)) = row else {
        return Ok(AgentToggles {
            mode: "inherit".into(),
            skills: Vec::new(),
            plugins: Vec::new(),
        });
    };
    Ok(AgentToggles {
        mode,
        skills: serde_json::from_str(&skills).unwrap_or_default(),
        plugins: serde_json::from_str(&plugins).unwrap_or_default(),
    })
}

fn toggle_names(kind: &str, names: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("{kind} names must not be empty");
        }
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

pub fn set_agent_toggles(
    conn: &Connection,
    slug: &str,
    mode: &str,
    skills: &[String],
    plugins: &[String],
) -> Result<()> {
    if !matches!(mode, "inherit" | "explicit") {
        anyhow::bail!("invalid mode '{mode}': expected 'inherit' or 'explicit'");
    }
    let skills = toggle_names("skill", skills)?;
    let plugins = toggle_names("plugin", plugins)?;
    let agent_id: i64 = conn
        .query_row("SELECT id FROM agents WHERE slug = ?1", [slug], |r| r.get(0))
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("agent {} not found", slug))?;
    conn.execute(
        "INSERT INTO agent_toggles (agent_id, mode, skills, plugins, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(agent_id) DO UPDATE SET
            mode=excluded.mode, skills=excluded.skills,
            plugins=excluded.plugins, updated_at=excluded.updated_at",
        params![
            agent_id,
            mode,
            serde_json::to_string(&skills)?,
            serde_json::to_string(&plugins)?,
            now(),
        ],
    )?;
    Ok(())
}

/// Delete an agent row. Call `release_agent_tickets` first: the foreign key
/// would clear `assignee_id` but leave the tickets stuck in their old status.
pub fn delete_agent(conn: &Connection, slug: &str) -> Result<()> {
    let changed = conn.execute("DELETE FROM agents WHERE slug=?1", [slug])?;
    if changed == 0 {
        anyhow::bail!("agent {} not found", slug);
    }
    Ok(())
}

/// Hand an agent's open tickets to `reassign_to`, or back to triage when None.
/// Returns the number of tickets moved.
pub fn release_agent_tickets(
    conn: &Connection,
    agent_id: i64,
    reassign_to: Option<i64>,
) -> Result<usize> {
    let moved = conn.execute(
        "UPDATE tickets SET
            assignee_id = ?1,
            status      = CASE
                WHEN ?1 IS NULL THEN 'new'
                WHEN status IN ('triaged','in_progress') THEN 'queued'
                ELSE status
            END,
            lease_until = NULL,
            updated_at  = ?2
         WHERE assignee_id = ?3
           AND status IN ('triaged','queued','in_progress','review')",
        params![reassign_to, now(), agent_id],
    )?;
    Ok(moved)
}

pub fn set_agent_status(conn: &Connection, id: i64, status: &str) -> Result<()> {
    conn.execute(
        "UPDATE agents SET status=?1 WHERE id=?2",
        params![status, id],
    )?;
    Ok(())
}

/// Returns true when the agent moved from idle to running.
pub fn try_start_agent(conn: &Connection, id: i64) -> Result<bool> {
    let n = conn.execute(
        "UPDATE agents SET status='running' WHERE id=?1 AND status='idle'",
        [id],
    )?;
    Ok(n == 1)
}

// ---------------------------------------------------------------- tickets

#[allow(clippy::too_many_arguments)]
pub fn create_ticket(
    conn: &Connection,
    division_id: i64,
    parent_id: Option<i64>,
    title: &str,
    body: &str,
    priority: i64,
    assignee_id: Option<i64>,
    created_by: &str,
    status: &str,
) -> Result<Ticket> {
    let ts = now();
    conn.execute(
        "INSERT INTO tickets (ref, division_id, parent_id, title, body, status, priority,
                              assignee_id, created_by, created_at, updated_at)
         VALUES (NULL, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
        params![
            division_id,
            parent_id,
            title,
            body,
            status,
            priority.clamp(0, 4),
            assignee_id,
            created_by,
            ts
        ],
    )?;
    let id = conn.last_insert_rowid();
    let ref_ = format!("BUR-{id}");
    conn.execute("UPDATE tickets SET ref=?1 WHERE id=?2", params![ref_, id])?;
    ticket_by_id(conn, id)?.context("ticket vanished after insert")
}

pub fn ticket_by_id(conn: &Connection, id: i64) -> Result<Option<Ticket>> {
    let sql = format!("{TICKET_SELECT} WHERE t.id = ?1");
    let mut t = match conn.query_row(&sql, [id], map_ticket).optional()? {
        Some(t) => t,
        None => return Ok(None),
    };
    t.children = children_of(conn, id)?;
    Ok(Some(t))
}

pub fn ticket_by_ref(conn: &Connection, key: &str) -> Result<Option<Ticket>> {
    let key = key.trim();
    if let Ok(id) = key.parse::<i64>() {
        return ticket_by_id(conn, id);
    }
    let sql = format!("{TICKET_SELECT} WHERE t.ref = ?1");
    let mut t = match conn.query_row(&sql, [key], map_ticket).optional()? {
        Some(t) => t,
        None => return Ok(None),
    };
    t.children = children_of(conn, t.id)?;
    Ok(Some(t))
}

pub fn children_of(conn: &Connection, id: i64) -> Result<Vec<ChildRef>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, COALESCE(t.ref,''), t.title, t.status, a.slug
         FROM tickets t LEFT JOIN agents a ON a.id = t.assignee_id
         WHERE t.parent_id = ?1 ORDER BY t.id",
    )?;
    let out = stmt
        .query_map([id], |r| {
            Ok(ChildRef {
                id: r.get(0)?,
                ref_: r.get(1)?,
                title: r.get(2)?,
                status: r.get(3)?,
                assignee: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

pub fn tickets(
    conn: &Connection,
    division_id: Option<i64>,
    status: Option<&str>,
    limit: i64,
) -> Result<Vec<Ticket>> {
    let mut sql = String::from(TICKET_SELECT);
    let mut clauses = Vec::new();
    if division_id.is_some() {
        clauses.push("t.division_id = ?1");
    }
    if status.is_some() {
        clauses.push(if division_id.is_some() {
            "t.status = ?2"
        } else {
            "t.status = ?1"
        });
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY t.priority ASC, t.id ASC LIMIT ");
    sql.push_str(&limit.clamp(1, 2000).to_string());

    let mut stmt = conn.prepare(&sql)?;
    let mut out: Vec<Ticket> = match (division_id, status) {
        (Some(d), Some(s)) => stmt
            .query_map(params![d, s], map_ticket)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        (Some(d), None) => stmt
            .query_map(params![d], map_ticket)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        (None, Some(s)) => stmt
            .query_map(params![s], map_ticket)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        (None, None) => stmt
            .query_map([], map_ticket)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    for t in &mut out {
        t.children = children_of(conn, t.id)?;
    }
    Ok(out)
}

/// Tickets assigned to a manager can never be executed: managers only triage,
/// and only tickets with `assignee_id IS NULL` reach them. Put such tickets
/// back into the triage queue instead of leaving them stuck in `queued`.
pub fn requeue_manager_assignments(conn: &Connection) -> Result<usize> {
    conn.execute(
        "UPDATE tickets SET assignee_id = NULL, status = 'new', updated_at = ?1
         WHERE (status = 'queued' AND assignee_id IS NULL)
            OR (status IN ('new','queued')
                AND assignee_id IN (SELECT id FROM agents WHERE role = 'manager'))",
        [now()],
    )?;
    Ok(conn.changes() as usize)
}

pub fn set_status(conn: &Connection, id: i64, status: &str) -> Result<()> {
    conn.execute(
        "UPDATE tickets SET status=?1, updated_at=?2, lease_until=NULL WHERE id=?3",
        params![status, now(), id],
    )?;
    Ok(())
}

/// Close every non-terminal sub-ticket under `parent_id`, depth first.
///
/// A parent is the unit a human reviews: approving it must not leave its
/// children stranded in `review` forever, which is how a solo reviewer ends up
/// with a queue full of decisions they already made. Returns the refs closed.
pub fn close_subtickets(conn: &Connection, parent_id: i64) -> Result<Vec<String>> {
    let mut closed: Vec<String> = Vec::new();
    let mut frontier = vec![parent_id];
    while let Some(pid) = frontier.pop() {
        let kids: Vec<(i64, String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, ref, COALESCE(status,'') FROM tickets WHERE parent_id=?1 ORDER BY id",
            )?;
            let rows = stmt.query_map([pid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<Vec<(i64, String, String)>>>()?
        };
        for (id, ref_, status) in kids {
            frontier.push(id);
            if matches!(status.as_str(), "done" | "blocked" | "failed") {
                continue;
            }
            set_status(conn, id, "done")?;
            add_event(conn, "cascade", Some(id), None, "closed with its parent")?;
            closed.push(ref_);
        }
    }
    Ok(closed)
}

pub fn set_assignee(conn: &Connection, id: i64, agent_id: Option<i64>) -> Result<()> {
    conn.execute(
        "UPDATE tickets SET assignee_id=?1, updated_at=?2 WHERE id=?3",
        params![agent_id, now(), id],
    )?;
    Ok(())
}

pub fn set_result(conn: &Connection, id: i64, result: &str, notes: &str) -> Result<()> {
    conn.execute(
        "UPDATE tickets SET result=?1, agent_notes=?2, updated_at=?3 WHERE id=?4",
        params![result, notes, now(), id],
    )?;
    Ok(())
}

pub fn set_priority(conn: &Connection, id: i64, p: i64) -> Result<()> {
    conn.execute(
        "UPDATE tickets SET priority=?1, updated_at=?2 WHERE id=?3",
        params![p.clamp(0, 4), now(), id],
    )?;
    Ok(())
}

pub fn set_title_body(conn: &Connection, id: i64, title: &str, body: &str) -> Result<()> {
    conn.execute(
        "UPDATE tickets SET title=?1, body=?2, updated_at=?3 WHERE id=?4",
        params![title, body, now(), id],
    )?;
    Ok(())
}

/// Atomically move a ticket to in_progress, bump attempts and take a lease.
pub fn claim(conn: &Connection, id: i64, agent_id: i64, lease_secs: i64) -> Result<bool> {
    let lease = (Utc::now() + chrono::Duration::seconds(lease_secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let n = conn.execute(
        "UPDATE tickets SET status='in_progress', assignee_id=?1, attempts=attempts+1,
                            lease_until=?2, updated_at=?3
         WHERE id=?4 AND status IN ('queued','new')",
        params![agent_id, lease, now(), id],
    )?;
    Ok(n == 1)
}

pub fn recover_leases(conn: &Connection) -> Result<Vec<i64>> {
    let ts = now();
    let mut stmt = conn.prepare(
        "SELECT id, attempts FROM tickets
         WHERE status='in_progress' AND lease_until IS NOT NULL AND lease_until < ?1",
    )?;
    let expired: Vec<(i64, i64)> = stmt
        .query_map([&ts], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut touched = Vec::new();
    for (id, attempts) in expired {
        if attempts >= 3 {
            set_status(conn, id, "failed")?;
            add_comment(
                conn,
                id,
                "bureau",
                "Lease expired 3 times — giving up, needs a human.",
            )?;
        } else {
            set_status(conn, id, "queued")?;
            add_comment(
                conn,
                id,
                "bureau",
                "Lease expired — requeued for another attempt.",
            )?;
        }
        touched.push(id);
    }
    Ok(touched)
}

pub fn new_unassigned(conn: &Connection, division_id: i64) -> Result<Option<Ticket>> {
    let sql = format!(
        "{TICKET_SELECT} WHERE t.division_id = ?1 AND t.status = 'new' AND t.assignee_id IS NULL
         ORDER BY t.priority ASC, t.id ASC LIMIT 1"
    );
    let mut t = match conn.query_row(&sql, [division_id], map_ticket).optional()? {
        Some(t) => t,
        None => return Ok(None),
    };
    t.children = children_of(conn, t.id)?;
    Ok(Some(t))
}

pub fn queued_for_agent(conn: &Connection, agent_id: i64) -> Result<Option<Ticket>> {
    let sql = format!(
        "{TICKET_SELECT} WHERE t.assignee_id = ?1 AND t.status = 'queued'
         ORDER BY t.priority ASC, t.id ASC LIMIT 1"
    );
    let mut t = match conn.query_row(&sql, [agent_id], map_ticket).optional()? {
        Some(t) => t,
        None => return Ok(None),
    };
    t.children = children_of(conn, t.id)?;
    Ok(Some(t))
}

pub fn idle_workers_with_work(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT a.id FROM agents a
         JOIN tickets t ON t.assignee_id = a.id AND t.status = 'queued'
         WHERE a.enabled = 1 AND a.status = 'idle' AND a.role = 'worker'
         ORDER BY a.id",
    )?;
    let out = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(out)
}

/// Keep a parent ticket in step with its children.
/// Children waiting on a human (review) count as finished; a failed or blocked
/// child pulls the parent into the attention column instead of pretending success.
/// Parents whose children are all terminal but who never moved (older builds
/// did not roll up on failure, and a crash can skip the last transition).
/// Self-heal them every tick; returns how many parents were repaired.
pub fn repair_parent_rollups(conn: &Connection) -> Result<usize> {
    let stale: Vec<(i64, i64)> = conn
        .prepare(
            "SELECT p.id, (SELECT c2.id FROM tickets c2 WHERE c2.parent_id = p.id LIMIT 1)
             FROM tickets p
             WHERE p.status NOT IN ('review','done','blocked','failed')
               AND EXISTS (SELECT 1 FROM tickets c WHERE c.parent_id = p.id)
               AND NOT EXISTS (
                     SELECT 1 FROM tickets c3
                     WHERE c3.parent_id = p.id
                       AND c3.status NOT IN ('done','review','blocked','failed'))",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut fixed = 0;
    for (_pid, child_id) in stale {
        rollup_parent(conn, child_id)?;
        fixed += 1;
    }
    Ok(fixed)
}

pub fn rollup_parent(conn: &Connection, child_id: i64) -> Result<()> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT p.id, COALESCE(p.status,'') FROM tickets c
             JOIN tickets p ON p.id = c.parent_id WHERE c.id=?1",
            [child_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((pid, parent_status)) = row else {
        return Ok(());
    };
    if matches!(
        parent_status.as_str(),
        "review" | "done" | "blocked" | "failed"
    ) {
        return Ok(());
    }
    let (open, bad, total): (i64, i64, i64) = conn.query_row(
        "SELECT
            SUM(CASE WHEN status NOT IN ('done','review','blocked','failed') THEN 1 ELSE 0 END),
            SUM(CASE WHEN status IN ('blocked','failed') THEN 1 ELSE 0 END),
            COUNT(*)
         FROM tickets WHERE parent_id=?1",
        [pid],
        |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get(2)?,
            ))
        },
    )?;
    if total == 0 || open > 0 {
        return Ok(());
    }
    // Name the sub-tickets: a parent that only says "3 of 4" makes the reviewer
    // hunt for which ones, so every rollup lists refs with their final status.
    let kids: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT ref, status FROM tickets WHERE parent_id=?1 ORDER BY id")?;
        let rows = stmt
            .query_map([pid], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
        rows
    };
    let bad_lines: String = kids
        .iter()
        .filter(|(_, st)| st == "blocked" || st == "failed")
        .map(|(rf, st)| format!("• {rf} — {st}"))
        .collect::<Vec<_>>()
        .join("\n");
    let all_lines: String = kids
        .iter()
        .map(|(rf, st)| format!("• {rf} — {st}"))
        .collect::<Vec<_>>()
        .join("\n");
    if bad > 0 {
        set_status(conn, pid, "blocked")?;
        add_comment(
            conn,
            pid,
            "bureau",
            &format!("{bad} of {total} sub-tickets need attention:\n{bad_lines}"),
        )?;
        add_event(
            conn,
            "rollup",
            Some(pid),
            None,
            "sub-tickets need attention",
        )?;
    } else {
        set_status(conn, pid, "review")?;
        add_comment(
            conn,
            pid,
            "bureau",
            &format!("All {total} delegated sub-tickets are finished:\n{all_lines}\nReady for your review."),
        )?;
        add_event(conn, "rollup", Some(pid), None, "all sub-tickets finished")?;
    }
    Ok(())
}

// ---------------------------------------------------------------- comments & events

pub fn add_comment(conn: &Connection, ticket_id: i64, author: &str, body: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO comments (ticket_id, author, body, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![ticket_id, author, body, now()],
    )?;
    conn.execute(
        "UPDATE tickets SET updated_at=?1 WHERE id=?2",
        params![now(), ticket_id],
    )?;
    Ok(())
}

pub fn comments(conn: &Connection, ticket_id: i64) -> Result<Vec<Comment>> {
    let mut stmt = conn.prepare(
        "SELECT id, ticket_id, author, body, created_at FROM comments
         WHERE ticket_id=?1 ORDER BY id ASC",
    )?;
    let out = stmt
        .query_map([ticket_id], |r| {
            Ok(Comment {
                id: r.get(0)?,
                ticket_id: r.get(1)?,
                author: r.get(2)?,
                body: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

pub fn add_event(
    conn: &Connection,
    kind: &str,
    ticket_id: Option<i64>,
    agent_id: Option<i64>,
    message: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO events (kind, ticket_id, agent_id, message, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![kind, ticket_id, agent_id, message, now()],
    )?;
    Ok(())
}

pub fn events(conn: &Connection, limit: i64) -> Result<Vec<Event>> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.kind, e.ticket_id, t.ref, e.agent_id, e.message, e.created_at
         FROM events e LEFT JOIN tickets t ON t.id = e.ticket_id
         ORDER BY e.id DESC LIMIT ?1",
    )?;
    let out = stmt
        .query_map([limit.clamp(1, 500)], |r| {
            Ok(Event {
                id: r.get(0)?,
                kind: r.get(1)?,
                ticket_id: r.get(2)?,
                ticket_ref: r.get(3)?,
                agent_id: r.get(4)?,
                message: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Arc::new(Mutex::new(conn))
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bureau-db-{tag}-{}-{n}", std::process::id()))
    }

    #[test]
    fn old_database_gains_plugin_profile_column() {
        let path = tmp("migrate");
        std::fs::create_dir_all(&path).unwrap();
        let file = path.join("old.db").to_string_lossy().into_owned();
        {
            let conn = Connection::open(&file).unwrap();
            conn.execute_batch(
                "CREATE TABLE agents (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    slug TEXT NOT NULL UNIQUE,
                    name TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'worker',
                    division_id INTEGER,
                    adapter TEXT NOT NULL DEFAULT 'demo',
                    model TEXT NOT NULL DEFAULT '',
                    system_hint TEXT NOT NULL DEFAULT '',
                    enabled INTEGER NOT NULL DEFAULT 1,
                    status TEXT NOT NULL DEFAULT 'idle',
                    created_at TEXT NOT NULL);
                 INSERT INTO agents (slug, name, created_at)
                 VALUES ('old', 'Old', '2026-01-01T00:00:00Z');",
            )
            .unwrap();
        }
        let db = open(&file).unwrap();
        let conn = db.lock().unwrap();
        assert!(has_column(&conn, "agents", "plugin_profile").unwrap());
        assert_eq!(agent_by_slug(&conn, "old").unwrap().unwrap().plugin_profile, None);
        drop(conn);
        std::fs::remove_dir_all(&path).ok();
    }

    #[test]
    fn agent_without_toggles_inherits() {
        let db = mem();
        let conn = db.lock().unwrap();
        let div = create_division(&conn, "hexa", "Hexa", "", "/tmp").unwrap();
        create_agent(&conn, "spark", "Spark", "worker", Some(div), "opencode", "", "").unwrap();

        // No row == inherit == today's behaviour.
        let t = agent_toggles(&conn, "spark").unwrap();
        assert_eq!(t.mode, "inherit");
        assert!(t.skills.is_empty() && t.plugins.is_empty());

        set_agent_toggles(
            &conn,
            "spark",
            "explicit",
            &["pdf".into(), "pdf".into()],
            &["ponytail".into()],
        )
        .unwrap();
        let t = agent_toggles(&conn, "spark").unwrap();
        assert_eq!(t.mode, "explicit");
        assert_eq!(t.skills, vec!["pdf"]);
        assert_eq!(t.plugins, vec!["ponytail"]);

        let err = set_agent_toggles(&conn, "spark", "sometimes", &[], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid mode 'sometimes'"), "{err}");
        assert_eq!(agent_toggles(&conn, "spark").unwrap().mode, "explicit");
    }

    #[test]
    fn last_run_round_trips_and_survives_a_reopen() {
        let path = tmp("last-run");
        std::fs::create_dir_all(&path).unwrap();
        let file = path.join("run.db").to_string_lossy().into_owned();
        {
            let db = open(&file).unwrap();
            let conn = db.lock().unwrap();
            let div = create_division(&conn, "hexa", "Hexa", "", "/tmp").unwrap();
            create_agent(&conn, "spark", "Spark", "worker", Some(div), "opencode", "", "").unwrap();
            assert!(last_agent_run(&conn, "spark").unwrap().is_none());

            record_agent_run(
                &conn,
                "spark",
                &LastRun {
                    honoured: Some(false),
                    evidence: vec!["loading plugin superpowers".into()],
                    at: "2026-11-13T12:00:00Z".into(),
                    reason: Some("selection excludes plugin 'superpowers'".into()),
                },
                "explicit",
            )
            .unwrap();
            // A second run replaces the first, it does not append.
            record_agent_run(
                &conn,
                "spark",
                &LastRun {
                    honoured: None,
                    evidence: Vec::new(),
                    at: "2026-11-13T12:01:00Z".into(),
                    reason: Some("opencode log not found".into()),
                },
                "explicit",
            )
            .unwrap();
        }
        let db = open(&file).unwrap();
        let conn = db.lock().unwrap();
        let run = last_agent_run(&conn, "spark").unwrap().unwrap();
        assert_eq!(run.honoured, None, "unknown must not become false");
        assert!(run.evidence.is_empty());
        assert_eq!(run.at, "2026-11-13T12:01:00Z");
        assert_eq!(run.reason.as_deref(), Some("opencode log not found"));
        drop(conn);
        std::fs::remove_dir_all(&path).ok();
    }

    #[test]
    fn old_database_gains_agent_toggles_without_touching_agents() {
        let path = tmp("migrate-toggles");
        std::fs::create_dir_all(&path).unwrap();
        let file = path.join("old.db").to_string_lossy().into_owned();
        {
            let conn = Connection::open(&file).unwrap();
            conn.execute_batch(
                "CREATE TABLE agents (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    slug TEXT NOT NULL UNIQUE,
                    name TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'worker',
                    division_id INTEGER,
                    adapter TEXT NOT NULL DEFAULT 'demo',
                    model TEXT NOT NULL DEFAULT '',
                    system_hint TEXT NOT NULL DEFAULT '',
                    enabled INTEGER NOT NULL DEFAULT 1,
                    status TEXT NOT NULL DEFAULT 'idle',
                    created_at TEXT NOT NULL);
                 INSERT INTO agents (slug, name, created_at)
                 VALUES ('old', 'Old', '2026-01-01T00:00:00Z');",
            )
            .unwrap();
        }
        let db = open(&file).unwrap();
        let conn = db.lock().unwrap();
        let table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_toggles'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(table, 1, "agent_toggles must be created on an old database");
        assert_eq!(agent_by_slug(&conn, "old").unwrap().unwrap().name, "Old");
        assert_eq!(agent_toggles(&conn, "old").unwrap().mode, "inherit");
        drop(conn);
        std::fs::remove_dir_all(&path).ok();
    }

    #[test]
    fn plugin_profile_sets_clears_and_rejects_unknown_names() {
        let profiles = tmp("profiles");
        for name in ["worker-lite", "worker-full", "manager-bare"] {
            let dir = profiles.join(name).join("opencode");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("opencode.json"), "{}").unwrap();
        }
        // SAFETY: no other unit test reads this variable.
        unsafe { std::env::set_var("BUREAU_PROFILES_DIR", &profiles) };

        let db = mem();
        let conn = db.lock().unwrap();
        let div = create_division(&conn, "hexa", "Hexa", "", "/tmp").unwrap();
        create_agent(&conn, "spark", "Spark", "worker", Some(div), "opencode", "", "").unwrap();
        let set = |name: &str| AgentPatch {
            plugin_profile: Some(name.into()),
            ..Default::default()
        };

        update_agent(&conn, "spark", &set("worker-lite")).unwrap();
        assert_eq!(
            agent_by_slug(&conn, "spark").unwrap().unwrap().plugin_profile.as_deref(),
            Some("worker-lite")
        );
        update_agent(&conn, "spark", &set("none")).unwrap();
        assert_eq!(agent_by_slug(&conn, "spark").unwrap().unwrap().plugin_profile, None);
        update_agent(&conn, "spark", &set("worker-full")).unwrap();
        update_agent(&conn, "spark", &set("")).unwrap();
        assert_eq!(agent_by_slug(&conn, "spark").unwrap().unwrap().plugin_profile, None);

        let err = update_agent(&conn, "spark", &set("nope")).unwrap_err().to_string();
        assert!(
            err.contains(
                "unknown plugin profile 'nope': expected one of manager-bare, worker-full, worker-lite (or 'none' to clear)"
            ),
            "{err}"
        );
        drop(conn);
        std::fs::remove_dir_all(&profiles).ok();
    }

    #[test]
    fn ticket_refs_are_sequential_and_children_rollup() {
        let db = mem();
        let conn = db.lock().unwrap();
        let div = create_division(&conn, "hexa", "Hexa", "", "/tmp").unwrap();
        let w = create_agent(
            &conn,
            "builder",
            "Builder",
            "worker",
            Some(div),
            "demo",
            "",
            "",
        )
        .unwrap();
        let parent =
            create_ticket(&conn, div, None, "Ship UI", "", 2, None, "human", "new").unwrap();
        assert_eq!(parent.ref_, "BUR-1");
        let child = create_ticket(
            &conn,
            div,
            Some(parent.id),
            "Do the thing",
            "",
            2,
            Some(w),
            "manager",
            "queued",
        )
        .unwrap();
        assert_eq!(child.ref_, "BUR-2");
        assert_eq!(children_of(&conn, parent.id).unwrap().len(), 1);

        assert!(claim(&conn, child.id, w, 60).unwrap());
        assert!(
            !claim(&conn, child.id, w, 60).unwrap(),
            "double claim must fail"
        );
        set_status(&conn, child.id, "done").unwrap();
        rollup_parent(&conn, child.id).unwrap();
        assert_eq!(
            ticket_by_id(&conn, parent.id).unwrap().unwrap().status,
            "review"
        );
        // The rollup must name its sub-tickets, so the reviewer can jump to them.
        let note = comments(&conn, parent.id).unwrap();
        let last = note.last().unwrap().body.clone();
        assert!(last.contains("BUR-2 — done"), "{last}");
        assert!(last.contains("Ready for your review"), "{last}");
    }

    #[test]
    fn closing_a_parent_closes_its_subtickets() {
        let db = mem();
        let conn = db.lock().unwrap();
        let div = create_division(&conn, "hexa", "Hexa", "", "/tmp").unwrap();
        let w = create_agent(&conn, "builder", "B", "worker", Some(div), "demo", "", "").unwrap();
        let parent = create_ticket(&conn, div, None, "Feature", "", 1, None, "human", "review").unwrap();
        let a = create_ticket(&conn, div, Some(parent.id), "part a", "", 1, Some(w), "manager", "review").unwrap();
        let b = create_ticket(&conn, div, Some(parent.id), "part b", "", 1, Some(w), "manager", "done").unwrap();
        let grand = create_ticket(&conn, div, Some(a.id), "detail", "", 1, Some(w), "manager", "review").unwrap();

        let closed = close_subtickets(&conn, parent.id).unwrap();
        assert_eq!(closed, vec![a.ref_.clone(), grand.ref_.clone()], "only open children get closed");
        for id in [a.id, grand.id, b.id] {
            assert_eq!(ticket_by_id(&conn, id).unwrap().unwrap().status, "done");
        }
        assert_eq!(ticket_by_id(&conn, parent.id).unwrap().unwrap().status, "review");
        // idempotent: a second accept closes nothing
        assert!(close_subtickets(&conn, parent.id).unwrap().is_empty());
    }

    #[test]
    fn blocked_rollup_names_only_the_failing_children() {
        let db = mem();
        let conn = db.lock().unwrap();
        let div = create_division(&conn, "hexa", "Hexa", "", "/tmp").unwrap();
        let w = create_agent(&conn, "builder", "B", "worker", Some(div), "demo", "", "",).unwrap();
        let parent = create_ticket(&conn, div, None, "Ship", "", 2, None, "human", "new").unwrap();
        let ok = create_ticket(&conn, div, Some(parent.id), "fine", "", 2, Some(w), "manager", "queued").unwrap();
        let bad = create_ticket(&conn, div, Some(parent.id), "broken", "", 2, Some(w), "manager", "queued").unwrap();
        set_status(&conn, ok.id, "review").unwrap();
        set_status(&conn, bad.id, "failed").unwrap();
        rollup_parent(&conn, bad.id).unwrap();
        assert_eq!(ticket_by_id(&conn, parent.id).unwrap().unwrap().status, "blocked");
        let note = comments(&conn, parent.id).unwrap().last().unwrap().body.clone();
        assert!(note.contains("BUR-3 — failed"), "{note}");
        assert!(!note.contains("BUR-2"), "passing children must not be listed: {note}");
    }

    #[test]
    fn manager_is_unique_per_division() {
        let db = mem();
        let conn = db.lock().unwrap();
        let div = create_division(&conn, "x", "X", "", "/tmp").unwrap();
        create_agent(&conn, "m1", "M1", "manager", Some(div), "demo", "", "").unwrap();
        let err = create_agent(&conn, "m2", "M2", "manager", Some(div), "demo", "", "");
        assert!(err.is_err());
    }

    #[test]
    fn repair_rolls_up_parents_stranded_by_older_builds() {
        let db = mem();
        let conn = db.lock().unwrap();
        let div = create_division(&conn, "hexa", "Hexa", "", "/tmp").unwrap();
        let parent =
            create_ticket(&conn, div, None, "Parent", "", 2, None, "human", "new").unwrap();
        create_ticket(
            &conn,
            div,
            Some(parent.id),
            "Child",
            "",
            2,
            None,
            "manager",
            "failed",
        )
        .unwrap();
        assert_eq!(
            ticket_by_id(&conn, parent.id).unwrap().unwrap().status,
            "new",
            "a stale parent stays untouched until the repair pass runs"
        );
        assert_eq!(repair_parent_rollups(&conn).unwrap(), 1);
        assert_eq!(
            ticket_by_id(&conn, parent.id).unwrap().unwrap().status,
            "blocked"
        );
        // Idempotent: nothing left to repair.
        assert_eq!(repair_parent_rollups(&conn).unwrap(), 0);
    }
}
