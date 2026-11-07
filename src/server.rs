//! Local web service: static UI + JSON API + WebSocket event stream.
//!
//! Security model (localhost lab PC): bound to 127.0.0.1 only; the `Host` header must
//! be loopback (DNS-rebinding guard); every `/api` and `/ws` call needs the random
//! session token (printed at start-up, passed in the URL fragment, never stored in
//! projects) and, if the browser sends one, a loopback `Origin`. The *service* owns the
//! hardware: the UI cannot bypass the limits enforced in the driver.

use crate::actor::{Event, Handle, Reply, Request};
use crate::analysis::fit::{FitMethod, Polarity, ResonanceEstimate};
use crate::analysis::hipims::{self, HipimsParams};
use crate::analysis::sauerbrey::{self, CrystalParams};
use crate::config::Config;
use crate::error::{AppError, InstrumentError, MeasureError};
use crate::measure::hipims::StepParams;
use crate::measure::pll::PllParams;
use crate::measure::qcontrol::{EngageParams, QCalParams};
use crate::measure::sweep::SweepParams;
use crate::project::{self, Project, TrackMeta};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request as HttpRequest, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{watch, Mutex as AsyncMutex, Notify};

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const STYLE_CSS: &str = include_str!("../web/style.css");

pub type LogLevelSetter = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

pub struct Inner {
    pub cfg: Arc<Config>,
    pub actor: Handle,
    pub project: AsyncMutex<Project>,
    pub token: String,
    pub port: u16,
    pub shutdown: Notify,
    pub shutdown_tx: watch::Sender<bool>,
    busy: Mutex<Option<String>>,
    log: Mutex<VecDeque<String>>,
    set_log_level: LogLevelSetter,
    pub default_simulate: bool,
}

#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

