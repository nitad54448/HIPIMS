//! HTTP-level integration tests: the real router + actor + simulator, no network socket.

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
    let mut c = Config::default();
    c.sim.realtime = false;
    c.data_dir = dir.clone();
    let cfg = Arc::new(c);
    let (handle, jh) = actor::spawn(cfg.clone()).expect("cannot spawn the instrument actor");
    let project = Project::new(&cfg.data_dir, "itest", cfg.to_toml()).unwrap();
    let state = AppState::new(cfg, handle, project, TOKEN.into(), true, Box::new(|_| Ok(())));
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
async fn device_address_from_the_ui_is_used_and_validated() {
    let r = rig("dev");
    let (s, t) = call(
        &r.app,
        Method::POST,
        "/api/connect",
        Some(json!({"simulate": true, "device": {"id": "dev1/../2", "host": "localhost", "port": 8004}})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{t}");
    let info =
        post(&r.app, "/api/connect", json!({"simulate": true, "device": {"id": "DEV4242", "host": "localhost", "port": 8004}})).await;
    assert_eq!(info["device"], "dev4242");
    let st = post_get(&r.app, "/api/status").await;
    assert_eq!(st["status"]["device"]["device"], "dev4242");
}

#[tokio::test]
async fn unsafe_parameters_are_rejected_by_the_service() {
    let r = rig("unsafe");
    post(&r.app, "/api/connect", json!({"simulate": true})).await;
    // 0.8 V x2 divider = 1.6 V > 1 V limit
    let (s, t) = call(&r.app, Method::POST, "/api/sweep/run", Some(json!({"start_hz": 5.997e6, "stop_hz": 5.9998e6, "points": 100, "drive_v": 0.8, "compensate_divider": true, "bandwidth_hz": null}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{t}");
    assert!(t.contains("safety limit"));
    let (s, _) = call(&r.app, Method::POST, "/api/sweep/run", Some(json!({"start_hz": 5.9998e6, "stop_hz": 5.997e6, "points": 100, "drive_v": 0.05, "compensate_divider": true, "bandwidth_hz": null}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = call(&r.app, Method::POST, "/api/track/start", Some(json!({"rate_hz": 0.0}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&r.app, Method::GET, "/api/tracks/..%2Fsecret/series", None).await;
    assert!(s.is_client_error(), "path traversal must be refused, got {s}");
}

#[tokio::test]
async fn full_workflow_on_the_simulator_then_save_load_report() {
    let r = rig("flow");
    post(&r.app, "/api/connect", json!({"simulate": true})).await;
    post(&r.app, "/api/project/meta", json!({"sample_id": "Ti-001", "operator": "<b>nd</b>", "notes": "x"})).await;

    // Sweep + fits
    let sw = post(
        &r.app,
        "/api/sweep/run",
        json!({"start_hz": 5.997e6, "stop_hz": 5.9998e6, "points": 280, "drive_v": 0.05, "compensate_divider": true, "bandwidth_hz": null}),
    )
    .await;
    let q = sw["lorentz"]["q"].as_f64().unwrap();
    assert!((q / 50_000.0 - 1.0).abs() < 0.1, "Q = {q}");
    let est = post(&r.app, "/api/sweep/select", json!({"method": "Circle"})).await;
    assert_eq!(est["method"], "Circle");

    // Q calibration (sweeps of the effective resonance; needs the PLL off)
    let cal = post(&r.app, "/api/q/calibrate", json!({"gains": [-4, -2, 0, 2, 4], "points": 200})).await;
    assert!(cal["slope"].as_f64().unwrap() < 0.0);
    assert!((cal["q_native"].as_f64().unwrap() / 50_000.0 - 1.0).abs() < 0.1);
    assert_eq!(cal["points"].as_array().unwrap().len(), 5);

    // PLL + advisor + recording
    let adv = post(&r.app, "/api/pll/advise", json!({"target_bw_hz": 100.0})).await;
    assert!(adv["p"].as_f64().unwrap() > 0.0, "advisor returns magnitudes");
    let pll = post(&r.app, "/api/pll/start", json!({})).await;
    assert_eq!(pll["state"], "locked");
    assert!(pll["p"].as_f64().unwrap() < 0.0, "normal slope => negative gains");
    let tr = post(&r.app, "/api/track/start", json!({"rate_hz": 1000.0})).await;
    let file = tr["file"].as_str().unwrap().to_string();

    // calibrating while the PLL runs is refused (the sweeps would fight it)
    let (s, t) = call(&r.app, Method::POST, "/api/q/calibrate", Some(json!({"gains": [-4, 0, 4]}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{t}");

    // Engage with the PLL running: gains rescaled, blog rule PLL BW > Q-control BW flagged
    let eng = post(&r.app, "/api/q/engage", json!({"q_target": 70000.0})).await;
    assert!(eng["gain"].as_f64().unwrap() > 0.0);
    assert!(eng["pll_p"].is_number());
    assert!(eng["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("PLL bandwidth")));
    post(&r.app, "/api/q/disengage", json!({})).await;

    // HiPIMS steps: baseline, partial overlaps, full overlap
    for start in [-400.0, -330.0, -250.0, -150.0, -50.0, 0.0] {
        post(&r.app, "/api/hipims/step", json!({"window_start_us": start, "window_us": 300.0, "pulses": 100, "points": 300})).await;
    }
    let prof = post(&r.app, "/api/hipims/profile", json!({"pulse_width_us": 100.0, "f0_hz": est["f0_hz"], "q": est["q"], "polarity": "Normal", "end_fraction": 0.05, "f_unloaded_hz": 6.0e6, "rho_g_cm3": 4.506})).await;
    let pts = prof["profile"]["points"].as_array().unwrap();
    assert_eq!(pts.len(), 6);
    let full = pts.last().unwrap()["delta_f_hz"].as_f64().unwrap();
    assert!((full + 25.0).abs() < 4.0, "simulated pulse shift is -25 Hz, profile says {full}");
    assert!(pts[0]["delta_f_hz"].as_f64().unwrap().abs() < 1.0, "baseline step shows no shift");

    std::thread::sleep(std::time::Duration::from_millis(50));
    let stop = post(&r.app, "/api/track/stop", json!({})).await;
    assert!(stop["samples"].as_u64().unwrap() > 100);

    // Sauerbrey on the recorded track (simulated drift -0.8 Hz/s)
    let (s, ser) = call(&r.app, Method::GET, &format!("/api/tracks/{file}/series?max=500"), None).await;
    assert_eq!(s, StatusCode::OK);
    let ser: Value = serde_json::from_str(&ser).unwrap();
    let ts = ser["t_s"].as_array().unwrap();
    assert_eq!(ts[0].as_f64().unwrap(), 0.0, "API time is relative to the first sample");
    let dur = ts.last().unwrap().as_f64().unwrap();
    let win = post(&r.app, "/api/analysis/window", json!({"track": file, "ta_s": 0.1 * dur, "tb_s": 0.9 * dur, "crystal": {"f_unloaded_hz": 6.0e6, "diameter_mm": 10.0, "rho_g_cm3": 4.506, "z_ratio": 0.628, "model": "auto"}})).await;
    assert!(win["result"]["thickness_nm"].as_f64().unwrap() > 0.0);
    let iff = post(&r.app, "/api/analysis/iff", json!({"reference_nm_s": 10.0, "neutral_nm_s": 6.0})).await;
    assert!((iff["result"]["percent"].as_f64().unwrap() - 40.0).abs() < 1e-9);

    // Save, reload from disk, report is escaped and complete
    post(&r.app, "/api/project/save", json!({})).await;
    let loaded = post(&r.app, "/api/project/load", json!({"name": "itest"})).await;
    assert_eq!(loaded["sample_id"], "Ti-001");
    assert_eq!(loaded["steps"].as_array().unwrap().len(), 6);
    assert_eq!(loaded["sweeps"].as_array().unwrap().len(), 1);
    assert_eq!(loaded["tracks"][0]["complete"], true);
    assert_eq!(loaded["analyses"].as_array().unwrap().len(), 3);
    let (s, html) = call(&r.app, Method::GET, "/api/report", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("Ti-001") && html.contains("&lt;b&gt;nd&lt;/b&gt;") && !html.contains("<b>nd</b>"));
    assert!(html.contains("HiPIMS gated profile") && html.contains("Q-control calibration"));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (s, log) = call(&r.app, Method::GET, "/api/log", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(log.contains("PLL locked"), "{log}");

    // Emergency stop leaves nothing running
    post(&r.app, "/api/estop", json!({})).await;
    std::thread::sleep(std::time::Duration::from_millis(100));
    let st = post_get(&r.app, "/api/status").await;
    assert_eq!(st["status"]["pll_active"], false);
    assert_eq!(st["status"]["safe_state_uncertain"], false);
}

async fn post_get(app: &Router, path: &str) -> Value {
    let (s, t) = call(app, Method::GET, path, None).await;
    assert!(s.is_success(), "{path} -> {s}: {t}");
    serde_json::from_str(&t).unwrap()
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
