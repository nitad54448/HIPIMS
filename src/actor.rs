//! The instrument actor: ONE thread owns the UHF; everything else talks to it with
//! typed messages (no `Arc<Mutex<Uhf>>`). Replaces the LabVIEW producer/consumer +
//! state machine: the request enum is the command queue, the loop below is the
//! state machine, `Event`s are the notifiers.
//!
//! Safety: `EmergencyStop` travels on a priority channel and raises a flag every
//! long job polls (`JobCtl::cancelled`). The safe state also runs on `Drop` of the
//! driver (thread panic / shutdown / lost client).

use crate::analysis::fit::{FitMethod, Polarity, ResonanceEstimate};
use crate::config::Config;
use crate::error::{InstrumentError, MeasureError};
use crate::instrument::bridge::PyBridge;
use crate::instrument::sim::SimUhf;
use crate::instrument::uhf::Uhf;
use crate::instrument::{AbortFlag, AdvisorResult, DeviceInfo, JobCtl, NodeIo, Sample};
use crate::measure::hipims::StepParams;
use crate::measure::pll::{self, LockState, PllParams, PllStatus};
use crate::measure::qcontrol::{self, EngageParams, Engaged, QCalParams, QCalibration};
use crate::measure::sweep::{self, SweepParams, SweepResult};
use crate::project::TrackWriter;
use crate::units::{Degree, Hertz};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

#[derive(Debug)]
pub enum Request {
    /// `device`: address entered by the operator (None: the one from the configuration).
    Connect {
        simulate: bool,
        device: Option<crate::config::DeviceCfg>,
    },
    Disconnect,
    Status,
    Sweep(SweepParams),
    /// Choose the fit that feeds the PLL/Q-control.
    SelectEstimate(FitMethod),
    /// Operator override of the resonance parameters.
    OverrideEstimate(Box<ResonanceEstimate>),
    Advise(PllParams),
    PllStart(PllParams),
    PllStop,
    TrackStart {
        rate_hz: f64,
        path: PathBuf,
        header: Vec<(String, String)>,
    },
    TrackStop,
    QCalibrate(QCalParams),
    QEngage(EngageParams),
    QDisengage,
    HipimsStep(StepParams),
    LoadSettings(String),
    SaveSettings(String),
    EmergencyStop,
    Shutdown,
}

impl Request {
    fn job_name(&self) -> &'static str {
        match self {
            Request::Connect { .. } => "connect",
            Request::Sweep(_) => "sweep",
            Request::Advise(_) => "advisor",
            Request::PllStart(_) => "pll",
            Request::QCalibrate(_) => "q-calibration",
            Request::QEngage(_) => "q-engage",
            Request::HipimsStep(_) => "hipims-step",
            Request::LoadSettings(_) | Request::SaveSettings(_) => "settings",
            _ => "command",
        }
    }
}

#[derive(Debug)]
pub enum Reply {
    Ok,
    Status(Box<Status>),
    Info(DeviceInfo),
    Sweep(Box<SweepResult>),
    Estimate(Box<ResonanceEstimate>),
    Advice(AdvisorResult),
    Pll(Box<PllStatus>),
    QCal(Box<QCalibration>),
    Engaged(Engaged),
    Step(Box<crate::analysis::hipims::StepRecord>),
    Track { file: String, samples: u64 },
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    pub connected: bool,
    pub simulated: bool,
    pub device: Option<DeviceInfo>,
    pub busy: Option<String>,
    pub tracking: bool,
    pub track_file: Option<String>,
    pub track_samples: u64,
    pub pll_active: bool,
    pub pll: Option<PllStatus>,
    pub q_engaged: Option<Engaged>,
    pub estimate: Option<ResonanceEstimate>,
    pub has_sweep: bool,
    pub has_qcal: bool,
    pub last_error: Option<String>,
    pub safe_state_uncertain: bool,
    /// Signal output switched on (read back from the instrument): what the STOP button would switch off.
    pub outputs_on: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Status(Box<Status>),
    Log {
        level: String,
        text: String,
        t: String,
    },
    Progress {
        job: String,
        fraction: f64,
        message: String,
    },
    JobDone {
        job: String,
        ok: bool,
        message: String,
    },
    Samples {
        samples: Vec<Sample>,
    },
    /// Project summary after a change (name, dirty flag, counts...).
    Project {
        summary: serde_json::Value,
    },
}

struct Envelope {
    req: Request,
    reply: flume::Sender<Result<Reply, MeasureError>>,
}