/// Random session token (hex).
pub fn new_token() -> String {
    let mut b = [0u8; 16];
    if getrandom::getrandom(&mut b).is_err() {
        // Extremely unlikely; fall back to time + pid so the service still starts.
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(1);
        b.copy_from_slice(&(t ^ (u128::from(std::process::id()) << 64)).to_le_bytes());
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl AppState {
    pub fn new(
        cfg: Arc<Config>,
        actor: Handle,
        project: Project,
        token: String,
        default_simulate: bool,
        set_log_level: LogLevelSetter,
    ) -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        let port = cfg.server.port;
        Self(Arc::new(Inner {
            cfg,
            actor,
            project: AsyncMutex::new(project),
            token,
            port,
            shutdown: Notify::new(),
            shutdown_tx,
            busy: Mutex::new(None),
            log: Mutex::new(VecDeque::new()),
            set_log_level,
            default_simulate,
        }))
    }

    fn claim(&self, name: &str) -> Result<JobGuard, AppError> {
        let mut b = self.busy.lock().map_err(|_| AppError::Internal("busy lock poisoned".into()))?;
        if let Some(cur) = b.as_ref() {
            return Err(AppError::Busy(format!("`{cur}` is running")));
        }
        *b = Some(name.to_string());
        Ok(JobGuard(self.clone()))
    }

    fn push_log(&self, line: String) {
        if let Ok(mut l) = self.log.lock() {
            if l.len() >= 2000 {
                l.pop_front();
            }
            l.push_back(line);
        }
    }

    fn note(&self, level: &str, text: impl Into<String>) {
        let _ = self.actor.events().send(Event::Log { level: level.into(), text: text.into(), t: crate::measure::now_iso() });
    }

    /// Broadcast the project summary (saved / unsaved state).
    async fn announce_project(&self, p: &Project) {
        let _ = self.actor.events().send(Event::Project { summary: summary(p) });
    }

    /// Save now; on failure keep `dirty` and tell the operator.
    async fn autosave(&self) {
        let mut p = self.project.lock().await;
        if p.dirty {
            match p.save() {
                Ok(()) => {}
                Err(e) => self.note("error", format!("autosave failed: {e}")),
            }
        }
        let s = summary(&p);
        let _ = self.actor.events().send(Event::Project { summary: s });
    }
}

struct JobGuard(AppState);
impl Drop for JobGuard {
    fn drop(&mut self) {
        if let Ok(mut b) = self.0.busy.lock() {
            *b = None;
        }
    }
}

fn summary(p: &Project) -> Value {
    json!({
        "name": p.name, "sample_id": p.sample_id, "operator": p.operator, "notes": p.notes,
        "created": p.created, "modified": p.modified, "dirty": p.dirty,
        "sweeps": p.sweeps.len(), "qcal": p.qcal.len(), "steps": p.steps.len(),
        "tracks": p.tracks, "analyses": p.analyses, "history": p.history,
        "estimate": p.estimate, "pll": p.pll, "hipims": p.hipims, "dir": p.dir.display().to_string(),
    })
}

// ------------------------------------------------------------------ errors

pub struct ApiError(pub AppError);

impl<E: Into<AppError>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = match &self.0 {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Busy(_) => StatusCode::CONFLICT,
            AppError::Project(_) => StatusCode::UNPROCESSABLE_ENTITY,
            AppError::Instrument(InstrumentError::NotConnected) => StatusCode::CONFLICT,
            AppError::Instrument(InstrumentError::LimitViolation { .. } | InstrumentError::InvalidParameter(_)) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            AppError::Measure(
                MeasureError::InvalidParams(_)
                | MeasureError::Instrument(InstrumentError::LimitViolation { .. } | InstrumentError::InvalidParameter(_)),
            ) => StatusCode::UNPROCESSABLE_ENTITY,
            AppError::Measure(
                MeasureError::Cancelled
                | MeasureError::Precondition(_)
                | MeasureError::SafetyAbort(_)
                | MeasureError::Instrument(InstrumentError::NotConnected),
            ) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (code, Json(json!({"error": self.0.to_string()}))).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(AppError::BadRequest(msg.into()))
}
fn internal(r: &dyn std::fmt::Debug) -> ApiError {
    ApiError(AppError::Internal(format!("unexpected actor reply: {r:?}")))
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<Value, ApiError> {
    serde_json::to_value(v).map_err(|e| ApiError(AppError::Internal(e.to_string())))
}

// ------------------------------------------------------------------ guard middleware

fn loopback_host(h: &str, port: u16) -> bool {
    h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}") || h == format!("[::1]:{port}")
}

async fn guard(State(s): State<AppState>, req: HttpRequest, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !loopback_host(host, s.port) {
        return (StatusCode::FORBIDDEN, "bad Host header").into_response();
    }
    let path = req.uri().path().to_string();
    if path.starts_with("/api") || path.starts_with("/ws") {
        if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            let o = origin.trim_start_matches("http://").trim_start_matches("https://");
            if !loopback_host(o, s.port) {
                return (StatusCode::FORBIDDEN, "bad Origin").into_response();
            }
        }
        let from_header = req.headers().get("x-hq-token").and_then(|v| v.to_str().ok()).map(str::to_string);
        let from_query = req.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=").map(str::to_string)));
        let ok = from_header.or(from_query).is_some_and(|t| t == s.token);
        if !ok {
            return (StatusCode::UNAUTHORIZED, Json(json!({"error": "missing or wrong session token"}))).into_response();
        }
    }
    next.run(req).await
}

