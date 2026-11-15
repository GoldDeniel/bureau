//! HTTP API + the single-page board UI.
use crate::db::{self, AgentPatch, Db, STATUSES};
use crate::discovery;
use crate::dispatch;
use crate::live;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
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
    /// Most sub-tickets a manager may create from one ticket.
    pub max_subtasks: usize,
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
        .route(
            "/api/divisions/{slug}/pause",
            post(pause_division).delete(resume_division),
        )
        .route("/api/agents", get(list_agents).post(create_agent))
        .route(
            "/api/agents/{slug}",
            get(agent_detail).patch(patch_agent).delete(delete_agent),
        )
        .route(
            "/api/agents/{slug}/toggles",
            put(update_agent_toggles).patch(update_agent_toggles),
        )
        .route("/api/catalog", get(get_catalog))
        .route(
            "/api/agents/{slug}/skills",
            get(agent_skills)
                .put(update_agent_skills)
                .patch(update_agent_skills),
        )
        .route("/api/discovery", get(get_discovery))
        .route("/api/discovery/refresh", post(refresh_discovery))
        .route("/api/tickets", get(list_tickets).post(create_ticket))
        .route("/api/tickets/{key}", get(get_ticket).patch(patch_ticket))
        .route("/api/tickets/{key}/comment", post(add_comment))
        .route("/api/tickets/{key}/log", get(ticket_log))
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

pub struct ApiError {
    code: Option<StatusCode>,
    err: anyhow::Error,
    /// Extra fields merged into the JSON body, such as the valid names.
    extra: Option<Value>,
}

impl ApiError {
    /// An error with a chosen status. Everything else keeps the 404/400 mapping.
    pub fn status(code: StatusCode, msg: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            err: anyhow::anyhow!(msg.into()),
            extra: None,
        }
    }

    /// A rejected name, with the full list the client may use instead.
    fn unknown(kind: &str, name: &str, valid: &[String]) -> Self {
        Self {
            code: Some(StatusCode::BAD_REQUEST),
            err: anyhow::anyhow!("unknown {kind} '{name}'"),
            extra: Some(json!({ "valid": valid })),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let msg = format!("{:#}", self.err);
        let code = self.code.unwrap_or(if msg.contains("not found") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_REQUEST
        });
        let mut body = json!({ "error": msg });
        if let (Some(Value::Object(extra)), Value::Object(map)) = (self.extra, &mut body) {
            map.extend(extra);
        }
        (code, Json(body)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self {
            code: None,
            err: e.into(),
            extra: None,
        }
    }
}