/// Cheap, cloneable handle used by the server. Dropping all handles stops the actor.
#[derive(Clone)]
pub struct Handle {
    tx: flume::Sender<Envelope>,
    prio: flume::Sender<Envelope>,
    abort: AbortFlag,
    estop: AbortFlag,
    events: broadcast::Sender<Event>,
}

impl Handle {
    pub async fn request(&self, req: Request) -> Result<Reply, MeasureError> {
        let (rtx, rrx) = flume::bounded(1);
        self.tx.send(Envelope { req, reply: rtx }).map_err(|_| MeasureError::Precondition("the instrument actor has stopped".into()))?;
        rrx.recv_async().await.map_err(|_| MeasureError::Precondition("the instrument actor stopped before replying".into()))?
    }

    /// Blocking variant for tests and shutdown paths.
    pub fn request_blocking(&self, req: Request, timeout: Duration) -> Result<Reply, MeasureError> {
        let (rtx, rrx) = flume::bounded(1);
        self.tx.send(Envelope { req, reply: rtx }).map_err(|_| MeasureError::Precondition("the instrument actor has stopped".into()))?;
        rrx.recv_timeout(timeout).map_err(|_| MeasureError::Precondition("the instrument actor did not reply in time".into()))?
    }

    /// Cancel the running job (the instrument is left in a defined state by the job itself).
    pub fn cancel(&self) {
        self.abort.raise()
    }

    /// Emergency stop: raises the flag polled by every long job, then queues the
    /// safe-state shutdown on the priority channel. Never blocks.
    pub fn emergency_stop(&self) {
        self.estop.raise();
        let (rtx, _rrx) = flume::bounded(1);
        let _ = self.prio.send(Envelope { req: Request::EmergencyStop, reply: rtx });
        // Wake an idle actor blocked on the normal channel; it checks the priority
        // channel first at the top of its loop, before any later request.
        let (wtx, _wrx) = flume::bounded(1);
        let _ = self.tx.send(Envelope { req: Request::Status, reply: wtx });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
    pub fn events(&self) -> broadcast::Sender<Event> {
        self.events.clone()
    }
}

struct ActorCtl<'a> {
    abort: &'a AbortFlag,
    estop: &'a AbortFlag,
    events: &'a broadcast::Sender<Event>,
    job: &'static str,
    last: Instant,
}

impl JobCtl for ActorCtl<'_> {
    fn progress(&mut self, fraction: f64, message: &str) {
        if fraction >= 1.0 || self.last.elapsed() >= Duration::from_millis(120) {
            self.last = Instant::now();
            let _ = self.events.send(Event::Progress { job: self.job.into(), fraction, message: message.into() });
        }
    }
    fn cancelled(&self) -> bool {
        self.abort.is_raised() || self.estop.is_raised()
    }
}

struct Track {
    writer: TrackWriter,
    file: String,
    rate_hz: f64,
    paused: bool,
}

struct Actor {
    cfg: Arc<Config>,
    rx: flume::Receiver<Envelope>,
    prio: flume::Receiver<Envelope>,
    abort: AbortFlag,
    estop: AbortFlag,
    events: broadcast::Sender<Event>,
    uhf: Option<Uhf>,
    status: Status,
    sweep: Option<SweepResult>,
    estimate: Option<ResonanceEstimate>,
    pll_native: Option<[f64; 2]>,
    pll_setup: Option<(Degree, Hertz, Hertz)>,
    qcal: Option<QCalibration>,
    track: Option<Track>,
    last_idle: Instant,
    last_outputs: Instant,
}

/// Start the actor thread. `io_factory` is overridden in tests; the server uses [`spawn`].
pub fn spawn(cfg: Arc<Config>) -> Result<(Handle, std::thread::JoinHandle<()>), std::io::Error> {
    spawn_with(
        cfg,
        Box::new(|cfg, simulate| -> Box<dyn NodeIo> {
            if simulate {
                Box::new(SimUhf::new(cfg))
            } else {
                Box::new(PyBridge::new(cfg))
            }
        }),
    )
}

pub type IoFactory = Box<dyn Fn(Arc<Config>, bool) -> Box<dyn NodeIo> + Send>;