// ------------------------------------------------------------------ router

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], APP_JS) }))
        .route("/style.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], STYLE_CSS) }))
        .route("/ws", get(ws_handler))
        .route("/api/status", get(status))
        .route("/api/config", get(config_info))
        .route("/api/connect", post(connect))
        .route("/api/disconnect", post(disconnect))
        .route("/api/settings/load", post(settings_load))
        .route("/api/settings/save", post(settings_save))
        .route("/api/sweep/run", post(sweep_run))
        .route("/api/sweep/select", post(sweep_select))
        .route("/api/sweep/override", post(sweep_override))
        .route("/api/pll/advise", post(pll_advise))
        .route("/api/pll/start", post(pll_start))
        .route("/api/pll/stop", post(pll_stop))
        .route("/api/track/start", post(track_start))
        .route("/api/track/stop", post(track_stop))
        .route("/api/q/calibrate", post(q_calibrate))
        .route("/api/q/engage", post(q_engage))
        .route("/api/q/disengage", post(q_disengage))
        .route("/api/hipims/step", post(hipims_step))
        .route("/api/hipims/profile", post(hipims_profile))
        .route("/api/hipims/clear", post(hipims_clear))
        .route("/api/analysis/window", post(analysis_window))
        .route("/api/analysis/iff", post(analysis_iff))
        .route("/api/materials", get(materials))
        .route("/api/tracks/{file}/series", get(track_series))
        .route("/api/project", get(project_get))
        .route("/api/project/meta", post(project_meta))
        .route("/api/project/new", post(project_new))
        .route("/api/project/save", post(project_save))
        .route("/api/project/load", post(project_load))
        .route("/api/projects", get(projects_list))
        .route("/api/report", get(report))
        .route("/api/log", get(log_text))
        .route("/api/loglevel", post(loglevel))
        .route("/api/cancel", post(cancel))
        .route("/api/estop", post(estop))
        .route("/api/shutdown", post(shutdown))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Background tasks: log collector and periodic autosave.
pub fn spawn_background(state: &AppState) {
    let s = state.clone();
    tokio::spawn(async move {
        let mut rx = s.actor.subscribe();
        loop {
            match rx.recv().await {
                Ok(Event::Log { level, text, t }) => s.push_log(format!("{t} [{level}] {text}")),
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    let s = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(20));
        loop {
            tick.tick().await;
            s.autosave().await;
        }
    });
}

// ------------------------------------------------------------------ websocket

async fn ws_handler(State(s): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |sock| ws_loop(s, sock))
}

async fn ws_loop(s: AppState, mut sock: WebSocket) {
    let mut rx = s.actor.subscribe();
    let mut down = s.shutdown_tx.subscribe();
    // Initial state: status snapshot and the log tail.
    if let Ok(Reply::Status(st)) = s.actor.request(Request::Status).await {
        if let Ok(t) = serde_json::to_string(&Event::Status(st)) {
            let _ = sock.send(Message::Text(t.into())).await;
        }
    }
    let tail: Vec<String> = s.log.lock().map(|l| l.iter().rev().take(200).rev().cloned().collect()).unwrap_or_default();
    let _ = sock.send(Message::Text(json!({"type": "log_tail", "lines": tail}).to_string().into())).await;
    {
        let p = s.project.lock().await;
        let _ = sock.send(Message::Text(json!({"type": "project", "summary": summary(&p)}).to_string().into())).await;
    }
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let text = match ev {
                    Ok(e) => match serde_json::to_string(&e) { Ok(t) => t, Err(_) => continue },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => json!({"type": "lagged", "missed": n}).to_string(),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if sock.send(Message::Text(text.into())).await.is_err() { break; }
            }
            m = sock.recv() => match m {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            _ = down.changed() => {
                let _ = sock.send(Message::Close(None)).await;
                break;
            }
        }
    }
}

// ------------------------------------------------------------------ handlers

async fn status(State(s): State<AppState>) -> ApiResult {
    let Reply::Status(st) = s.actor.request(Request::Status).await? else { return Err(internal(&"status")) };
    let p = s.project.lock().await;
    Ok(Json(json!({"status": st, "project": summary(&p), "version": env!("CARGO_PKG_VERSION"), "default_simulate": s.default_simulate})))
}

async fn config_info(State(s): State<AppState>) -> ApiResult {
    let c = &s.cfg;
    Ok(Json(json!({"limits": c.limits, "roles": c.roles, "timing": c.timing, "device": c.device, "sim": c.sim, "routing": c.routing})))
}

#[derive(Deserialize)]
struct ConnectBody {
    #[serde(default)]
    simulate: bool,
    /// Device address from the UI (serial, host, port).
    #[serde(default)]
    device: Option<crate::config::DeviceCfg>,
}

async fn connect(State(s): State<AppState>, Json(b): Json<ConnectBody>) -> ApiResult {
    let _g = s.claim("connect")?;
    let Reply::Info(info) = s.actor.request(Request::Connect { simulate: b.simulate, device: b.device }).await? else {
        return Err(internal(&"connect"));
    };
    let mut p = s.project.lock().await;
    p.device = Some(info.clone());
    p.log(
        "connect",
        format!("{} {} serial {}{}", info.device, info.devtype, info.serial, if info.simulated { " (simulated)" } else { "" }),
    );
    Ok(Json(to_json(&info)?))
}