macro_rules! ticket_or_404 {
    ($conn:expr, $key:expr) => {
        db::ticket_by_ref(&$conn, &$key)?
            .ok_or_else(|| ApiError::from(anyhow::anyhow!("ticket {} not found", $key)))?
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

/// Pause: no new triage, no new worker claims; in-flight tickets finish.
async fn pause_division(
    State(st): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Value>, ApiError> {
    set_paused(&st, &slug, true)
}

async fn resume_division(
    State(st): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Value>, ApiError> {
    set_paused(&st, &slug, false)
}

fn set_paused(st: &AppState, slug: &str, paused: bool) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    if db::set_division_paused(&conn, slug, paused)? == 0 {
        return Err(ApiError::from(anyhow::anyhow!("division '{slug}' not found")));
    }
    let division = db::divisions(&conn)?
        .into_iter()
        .find(|d| d.slug == slug);
    Ok(Json(json!({ "division": division })))
}

// ---------------------------------------------------------------- agents

async fn list_agents(State(st): State<AppState>) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    Ok(Json(json!({
        "agents": db::agents(&conn, None)?,
        "plugin_profiles": db::plugin_profiles(),
    })))
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

/// Who is acting, from `X-Bureau-Agent`. No header means the human/CLI surface.
/// A named agent must be a manager and stay inside its own division.
fn require_manager_scope(
    conn: &rusqlite::Connection,
    headers: &HeaderMap,
    target: &db::Agent,
) -> Result<(), ApiError> {
    let Some(raw) = headers.get("x-bureau-agent") else {
        return Ok(());
    };
    let acting = raw.to_str().unwrap_or("").trim();
    let actor = db::agent_by_slug(conn, acting)?.ok_or_else(|| {
        ApiError::status(
            StatusCode::FORBIDDEN,
            format!("acting agent '{}' not found", acting),
        )
    })?;
    if actor.role != "manager" {
        return Err(ApiError::status(
            StatusCode::FORBIDDEN,
            format!("agent {} is not a manager", actor.slug),
        ));
    }
    if actor.division_id.is_none() || actor.division_id != target.division_id {
        return Err(ApiError::status(
            StatusCode::FORBIDDEN,
            format!(
                "manager {} (division {}) cannot manage agent {} (division {})",
                actor.slug,
                division_label(conn, actor.division_id),
                target.slug,
                division_label(conn, target.division_id),
            ),
        ));
    }
    Ok(())
}

fn division_label(conn: &rusqlite::Connection, id: Option<i64>) -> String {
    id.and_then(|id| db::division_by_id(conn, id).ok().flatten())
        .map(|d| d.name)
        .unwrap_or_else(|| "none".into())
}

async fn patch_agent(
    State(st): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Json(mut patch): Json<AgentPatch>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    let target = db::agent_by_slug(&conn, &slug)?
        .ok_or_else(|| anyhow::anyhow!("agent {} not found", slug))?;
    require_manager_scope(&conn, &headers, &target)?;
    if let Some(profile) = &patch.plugin_profile {
        patch.plugin_profile = Some(profile.trim().to_string());
    }
    db::update_agent(&conn, &slug, &patch)?;
    Ok(Json(json!({ "agent": db::agent_by_slug(&conn, &slug)? })))
}

/// Entries the board renders for one toggle list. `inherit` shows the whole
/// discovered default set; `explicit` shows exactly what this agent declared,
/// marking anything no longer installed as unavailable.
fn toggle_entries(discovered: &[discovery::Item], selected: &[String], mode: &str) -> Vec<Value> {
    fn entry(name: &str, origin: &str, enabled: bool, reason: Value) -> Value {
        json!({ "name": name, "origin": origin, "enabled": enabled, "reason": reason })
    }
    if mode == "inherit" {
        return discovered
            .iter()
            .map(|item| entry(&item.name, "inherit", true, Value::Null))
            .collect();
    }
    selected
        .iter()
        .map(|name| {
            if discovered.iter().any(|item| &item.name == name) {
                entry(name, "explicit", true, Value::Null)
            } else {
                entry(
                    name,
                    "unavailable",
                    false,
                    json!("declared but not installed"),
                )
            }
        })
        .collect()
}

fn agent_detail_json(conn: &rusqlite::Connection, slug: &str) -> Result<Value, ApiError> {
    let agent = db::agent_by_slug(conn, slug)?
        .ok_or_else(|| anyhow::anyhow!("agent {} not found", slug))?;
    let selected = db::agent_toggles(conn, slug)?;
    let found = discovery::get();
    let mode = selected.mode.clone();
    Ok(json!({
        "agent": agent,
        "toggles": {
            "mode": mode,
            "skills": toggle_entries(&found.skills, &selected.skills, &selected.mode),
            "plugins": toggle_entries(&found.plugins, &selected.plugins, &selected.mode),
            "discovery": {
                "skills_at": found.skills_at,
                "plugins_at": found.plugins_at,
                "skills_source": found.skills_source,
                "stale": found.stale(),
            },
        },
        "last_run": db::last_agent_run(conn, slug)?,
    }))
}

async fn agent_detail(
    State(st): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    let target = db::agent_by_slug(&conn, &slug)?
        .ok_or_else(|| anyhow::anyhow!("agent {} not found", slug))?;
    require_manager_scope(&conn, &headers, &target)?;
    Ok(Json(agent_detail_json(&conn, &slug)?))
}

#[derive(Deserialize)]
struct ToggleBody {
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    plugins: Vec<String>,
    #[serde(default = "default_toggle_mode")]
    mode: String,
}

fn default_toggle_mode() -> String {
    "explicit".into()
}

/// PUT and PATCH both land here. An unknown name is rejected with the full
/// valid list and nothing is written.
async fn update_agent_toggles(
    State(st): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ToggleBody>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    let target = db::agent_by_slug(&conn, &slug)?
        .ok_or_else(|| anyhow::anyhow!("agent {} not found", slug))?;
    require_manager_scope(&conn, &headers, &target)?;

    let mode = body.mode.trim();
    if !matches!(mode, "inherit" | "explicit") {
        return Err(anyhow::anyhow!(
            "invalid mode '{mode}': expected 'inherit' or 'explicit'"
        )
        .into());
    }
    // A write must validate against real discovery, so wait for a cold cache.
    let found = discovery::cached().unwrap_or_else(discovery::refresh);
    let skills: Vec<String> = body.skills.iter().map(|s| s.trim().to_string()).collect();
    let plugins: Vec<String> = body.plugins.iter().map(|s| s.trim().to_string()).collect();
    let valid_skills: Vec<String> = found.skills.iter().map(|i| i.name.clone()).collect();
    let valid_plugins: Vec<String> = found.plugins.iter().map(|i| i.name.clone()).collect();
    for name in &skills {
        if !valid_skills.contains(name) {
            return Err(ApiError::unknown("skill", name, &valid_skills));
        }
    }
    for name in &plugins {
        if !valid_plugins.contains(name) {
            return Err(ApiError::unknown("plugin", name, &valid_plugins));
        }
    }
    db::set_agent_toggles(&conn, &slug, mode, &skills, &plugins)?;
    Ok(Json(agent_detail_json(&conn, &slug)?))
}

/// The full discovered catalog, independent of any agent.
async fn get_catalog() -> Json<Value> {
    Json(serde_json::to_value(discovery::catalog()).unwrap_or_else(|_| json!({})))
}

fn catalog_names(items: &[discovery::CatalogItem]) -> Vec<String> {
    items.iter().map(|item| item.name.clone()).collect()
}

/// Catalog rows plus this agent's overrides. Every discovered item appears;
/// an override changes `enabled` and marks `origin`. Saved names that are no
/// longer installed stay visible as `unavailable` instead of vanishing.
fn effective_entries(catalog: &[discovery::CatalogItem], overrides: &[db::Toggle]) -> Vec<Value> {
    let mut entries: Vec<Value> = catalog
        .iter()
        .map(|item| {
            let over = overrides.iter().find(|o| o.name == item.name);
            json!({
                "name": item.name,
                "source": item.source,
                "description": item.description,
                "default_enabled": item.default_enabled,
                "enabled": over.map(|o| o.enabled).unwrap_or(item.default_enabled),
                "origin": if over.is_some() { "override" } else { "default" },
            })
        })
        .collect();
    for over in overrides {
        if catalog.iter().any(|item| item.name == over.name) {
            continue;
        }
        entries.push(json!({
            "name": over.name,
            "source": over.source,
            "description": Value::Null,
            "default_enabled": false,
            "enabled": false,
            "origin": "unavailable",
            "reason": "declared but not installed",
        }));
    }
    entries
}

fn skill_state_json(conn: &rusqlite::Connection, slug: &str) -> Result<Value, ApiError> {
    let agent = db::agent_by_slug(conn, slug)?
        .ok_or_else(|| anyhow::anyhow!("agent {} not found", slug))?;
    let found = discovery::catalog();
    Ok(json!({
        "agent": agent.slug,
        "skills": effective_entries(&found.skills, &db::agent_skill_overrides(conn, slug)?),
        "plugins": effective_entries(&found.plugins, &db::agent_plugin_overrides(conn, slug)?),
        "reason": found.reason,
    }))
}

async fn agent_skills(
    State(st): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let conn = st.db.lock().unwrap();
    let target = db::agent_by_slug(&conn, &slug)?
        .ok_or_else(|| anyhow::anyhow!("agent {} not found", slug))?;
    require_manager_scope(&conn, &headers, &target)?;
    Ok(Json(skill_state_json(&conn, &slug)?))
}

/// A toggle may be `"name"` (on) or `{"name": "...", "enabled": false}`.
#[derive(Deserialize)]
#[serde(untagged)]
enum ToggleInput {
    Named { name: String, enabled: bool },
    Plain(String),
}

impl ToggleInput {
    fn into_parts(self) -> (String, bool) {
        match self {
            Self::Named { name, enabled } => (name, enabled),
            Self::Plain(name) => (name, true),
        }
    }
}

#[derive(Deserialize)]
struct SkillToggleBody {
    #[serde(default)]
    skills: Vec<ToggleInput>,
    #[serde(default)]
    plugins: Vec<ToggleInput>,
}

fn normalize_toggles(kind: &str, inputs: Vec<ToggleInput>) -> Result<Vec<db::Toggle>, ApiError> {
    let mut out: Vec<db::Toggle> = Vec::new();
    for input in inputs {
        let (name, enabled) = input.into_parts();
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(anyhow::anyhow!("{kind} names must not be empty").into());
        }
        match out.iter_mut().find(|toggle| toggle.name == name) {
            Some(existing) => existing.enabled = enabled,
            None => out.push(db::Toggle {
                name,
                enabled,
                source: String::new(),
            }),
        }
    }
    Ok(out)
}

fn toggle_message(slug: &str, skills: &[db::Toggle], plugins: &[db::Toggle]) -> String {
    let state = |enabled: bool| if enabled { "on" } else { "off" };
    let parts: Vec<String> = skills
        .iter()
        .map(|t| format!("skill {} {}", t.name, state(t.enabled)))
        .chain(
            plugins
                .iter()
                .map(|t| format!("plugin {} {}", t.name, state(t.enabled))),
        )
        .collect();
    if parts.is_empty() {
        format!("{slug} sent no skill or plugin changes")
    } else {
        format!("{} set {}", slug, parts.join(", "))
    }
}

/// PUT and PATCH both land here. Names are validated against the catalog and
/// the whole batch is one transaction; an unknown name writes nothing.
async fn update_agent_skills(
    State(st): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SkillToggleBody>,
) -> Result<Json<Value>, ApiError> {
    let mut conn = st.db.lock().unwrap();
    let target = db::agent_by_slug(&conn, &slug)?
        .ok_or_else(|| anyhow::anyhow!("agent {} not found", slug))?;
    require_manager_scope(&conn, &headers, &target)?;

    let found = discovery::catalog();
    let mut skills = normalize_toggles("skill", body.skills)?;
    let mut plugins = normalize_toggles("plugin", body.plugins)?;
    for toggle in &mut skills {
        let item = found
            .skills
            .iter()
            .find(|item| item.name == toggle.name)
            .ok_or_else(|| {
                ApiError::unknown("skill", &toggle.name, &catalog_names(&found.skills))
            })?;
        toggle.source = item.source.clone();
    }
    for toggle in &mut plugins {
        let item = found
            .plugins
            .iter()
            .find(|item| item.name == toggle.name)
            .ok_or_else(|| {
                ApiError::unknown("plugin", &toggle.name, &catalog_names(&found.plugins))
            })?;
        toggle.source = item.source.clone();
    }

    let message = toggle_message(&slug, &skills, &plugins);
    let tx = conn.transaction()?;
    db::apply_agent_toggles(&tx, &slug, &skills, &plugins)?;
    db::add_event(&tx, "skills", None, Some(target.id), &message)?;
    tx.commit()?;
    Ok(Json(skill_state_json(&conn, &slug)?))
}

async fn get_discovery() -> Json<Value> {
    Json(discovery::get().to_json())
}async fn refresh_discovery() -> Json<Value> {
    Json(discovery::refresh().to_json())
}

#[derive(Deserialize)]
struct DeleteAgentQuery {
    reassign: Option<String>,
}

/// Delete an agent. Its open tickets go to `?reassign=<slug>` or back to
/// triage; comments and events keep their history.
async fn delete_agent(
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Query(q): Query<DeleteAgentQuery>,
) -> Result<Json<Value>, ApiError> {
    let mut conn = st.db.lock().unwrap();
    let agent = db::agent_by_slug(&conn, &slug)?
        .ok_or_else(|| anyhow::anyhow!("agent {} not found", slug))?;
    if agent.status == "running" {
        return Err(
            anyhow::anyhow!("agent {} is running; wait for it to finish first", slug).into(),
        );
    }
    let reassign_to = match q.reassign.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(target) => {
            let target_agent = db::agent_by_slug(&conn, target)?
                .ok_or_else(|| anyhow::anyhow!("agent {} not found", target))?;
            if target_agent.id == agent.id {
                return Err(anyhow::anyhow!("cannot reassign {}'s tickets to itself", slug).into());
            }
            if target_agent.role != "worker" {
                return Err(anyhow::anyhow!(
                    "agent {} is a manager and cannot own tickets",
                    target
                )
                .into());
            }
            if target_agent.division_id != agent.division_id {
                return Err(anyhow::anyhow!(
                    "agent {} is not in the same division as {}",
                    target,
                    slug
                )
                .into());
            }
            Some(target_agent.id)
        }
        None => None,
    };
    let tx = conn.transaction()?;
    let reassigned = db::release_agent_tickets(&tx, agent.id, reassign_to)?;
    db::delete_agent(&tx, &slug)?;
    tx.commit()?;
    Ok(Json(json!({ "deleted": slug, "reassigned": reassigned })))
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
            // one decision closes the whole subtree: children are not separate reviews
            let closed = db::close_subtickets(&conn, ticket.id)?;
            if !closed.is_empty() {
                db::add_comment(
                    &conn,
                    ticket.id,
                    "bureau",
                    &format!(
                        "Accepted; {} sub-ticket(s) closed with it: {}",
                        closed.len(),
                        closed.join(", ")
                    ),
                )?;
            }
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

#[derive(Deserialize)]
struct LogQuery {
    #[serde(default)]
    lines: Option<usize>,
}

/// Tail of the agent's stdout/stderr for this ticket — the "chat window".
async fn ticket_log(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Query(q): Query<LogQuery>,
) -> Result<Json<Value>, ApiError> {
    let (ref_, status, path) = {
        let conn = st.db.lock().unwrap();
        let t = ticket_or_404!(conn, key);
        let dir = conn
            .path()
            .map(|p| {
                std::path::Path::new(p)
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .to_path_buf()
            })
            .unwrap_or_default();
        let path = dir.join("logs").join(format!("{}.log", t.ref_));
        (t.ref_.clone(), t.status.clone(), path)
    };
    let bytes = std::fs::read(&path).unwrap_or_default();
    let start = bytes.len().saturating_sub(256 * 1024);
    let text = String::from_utf8_lossy(&bytes[start..]);
    let want = q.lines.unwrap_or(400).clamp(10, 4000);
    let mut lines: Vec<&str> = text.lines().collect();
    if lines.len() > want {
        lines = lines.split_off(lines.len() - want);
    }
    Ok(Json(json!({
        "ref": ref_,
        "status": status,
        "exists": path.exists(),
        "bytes": bytes.len(),
        "lines": lines,
    })))
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
        let result = dispatch::tick(state.db.clone(), state.max_parallel, state.max_subtasks).await;
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