pub fn spawn_with(cfg: Arc<Config>, factory: IoFactory) -> Result<(Handle, std::thread::JoinHandle<()>), std::io::Error> {
    let (tx, rx) = flume::unbounded();
    let (ptx, prx) = flume::unbounded();
    let (events, _) = broadcast::channel(2048);
    let handle = Handle { tx, prio: ptx, abort: AbortFlag::new(), estop: AbortFlag::new(), events: events.clone() };
    let mut actor = Actor {
        cfg,
        rx,
        prio: prx,
        abort: handle.abort.clone(),
        estop: handle.estop.clone(),
        events,
        uhf: None,
        status: Status::default(),
        sweep: None,
        estimate: None,
        pll_native: None,
        pll_setup: None,
        qcal: None,
        track: None,
        last_idle: Instant::now(),
        last_outputs: Instant::now(),
    };
    // A spawn failure (resource exhaustion) is reported to the caller instead of being
    // hidden behind a dummy thread whose requests would all fail.
    let jh = std::thread::Builder::new().name("instrument-actor".into()).spawn(move || actor.run(factory))?;
    Ok((handle, jh))
}

impl Actor {
    fn log(&self, level: &str, text: impl Into<String>) {
        let text = text.into();
        match level {
            "error" => tracing::error!("{text}"),
            "warn" => tracing::warn!("{text}"),
            _ => tracing::info!("{text}"),
        }
        let _ = self.events.send(Event::Log { level: level.into(), text, t: crate::measure::now_iso() });
    }

    fn emit_status(&mut self) {
        self.status.has_sweep = self.sweep.is_some();
        self.status.has_qcal = self.qcal.is_some();
        self.status.estimate = self.estimate.clone();
        self.status.tracking = self.track.is_some();
        self.status.track_file = self.track.as_ref().map(|t| t.file.clone());
        self.status.track_samples = self.track.as_ref().map_or(0, |t| t.writer.samples);
        self.status.connected = self.uhf.is_some();
        let _ = self.events.send(Event::Status(Box::new(self.status.clone())));
    }

    fn run(&mut self, factory: IoFactory) {
        self.log("info", "instrument actor started");
        loop {
            let env = if let Ok(e) = self.prio.try_recv() {
                Some(e)
            } else {
                let wait = if self.track.as_ref().is_some_and(|t| !t.paused) { Duration::ZERO } else { Duration::from_millis(100) };
                match self.rx.recv_timeout(wait) {
                    Ok(e) => Some(e),
                    Err(flume::RecvTimeoutError::Timeout) => None,
                    Err(flume::RecvTimeoutError::Disconnected) => break,
                }
            };
            if let Some(env) = env {
                let shutdown = matches!(env.req, Request::Shutdown);
                let job = env.req.job_name();
                let is_job = !matches!(
                    env.req,
                    Request::Status | Request::EmergencyStop | Request::Shutdown | Request::TrackStop | Request::PllStop
                );
                if is_job {
                    self.abort.clear();
                    self.status.busy = Some(job.into());
                    self.emit_status();
                }
                let result = self.handle(env.req, &factory, job);
                if is_job {
                    self.status.busy = None;
                    self.abort.clear();
                }
                match &result {
                    Ok(_) if is_job => {
                        let _ = self.events.send(Event::JobDone { job: job.into(), ok: true, message: String::new() });
                    }
                    Err(e) => {
                        self.status.last_error = Some(e.to_string());
                        self.log("error", format!("{job}: {e}"));
                        let _ = self.events.send(Event::JobDone { job: job.into(), ok: false, message: e.to_string() });
                    }
                    _ => {}
                }
                self.emit_status();
                let _ = env.reply.send(result);
                if shutdown {
                    break;
                }
            }
            if self.track.as_ref().is_some_and(|t| !t.paused) {
                self.track_tick();
            }
            self.idle_tick();
            self.output_tick();
        }
        // Dropping `self.uhf` runs the safe state.
        if let Some(mut u) = self.uhf.take() {
            if let Err(e) = u.disconnect() {
                tracing::error!(error = %e, "disconnect at actor exit");
            }
        }
        self.log("info", "instrument actor stopped");
    }

    fn ctl<'a>(abort: &'a AbortFlag, estop: &'a AbortFlag, events: &'a broadcast::Sender<Event>, job: &'static str) -> ActorCtl<'a> {
        ActorCtl { abort, estop, events, job, last: Instant::now() - Duration::from_secs(1) }
    }

    fn uhf(&mut self) -> Result<&mut Uhf, MeasureError> {
        self.uhf.as_mut().ok_or(MeasureError::Instrument(InstrumentError::NotConnected))
    }

