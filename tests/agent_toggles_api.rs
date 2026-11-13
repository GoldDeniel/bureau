//! Toggles data model, cached discovery and the agent toggles API.
//!
//! One test on purpose: HOME, PATH and BUREAU_PROFILES_DIR are process-wide, so
//! every fixture and request must run in this single thread.
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use bureau::api::{AppState, router};
use bureau::db::{self, Db};
use bureau::discovery;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
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
    format!(
        "bureau-{tag}-{}-{nanos}-{seq}",
        std::process::id()
    )
}

fn tmp(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(unique(tag))
}

fn write_skill(root: &Path, name: &str, description: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: \"{description}\"\n---\n\nbody\n"),
    )
    .unwrap();
}

fn names(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("expected an array, got {value}"))
        .iter()
        .map(|entry| entry["name"].as_str().unwrap().to_string())
        .collect()
}

fn setup() -> (Router, Db) {
    let db = db::open(&format!("{}.db", unique("toggles"))).unwrap();
    {
        let conn = db.lock().unwrap();
        let alpha = db::create_division(&conn, "alpha", "Alpha Division", "", "/tmp").unwrap();
        let beta = db::create_division(&conn, "beta", "Beta Division", "", "/tmp").unwrap();
        db::create_agent(&conn, "sentry", "Sentry", "manager", Some(alpha), "demo", "", "").unwrap();
        db::create_agent(&conn, "spark", "Spark", "worker", Some(alpha), "demo", "", "").unwrap();
        db::create_agent(&conn, "overseer", "Overseer", "manager", Some(beta), "demo", "", "").unwrap();
    }
    let app = router(AppState {
        db: db.clone(),
        dispatching: Arc::new(AtomicBool::new(false)),
        max_parallel: 2,
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

#[tokio::test]
async fn toggles_discovery_and_scoping() {
    let root = tmp("toggle-fixture");
    let home = root.join("home");
    let skills = home.join(".config/opencode/skills");
    write_skill(&skills, "gamma", "Gamma skill");
    write_skill(&skills, "delta", "Delta skill");
    // A directory without SKILL.md is not a skill.
    std::fs::create_dir_all(skills.join("not-a-skill")).unwrap();

    let profiles = root.join("profiles");
    let profile = profiles.join("worker-full/opencode");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(
        profile.join("opencode.json"),
        r#"{"plugins": ["p-one"]}"#,
    )
    .unwrap();

    let plugin_dir = home.join(".config/opencode/plugin");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("actual.js"), "export default {}\n").unwrap();

    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let shim = bin.join("opencode");
    std::fs::write(
        &shim,
        "#!/bin/sh\nif [ \"$1\" = debug ] && [ \"$2\" = skill ]; then\n  echo '[{\"name\":\"shim-skill\",\"description\":\"from shim\"}]'\n  exit 0\nfi\necho 'no such subcommand' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();

    // SAFETY: this binary has one test, so no other thread reads the environment.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("PATH", &bin);
        std::env::set_var("BUREAU_PROFILES_DIR", &profiles);
    }

    // Source 1: a newer opencode that answers `debug skill` with JSON.
    let found = discovery::refresh();
    assert_eq!(names(&json!(found.skills)), vec!["shim-skill"]);
    assert_eq!(found.skills_source, "opencode debug skill");

    // Source 2: the installed 2.2.7 has no skill subcommand, so the scan runs.
    std::fs::remove_file(&shim).unwrap();
    let found = discovery::refresh();
    assert_eq!(names(&json!(found.skills)), vec!["delta", "gamma"]);
    assert_eq!(found.skills_source, "filesystem");
    assert_eq!(
        names(&json!(found.plugins)),
        vec!["actual", "p-one"],
        "installed plugin and profile plugin are both discovered"
    );

    let (app, db) = setup();
    let uri = "/api/agents/spark";

    // A row-less agent reads as inherit with the whole discovered default set.
    let (status, body) = send(&app, "GET", uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["toggles"]["mode"], "inherit");
    assert_eq!(names(&body["toggles"]["skills"]), vec!["delta", "gamma"]);
    assert_eq!(names(&body["toggles"]["plugins"]), vec!["actual", "p-one"]);
    assert!(
        body["toggles"]["skills"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["origin"] == "inherit" && e["enabled"] == true),
        "{body}"
    );
    assert!(body["toggles"]["discovery"]["skills_at"].is_string(), "{body}");
    assert_eq!(body["toggles"]["discovery"]["stale"], false, "{body}");
    assert_eq!(body["toggles"]["discovery"]["skills_source"], "filesystem");
    assert_eq!(body["last_run"], Value::Null);

    // An explicit valid subset persists and round-trips.
    let (status, body) = send(
        &app,
        "PUT",
        &format!("{uri}/toggles"),
        Some(json!({"mode": "explicit", "skills": ["gamma"], "plugins": ["p-one"]})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["toggles"]["mode"], "explicit");
    assert_eq!(names(&body["toggles"]["skills"]), vec!["gamma"]);
    assert_eq!(body["toggles"]["skills"][0]["origin"], "explicit");
    assert_eq!(names(&body["toggles"]["plugins"]), vec!["p-one"]);
    {
        let conn = db.lock().unwrap();
        let stored = db::agent_toggles(&conn, "spark").unwrap();
        assert_eq!(stored.mode, "explicit");
        assert_eq!(stored.skills, vec!["gamma"]);
        assert_eq!(stored.plugins, vec!["p-one"]);
    }

    // An unknown name is rejected with the valid list, and nothing is written.
    let (status, body) = send(
        &app,
        "PUT",
        &format!("{uri}/toggles"),
        Some(json!({"mode": "explicit", "skills": ["nope"], "plugins": []})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "unknown skill 'nope'", "{body}");
    assert_eq!(body["valid"], json!(["delta", "gamma"]), "{body}");
    {
        let conn = db.lock().unwrap();
        assert_eq!(
            db::agent_toggles(&conn, "spark").unwrap().skills,
            vec!["gamma"],
            "a rejected request must not change stored toggles"
        );
    }

    // PATCH is accepted on the same path.
    let (status, body) = send(
        &app,
        "PATCH",
        &format!("{uri}/toggles"),
        Some(json!({"mode": "explicit", "skills": ["delta"], "plugins": []})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(names(&body["toggles"]["skills"]), vec!["delta"]);

    // Division scoping matches PATCH /api/agents/{slug}.
    let (status, body) = send(
        &app,
        "PUT",
        &format!("{uri}/toggles"),
        Some(json!({"mode": "inherit", "skills": [], "plugins": []})),
        Some("overseer"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = send(
        &app,
        "PUT",
        &format!("{uri}/toggles"),
        Some(json!({"mode": "explicit", "skills": ["gamma"], "plugins": []})),
        Some("sentry"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["toggles"]["mode"], "explicit");

    // Discovery endpoints.
    let (status, body) = send(&app, "GET", "/api/discovery", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(names(&body["skills"]), vec!["delta", "gamma"]);
    let (status, body) = send(&app, "POST", "/api/discovery/refresh", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["skills_at"].is_string() && body["plugins_at"].is_string(), "{body}");

    // A HOME with nothing in it degrades to empty lists, never an error.
    let missing = root.join("does-not-exist");
    unsafe {
        std::env::set_var("HOME", &missing);
        std::env::remove_var("BUREAU_PROFILES_DIR");
        std::env::set_var("PATH", &missing);
    }
    let found = discovery::refresh();
    assert!(found.skills.is_empty(), "{found:?}");
    assert!(found.plugins.is_empty(), "{found:?}");
    assert!(
        found.reason.as_deref().unwrap_or("").contains("no skills directory"),
        "{found:?}"
    );
    let (status, body) = send(&app, "GET", uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["toggles"]["skills"][0]["origin"], "unavailable", "{body}");
    assert_eq!(body["toggles"]["skills"][0]["enabled"], false, "{body}");

    std::fs::remove_dir_all(&root).ok();
}
