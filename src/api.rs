//! HTTP API + the single-page board UI.
use crate::db::{self, AgentPatch, Db, STATUSES};
use crate::dispatch;
use crate::live;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub dispatching: Arc<AtomicBool>,
    pub max_parallel: usize,
    /// When set, every /api route except health requires this token.
    pub token: Option<String>,
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn router(state: AppState) -> Router {
    let auth_state = state.clone();
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/api/health", get(health))
        .route("/api/state", get(snapshot))
        .route("/api/divisions", get(list_divisions).post(create_division))
        .route("/api/agents", get(list_agents).post(create_agent))
        .route("/api/agents/{slug}", patch(patch_agent))
        .route("/api/tickets", get(list_tickets).post(create_ticket))
        .route("/api/tickets/{key}", get(get_ticket).patch(patch_ticket))
        .route("/api/tickets/{key}/comment", post(add_comment))
        .route("/api/tickets/{key}/assign", post(assign_ticket))
        .route("/api/tickets/{key}/claim", post(claim_ticket))
        .route("/api/events", get(list_events))
        .route("/api/dispatch", get(dispatch_status))
        .route("/api/dispatch/tick", post(run_tick))
        .fallback(|| async { (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))) })
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            require_token,
        ))
        .with_state(state)
}

fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn query_token(query: &str) -> Option<&str> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
}

fn cookie_token(cookie: &str) -> Option<&str> {
    cookie
        .split(';')
        .find_map(|part| part.trim().strip_prefix("bureau_token="))
}

async fn require_token(
    State(st): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(expected) = st.token.as_deref() else {
        return next.run(req).await;
    };
    let path = req.uri().path();
    // The board itself is public so it can ask for the token; health stays open for probes.
    if matches!(path, "/" | "/health" | "/api/health") {
        return next.run(req).await;
    }
    let header = req
        .headers()
        .get("x-bureau-token")
        .and_then(|v| v.to_str().ok());
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let cookie = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(cookie_token);
    let query = req.uri().query().and_then(query_token);

    if [header, bearer, cookie, query]
        .into_iter()
        .flatten()
        .any(|t| token_eq(t, expected))
    {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "a Bureau access token is required", "code": "token_required"})),
        )
            .into_response()
    }
}

async fn index() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("../assets/index.html"),
    )
        .into_response()
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "name": "bureau", "version": VERSION }))
}

// ---------------------------------------------------------------- errors

pub struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let msg = format!("{:#}", self.0);
        let code = if msg.contains("not found") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_REQUEST
        };
        (code, Json(json!({ "error": msg }))).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

macro_rules! ticket_or_404 {
    ($conn:expr, $key:expr) => {
        db::ticket_by_ref(&$conn, &$key)?
            .ok_or_else(|| ApiError(anyhow::anyhow!("ticket {} not found", $key)))?
    };
}

// ---------------------------------------------------------------- state

#[derive(Serialize)]
struct Stats {
    divisions: usize,
    agents: usize,
    tickets: i64,
    open: i64,
    needs_review: i64,
    running: i64,
}

async fn snapshot(State(st): State<AppState>) -> Result<Json<Value>, ApiError> {
    let (divisions, agents, tickets, events) = {
        let conn = st.db.lock().unwrap();
        (
            db::divisions(&conn)?,
            db::agents(&conn, None)?,
            db::tickets(&conn, None, None, 1000)?,
            db::events(&conn, 40)?,
        )
    };
    let stats = Stats {
        divisions: divisions.len(),
        agents: agents.len(),
        tickets: tickets.len() as i64,
        open: tickets
            .iter()
            .filter(|t| !matches!(t.status.as_str(), "done" | "failed"))
            .count() as i64,
        needs_review: tickets.iter().filter(|t| t.status == "review").count() as i64,
        running: agents.iter().filter(|a| a.status == "running").count() as i64,
    };
    // What is actually running right now: the agent CLIs we spawned, plus any
    // subagents/helpers they spawned themselves.
    let procs = live::processes();
    let running: Vec<Value> = tickets
        .iter()
        .filter(|t| t.status == "in_progress")
        .map(|t| {
            let kids = live::matching_under(&procs, std::process::id() as i32, &t.ref_);
            let role = agents
                .iter()
                .find(|a| Some(&a.slug) == t.assignee_slug.as_ref())
                .map(|a| a.role.clone())
                .unwrap_or_else(|| "worker".to_string());
            json!({
                "id": t.id,
                "ref": t.ref_,
                "title": t.title,
                "agent": t.assignee_slug.clone(),
                "role": role,
                "started_at": t.updated_at,
                "procs": kids,
            })
        })
        .collect();

    Ok(Json(json!({
        "version": VERSION,
        "pid": std::process::id(),
        "divisions": divisions,
        "agents": agents,
        "tickets": tickets,
        "events": events,
        "live": running,
        "stats": stats,
        "statuses": STATUSES,
        "adapters": ["demo", "ollama", "hermes", "opencode", "shell", "echo"],
        "dispatching": st.dispatching.load(Ordering::SeqCst),
    })))
}