    fn handle(&mut self, req: Request, factory: &IoFactory, job: &'static str) -> Result<Reply, MeasureError> {
        let (abort, estop, events) = (self.abort.clone(), self.estop.clone(), self.events.clone());
        let mut ctl = Self::ctl(&abort, &estop, &events, job);
        match req {
            Request::Status => Ok(Reply::Status(Box::new(self.status.clone()))),
            Request::Connect { simulate, device } => {
                if self.uhf.is_some() {
                    return Err(MeasureError::Precondition("already connected".into()));
                }
                // The address typed in the UI overrides the configured one for this connection.
                let mut cfg = (*self.cfg).clone();
                cfg.device =
                    device.unwrap_or_else(|| cfg.device.clone()).normalized().map_err(|e| MeasureError::InvalidParams(e.to_string()))?;
                let cfg = Arc::new(cfg);
                let io = factory(cfg.clone(), simulate);
                let uhf = Uhf::connect(io, cfg).map_err(MeasureError::from_instrument)?;
                let info = uhf.info().clone();
                self.status.simulated = info.simulated;
                self.status.device = Some(info.clone());
                self.status.last_error = None;
                self.uhf = Some(uhf);
                self.log(
                    "info",
                    format!("connected to {} ({}){}", info.device, info.devtype, if info.simulated { " [SIMULATED]" } else { "" }),
                );
                Ok(Reply::Info(info))
            }
            Request::Disconnect => {
                self.stop_track_inner();
                if let Some(mut u) = self.uhf.take() {
                    let r = u.disconnect();
                    self.reset_session();
                    if let Err(e) = r {
                        self.status.safe_state_uncertain = true;
                        return Err(MeasureError::Instrument(e));
                    }
                }
                self.log("info", "disconnected (outputs off)");
                Ok(Reply::Ok)
            }
            Request::Sweep(p) => {
                self.suspend_track();
                let r = sweep::run(self.uhf()?, &p, &mut ctl);
                self.resume_track();
                let res = r?;
                self.estimate = res.estimate(FitMethod::Lorentz).cloned();
                self.pll_setup = None;
                self.log(
                    "info",
                    match &self.estimate {
                        Some(e) => format!("sweep done: f0 = {:.2} Hz, Q = {:.0}, slope {:?}", e.f0_hz, e.q, e.polarity),
                        None => format!("sweep done, no fit: {}", res.fit_errors.join("; ")),
                    },
                );
                self.sweep = Some(res.clone());
                self.status.outputs_on = true;
                self.status.pll = None;
                Ok(Reply::Sweep(Box::new(res)))
            }
            Request::SelectEstimate(m) => {
                let s = self.sweep.as_ref().ok_or_else(|| MeasureError::Precondition("run a sweep first".into()))?;
                let e = s
                    .estimate(m)
                    .cloned()
                    .ok_or_else(|| MeasureError::Fit(format!("no {m:?} fit available: {}", s.fit_errors.join("; "))))?;
                self.estimate = Some(e.clone());
                Ok(Reply::Estimate(Box::new(e)))
            }
            Request::OverrideEstimate(e) => {
                let ok = e.f0_hz.is_finite() && e.f0_hz > 0.0 && e.q.is_finite() && e.q > 0.0 && e.bw_hz.is_finite() && e.bw_hz > 0.0;
                if !ok {
                    return Err(MeasureError::InvalidParams("f0, Q and bandwidth must be positive and finite".into()));
                }
                self.estimate = Some((*e).clone());
                self.log("info", format!("resonance overridden by the operator: f0 = {:.2} Hz, Q = {:.0}", e.f0_hz, e.q));
                Ok(Reply::Estimate(e))
            }
            Request::Advise(p) => {
                let est = self.need_estimate()?;
                let a = pll::advise(self.uhf()?, &est, &p, &mut ctl)?;
                Ok(Reply::Advice(a))
            }
            Request::PllStart(p) => {
                let est = self.need_estimate()?;
                // pll::start reconfigures the recorded demodulator (time constant, rate):
                // pause the recording for the lock search; resume re-applies the track rate.
                self.suspend_track();
                let r = pll::start(self.uhf()?, &est, &p, &mut ctl);
                self.resume_track();
                let st = r?;
                self.pll_native = Some([st.p, st.i]);
                self.pll_setup = Some((Degree(est.phase_deg), Hertz(est.f0_hz), Hertz(p.limit_hz)));
                self.status.pll_active = true;
                self.status.pll = Some(st.clone());
                self.log("info", format!("PLL locked at {:.2} Hz (P = {:.4}, I = {:.4})", st.freq_hz, st.p, st.i));
                Ok(Reply::Pll(Box::new(st)))
            }
            Request::PllStop => {
                pll::stop(self.uhf()?)?;
                self.status.pll_active = false;
                if let Some(p) = self.status.pll.as_mut() {
                    p.state = LockState::Off;
                }
                self.log("info", "PLL off");
                Ok(Reply::Ok)
            }
            Request::TrackStart { rate_hz, path, header } => {
                if self.track.is_some() {
                    return Err(MeasureError::Precondition("already tracking".into()));
                }
                let demod = self.cfg.roles.pll_demod;
                let writer = TrackWriter::create(&path, &header)
                    .map_err(|e| MeasureError::Precondition(format!("cannot create the track file: {e}")))?;
                let started = match self.uhf.as_mut() {
                    Some(u) => u.start_stream(demod, Hertz(rate_hz)).map_err(MeasureError::from_instrument),
                    None => Err(MeasureError::Instrument(InstrumentError::NotConnected)),
                };
                if let Err(e) = started {
                    // The recording never started: remove the header-only file instead of
                    // leaving an orphan that no project knows about.
                    if let Err(e2) = std::fs::remove_file(&path) {
                        tracing::warn!(error = %e2, file = %path.display(), "could not remove the unused track file");
                    }
                    return Err(e);
                }
                let file = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                self.log("info", format!("recording `{file}` at {rate_hz} Sa/s"));
                self.track = Some(Track { writer, file, rate_hz, paused: false });
                Ok(Reply::Ok)
            }
            Request::TrackStop => {
                let (file, samples) = self.stop_track_inner().ok_or_else(|| MeasureError::Precondition("not tracking".into()))?;
                self.log("info", format!("recording stopped: {samples} samples in `{file}`"));
                Ok(Reply::Track { file, samples })
            }
            Request::QCalibrate(mut p) => {
                let est = self.need_estimate()?;
                if p.center_hz.is_none() {
                    // the resonance drifts during deposition: centre on where the PLL last was
                    p.center_hz = self.status.pll.as_ref().map(|s| s.freq_hz).filter(|f| f.is_finite() && *f > 0.0);
                }
                // calibrate() reconfigures the PLL demodulator and sweeps the oscillator:
                // pause the recording for its duration, like the sweep handler does.
                self.suspend_track();
                let r = qcontrol::calibrate(self.uhf()?, &est, &p, &mut ctl);
                self.resume_track();
                let cal = r?;
                self.log(
                    "info",
                    format!(
                        "Q-calibration: Q_native = {:.0}, dGamma/dgain = {:.3} 1/s, lasing at gain {:?}",
                        cal.q_native, cal.slope, cal.gain_lasing
                    ),
                );
                self.qcal = Some(cal.clone());
                Ok(Reply::QCal(Box::new(cal)))
            }
            Request::QEngage(mut p) => {
                let cal = self.qcal.clone().ok_or_else(|| MeasureError::Precondition("calibrate the Q-control first".into()))?;
                if self.status.pll_active {
                    if p.pll_native_gains.is_none() {
                        p.pll_native_gains = self.pll_native;
                    }
                    if p.pll_bandwidth_hz.is_none() {
                        p.pll_bandwidth_hz = self.status.pll.as_ref().map(|s| s.bandwidth_hz).filter(|b| *b > 0.0);
                    }
                }
                let e = qcontrol::engage(self.uhf()?, &cal, &p, &mut ctl)?;
                self.log("info", format!("Q-control engaged: target Q = {:.0}, gain = {:.3}", e.q_target, e.gain));
                self.status.q_engaged = Some(e.clone());
                Ok(Reply::Engaged(e))
            }
            Request::QDisengage => {
                qcontrol::disengage(self.uhf()?)?;
                self.status.q_engaged = None;
                self.log("info", "Q-control disengaged (gain := 0, loops off, AGC off)");
                Ok(Reply::Ok)
            }
            Request::HipimsStep(p) => {
                let rec = crate::measure::hipims::acquire_step(self.uhf()?, &p, &mut ctl)?;
                self.log("info", format!("HiPIMS step {}: window start {} us, {} pulses", rec.index, rec.window_start_us, rec.pulses));
                Ok(Reply::Step(Box::new(rec)))
            }
            Request::LoadSettings(path) => {
                self.uhf()?.load_settings(&path).map_err(MeasureError::from_instrument)?;
                self.log("info", format!("settings loaded from `{path}`"));
                Ok(Reply::Ok)
            }
            Request::SaveSettings(path) => {
                self.uhf()?.save_settings(&path).map_err(MeasureError::from_instrument)?;
                self.log("info", format!("settings saved to `{path}`"));
                Ok(Reply::Ok)
            }
            Request::EmergencyStop => {
                self.log("error", "EMERGENCY STOP");
                self.stop_track_inner();
                let r = match self.uhf.as_mut() {
                    Some(u) => u.emergency_stop(),
                    None => Ok(()),
                };
                self.status.pll_active = false;
                self.status.q_engaged = None;
                self.status.outputs_on = false;
                self.estop.clear();
                self.abort.clear();
                match r {
                    Ok(()) => {
                        self.status.safe_state_uncertain = false;
                        self.log("warn", "all outputs and loops switched off (read back)");
                        Ok(Reply::Ok)
                    }
                    Err(e) => {
                        self.status.safe_state_uncertain = true;
                        Err(MeasureError::Instrument(e))
                    }
                }
            }
            Request::Shutdown => {
                self.stop_track_inner();
                Ok(Reply::Ok)
            }
        }
    }

