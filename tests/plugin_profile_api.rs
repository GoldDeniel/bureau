//! PATCH /api/agents/{slug} plugin_profile: division-scoped manager control.
//!
//! One test on purpose: BUREAU_PROFILES_DIR is process-wide, so the profile
//! fixture and every request must run in this single thread.
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

fn unique(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join(format!("bureau-{tag}-{}-{nanos}-{seq}", std::process::id()))
        .to_string_lossy()
        .to_string()
}

fn setup() -> (Router, Db) {
    let profiles = unique("profiles");
    for name in ["manager-bare", "worker-full", "worker-lite"] {
        let dir = std::path::Path::new(&profiles).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("opencode.json"), "{}").unwrap();
    }
    // Directories without opencode.json are not profiles.
    std::fs::create_dir_all(std::path::Path::new(&profiles).join("not-a-profile")).unwrap();
    // Managed profiles live under generated/ and are listed like any other.
    let managed = std::path::Path::new(&profiles).join("generated/worker-gen/opencode");
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::write(managed.join("opencode.json"), "{}").unwrap();
    // SAFETY: this binary has one test, so no other thread reads the environment.
    unsafe { std::env::set_var("BUREAU_PROFILES_DIR", &profiles) };

    let db = db::open(&unique("plugin-profile.db")).unwrap();
    {
        let conn = db.lock().unwrap();
        let alpha = db::create_division(&conn, "alpha", "Alpha Division", "", "/tmp").unwrap();
        let beta = db::create_division(&conn, "beta", "Beta Division", "", "/tmp").unwrap();
        db::create_agent(&conn, "sentry", "Sentry", "manager", Some(alpha), "demo", "", "").unwrap();
        db::create_agent(&conn, "spark", "Spark", "worker", Some(alpha), "demo", "", "").unwrap();
        db::create_agent(&conn, "overseer", "Overseer", "manager", Some(beta), "demo", "", "").unwrap();
        db::create_agent(&conn, "forge", "Forge", "worker", Some(beta), "demo", "", "").unwrap();
    }
    let app = router(AppState {
        db: db.clone(),
        dispatching: Arc::new(AtomicBool::new(false)),
        max_parallel: 2,
        max_subtasks: 3,
        token: None,
    });
    (app, db)
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    actor: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(actor) = actor {
        req = req.header("x-bureau-agent", actor);
    }
    let req = req
        .body(
            body.map(|b| Body::from(b.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

fn profile_of(body: &Value, slug: &str) -> Value {
    body["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["slug"] == slug)
        .unwrap_or_else(|| panic!("agent {slug} missing from {body}"))["plugin_profile"]
        .clone()
}

#[tokio::test]
async fn plugin_profile_patch_is_division_scoped() {
    let (app, _db) = setup();
    let uri = "/api/agents/spark";

    // No header: the human/CLI surface may set a profile.
    let (status, body) = send(
        &app,
        "PATCH",
        uri,
        Some(json!({"plugin_profile": "worker-lite"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["agent"]["plugin_profile"], "worker-lite", "{body}");

    // The list endpoint carries the value and the names the board can offer.
    let (status, body) = send(&app, "GET", "/api/agents", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(profile_of(&body, "spark"), "worker-lite", "{body}");
    assert_eq!(
        body["plugin_profiles"],
        json!(["inherit", "manager-bare", "worker-full", "worker-gen", "worker-lite"]),
        "{body}"
    );

    // A manager of another division is refused, and the body names both.
    let (status, body) = send(
        &app,
        "PATCH",
        uri,
        Some(json!({"plugin_profile": "worker-full"})),
        Some("overseer"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let msg = body["error"].as_str().unwrap();
    assert!(msg.contains("Alpha Division"), "{msg}");
    assert!(msg.contains("Beta Division"), "{msg}");

    // A worker is not a manager.
    let (status, _) = send(
        &app,
        "PATCH",
        uri,
        Some(json!({"plugin_profile": "worker-full"})),
        Some("spark"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Unknown names are 400 and the body lists the valid ones.
    let (status, body) = send(
        &app,
        "PATCH",
        uri,
        Some(json!({"plugin_profile": "nope"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let msg = body["error"].as_str().unwrap();
    assert!(msg.contains("worker-lite") && msg.contains("manager-bare"), "{msg}");

    // Paths never reach the filesystem lookup.
    let (status, _) = send(
        &app,
        "PATCH",
        uri,
        Some(json!({"plugin_profile": "../escape"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The correct division's manager may set it.
    let (status, body) = send(
        &app,
        "PATCH",
        uri,
        Some(json!({"plugin_profile": "worker-full"})),
        Some("sentry"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["agent"]["plugin_profile"], "worker-full", "{body}");

    // "inherit" is a real choice: use the global config as-is.
    let (status, body) = send(
        &app,
        "PATCH",
        uri,
        Some(json!({"plugin_profile": "inherit"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["agent"]["plugin_profile"], "inherit", "{body}");

    // A missing key leaves the profile unchanged.
    let (status, body) = send(
        &app,
        "PATCH",
        uri,
        Some(json!({"model": "some-model"})),
        Some("sentry"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["agent"]["plugin_profile"], "inherit", "{body}");

    // Present null clears it.
    let (status, body) = send(
        &app,
        "PATCH",
        uri,
        Some(json!({"plugin_profile": null})),
        Some("sentry"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["agent"]["plugin_profile"], Value::Null, "{body}");
    let (_, body) = send(&app, "GET", "/api/agents", None, None).await;
    assert_eq!(profile_of(&body, "spark"), Value::Null, "{body}");

    // "none" also clears.
    send(&app, "PATCH", uri, Some(json!({"plugin_profile": "worker-lite"})), None).await;
    let (status, body) = send(
        &app,
        "PATCH",
        uri,
        Some(json!({"plugin_profile": "none"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["agent"]["plugin_profile"], Value::Null, "{body}");

    // Existing status codes are untouched.
    let (status, body) = send(
        &app,
        "PATCH",
        "/api/agents/ghost",
        Some(json!({"plugin_profile": "worker-lite"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // The board's one-call snapshot keeps working and carries the field.
    let (status, body) = send(&app, "GET", "/api/state", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(profile_of(&body, "sentry"), Value::Null, "{body}");
}
