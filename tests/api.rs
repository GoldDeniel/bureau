//! End-to-end API tests: HTTP in, company loop, board state out.
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use bureau::api::{AppState, router};
use bureau::db::{self, Db};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tower::ServiceExt;

fn temp_db_path() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join(format!(
            "bureau-test-{}-{nanos}-{seq}.db",
            std::process::id()
        ))
        .to_string_lossy()
        .to_string()
}

fn setup() -> (Router, Db) {
    let db = db::open(&temp_db_path()).unwrap();
    {
        let conn = db.lock().unwrap();
        let d = db::create_division(&conn, "test", "Test Division", "charter", "/tmp").unwrap();
        db::create_agent(&conn, "mgr", "Manager", "manager", Some(d), "demo", "", "").unwrap();
        db::create_agent(&conn, "w1", "Worker One", "worker", Some(d), "demo", "", "").unwrap();
        db::create_agent(&conn, "w2", "Worker Two", "worker", Some(d), "demo", "", "").unwrap();
    }
    let app = router(AppState {
        db: db.clone(),
        dispatching: Arc::new(AtomicBool::new(false)),
        max_parallel: 4,
        max_subtasks: 3,
        token: None,
    });
    (app, db)
}

async fn send_with(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = req
        .header("content-type", "application/json")
        .body(
            body.map(|b| Body::from(b.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, body)
}

async fn send(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    send_with(app, method, uri, body, &[]).await
}

#[tokio::test]
async fn full_company_loop_over_http() {
    let (app, db) = setup();

    let (status, body) = send(&app, "GET", "/api/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);

    // Ticket lands unassigned → waits for the manager.
    let (status, body) = send(
        &app,
        "POST",
        "/api/tickets",
        Some(json!({"title": "Add a save system", "body": "Slots, autosave.", "division": "test", "priority": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let parent_ref = body["ticket"]["ref"].as_str().unwrap().to_string();
    assert_eq!(body["ticket"]["status"], "new");

    // One pass now drains the company: the manager triages, and the workers it
    // created are picked up in the same tick instead of waiting for the next one.
    let report = bureau::dispatch::tick(db.clone(), 4, 3).await.unwrap();
    assert_eq!(report.managers_started, 1, "{report:?}");
    assert_eq!(report.workers_started, 2, "{report:?}");
    let (_, body) = send(&app, "GET", &format!("/api/tickets/{parent_ref}"), None).await;
    let kids = body["ticket"]["children"].as_array().unwrap();
    assert!(kids.len() >= 2, "{body}");
    assert!(kids.iter().all(|k| k["status"] == "review"), "{body}");
    assert!(!body["comments"].as_array().unwrap().is_empty());
    assert_eq!(
        body["ticket"]["status"], "review",
        "parent rolls up: {body}"
    );

    // Human approves a child; parent stays in review until all children are closed.
    let child_ref = kids[0]["ref"].as_str().unwrap();
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("/api/tickets/{child_ref}"),
        Some(json!({"status": "done"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Comment + state snapshot.
    let (status, _) = send(
        &app,
        "POST",
        &format!("/api/tickets/{parent_ref}/comment"),
        Some(json!({"body": "nice work", "author": "human:you"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["divisions"][0]["slug"], "test");
    assert_eq!(body["divisions"][0]["manager"]["slug"], "mgr");
    assert!(body["stats"]["needs_review"].as_i64().unwrap() >= 1);
    assert!(body["events"].as_array().unwrap().len() >= 4);

    // Unknown ticket is a 404, not a 500.
    let (status, body) = send(&app, "GET", "/api/tickets/BUR-999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn board_ui_is_served() {
    let (app, _db) = setup();
    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers()["content-type"].to_str().unwrap().to_string();
    assert!(ct.starts_with("text/html"));
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("Bureau"));
    assert!(html.contains("/api/state"));
}

/// Static contracts for the board state/scroll work. These are cheap guards:
/// if a refactor drops the short-circuits or the URL/localStorage keys, the
/// browser behaviour silently regresses and only a real F5 would show it.
#[tokio::test]
async fn board_ui_keeps_persistence_and_poll_contracts() {
    let (app, _db) = setup();
    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    for needle in [
        "bureau_view",                     // division / done toggle / search
        "bureau_board",                    // board + column scroll
        "bureau_dr_scroll",                // drawer read position
        "S.snapJson",                      // board poll short-circuit
        "S.openJson === json",             // drawer poll short-circuit
        "scrollTo(sx, sy)",                // page scroll survives a board repaint
        "renderLive(snap.live || [])",     // live chips still tick on an unchanged poll
        "bodyHeight * 0.5",                // drawer scroll restore guarded against content loss
        "newLog.scrollTop = S.logPin.top", // agent log keeps its place
        "cm.value = draftVal",             // half-typed comment survives a poll
        "wireFolds",                       // blocks expand in place, not in a modal
        "data-fold-toggle",                // ...with a real toggle button
        "failure_totals",                  // board can group failures by kind
        "fk-rate_limit",                   // failure tags have their own palette
        "ladders",                         // per-agent fallback chains reach the UI
        "/models",                         // the ladder has its own endpoint
        "f-div",                           // agent editor can move divisions
        "ag-del",                          // agent editor has a delete button
        "?reassign=",                      // delete can hand open tickets to a teammate
        "popstate",                        // back/forward
        ".call(history", // unbound pushState/replaceState throws "Illegal invocation"
    ] {
        assert!(html.contains(needle), "board is missing `{needle}`");
    }
    // The old overlay is deliberately gone: expanding must happen in place.
    for gone in ["focus-wrap", "openFocus", "data-focus"] {
        assert!(
            !html.contains(gone),
            "board still ships `{gone}`; expansion belongs inline now"
        );
    }
}

#[tokio::test]
async fn token_gate_protects_api_but_not_board_or_health() {
    let (_, db) = setup();
    let app = router(AppState {
        db,
        dispatching: Arc::new(AtomicBool::new(false)),
        max_parallel: 4,
        max_subtasks: 3,
        token: Some("s3cret".into()),
    });

    let (status, _) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = send(&app, "GET", "/api/health", None).await;
    assert_eq!(status, StatusCode::OK);

    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );

    let (status, body) = send_with(
        &app,
        "GET",
        "/api/state",
        None,
        &[("x-bureau-token", "s3cret")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["divisions"][0]["slug"], "test");

    let (status, _) = send(&app, "GET", "/api/state?token=s3cret", None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = send_with(
        &app,
        "GET",
        "/api/state",
        None,
        &[("authorization", "Bearer wrong")],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn manager_assignments_go_back_to_triage() {
    let (app, db) = setup();

    // Creating a ticket "for" a manager means: this manager triages it.
    let (_, body) = send(
        &app,
        "POST",
        "/api/tickets",
        Some(json!({"title": "manager owns this", "division": "test", "assignee": "mgr"})),
    )
    .await;
    assert_eq!(body["ticket"]["status"], "new", "{body}");
    assert!(body["ticket"]["assignee_id"].is_null(), "{body}");

    // A worker assignment still queues directly.
    let (_, body) = send(
        &app,
        "POST",
        "/api/tickets",
        Some(json!({"title": "worker owns this", "division": "test", "assignee": "w1"})),
    )
    .await;
    assert_eq!(body["ticket"]["status"], "queued", "{body}");
    let r = body["ticket"]["ref"].as_str().unwrap().to_string();

    // Reassigning to a manager (what the board's drawer does) sends it to triage.
    let (_, body) = send(
        &app,
        "PATCH",
        &format!("/api/tickets/{r}"),
        Some(json!({"assignee": "mgr"})),
    )
    .await;
    assert_eq!(body["ticket"]["status"], "new", "{body}");
    assert!(body["ticket"]["assignee_id"].is_null(), "{body}");

    // Legacy rows stuck in `queued` with a manager are repaired by the loop.
    {
        let conn = db.lock().unwrap();
        let manager = db::agent_by_slug(&conn, "mgr").unwrap().unwrap();
        let division = db::division_by_slug(&conn, "test").unwrap().unwrap();
        let legacy = db::create_ticket(
            &conn,
            division.id,
            None,
            "legacy stuck row",
            "",
            2,
            Some(manager.id),
            "human",
            "queued",
        )
        .unwrap();
        assert_eq!(db::requeue_manager_assignments(&conn).unwrap(), 1);
        let after = db::ticket_by_id(&conn, legacy.id)
            .unwrap()
            .expect("legacy ticket exists");
        assert!(after.assignee_id.is_none(), "assignee should be cleared");
        assert_eq!(after.status, "new", "should be back in triage");
    }
}

#[tokio::test]
async fn invalid_status_is_rejected() {
    let (app, _db) = setup();
    let (_, body) = send(
        &app,
        "POST",
        "/api/tickets",
        Some(json!({"title": "x", "division": "test"})),
    )
    .await;
    let r = body["ticket"]["ref"].as_str().unwrap();
    let (status, body) = send(
        &app,
        "PATCH",
        &format!("/api/tickets/{r}"),
        Some(json!({"status": "banana"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("invalid status"));
}

#[tokio::test]
async fn patch_agent_edits_name_role_and_division() {
    let (app, db) = setup();
    {
        let conn = db.lock().unwrap();
        let other = db::create_division(&conn, "other", "Other Division", "", "/tmp").unwrap();
        db::create_agent(
            &conn,
            "mgr2",
            "Manager Two",
            "manager",
            Some(other),
            "demo",
            "",
            "",
        )
        .unwrap();
    }

    let (status, body) = send(
        &app,
        "PATCH",
        "/api/agents/w1",
        Some(json!({"name": "Renamed", "division": "other"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["agent"]["name"], "Renamed", "{body}");
    assert_eq!(body["agent"]["division_slug"], "other", "{body}");
    assert_eq!(body["agent"]["role"], "worker", "{body}");

    // "other" already has mgr2, so the one_manager_per_division index refuses.
    let (status, body) = send(
        &app,
        "PATCH",
        "/api/agents/w1",
        Some(json!({"role": "manager"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("already has a manager"),
        "{body}"
    );

    let conn = db.lock().unwrap();
    let agent = db::agent_by_slug(&conn, "w1").unwrap().unwrap();
    assert_eq!(
        agent.role, "worker",
        "failed patch must not change the role"
    );
}

#[tokio::test]
async fn patch_agent_rejects_invalid_role_and_division() {
    let (app, db) = setup();

    let (status, body) = send(
        &app,
        "PATCH",
        "/api/agents/w1",
        Some(json!({"role": "boss"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("invalid role"),
        "{body}"
    );

    let (status, body) = send(
        &app,
        "PATCH",
        "/api/agents/w1",
        Some(json!({"division": "nope"})),
    )
    .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("division nope not found"),
        "{body}"
    );

    let conn = db.lock().unwrap();
    let agent = db::agent_by_slug(&conn, "w1").unwrap().unwrap();
    assert_eq!(agent.role, "worker");
    assert_eq!(agent.division_slug.as_deref(), Some("test"));
}

#[tokio::test]
async fn delete_agent_removes_it() {
    let (app, db) = setup();

    let (status, body) = send(&app, "DELETE", "/api/agents/w2", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], "w2", "{body}");

    let (status, body) = send(&app, "GET", "/api/agents", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let slugs: Vec<&str> = body["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["slug"].as_str())
        .collect();
    assert!(!slugs.contains(&"w2"), "{body}");

    let conn = db.lock().unwrap();
    assert!(db::agent_by_slug(&conn, "w2").unwrap().is_none());
}

#[tokio::test]
async fn delete_agent_unassigns_open_tickets_and_keeps_history() {
    let (app, db) = setup();
    let (_, body) = send(
        &app,
        "POST",
        "/api/tickets",
        Some(json!({"title": "in flight", "division": "test"})),
    )
    .await;
    let ticket_ref = body["ticket"]["ref"].as_str().unwrap().to_string();

    // Two patches: assigning first lets `status` stay in_progress afterwards.
    for patch in [json!({"assignee": "w1"}), json!({"status": "in_progress"})] {
        let (status, body) = send(
            &app,
            "PATCH",
            &format!("/api/tickets/{ticket_ref}"),
            Some(patch),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    let (status, body) = send(
        &app,
        "POST",
        &format!("/api/tickets/{ticket_ref}/comment"),
        Some(json!({"body": "w1 was here", "author": "w1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ticket_id = body["ticket"]["id"].as_i64().unwrap();
    {
        let conn = db.lock().unwrap();
        let w1 = db::agent_by_slug(&conn, "w1").unwrap().unwrap();
        db::add_event(
            &conn,
            "work",
            Some(ticket_id),
            Some(w1.id),
            "w1 finished a step",
        )
        .unwrap();
    }

    let (status, body) = send(&app, "DELETE", "/api/agents/w1", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["reassigned"], 1, "{body}");

    let (status, body) = send(&app, "GET", &format!("/api/tickets/{ticket_ref}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["ticket"]["assignee_id"].is_null(), "{body}");
    assert_eq!(body["ticket"]["status"], "new", "{body}");
    let comments = body["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1, "{body}");
    assert_eq!(comments[0]["author"], "w1", "{body}");
    assert_eq!(comments[0]["body"], "w1 was here", "{body}");

    let conn = db.lock().unwrap();
    let event = db::events(&conn, 100)
        .unwrap()
        .into_iter()
        .find(|e| e.message == "w1 finished a step")
        .expect("event history survives agent deletion");
    assert!(
        event.agent_id.is_none(),
        "deleted agent must become NULL in event history"
    );
}

#[tokio::test]
async fn delete_agent_reassigns_open_tickets() {
    let (app, db) = setup();
    let (_, body) = send(
        &app,
        "POST",
        "/api/tickets",
        Some(json!({"title": "hand me over", "division": "test", "assignee": "w1"})),
    )
    .await;
    let ticket_ref = body["ticket"]["ref"].as_str().unwrap().to_string();
    assert_eq!(body["ticket"]["status"], "queued", "{body}");

    let (status, body) = send(&app, "DELETE", "/api/agents/w1?reassign=w2", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["reassigned"], 1, "{body}");

    let (_, body) = send(&app, "GET", &format!("/api/tickets/{ticket_ref}"), None).await;
    assert_eq!(body["ticket"]["assignee_slug"], "w2", "{body}");
    assert_eq!(body["ticket"]["status"], "queued", "{body}");

    let conn = db.lock().unwrap();
    assert!(db::agent_by_slug(&conn, "w1").unwrap().is_none());
}

#[tokio::test]
async fn delete_agent_moves_open_tickets_and_keeps_history() {
    let (app, db) = setup();
    let mut refs = Vec::new();
    {
        let conn = db.lock().unwrap();
        let d = db::division_by_slug(&conn, "test").unwrap().unwrap();
        let w1 = db::agent_by_slug(&conn, "w1").unwrap().unwrap();
        for status in ["triaged", "queued", "in_progress", "review", "done"] {
            let t = db::create_ticket(
                &conn,
                d.id,
                None,
                status,
                "",
                2,
                Some(w1.id),
                "human",
                status,
            )
            .unwrap();
            refs.push((t.ref_, status.to_string()));
        }
        db::add_comment(&conn, 1, "w1", "historical comment").unwrap();
    }

    let (status, body) = send(&app, "DELETE", "/api/agents/w1?reassign=w2", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], "w1", "{body}");
    assert_eq!(body["reassigned"], 4, "{body}");

    let (_, body) = send(&app, "GET", "/api/agents", None).await;
    let slugs: Vec<&str> = body["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["slug"].as_str())
        .collect();
    assert!(!slugs.contains(&"w1"), "{body}");

    let (_, body) = send(&app, "GET", "/api/tickets", None).await;
    for (r, old) in &refs {
        let t = body["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["ref"] == *r)
            .unwrap();
        if old == "done" {
            assert!(
                t["assignee_id"].is_null(),
                "done ticket keeps its history: {t}"
            );
        } else {
            assert_eq!(t["assignee_slug"], "w2", "{t}");
            assert_ne!(
                t["status"], "in_progress",
                "reassigned work must be claimable again: {t}"
            );
        }
    }

    // No reassign: the remaining open tickets fall back to triage.
    let (status, body) = send(&app, "DELETE", "/api/agents/w2", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["reassigned"], 4, "{body}");
    let (_, body) = send(&app, "GET", "/api/tickets", None).await;
    for (r, old) in &refs {
        let t = body["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["ref"] == *r)
            .unwrap();
        if old == "done" {
            continue;
        }
        assert!(t["assignee_id"].is_null(), "{t}");
        assert_eq!(t["status"], "new", "{t}");
    }
}

#[tokio::test]
async fn delete_agent_refuses_running_and_unknown() {
    let (app, db) = setup();
    {
        let conn = db.lock().unwrap();
        let w1 = db::agent_by_slug(&conn, "w1").unwrap().unwrap();
        db::set_agent_status(&conn, w1.id, "running").unwrap();
    }
    let (status, body) = send(&app, "DELETE", "/api/agents/w1", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("running"),
        "{body}"
    );

    {
        let conn = db.lock().unwrap();
        let w1 = db::agent_by_slug(&conn, "w1")
            .unwrap()
            .expect("refused delete must keep the row");
        assert_eq!(w1.status, "running");
    }

    let (status, body) = send(&app, "DELETE", "/api/agents/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("not found"),
        "{body}"
    );
}


#[tokio::test]
async fn agent_ladder_round_trips_and_labels_its_provider() {
    let (app, db) = setup();

    // A provider-less rung inherits the model spec's own prefix, so the board
    // does not have to repeat it.
    let (status, body) = send(
        &app,
        "PUT",
        "/api/agents/w1/models",
        Some(json!({ "models": [
            { "provider": "", "model": "nvidia/minimax-m3", "adapter": "opencode" },
            { "provider": "openrouter", "model": "deepseek/v4", "adapter": "" }
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let models = body["models"].as_array().expect("models array");
    assert_eq!(models.len(), 2);
    // The provider is always derived from the model spec, never trusted from the
    // caller. `rotation::provider_of` is what the dispatcher rotates on, so a
    // stored provider that disagreed with it would be a rotation bug: the rung
    // would cool down one provider while the run failed on another.
    assert_eq!(models[0]["provider"], "nvidia");
    assert_eq!(models[1]["provider"], "deepseek");
    assert_ne!(
        models[1]["provider"], "openrouter",
        "a caller-supplied provider must not override the model's own prefix"
    );
    // Position is the order they will be tried in.
    assert_eq!(models[0]["position"], 0);
    assert_eq!(models[1]["position"], 1);

    // The snapshot carries the ladder so the drawer can render without a
    // second round trip.
    let (_, snap) = send(&app, "GET", "/api/state", None).await;
    let ladders = snap["ladders"].as_object().expect("ladders");
    assert!(ladders.values().any(|v| v.as_array().is_some_and(|a| a.len() == 2)));

    // Replacing it wholesale drops the old rungs rather than appending.
    let (status, body) = send(
        &app,
        "PUT",
        "/api/agents/w1/models",
        Some(json!({ "models": [ { "provider": "local", "model": "qwen3", "adapter": "ollama" } ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["models"].as_array().unwrap().len(), 1);

    let conn = db.lock().unwrap();
    let agent = db::agent_by_slug(&conn, "w1").unwrap().unwrap();
    assert_eq!(db::agent_models(&conn, agent.id).unwrap().len(), 1);
}

#[tokio::test]
async fn a_failed_attempt_is_classified_and_served_with_the_ticket() {
    let (app, db) = setup();
    let (_, created) = send(
        &app,
        "POST",
        "/api/tickets",
        Some(json!({ "title": "t", "body": "", "division": "test", "priority": 2 })),
    )
    .await;
    let ticket_id = created["ticket"]["id"].as_i64().unwrap();
    let rref = created["ticket"]["ref"].as_str().unwrap().to_string();

    let (agent, provider) = {
        let conn = db.lock().unwrap();
        let a = db::agent_by_slug(&conn, "w1").unwrap().unwrap();
        // Record the same shape real failures have: noise, then the error.
        let raw = format!("{}Error: Too Many Requests: {{\"status\":429}}", "step\n".repeat(300));
        let c = bureau::failure::classify(&raw, Some(1));
        assert_eq!(c.kind, bureau::failure::FailureKind::RateLimit);
        db::record_attempt(
            &conn,
            ticket_id,
            &a,
            bureau::rotation::provider_of("nvidia/minimax-m3"),
            c.kind.as_str(),
            &c.detail,
            &raw,
        )
        .unwrap();
        db::set_status(&conn, ticket_id, "failed").unwrap();
        (a.clone(), "nvidia")
    };
    assert_eq!(agent.slug, "w1");
    assert_eq!(provider, "nvidia");

    let (status, body) = send(&app, "GET", &format!("/api/tickets/{rref}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let attempts = body["attempts"].as_array().expect("attempts");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["kind"], "rate_limit");
    assert_eq!(attempts[0]["provider"], "nvidia");
    assert!(
        attempts[0]["detail"].as_str().unwrap().contains("429"),
        "the detail must be the matching line, not the whole blob"
    );

    // The board's tally counts distinct tickets, so one ticket failing ten
    // times cannot masquerade as a systemic outage.
    let (_, snap) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(snap["failure_totals"]["rate_limit"], 1);
    let card = snap["attempts"].as_object().unwrap();
    assert!(card.contains_key(&ticket_id.to_string()), "snapshot labels the card");
}