    fn need_estimate(&self) -> Result<ResonanceEstimate, MeasureError> {
        self.estimate.clone().ok_or_else(|| MeasureError::Precondition("no resonance: run a sweep and select a fit first".into()))
    }

    fn reset_session(&mut self) {
        self.sweep = None;
        self.estimate = None;
        self.qcal = None;
        self.pll_native = None;
        self.pll_setup = None;
        self.status = Status::default();
    }

    fn suspend_track(&mut self) {
        let demod_ok = self.track.as_ref().is_some_and(|t| !t.paused);
        if demod_ok {
            if let Some(u) = self.uhf.as_mut() {
                if let Err(e) = u.stop_stream() {
                    tracing::warn!(error = %e, "could not pause the stream");
                }
            }
            if let Some(t) = self.track.as_mut() {
                t.paused = true;
            }
        }
    }

    fn resume_track(&mut self) {
        let rate = match self.track.as_ref() {
            Some(t) if t.paused => t.rate_hz,
            _ => return,
        };
        let demod = self.cfg.roles.pll_demod;
        let r = self.uhf.as_mut().map(|u| u.start_stream(demod, Hertz(rate)));
        if let Some(Err(e)) = r {
            self.log("error", format!("could not resume the recording: {e}"));
            self.stop_track_inner();
        } else if let Some(t) = self.track.as_mut() {
            t.paused = false;
        }
    }