// ---------------------------------------------------------------- divisions

async fn list_divisions(State(st): State<AppState>) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    Ok(Json(json!({ "divisions": db::divisions(&conn)? })))
}

#[derive(Deserialize)]
struct NewDivision {
    slug: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    project_path: String,
}

async fn create_division(
    State(st): State<AppState>,
    Json(body): Json<NewDivision>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    let id = db::create_division(
        &conn,
        &body.slug,
        &body.name,
        &body.description,
        &body.project_path,
    )?;
    Ok(Json(
        json!({ "id": id, "division": db::division_by_id(&conn, id)? }),
    ))
}

// ---------------------------------------------------------------- agents

async fn list_agents(State(st): State<AppState>) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    Ok(Json(json!({ "agents": db::agents(&conn, None)? })))
}

#[derive(Deserialize)]
struct NewAgent {
    slug: String,
    name: String,
    #[serde(default = "default_role")]
    role: String,
    division: String,
    #[serde(default = "default_adapter")]
    adapter: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    system_hint: String,
}

fn default_role() -> String {
    "worker".into()
}
fn default_adapter() -> String {
    "demo".into()
}

async fn create_agent(
    State(st): State<AppState>,
    Json(body): Json<NewAgent>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    let division = db::division_by_slug(&conn, &body.division)?
        .ok_or_else(|| anyhow::anyhow!("division {} not found", body.division))?;
    let id = db::create_agent(
        &conn,
        &body.slug,
        &body.name,
        if body.role == "manager" {
            "manager"
        } else {
            "worker"
        },
        Some(division.id),
        &body.adapter,
        &body.model,
        &body.system_hint,
    )?;
    Ok(Json(
        json!({ "id": id, "agent": db::agent_by_slug(&conn, &body.slug)? }),
    ))
}

async fn patch_agent(
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Json(patch): Json<AgentPatch>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    if db::agent_by_slug(&conn, &slug)?.is_none() {
        return Err(anyhow::anyhow!("agent {} not found", slug).into());
    }
    db::update_agent(&conn, &slug, &patch)?;
    Ok(Json(json!({ "agent": db::agent_by_slug(&conn, &slug)? })))
}

// ---------------------------------------------------------------- tickets

#[derive(Deserialize)]
struct TicketQuery {
    division: Option<String>,
    status: Option<String>,
    limit: Option<i64>,
}

async fn list_tickets(
    State(st): State<AppState>,
    Query(q): Query<TicketQuery>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    let division_id = match q.division.as_deref() {
        Some(slug) => Some(
            db::division_by_slug(&conn, slug)?
                .ok_or_else(|| anyhow::anyhow!("division {} not found", slug))?
                .id,
        ),
        None => None,
    };
    let tickets = db::tickets(
        &conn,
        division_id,
        q.status.as_deref(),
        q.limit.unwrap_or(500),
    )?;
    Ok(Json(json!({ "tickets": tickets, "count": tickets.len() })))
}

#[derive(Deserialize)]
struct NewTicket {
    title: String,
    #[serde(default)]
    body: String,
    division: String,
    #[serde(default = "default_priority")]
    priority: i64,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    created_by: Option<String>,
}

fn default_priority() -> i64 {
    2
}