async fn disconnect(State(s): State<AppState>) -> ApiResult {
    let _g = s.claim("disconnect")?;
    s.actor.request(Request::Disconnect).await?;
    s.project.lock().await.log("connect", "disconnected");
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct PathBody {
    path: String,
}

async fn settings_load(State(s): State<AppState>, Json(b): Json<PathBody>) -> ApiResult {
    let _g = s.claim("settings")?;
    s.actor.request(Request::LoadSettings(b.path.clone())).await?;
    s.project.lock().await.log("settings", format!("loaded instrument settings from {}", b.path));
    Ok(Json(json!({"ok": true})))
}

async fn settings_save(State(s): State<AppState>, Json(b): Json<PathBody>) -> ApiResult {
    let _g = s.claim("settings")?;
    s.actor.request(Request::SaveSettings(b.path.clone())).await?;
    Ok(Json(json!({"ok": true})))
}

async fn sweep_run(State(s): State<AppState>, Json(p): Json<SweepParams>) -> ApiResult {
    let _g = s.claim("sweep")?;
    let Reply::Sweep(r) = s.actor.request(Request::Sweep(p.clone())).await? else { return Err(internal(&"sweep")) };
    {
        let mut pr = s.project.lock().await;
        pr.estimate = r.estimate(FitMethod::Lorentz).cloned();
        pr.log("sweep", format!("{:.0}-{:.0} Hz, {} points, drive {} V", p.start_hz, p.stop_hz, p.points, p.drive_v));
        pr.sweeps.push((*r).clone());
        pr.touch();
    }
    s.autosave().await;
    Ok(Json(to_json(&r)?))
}

#[derive(Deserialize)]
struct MethodBody {
    method: FitMethod,
}

async fn sweep_select(State(s): State<AppState>, Json(b): Json<MethodBody>) -> ApiResult {
    let _g = s.claim("select")?;
    let Reply::Estimate(e) = s.actor.request(Request::SelectEstimate(b.method)).await? else { return Err(internal(&"select")) };
    let mut p = s.project.lock().await;
    p.estimate = Some((*e).clone());
    p.log("fit", format!("selected {:?}: f0 = {:.2} Hz, Q = {:.0}", b.method, e.f0_hz, e.q));
    Ok(Json(to_json(&e)?))
}

#[derive(Deserialize)]
struct OverrideBody {
    f0_hz: f64,
    bw_hz: f64,
    phase_deg: f64,
    polarity: Polarity,
    #[serde(default)]
    slope_deg_per_hz: Option<f64>,
}

async fn sweep_override(State(s): State<AppState>, Json(b): Json<OverrideBody>) -> ApiResult {
    let _g = s.claim("override")?;
    if !(b.f0_hz > 0.0 && b.bw_hz > 0.0) {
        return Err(bad("f0 and bandwidth must be positive"));
    }
    let q = b.f0_hz / b.bw_hz;
    let est = ResonanceEstimate {
        method: FitMethod::Lorentz,
        f0_hz: b.f0_hz,
        bw_hz: b.bw_hz,
        q,
        amplitude_v: 0.0,
        offset_v: 0.0,
        phase_deg: b.phase_deg,
        polarity: b.polarity,
        slope_deg_per_hz: b.slope_deg_per_hz.unwrap_or_else(|| (2.0 / b.bw_hz).to_degrees()),
        rms_residual: 0.0,
        f0_sigma_hz: None,
        bw_sigma_hz: None,
        circle: None,
    };
    let Reply::Estimate(e) = s.actor.request(Request::OverrideEstimate(Box::new(est))).await? else { return Err(internal(&"override")) };
    let mut p = s.project.lock().await;
    p.estimate = Some((*e).clone());
    p.log("fit", format!("operator override: f0 = {:.2} Hz, BW = {:.3} Hz, polarity {:?}", e.f0_hz, e.bw_hz, e.polarity));
    Ok(Json(to_json(&e)?))
}

async fn pll_advise(State(s): State<AppState>, Json(p): Json<PllParams>) -> ApiResult {
    let _g = s.claim("advisor")?;
    let Reply::Advice(a) = s.actor.request(Request::Advise(p)).await? else { return Err(internal(&"advise")) };
    Ok(Json(to_json(&a)?))
}

async fn pll_start(State(s): State<AppState>, Json(p): Json<PllParams>) -> ApiResult {
    let _g = s.claim("pll")?;
    let Reply::Pll(st) = s.actor.request(Request::PllStart(p)).await? else { return Err(internal(&"pll")) };
    let mut pr = s.project.lock().await;
    pr.pll = Some((*st).clone());
    pr.log("pll", format!("locked at {:.2} Hz, P = {:.4}, I = {:.4}, BW {:.0} Hz", st.freq_hz, st.p, st.i, st.bandwidth_hz));
    Ok(Json(to_json(&st)?))
}

async fn pll_stop(State(s): State<AppState>) -> ApiResult {
    s.actor.request(Request::PllStop).await?;
    s.project.lock().await.log("pll", "PLL off");
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct TrackBody {
    rate_hz: f64,
}

async fn track_start(State(s): State<AppState>, Json(b): Json<TrackBody>) -> ApiResult {
    if !(b.rate_hz.is_finite() && b.rate_hz >= 1.0) {
        return Err(bad("rate must be >= 1 Sa/s"));
    }
    let (path, header, file) = {
        let p = s.project.lock().await;
        let file = format!("track_{}.csv", chrono::Local::now().format("%Y%m%d-%H%M%S"));
        let path = p.tracks_dir().join(&file);
        let mut h: Vec<(String, String)> = vec![
            ("format".into(), "hipims-qcm track v1".into()),
            ("project".into(), p.name.clone()),
            ("sample_id".into(), p.sample_id.clone()),
            ("operator".into(), p.operator.clone()),
            ("software".into(), format!("hipims-qcm {} git {}", p.software_version, p.git_hash)),
            ("started".into(), crate::measure::now_iso()),
            ("rate_hz".into(), b.rate_hz.to_string()),
            ("t_s".into(), "instrument clock, monotonic, seconds".into()),
            ("wall_unix_s".into(), "wall clock, unix seconds (derived per batch)".into()),
        ];
        if let Some(d) = &p.device {
            h.push((
                "instrument".into(),
                format!("{} {} serial {} fw {} simulated={}", d.device, d.devtype, d.serial, d.firmware, d.simulated),
            ));
        }
        if let Some(e) = &p.estimate {
            h.push(("resonance".into(), format!("f0={:.3} Hz Q={:.0} phase={:.2} deg {:?}", e.f0_hz, e.q, e.phase_deg, e.polarity)));
        }
        if let Some(l) = &p.pll {
            h.push(("pll".into(), format!("P={} I={} D={} BW={} Hz", l.p, l.i, l.d, l.bandwidth_hz)));
        }
        (path, h, file)
    };
    s.actor.request(Request::TrackStart { rate_hz: b.rate_hz, path, header }).await?;
    let mut p = s.project.lock().await;
    p.tracks.push(TrackMeta { file: file.clone(), started: crate::measure::now_iso(), samples: 0, rate_hz: b.rate_hz, complete: false });
    p.log("track", format!("recording {file} at {} Sa/s", b.rate_hz));
    drop(p);
    s.autosave().await;
    Ok(Json(json!({"file": file})))
}

async fn track_stop(State(s): State<AppState>) -> ApiResult {
    let Reply::Track { file, samples } = s.actor.request(Request::TrackStop).await? else { return Err(internal(&"track")) };
    {
        let mut p = s.project.lock().await;
        if let Some(t) = p.tracks.iter_mut().find(|t| t.file == file) {
            t.samples = samples;
            t.complete = true;
        }
        p.log("track", format!("stopped {file}: {samples} samples"));
    }
    s.autosave().await;
    Ok(Json(json!({"file": file, "samples": samples})))
}

async fn q_calibrate(State(s): State<AppState>, Json(p): Json<QCalParams>) -> ApiResult {
    let _g = s.claim("q-calibration")?;
    let Reply::QCal(c) = s.actor.request(Request::QCalibrate(p)).await? else { return Err(internal(&"qcal")) };
    {
        let mut pr = s.project.lock().await;
        pr.log("q-control", format!("calibration: Q_native {:.0}, slope {:.3}", c.q_native, c.slope));
        pr.qcal.push((*c).clone());
        pr.touch();
    }
    s.autosave().await;
    Ok(Json(to_json(&c)?))
}

async fn q_engage(State(s): State<AppState>, Json(p): Json<EngageParams>) -> ApiResult {
    let _g = s.claim("q-engage")?;
    let Reply::Engaged(e) = s.actor.request(Request::QEngage(p)).await? else { return Err(internal(&"engage")) };
    s.project.lock().await.log("q-control", format!("engaged: target Q {:.0}, gain {:.3}", e.q_target, e.gain));
    Ok(Json(to_json(&e)?))
}

async fn q_disengage(State(s): State<AppState>) -> ApiResult {
    s.actor.request(Request::QDisengage).await?;
    s.project.lock().await.log("q-control", "disengaged");
    Ok(Json(json!({"ok": true})))
}

async fn hipims_step(State(s): State<AppState>, Json(mut p): Json<StepParams>) -> ApiResult {
    let _g = s.claim("hipims-step")?;
    p.index = s.project.lock().await.steps.len();
    let Reply::Step(r) = s.actor.request(Request::HipimsStep(p)).await? else { return Err(internal(&"step")) };
    {
        let mut pr = s.project.lock().await;
        pr.log("hipims", format!("step {}: window start {} us, {} pulses", r.index, r.window_start_us, r.pulses));
        pr.steps.push((*r).clone());
        pr.touch();
    }
    s.autosave().await;
    Ok(Json(to_json(&r)?))
}

async fn hipims_clear(State(s): State<AppState>) -> ApiResult {
    // Raw steps stay immutable: "clear" starts a new series by archiving the old one in the history.
    let mut p = s.project.lock().await;
    let n = p.steps.len();
    p.log("hipims", format!("operator started a new step series ({n} earlier steps kept in the project)"));
    p.hipims = None;
    p.steps_series_start();
    Ok(Json(json!({"ok": true})))
}

async fn hipims_profile(State(s): State<AppState>, Json(hp): Json<HipimsParams>) -> ApiResult {
    let mut p = s.project.lock().await;
    let steps: Vec<_> = p.current_series().to_vec();
    let pr = hipims::profile(&steps, &hp).map_err(|e| ApiError(AppError::Project(crate::error::ProjectError::Analysis(e))))?;
    p.hipims = Some(hp.clone());
    let id = p.push_analysis("hipims-profile", to_json(&hp)?, to_json(&pr)?);
    Ok(Json(json!({"profile": pr, "revision": id})))
}

#[derive(Deserialize)]
struct WindowBody {
    track: String,
    ta_s: f64,
    tb_s: f64,
    crystal: CrystalParams,
}

async fn analysis_window(State(s): State<AppState>, Json(b): Json<WindowBody>) -> ApiResult {
    let mut p = s.project.lock().await;
    let path = p.track_path(&b.track)?;
    let series = project::load_series(&path, None)?.rebased();
    let r = sauerbrey::analyze_window(&series, &b.crystal, b.ta_s, b.tb_s)
        .map_err(|e| ApiError(AppError::Project(crate::error::ProjectError::Analysis(e))))?;
    let id = p.push_analysis("window", json!({"track": b.track, "ta_s": b.ta_s, "tb_s": b.tb_s, "crystal": b.crystal}), to_json(&r)?);
    Ok(Json(json!({"result": r, "revision": id})))
}

#[derive(Deserialize)]
struct IffBody {
    reference_nm_s: f64,
    neutral_nm_s: f64,
}

async fn analysis_iff(State(s): State<AppState>, Json(b): Json<IffBody>) -> ApiResult {
    let r = sauerbrey::iff(b.reference_nm_s, b.neutral_nm_s).map_err(bad)?;
    let mut p = s.project.lock().await;
    let id = p.push_analysis("iff", json!({"reference": b.reference_nm_s, "neutral": b.neutral_nm_s}), to_json(&r)?);
    Ok(Json(json!({"result": r, "revision": id})))
}

async fn materials() -> ApiResult {
    Ok(Json(to_json(&sauerbrey::presets())?))
}

async fn track_series(State(s): State<AppState>, Path(file): Path<String>, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let max = q.get("max").and_then(|m| m.parse::<usize>().ok()).unwrap_or(4000).clamp(10, 100_000);
    let p = s.project.lock().await;
    let path = p.track_path(&file)?;
    let series = project::load_series(&path, Some(max))?.rebased();
    Ok(Json(to_json(&series)?))
}

async fn project_get(State(s): State<AppState>) -> ApiResult {
    let p = s.project.lock().await;
    Ok(Json(to_json(&*p)?))
}

#[derive(Deserialize)]
struct MetaBody {
    sample_id: String,
    operator: String,
    notes: String,
}

async fn project_meta(State(s): State<AppState>, Json(b): Json<MetaBody>) -> ApiResult {
    let mut p = s.project.lock().await;
    if b.sample_id != p.sample_id {
        let msg = format!("sample ID: `{}` -> `{}`", p.sample_id, b.sample_id);
        p.log("meta", msg);
    }
    if b.operator != p.operator {
        let msg = format!("operator: `{}` -> `{}`", p.operator, b.operator);
        p.log("meta", msg);
    }
    p.sample_id = b.sample_id;
    p.operator = b.operator;
    p.notes = b.notes;
    p.touch();
    s.announce_project(&p).await;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

async fn project_new(State(s): State<AppState>, Json(b): Json<NameBody>) -> ApiResult {
    let st = s.actor.request(Request::Status).await?;
    if matches!(&st, Reply::Status(x) if x.tracking) {
        return Err(ApiError(AppError::Busy("stop the recording before switching project".into())));
    }
    let mut p = s.project.lock().await;
    if p.dirty {
        p.save()?;
    }
    let mut np = Project::new(&s.cfg.data_dir, &b.name, s.cfg.to_toml())?;
    np.device = p.device.clone();
    np.log("project", "created");
    np.save()?;
    *p = np;
    s.announce_project(&p).await;
    Ok(Json(summary(&p)))
}

async fn project_save(State(s): State<AppState>) -> ApiResult {
    let mut p = s.project.lock().await;
    p.save()?;
    s.announce_project(&p).await;
    Ok(Json(json!({"saved": p.modified})))
}

async fn project_load(State(s): State<AppState>, Json(b): Json<NameBody>) -> ApiResult {
    let st = s.actor.request(Request::Status).await?;
    if matches!(&st, Reply::Status(x) if x.tracking) {
        return Err(ApiError(AppError::Busy("stop the recording before switching project".into())));
    }
    let mut p = s.project.lock().await;
    if p.dirty {
        p.save()?;
    }
    let lp = Project::load(&s.cfg.data_dir, &b.name)?;
    *p = lp;
    p.log("project", "loaded");
    s.announce_project(&p).await;
    Ok(Json(to_json(&*p)?))
}

async fn projects_list(State(s): State<AppState>) -> ApiResult {
    Ok(Json(json!(Project::list(&s.cfg.data_dir))))
}

async fn report(State(s): State<AppState>) -> Response {
    let p = s.project.lock().await;
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], crate::report::render(&p)).into_response()
}

async fn log_text(State(s): State<AppState>) -> Response {
    let text = s.log.lock().map(|l| l.iter().cloned().collect::<Vec<_>>().join("\n")).unwrap_or_default();
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8"), (header::CONTENT_DISPOSITION, "attachment; filename=\"session.log\"")], text)
        .into_response()
}