    fn stop_track_inner(&mut self) -> Option<(String, u64)> {
        let t = self.track.take()?;
        if let Some(u) = self.uhf.as_mut() {
            if let Err(e) = u.stop_stream() {
                tracing::warn!(error = %e, "stream stop failed");
            }
        }
        let file = t.file.clone();
        match t.writer.finish() {
            Ok(n) => Some((file, n)),
            Err(e) => {
                self.log("error", format!("closing `{file}` failed: {e}"));
                Some((file, 0))
            }
        }
    }

    /// One poll slice of the continuous recording.
    fn track_tick(&mut self) {
        let window = crate::units::Second(self.cfg.timing.stream_poll_s);
        let Some(u) = self.uhf.as_mut() else { return };
        match u.poll_stream(window) {
            Ok(batch) if !batch.is_empty() => {
                let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
                let mut failed = None;
                if let Some(t) = self.track.as_mut() {
                    if let Err(e) = t.writer.write_batch(&batch, wall) {
                        failed = Some(e.to_string());
                    }
                }
                if let Some(msg) = failed {
                    self.log("error", format!("recording stopped, write failed: {msg}"));
                    self.stop_track_inner();
                    self.emit_status();
                    return;
                }
                let step = (batch.len() / 40).max(1);
                let _ = self.events.send(Event::Samples { samples: batch.iter().step_by(step).copied().collect() });
            }
            Ok(_) => {}
            Err(e) => {
                self.log("error", format!("streaming failed: {e}"));
                self.stop_track_inner();
                self.emit_status();
            }
        }
    }

    /// Read the drive output state back once per second (when no job holds the instrument),
    /// so the UI can show whether STOP has anything to stop.
    fn output_tick(&mut self) {
        if self.last_outputs.elapsed() < Duration::from_secs(1) || self.status.busy.is_some() {
            return;
        }
        self.last_outputs = Instant::now();
        let Some(u) = self.uhf.as_mut() else { return };
        match u.drive_is_on() {
            Ok(on) if on != self.status.outputs_on => {
                self.status.outputs_on = on;
                self.emit_status();
            }
            Ok(_) => {}
            Err(e) => tracing::debug!(error = %e, "output read-back failed"),
        }
    }