async fn create_ticket(
    State(st): State<AppState>,
    Json(body): Json<NewTicket>,
) -> Result<Json<Value>, ApiError> {
    if body.title.trim().is_empty() {
        return Err(anyhow::anyhow!("title is required").into());
    }
    let conn = st.db.lock().unwrap();
    let division = db::division_by_slug(&conn, &body.division)?
        .ok_or_else(|| anyhow::anyhow!("division {} not found", body.division))?;
    let (assignee, handed_to) = match &body.assignee {
        Some(slug) if !slug.is_empty() => {
            let agent = db::agent_by_slug(&conn, slug)?
                .ok_or_else(|| anyhow::anyhow!("agent {} not found", slug))?;
            if agent.role == "manager" {
                // A manager is a triager, not a worker: assigning to one means
                // "let this manager break it down", so keep it unassigned.
                (None, Some(agent.slug))
            } else {
                (Some(agent.id), None)
            }
        }
        _ => (None, None),
    };
    let parent_id = match &body.parent {
        Some(p) if !p.is_empty() => Some(ticket_or_404!(conn, p.clone()).id),
        _ => None,
    };
    let status = if assignee.is_some() { "queued" } else { "new" };
    let ticket = db::create_ticket(
        &conn,
        division.id,
        parent_id,
        body.title.trim(),
        &body.body,
        body.priority,
        assignee,
        body.created_by.as_deref().unwrap_or("human"),
        status,
    )?;
    db::add_event(
        &conn,
        "created",
        Some(ticket.id),
        None,
        &format!(
            "{} opened {}",
            body.created_by.as_deref().unwrap_or("human"),
            ticket.ref_
        ),
    )?;
    if let Some(manager) = &handed_to {
        db::add_event(
            &conn,
            "assign",
            Some(ticket.id),
            None,
            &format!("{} handed to manager {} for triage", ticket.ref_, manager),
        )?;
    }
    if let Some(p) = parent_id {
        db::rollup_parent(&conn, ticket.id).ok();
        let _ = p;
    }
    Ok(Json(json!({ "ticket": ticket })))
}

async fn get_ticket(
    State(st): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    let ticket = ticket_or_404!(conn, key);
    let comments = db::comments(&conn, ticket.id)?;
    Ok(Json(json!({ "ticket": ticket, "comments": comments })))
}

#[derive(Deserialize)]
struct TicketPatch {
    status: Option<String>,
    priority: Option<i64>,
    assignee: Option<String>,
    title: Option<String>,
    body: Option<String>,
    note: Option<String>,
}

async fn patch_ticket(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Json(p): Json<TicketPatch>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    let ticket = ticket_or_404!(conn, key);

    if let Some(status) = &p.status {
        if !STATUSES.contains(&status.as_str()) {
            return Err(anyhow::anyhow!("invalid status '{}'", status).into());
        }
        db::set_status(&conn, ticket.id, status)?;
        db::add_event(
            &conn,
            "status",
            Some(ticket.id),
            None,
            &format!("{} → {}", ticket.ref_, status),
        )?;
        if status == "done" {
            db::rollup_parent(&conn, ticket.id)?;
        }
    }
    if let Some(priority) = p.priority {
        db::set_priority(&conn, ticket.id, priority)?;
    }
    if let Some(title) = &p.title {
        let body = p.body.clone().unwrap_or_else(|| ticket.body.clone());
        db::set_title_body(&conn, ticket.id, title, &body)?;
    } else if let Some(body) = &p.body {
        db::set_title_body(&conn, ticket.id, &ticket.title, body)?;
    }
    if let Some(assignee) = &p.assignee {
        assign_to(&conn, &ticket, assignee)?;
    }
    if let Some(note) = &p.note
        && !note.trim().is_empty()
    {
        db::add_comment(&conn, ticket.id, "human", note)?;
    }
    Ok(Json(
        json!({ "ticket": db::ticket_by_id(&conn, ticket.id)? }),
    ))
}

#[derive(Deserialize)]
struct CommentBody {
    body: String,
    #[serde(default)]
    author: Option<String>,
}

async fn add_comment(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Json(c): Json<CommentBody>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    let ticket = ticket_or_404!(conn, key);
    if c.body.trim().is_empty() {
        return Err(anyhow::anyhow!("comment body is required").into());
    }
    let author = c.author.unwrap_or_else(|| "human".into());
    db::add_comment(&conn, ticket.id, &author, c.body.trim())?;
    db::add_event(
        &conn,
        "comment",
        Some(ticket.id),
        None,
        &format!("{author} commented on {}", ticket.ref_),
    )?;
    Ok(Json(json!({
        "ticket": db::ticket_by_id(&conn, ticket.id)?,
        "comments": db::comments(&conn, ticket.id)?,
    })))
}

