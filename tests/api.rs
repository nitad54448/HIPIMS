//! HTTP-level integration tests: the real router + actor, no network socket (no instrument is connected).

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use hipims_qcm::actor;
use hipims_qcm::config::Config;
use hipims_qcm::project::Project;
use hipims_qcm::server::{router, AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "test-token";

struct Rig {
    app: Router,
    state: AppState,
    dir: std::path::PathBuf,
    actor_thread: Option<std::thread::JoinHandle<()>>,
}

fn rig(name: &str) -> Rig {
    let dir = std::env::temp_dir().join(format!("hqcm_api_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cfg = Arc::new(Config { data_dir: dir.clone(), ..Default::default() });
    let (handle, jh) = actor::spawn(cfg.clone()).expect("cannot spawn the instrument actor");
    let project = Project::new(&cfg.data_dir, "itest", cfg.to_toml()).unwrap();
    let state = AppState::new(cfg, handle, project, TOKEN.into(), Box::new(|_| Ok(())));
    hipims_qcm::server::spawn_background(&state);
    Rig { app: router(state.clone()), state, dir, actor_thread: Some(jh) }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(jh) = self.actor_thread.take() {
            actor::shutdown_and_join(&self.state.actor, jh);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn call(app: &Router, method: Method, path: &str, body: Option<Value>) -> (StatusCode, String) {
    call_with(app, method, path, body, &[("host", "127.0.0.1:8765"), ("x-hq-token", TOKEN)]).await
}

async fn call_with(app: &Router, method: Method, path: &str, body: Option<Value>, headers: &[(&str, &str)]) -> (StatusCode, String) {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = match body {
        Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn post(app: &Router, path: &str, body: Value) -> Value {
    let (s, t) = call(app, Method::POST, path, Some(body)).await;
    assert!(s.is_success(), "{path} -> {s}: {t}");
    serde_json::from_str(&t).unwrap()
}

#[tokio::test]
async fn security_guards() {
    let r = rig("sec");
    // static UI needs no token but must have a loopback Host
    let (s, t) = call_with(&r.app, Method::GET, "/", None, &[("host", "127.0.0.1:8765")]).await;
    assert_eq!(s, StatusCode::OK);
    assert!(t.contains("QCM HiPIMS Workstation") && t.contains("id=\"exitBtn\""), "UI must contain the Exit button");
    let (s, _) = call_with(&r.app, Method::GET, "/", None, &[("host", "evil.example:8765")]).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "DNS rebinding guard");
    let (s, _) = call_with(&r.app, Method::GET, "/api/status", None, &[("host", "127.0.0.1:8765")]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call_with(&r.app, Method::GET, "/api/status", None, &[("host", "127.0.0.1:8765"), ("x-hq-token", "wrong")]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call_with(
        &r.app,
        Method::POST,
        "/api/estop",
        Some(json!({})),
        &[("host", "127.0.0.1:8765"), ("x-hq-token", TOKEN), ("origin", "http://evil.example")],
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "cross-origin request");
    let (s, _) = call_with(
        &r.app,
        Method::GET,
        &format!("/api/status?token={TOKEN}"),
        None,
        &[("host", "localhost:8765"), ("origin", "http://localhost:8765")],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "query-token + same origin is accepted");
}

#[tokio::test]
async fn requests_before_connect_are_refused_cleanly() {
    let r = rig("pre");
    let (s, t) = call(&r.app, Method::POST, "/api/sweep/run", Some(json!({"start_hz": 5.997e6, "stop_hz": 5.9998e6, "points": 100, "drive_v": 0.05, "compensate_divider": true, "bandwidth_hz": null}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{t}");
    assert!(t.contains("not connected"));
    let (s, _) = call(&r.app, Method::POST, "/api/hipims/step", Some(json!({}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
}

#[tokio::test]
async fn invalid_device_address_from_the_ui_is_refused() {
    let r = rig("dev");
    let (s, t) = call(
        &r.app,
        Method::POST,
        "/api/connect",
        Some(json!({"device": {"id": "dev1/../2", "host": "localhost", "port": 8004}})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{t}");
}

#[tokio::test]
async fn malformed_requests_are_rejected_by_the_service() {
    let r = rig("unsafe");
    let (s, _) = call(&r.app, Method::POST, "/api/track/start", Some(json!({"rate_hz": 0.0}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&r.app, Method::GET, "/api/tracks/..%2Fsecret/series", None).await;
    assert!(s.is_client_error(), "path traversal must be refused, got {s}");
}

#[tokio::test]
async fn shutdown_endpoint_signals_the_main_loop() {
    let r = rig("shut");
    let waiter = {
        let s = r.state.clone();
        tokio::spawn(async move { s.shutdown.notified().await })
    };
    post(&r.app, "/api/shutdown", json!({})).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), waiter).await.expect("shutdown must be signalled").unwrap();
}
