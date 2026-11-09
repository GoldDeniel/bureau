//! End-to-end API tests: HTTP in, company loop, board state out.
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use bureau::api::{AppState, router};
use bureau::db::{self, Db};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tower::ServiceExt;

fn temp_db_path() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir()
        .join(format!("bureau-test-{nanos}.db"))
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

    // Pass 1: manager triages and delegates.
    let report = bureau::dispatch::tick(db.clone(), 4).await.unwrap();
    assert_eq!(report.managers_started, 1, "{report:?}");
    let (_, body) = send(&app, "GET", &format!("/api/tickets/{parent_ref}"), None).await;
    assert_eq!(body["ticket"]["status"], "triaged", "{body}");
    assert!(body["ticket"]["children"].as_array().unwrap().len() >= 2);
    assert!(!body["comments"].as_array().unwrap().is_empty());

    // Pass 2: workers execute the delegated tickets.
    let report = bureau::dispatch::tick(db.clone(), 4).await.unwrap();
    assert_eq!(report.workers_started, 2, "{report:?}");
    let (_, body) = send(&app, "GET", &format!("/api/tickets/{parent_ref}"), None).await;
    let kids = body["ticket"]["children"].as_array().unwrap();
    assert!(kids.iter().all(|k| k["status"] == "review"), "{body}");
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

#[tokio::test]
async fn token_gate_protects_api_but_not_board_or_health() {
    let (_, db) = setup();
    let app = router(AppState {
        db,
        dispatching: Arc::new(AtomicBool::new(false)),
        max_parallel: 4,
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