/// One place for "who owns this ticket" rules.
///
/// Workers execute, managers only triage. Assigning a manager therefore means
/// "send it back to triage" (managers only see tickets with no assignee), and
/// clearing the assignee does the same for anything not started yet.
fn assign_to(conn: &rusqlite::Connection, ticket: &db::Ticket, slug: &str) -> anyhow::Result<()> {
    let slug = slug.trim();
    if slug.is_empty() {
        db::set_assignee(conn, ticket.id, None)?;
        if matches!(ticket.status.as_str(), "new" | "triaged" | "queued") {
            db::set_status(conn, ticket.id, "new")?;
        }
        db::add_event(
            conn,
            "assign",
            Some(ticket.id),
            None,
            &format!("{} unassigned → triage", ticket.ref_),
        )?;
        return Ok(());
    }
    let agent = db::agent_by_slug(conn, slug)?
        .ok_or_else(|| anyhow::anyhow!("agent {} not found", slug))?;
    if agent.role == "manager" {
        db::set_assignee(conn, ticket.id, None)?;
        if matches!(ticket.status.as_str(), "new" | "triaged" | "queued") {
            db::set_status(conn, ticket.id, "new")?;
        }
        db::add_event(
            conn,
            "assign",
            Some(ticket.id),
            Some(agent.id),
            &format!(
                "{} handed to manager {} for triage",
                ticket.ref_, agent.slug
            ),
        )?;
    } else {
        db::set_assignee(conn, ticket.id, Some(agent.id))?;
        if matches!(ticket.status.as_str(), "new" | "triaged") {
            db::set_status(conn, ticket.id, "queued")?;
        }
        db::add_event(
            conn,
            "assign",
            Some(ticket.id),
            Some(agent.id),
            &format!("{} assigned to {}", ticket.ref_, agent.slug),
        )?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct AssignBody {
    agent: String,
}

async fn assign_ticket(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Json(a): Json<AssignBody>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    let ticket = ticket_or_404!(conn, key);
    assign_to(&conn, &ticket, &a.agent)?;
    Ok(Json(
        json!({ "ticket": db::ticket_by_id(&conn, ticket.id)? }),
    ))
}

#[derive(Deserialize)]
struct ClaimBody {
    agent: String,
}

async fn claim_ticket(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Json(c): Json<ClaimBody>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    let ticket = ticket_or_404!(conn, key);
    let agent = db::agent_by_slug(&conn, &c.agent)?
        .ok_or_else(|| anyhow::anyhow!("agent {} not found", c.agent))?;
    if !db::claim(&conn, ticket.id, agent.id, 3600)? {
        return Err(anyhow::anyhow!(
            "{} is not claimable (status {})",
            ticket.ref_,
            ticket.status
        )
        .into());
    }
    db::add_event(
        &conn,
        "claim",
        Some(ticket.id),
        Some(agent.id),
        &format!("{} claimed {} (1h lease)", agent.slug, ticket.ref_),
    )?;
    Ok(Json(
        json!({ "ticket": db::ticket_by_id(&conn, ticket.id)? }),
    ))
}

// ---------------------------------------------------------------- events & dispatch

#[derive(Deserialize)]
struct EventQuery {
    limit: Option<i64>,
}

async fn list_events(
    State(st): State<AppState>,
    Query(q): Query<EventQuery>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    Ok(Json(
        json!({ "events": db::events(&conn, q.limit.unwrap_or(60))? }),
    ))
}

async fn dispatch_status(State(st): State<AppState>) -> Json<Value> {
    Json(json!({ "running": st.dispatching.load(Ordering::SeqCst) }))
}

async fn run_tick(State(st): State<AppState>) -> Result<Json<Value>, ApiError> {
    if st.dispatching.swap(true, Ordering::SeqCst) {
        return Ok(Json(json!({ "status": "busy" })));
    }
    let state = st.clone();
    tokio::spawn(async move {
        let result = dispatch::tick(state.db.clone(), state.max_parallel).await;
        {
            let conn = state.db.lock().unwrap();
            match result {
                Ok(rep) => {
                    let msg = format!(
                        "tick: {} manager job(s), {} worker job(s), {} recovered in {}ms",
                        rep.managers_started, rep.workers_started, rep.recovered, rep.ms
                    );
                    let _ = db::add_event(&conn, "tick", None, None, &msg);
                }
                Err(e) => {
                    let _ = db::add_event(&conn, "error", None, None, &format!("tick failed: {e}"));
                }
            }
        }
        state.dispatching.store(false, Ordering::SeqCst);
    });
    Ok(Json(json!({ "status": "started" })))
}
