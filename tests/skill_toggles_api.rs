//! GET /api/catalog, GET/PUT/PATCH /api/agents/{slug}/skills: per-agent
//! skill and plugin toggles, their validation and their persistence.
//!
//! One test on purpose: HOME, PATH and BUREAU_PROFILES_DIR are process-wide, so
//! every fixture and request must run in this single thread.
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use bureau::api::{AppState, router};
use bureau::db::{self, Db, Toggle};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
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
    format!("bureau-{tag}-{}-{nanos}-{seq}", std::process::id())
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

fn entry<'a>(value: &'a Value, name: &str) -> &'a Value {
    value
        .as_array()
        .unwrap_or_else(|| panic!("expected an array, got {value}"))
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("{name} missing from {value}"))
}

fn app_for(db: &Db) -> Router {
    router(AppState {
        db: db.clone(),
        dispatching: Arc::new(AtomicBool::new(false)),
        max_parallel: 2,
        token: None,
    })
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

fn setup(root: &Path) -> (Router, Db, PathBuf) {
    let db_path = root.join("bureau.db");
    let db = db::open(&db_path.to_string_lossy()).unwrap();
    {
        let conn = db.lock().unwrap();
        let alpha = db::create_division(&conn, "alpha", "Alpha Division", "", "/tmp").unwrap();
        let beta = db::create_division(&conn, "beta", "Beta Division", "", "/tmp").unwrap();
        db::create_agent(&conn, "sentry", "Sentry", "manager", Some(alpha), "demo", "", "").unwrap();
        db::create_agent(&conn, "spark", "Spark", "worker", Some(alpha), "demo", "", "").unwrap();
        db::create_agent(&conn, "overseer", "Overseer", "manager", Some(beta), "demo", "", "").unwrap();
    }
    (app_for(&db), db, db_path)
}

#[tokio::test]
async fn skill_catalog_and_toggles_round_trip() {
    let root = std::env::temp_dir().join(unique("skill-toggles"));
    let home = root.join("home");
    let skills = home.join(".agents/skills");
    let config_skills = home.join(".config/opencode/skills");
    write_skill(&skills, "gamma", "Gamma from agents");
    write_skill(&config_skills, "delta", "Delta from config");
    // A directory without SKILL.md is not a skill.
    std::fs::create_dir_all(skills.join("not-a-skill")).unwrap();

    let profiles = root.join("profiles");
    let profile = profiles.join("worker-lite/opencode");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("opencode.json"), r#"{"plugins": ["p-one"]}"#).unwrap();
    let generated = profiles.join("generated/forge");
    std::fs::create_dir_all(generated.join("opencode")).unwrap();
    std::fs::write(
        generated.join("opencode/opencode.json"),
        r#"{"plugins": ["p-two"]}"#,
    )
    .unwrap();
    write_skill(&generated.join("skills"), "gen-skill", "Only in a generated profile");

    let plugin_dir = home.join(".config/opencode/plugin");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("actual.js"), "export default {}\n").unwrap();

    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();

    // SAFETY: this binary has one test, so no other thread reads the environment.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("PATH", &bin);
        std::env::set_var("BUREAU_PROFILES_DIR", &profiles);
    }

    let (app, db, db_path) = setup(&root);

    // The catalog names every source and carries the SKILL.md description.
    let (status, body) = send(&app, "GET", "/api/catalog", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(names(&body["skills"]), vec!["delta", "gamma", "gen-skill"], "{body}");
    assert_eq!(entry(&body["skills"], "gamma")["source"], "~/.agents/skills", "{body}");
    assert_eq!(
        entry(&body["skills"], "delta")["source"],
        "~/.config/opencode/skills",
        "{body}"
    );
    assert_eq!(entry(&body["skills"], "gen-skill")["source"], "generated:forge", "{body}");
    assert_eq!(
        entry(&body["skills"], "delta")["description"],
        "Delta from config",
        "{body}"
    );
    assert!(
        body["skills"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["default_enabled"] == true),
        "{body}"
    );
    assert_eq!(names(&body["plugins"]), vec!["actual", "p-one", "p-two"], "{body}");
    assert_eq!(entry(&body["plugins"], "p-one")["source"], "profile:worker-lite", "{body}");
    assert_eq!(entry(&body["plugins"], "p-two")["source"], "generated:forge", "{body}");
    assert_eq!(entry(&body["plugins"], "actual")["source"], "installed", "{body}");

    let uri = "/api/agents/spark/skills";
    let (status, body) = send(&app, "GET", uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["agent"], "spark", "{body}");
    assert_eq!(names(&body["skills"]), vec!["delta", "gamma", "gen-skill"], "{body}");
    assert!(
        body["skills"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["enabled"] == true && item["origin"] == "default"),
        "an agent with no overrides inherits every discovered default: {body}"
    );
    assert!(
        body["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["enabled"] == true && item["origin"] == "default"),
        "{body}"
    );

    // Explicit off is stored; the rest stay at their default.
    let (status, body) = send(
        &app,
        "PUT",
        uri,
        Some(json!({
            "skills": [{"name": "gamma", "enabled": false}],
            "plugins": [{"name": "p-one", "enabled": false}]
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(entry(&body["skills"], "gamma")["enabled"], false, "{body}");
    assert_eq!(entry(&body["skills"], "gamma")["origin"], "override", "{body}");
    assert_eq!(
        entry(&body["skills"], "gamma")["source"],
        "~/.agents/skills",
        "{body}"
    );
    assert_eq!(entry(&body["skills"], "delta")["enabled"], true, "{body}");
    assert_eq!(entry(&body["plugins"], "p-one")["enabled"], false, "{body}");
    assert_eq!(entry(&body["plugins"], "p-two")["enabled"], true, "{body}");
    {
        let conn = db.lock().unwrap();
        let stored = db::agent_skill_overrides(&conn, "spark").unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "gamma");
        assert!(!stored[0].enabled);
        assert_eq!(
            db::agent_plugin_overrides(&conn, "spark").unwrap()[0].name,
            "p-one"
        );
        // The change is an audit event, visible through `bureau events`.
        let events = db::events(&conn, 10).unwrap();
        let event = events
            .iter()
            .find(|e| e.kind == "skills")
            .unwrap_or_else(|| panic!("no skills event in {events:?}"));
        assert!(event.message.contains("skill gamma off"), "{}", event.message);
        assert!(event.message.contains("plugin p-one off"), "{}", event.message);
    }

    // The state survives a re-read and a full database reopen (server restart).
    let (status, body) = send(&app, "GET", uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(entry(&body["skills"], "gamma")["enabled"], false, "{body}");
    drop(app);
    drop(db);
    let reopened = db::open(&db_path.to_string_lossy()).unwrap();
    let app = app_for(&reopened);
    let (status, body) = send(&app, "GET", uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(entry(&body["skills"], "gamma")["enabled"], false, "{body}");
    assert_eq!(entry(&body["plugins"], "p-one")["enabled"], false, "{body}");

    // PATCH is accepted on the same path, and a plain string means on.
    let (status, body) = send(
        &app,
        "PATCH",
        uri,
        Some(json!({"skills": ["gamma"]})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(entry(&body["skills"], "gamma")["enabled"], true, "{body}");
    assert_eq!(entry(&body["skills"], "gamma")["origin"], "default", "{body}");
    {
        let conn = reopened.lock().unwrap();
        assert!(db::agent_skill_overrides(&conn, "spark").unwrap().is_empty());
    }

    // An unknown skill or plugin is 400 with the allowed names, and nothing
    // from the rejected batch is written.
    let (status, body) = send(
        &app,
        "PUT",
        uri,
        Some(json!({"skills": [{"name": "delta", "enabled": true},
                               {"name": "nope", "enabled": false}],
                    "plugins": [{"name": "p-two", "enabled": false}]})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "unknown skill 'nope'", "{body}");
    assert_eq!(
        body["valid"],
        json!(["delta", "gamma", "gen-skill"]),
        "{body}"
    );
    {
        let conn = reopened.lock().unwrap();
        let plugins = db::agent_plugin_overrides(&conn, "spark").unwrap();
        assert!(
            !plugins.iter().any(|toggle| toggle.name == "p-two"),
            "a rejected batch must not write its valid half: {plugins:?}"
        );
    }

    let (status, body) = send(
        &app,
        "PUT",
        uri,
        Some(json!({"plugins": [{"name": "nope", "enabled": false}]})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "unknown plugin 'nope'", "{body}");
    assert_eq!(body["valid"], json!(["actual", "p-one", "p-two"]), "{body}");

    // A saved name that disappears from disk stays visible as unavailable.
    {
        let conn = reopened.lock().unwrap();
        db::apply_agent_toggles(
            &conn,
            "spark",
            &[Toggle {
                name: "ghost-skill".into(),
                enabled: false,
                source: "~/.agents/skills".into(),
            }],
            &[],
        )
        .unwrap();
    }
    let (_, body) = send(&app, "GET", uri, None, None).await;
    let missing = entry(&body["skills"], "ghost-skill");
    assert_eq!(missing["origin"], "unavailable", "{body}");
    assert_eq!(missing["enabled"], false, "{body}");
    assert_eq!(missing["source"], "~/.agents/skills", "{body}");

    // Division scoping matches PATCH /api/agents/{slug}.
    let (status, body) = send(
        &app,
        "PUT",
        uri,
        Some(json!({"skills": [{"name": "gamma", "enabled": false}]})),
        Some("overseer"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _) = send(
        &app,
        "PUT",
        uri,
        Some(json!({"skills": [{"name": "gamma", "enabled": false}]})),
        Some("spark"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Unknown agents stay 404, and the legacy snapshot shape is untouched.
    let (status, body) = send(&app, "GET", "/api/agents/ghost/skills", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = send(&app, "GET", "/api/state", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["agents"].is_array(), "{body}");
    assert!(body["events"].is_array(), "{body}");

    drop(app);
    drop(reopened);
    std::fs::remove_dir_all(&root).ok();
}