#[derive(Deserialize)]
struct LevelBody {
    level: String,
}

async fn loglevel(State(s): State<AppState>, Json(b): Json<LevelBody>) -> ApiResult {
    (s.set_log_level)(&b.level).map_err(bad)?;
    s.note("info", format!("tracing level set to `{}` (instrument traffic is logged at debug)", b.level));
    Ok(Json(json!({"ok": true})))
}

async fn cancel(State(s): State<AppState>) -> ApiResult {
    s.actor.cancel();
    s.note("warn", "cancel requested");
    Ok(Json(json!({"ok": true})))
}

async fn estop(State(s): State<AppState>, headers: HeaderMap) -> ApiResult {
    let _ = headers;
    s.actor.emergency_stop();
    s.note("error", "emergency stop requested by the operator");
    s.project.lock().await.log("safety", "EMERGENCY STOP");
    Ok(Json(json!({"ok": true})))
}

async fn shutdown(State(s): State<AppState>) -> ApiResult {
    s.note("warn", "shutdown requested: outputs will be switched off");
    s.shutdown.notify_one();
    Ok(Json(json!({"ok": true})))
}

/// Run the server until Ctrl-C or `/api/shutdown`; then stop the instrument safely.
pub async fn serve(state: AppState, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    spawn_background(&state);
    let app = router(state.clone());
    let s2 = state.clone();
    let signal = async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = s2.shutdown.notified() => {}
        }
        // 1. abort running jobs and switch the hardware to its safe state, 2. close sockets.
        s2.actor.emergency_stop();
        let _ = s2.shutdown_tx.send(true);
    };
    let r = axum::serve(listener, app).with_graceful_shutdown(signal).await;
    // Final save (a failure is reported, never silent).
    let mut p = state.project.lock().await;
    if p.dirty {
        if let Err(e) = p.save() {
            tracing::error!(error = %e, "final project save failed");
        }
    }
    r
}