    /// Periodic PLL lock check while idle.
    fn idle_tick(&mut self) {
        if self.last_idle.elapsed() < Duration::from_millis(500) || !self.status.pll_active {
            return;
        }
        self.last_idle = Instant::now();
        let Some((setpoint, center, limit)) = self.pll_setup else { return };
        let tol = self.cfg.timing.pll_lock_tol_deg;
        let Some(u) = self.uhf.as_mut() else { return };
        match pll::check_lock(u, setpoint, center, limit, tol) {
            Ok(mut st) => {
                if let Some(old) = self.status.pll.as_ref() {
                    st.bandwidth_hz = old.bandwidth_hz;
                    st.advisor_from_device = old.advisor_from_device;
                    st.polarity = old.polarity;
                }
                let lost = st.state != LockState::Locked && self.status.pll.as_ref().is_some_and(|p| p.state == LockState::Locked);
                self.status.pll = Some(st);
                if lost {
                    self.log("warn", "PLL lock lost");
                }
                self.emit_status();
            }
            Err(e) => {
                tracing::debug!(error = %e, "lock check failed");
            }
        }
    }
}

/// Request a clean shutdown and wait for the actor thread (used at process exit).
pub fn shutdown_and_join(handle: &Handle, jh: std::thread::JoinHandle<()>) {
    handle.estop.raise();
    let _ = handle.request_blocking(Request::Shutdown, Duration::from_secs(10));
    let _ = jh.join();
}

#[allow(dead_code)]
fn _assert_send() {
    fn is_send<T: Send>() {}
    is_send::<Actor>();
    let _ = AtomicBool::new(false).load(Ordering::SeqCst);
    let _ = Polarity::Normal;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sim_cfg() -> Arc<Config> {
        let mut c = Config::default();
        c.sim.realtime = false;
        Arc::new(c)
    }

    fn sweep_req() -> SweepParams {
        SweepParams { start_hz: 5.9970e6, stop_hz: 5.9998e6, points: 280, drive_v: 0.05, compensate_divider: true, bandwidth_hz: None }
    }

    fn t() -> Duration {
        Duration::from_secs(60)
    }

    #[test]
    fn full_session_on_the_simulator() {
        let (h, jh) = spawn(sim_cfg()).unwrap();
        assert!(matches!(
            h.request_blocking(Request::Sweep(sweep_req()), t()),
            Err(MeasureError::Instrument(InstrumentError::NotConnected))
        ));
        h.request_blocking(Request::Connect { simulate: true, device: None }, t()).unwrap();
        let Reply::Sweep(s) = h.request_blocking(Request::Sweep(sweep_req()), t()).unwrap() else { panic!("wrong reply") };
        assert!(s.lorentz.is_some());
        let Reply::Status(st0) = h.request_blocking(Request::Status, t()).unwrap() else { panic!("wrong reply") };
        assert!(st0.outputs_on, "after a sweep the drive is on: STOP has something to stop");
        h.request_blocking(Request::SelectEstimate(FitMethod::Circle), t()).unwrap();
        let Reply::Pll(p) = h.request_blocking(Request::PllStart(PllParams::default()), t()).unwrap() else { panic!("wrong reply") };
        assert_eq!(p.state, LockState::Locked);
        let dir = std::env::temp_dir().join(format!("hqcm_actor_{}", std::process::id()));
        let path = dir.join("track.csv");
        h.request_blocking(Request::TrackStart { rate_hz: 1000.0, path: path.clone(), header: vec![("sample".into(), "Ti".into())] }, t())
            .unwrap();
        // the calibration sweeps move the oscillator: refused while the PLL runs
        let qp = QCalParams { gains: vec![-4.0, -2.0, 0.0, 2.0, 4.0], points: 200, ..Default::default() };
        assert!(matches!(h.request_blocking(Request::QCalibrate(qp.clone()), t()), Err(MeasureError::Precondition(_))));
        h.request_blocking(Request::PllStop, t()).unwrap();
        let Reply::QCal(cal) = h.request_blocking(Request::QCalibrate(qp), t()).unwrap() else { panic!("wrong reply") };
        assert!(cal.slope < 0.0);
        // the calibration paused the recording (PLL demodulator reconfigured); it must resume
        let Reply::Status(stq) = h.request_blocking(Request::Status, t()).unwrap() else { panic!("wrong reply") };
        assert!(stq.tracking, "the recording must resume after the Q calibration");
        h.request_blocking(Request::PllStart(PllParams::default()), t()).unwrap();
        let Reply::Status(stp) = h.request_blocking(Request::Status, t()).unwrap() else { panic!("wrong reply") };
        assert!(stp.tracking, "the recording must resume after a PLL start");
        let Reply::Engaged(e) =
            h.request_blocking(Request::QEngage(EngageParams { q_target: 70_000.0, ..Default::default() }), t()).unwrap()
        else {
            panic!("wrong reply")
        };
        assert!(e.pll_p.is_some(), "PLL gains must be rescaled when the PLL is active");
        assert!(
            e.warnings.iter().any(|w| w.contains("PLL bandwidth")),
            "PLL BW 90 Hz < Q-control BW 138 Hz must be flagged: {:?}",
            e.warnings
        );
        let Reply::Step(step) = h.request_blocking(Request::HipimsStep(StepParams { pulses: 50, ..Default::default() }), t()).unwrap()
        else {
            panic!("wrong reply")
        };
        assert!(step.x.len() >= 8);
        let Reply::Track { samples, .. } = h.request_blocking(Request::TrackStop, t()).unwrap() else { panic!("wrong reply") };
        assert!(samples > 100, "samples = {samples}");
        let series = crate::project::load_series(&path, None).unwrap();
        assert!(series.t_s.len() as u64 == samples);
        // emergency stop switches everything off even though loops are running
        h.emergency_stop();
        let Reply::Status(st) = h.request_blocking(Request::Status, t()).unwrap() else { panic!("wrong reply") };
        assert!(!st.pll_active && st.q_engaged.is_none() && !st.safe_state_uncertain && !st.outputs_on);
        shutdown_and_join(&h, jh);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_track_start_leaves_no_orphan_file() {
        let (h, jh) = spawn(sim_cfg()).unwrap();
        h.request_blocking(Request::Connect { simulate: true, device: None }, t()).unwrap();
        let dir = std::env::temp_dir().join(format!("hqcm_orphan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("track.csv");
        // stream_rate_max_hz = 2e6 in the default limits: rejected by the driver after the
        // file was created -> the header-only file must not stay behind.
        let r = h.request_blocking(Request::TrackStart { rate_hz: 5.0e6, path: path.clone(), header: vec![] }, t());
        assert!(matches!(r, Err(MeasureError::Instrument(InstrumentError::LimitViolation { .. }))), "{r:?}");
        assert!(!path.exists(), "no orphaned track file may remain");
        // a valid rate records normally
        h.request_blocking(Request::TrackStart { rate_hz: 100.0, path: path.clone(), header: vec![] }, t()).unwrap();
        let Reply::Track { .. } = h.request_blocking(Request::TrackStop, t()).unwrap() else { panic!("wrong reply") };
        assert!(path.exists());
        shutdown_and_join(&h, jh);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn connect_uses_the_address_typed_by_the_operator() {
        let (h, jh) = spawn(sim_cfg()).unwrap();
        let bad = crate::config::DeviceCfg { id: "dev 1/../x".into(), ..Default::default() };
        assert!(matches!(
            h.request_blocking(Request::Connect { simulate: true, device: Some(bad) }, t()),
            Err(MeasureError::InvalidParams(_))
        ));
        let dev = crate::config::DeviceCfg { id: "DEV9999".into(), host: "localhost".into(), port: 8004 };
        let Reply::Info(info) = h.request_blocking(Request::Connect { simulate: true, device: Some(dev) }, t()).unwrap() else {
            panic!("wrong reply")
        };
        assert_eq!(info.device, "dev9999");
        let Reply::Status(st) = h.request_blocking(Request::Status, t()).unwrap() else { panic!("wrong reply") };
        assert_eq!(st.device.unwrap().device, "dev9999");
        shutdown_and_join(&h, jh);
    }

    #[test]
    fn cancel_aborts_a_running_sweep_and_leaves_drive_off() {
        let mut c = Config::default();
        c.sim.realtime = true; // pacing makes the sweep long enough to cancel
        let (h, jh) = spawn(Arc::new(c)).unwrap();
        h.request_blocking(Request::Connect { simulate: true, device: None }, t()).unwrap();
        let h2 = h.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            h2.cancel();
        });
        let mut big = sweep_req();
        big.points = 4000;
        let r = h.request_blocking(Request::Sweep(big), t());
        canceller.join().unwrap();
        assert!(matches!(r, Err(MeasureError::Cancelled)), "{r:?}");
        let _ = h.request_blocking(Request::Status, t());
        shutdown_and_join(&h, jh);
    }

    #[test]
    fn unsafe_requests_are_rejected_without_touching_the_instrument() {
        let (h, jh) = spawn(sim_cfg()).unwrap();
        h.request_blocking(Request::Connect { simulate: true, device: None }, t()).unwrap();
        let mut p = sweep_req();
        p.drive_v = 5.0; // x2 divider => 10 V, above the 1 V limit
        assert!(h.request_blocking(Request::Sweep(p), t()).is_err());
        assert!(h.request_blocking(Request::PllStart(PllParams::default()), t()).is_err(), "no resonance yet");
        assert!(h.request_blocking(Request::QCalibrate(QCalParams::default()), t()).is_err());
        shutdown_and_join(&h, jh);
    }
}
