//! Physics simulation of a UHF with a 6 MHz QCM, used for tests/CI and for
//! developing on a PC without LabOne. It implements [`NodeIo`] with *virtual time*:
//! `wait()` and `stream_poll()` advance the simulated clock (and sleep in real time
//! only if `sim.realtime` is set, for interactive demos).
//!
//! Modelled physics
//! * resonator `H(f) = 1 / (1 + i (f - f_res)/gamma_f)` rotated by an electrical
//!   phase; `gamma_f = Gamma / (2 pi)`; the phase *decreases* through resonance
//!   (normal slope) or increases (`inverted_slope`);
//! * Q-control (two PIDs on X and Y, `P = -gain`): `Gamma_eff = Gamma_native + gain_slope * gain`
//!   while both loops are enabled; the peak grows as `Gamma_native / Gamma_eff`; at
//!   `Gamma_eff <= 0` the response is saturated (lasing);
//!   amplitude steps relax as `exp(-Gamma_eff t)` (plus an optional fast parasitic
//!   component); `Gamma_eff < 0` => exponential growth (lasing);
//! * PLL: first-order tracking with rate `|I| s / (1 + |P| s)`; the *wrong gain sign*
//!   drives the oscillator to the limit (loss of lock);
//! * slow deposition drift of the resonance and a HiPIMS plasma pulse with a fast
//!   resonance shift, seen through the demodulator filter.

use super::*;
use crate::config::Config;
use crate::error::TransportError;
use std::collections::HashMap;
use std::sync::Arc;

const ELECTRICAL_PHASE: f64 = 0.7;
const GAIN: f64 = 0.01;

struct Rng(u64);
impl Rng {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let v = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (v >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        let (a, b) = (self.uniform().max(1e-300), self.uniform());
        (-2.0 * a.ln()).sqrt() * (2.0 * std::f64::consts::PI * b).cos()
    }
}

#[derive(Clone, Copy)]
struct AmpEvent {
    t_event: f64,
    from: f64,
    to: f64,
}

pub struct SimUhf {
    cfg: Arc<Config>,
    nodes: HashMap<String, NodeValue>,
    t: f64,
    rng: Rng,
    connected: bool,
    f_osc: f64,
    amp: AmpEvent,
    stream: Option<StreamConfig>,
    stream_t: f64,
    /// Test hook: count of node writes.
    pub writes: usize,
}

impl SimUhf {
    pub fn new(cfg: Arc<Config>) -> Self {
        let f0 = cfg.sim.f0_hz;
        Self {
            cfg,
            nodes: HashMap::new(),
            t: 0.0,
            rng: Rng(0x9E37_79B9_7F4A_7C15),
            connected: false,
            f_osc: f0,
            amp: AmpEvent { t_event: 0.0, from: 0.0, to: 0.0 },
            stream: None,
            stream_t: 0.0,
            writes: 0,
        }
    }

    fn dev(&self) -> &str {
        &self.cfg.device.id
    }
    fn path(&self, s: &str) -> String {
        format!("/{}/{}", self.dev(), s)
    }
    fn nd(&self, s: &str) -> f64 {
        self.nodes.get(&self.path(s)).map_or(0.0, |v| v.as_f64())
    }
    fn ni(&self, s: &str) -> i64 {
        self.nodes.get(&self.path(s)).map_or(0, |v| v.as_i64())
    }

    fn gamma_native(&self) -> f64 {
        std::f64::consts::PI * self.cfg.sim.f0_hz / self.cfg.sim.q
    }

    fn pid_on(&self, idx: u8) -> bool {
        self.ni(&format!("pids/{idx}/enable")) != 0
    }

    /// Feedback gain of the Q-control (`-P`), zero unless both loops are enabled.
    fn q_gain(&self) -> f64 {
        let (x, y) = (self.cfg.pid.q_x_index, self.cfg.pid.q_y_index);
        if self.pid_on(x) && self.pid_on(y) {
            -self.nd(&format!("pids/{x}/p"))
        } else {
            0.0
        }
    }

    fn gamma_eff(&self) -> f64 {
        self.gamma_native() + self.cfg.sim.gain_slope * self.q_gain()
    }

    fn gamma_hz(&self) -> f64 {
        self.gamma_eff().abs().max(1e-3) / (2.0 * std::f64::consts::PI)
    }

    fn f_res(&self, t: f64) -> f64 {
        self.cfg.sim.f0_hz + self.cfg.sim.drift_hz_per_s * t
    }

    fn drive_target(&self) -> f64 {
        let r = &self.cfg.routing;
        if self.ni(&format!("sigouts/{}/on", r.sigout_drive)) != 0
            && self.ni(&format!("sigouts/{}/enables/{}", r.sigout_drive, r.drive_mixer)) != 0
        {
            self.nd(&format!("sigouts/{}/amplitudes/{}", r.sigout_drive, r.drive_mixer))
        } else {
            0.0
        }
    }

    /// Effective (relaxing) drive amplitude at time `t`.
    fn amp_env(&self, t: f64) -> f64 {
        let e = self.amp;
        let dt = (t - e.t_event).max(0.0);
        let mut a = e.to + (e.from - e.to) * (-self.gamma_eff() * dt).exp();
        if self.cfg.sim.parasitic_decay {
            a += 0.15 * (e.from - e.to) * (-dt / 20e-6).exp();
        }
        a
    }

    fn retarget_amp(&mut self) {
        let now = self.amp_env(self.t);
        self.amp = AmpEvent { t_event: self.t, from: now, to: self.drive_target() };
    }

    /// Complex response (x, y) at drive frequency `f`, resonance `f_res`, amplitude `a`.
    fn response(&self, f: f64, f_res: f64, a: f64, gamma_hz: f64) -> (f64, f64) {
        if self.gamma_eff() <= 0.02 * self.gamma_native() {
            // lasing: the feedback saturates, no resonance structure left
            return (0.5, 0.5);
        }
        let boost = self.gamma_native() / self.gamma_eff();
        let a = a * boost;
        let u = (f - f_res) / gamma_hz;
        let (re, mut im) = (1.0 / (1.0 + u * u), -u / (1.0 + u * u));
        if self.cfg.sim.inverted_slope {
            im = -im;
        }
        let (c, s) = (ELECTRICAL_PHASE.cos(), ELECTRICAL_PHASE.sin());
        (a * GAIN * (re * c - im * s), a * GAIN * (re * s + im * c))
    }

    fn pll_params(&self) -> Option<(f64, f64, f64, f64, f64)> {
        let idx = self.cfg.pid.pll_index;
        if !self.pid_on(idx) {
            return None;
        }
        let g = |n: &str| self.nd(&format!("pids/{idx}/{n}"));
        Some((g("p"), g("i"), g("setpoint"), g("center"), g("limitupper")))
    }

    /// Advance virtual time by `dt` with PLL dynamics in <= 1 ms chunks.
    fn advance(&mut self, dt: f64) {
        let mut left = dt.max(0.0);
        while left > 0.0 {
            let h = left.min(1e-3);
            self.step(h);
            left -= h;
        }
    }

    fn step(&mut self, h: f64) {
        self.t += h;
        if let Some((p, i, setpoint_deg, center, limit)) = self.pll_params() {
            let gh = self.gamma_hz();
            let slope = (1.0 / gh).to_degrees(); // deg / Hz
            let good_sign = (p * if self.cfg.sim.inverted_slope { 1.0 } else { -1.0 }) >= 0.0
                && (i * if self.cfg.sim.inverted_slope { 1.0 } else { -1.0 }) >= 0.0;
            let lam = i.abs() * slope / (1.0 + p.abs() * slope);
            let f_res = self.f_res(self.t);
            let th_set = setpoint_deg.to_radians();
            let dtheta = if self.cfg.sim.inverted_slope { th_set - ELECTRICAL_PHASE } else { ELECTRICAL_PHASE - th_set };
            let dtheta = dtheta.clamp(-1.39, 1.39);
            let target = (f_res + gh * dtheta.tan()).clamp(center - limit, center + limit);
            if good_sign {
                self.f_osc += (target - self.f_osc) * (1.0 - (-lam * h).exp());
            } else {
                let d = self.f_osc - f_res;
                let d = if d.abs() < 1e-3 { 1e-3 } else { d };
                self.f_osc = (f_res + d * (lam * h).exp()).clamp(center - limit, center + limit);
            }
            let osc = self.cfg.roles.osc_main;
            let key = self.path(&format!("oscs/{osc}/freq"));
            self.nodes.insert(key, NodeValue::Double(self.f_osc));
        }
    }

    fn sample_at(&mut self, t: f64, noise: bool) -> Sample {
        let f_res = self.f_res(t);
        let a = self.amp_env(t);
        let gh = self.gamma_hz();
        let (mut x, mut y) = self.response(self.f_osc, f_res, a, gh);
        if noise {
            let sig = self.cfg.sim.noise_rel * a * GAIN;
            x += sig * self.rng.gauss();
            y += sig * self.rng.gauss();
        }
        Sample { t_s: t, x, y, freq_hz: self.f_osc, phase_rad: y.atan2(x) }
    }

    fn pace(&self, d: Duration) {
        if self.cfg.sim.realtime {
            std::thread::sleep(d);
        }
    }

    fn need_connected(&self, op: &str) -> Result<(), TransportError> {
        if self.connected {
            Ok(())
        } else {
            Err(TransportError::Remote { op: op.into(), message: "simulator not connected".into() })
        }
    }

    /// Resonance shift (Hz) caused by the simulated plasma pulse at time `t` (us) after the pulse start.
    fn pulse_shift_hz(&self, t_us: f64) -> f64 {
        let s = &self.cfg.sim;
        if t_us <= 0.0 {
            0.0
        } else {
            let t = t_us.min(s.pulse_width_us);
            s.pulse_shift_hz * (1.0 - (-t / s.pulse_rise_us.max(1e-3)).exp())
        }
    }
}

impl NodeIo for SimUhf {
    fn connect(&mut self) -> Result<DeviceInfo, TransportError> {
        self.connected = true;
        Ok(DeviceInfo {
            device: self.dev().to_string(),
            devtype: "UHFLI".into(),
            serial: "SIM-0001".into(),
            options: vec!["PID".into(), "MF".into(), "MD".into()],
            firmware: "sim".into(),
            simulated: true,
        })
    }

    fn set(&mut self, node: &str, value: NodeValue) -> Result<(), TransportError> {
        self.need_connected("set")?;
        if !node.starts_with(&format!("/{}/", self.dev())) {
            return Err(TransportError::Remote { op: "set".into(), message: format!("unknown node `{node}`") });
        }
        if let NodeValue::Double(d) = value {
            if !d.is_finite() {
                return Err(TransportError::Remote { op: "set".into(), message: format!("non-finite value for `{node}`") });
            }
        }
        self.writes += 1;
        self.nodes.insert(node.to_string(), value);
        let osc = self.cfg.roles.osc_main;
        if node == self.path(&format!("oscs/{osc}/freq")) && self.pll_params().is_none() {
            self.f_osc = value.as_f64();
        }
        let r = &self.cfg.routing;
        let amp_nodes = [
            self.path(&format!("sigouts/{}/amplitudes/{}", r.sigout_drive, r.drive_mixer)),
            self.path(&format!("sigouts/{}/on", r.sigout_drive)),
            self.path(&format!("sigouts/{}/enables/{}", r.sigout_drive, r.drive_mixer)),
        ];
        if amp_nodes.iter().any(|n| n == node) {
            // Re-target; the previous envelope is evaluated *before* the new value (it uses
            // the old `to`, which is still stored in `self.amp`).
            self.retarget_amp();
        }
        Ok(())
    }

    fn get(&mut self, node: &str) -> Result<NodeValue, TransportError> {
        self.need_connected("get")?;
        Ok(self.nodes.get(node).copied().unwrap_or(NodeValue::Int(0)))
    }

    fn sync(&mut self) -> Result<(), TransportError> {
        self.need_connected("sync")
    }

    fn read_sample(&mut self, _demod: u8) -> Result<Sample, TransportError> {
        self.need_connected("read_sample")?;
        let t = self.t;
        Ok(self.sample_at(t, true))
    }

    fn wait(&mut self, d: Duration) {
        self.advance(d.as_secs_f64());
        self.pace(d);
    }

    fn sweep(&mut self, req: &SweepRequest, ctl: &mut dyn JobCtl) -> Result<SweepData, TransportError> {
        self.need_connected("sweep")?;
        let n = req.points;
        let a = self.drive_target();
        let gh = self.gamma_hz();
        let f_res = self.f_res(self.t);
        let mut out = SweepData::default();
        for k in 0..n {
            if ctl.cancelled() {
                return Err(TransportError::Cancelled);
            }
            let f = req.start_hz + (req.stop_hz - req.start_hz) * k as f64 / (n - 1) as f64;
            let (mut x, mut y) = self.response(f, f_res, a, gh);
            let sig = self.cfg.sim.noise_rel * a * GAIN;
            x += sig * self.rng.gauss();
            y += sig * self.rng.gauss();
            out.freq_hz.push(f);
            out.x.push(x);
            out.y.push(y);
            // settling time of one point ~ 5 time constants of the sweep bandwidth
            let dt = 5.0 / (2.0 * std::f64::consts::PI * req.bandwidth_hz.max(1e-3));
            self.t += dt;
            if k % 8 == 0 {
                ctl.progress(k as f64 / n as f64, "sweeping");
                self.pace(Duration::from_millis(5));
            }
        }
        Ok(out)
    }

    fn stream_start(&mut self, cfg: &StreamConfig) -> Result<(), TransportError> {
        self.need_connected("stream_start")?;
        self.stream = Some(cfg.clone());
        self.stream_t = self.t;
        Ok(())
    }

    fn stream_poll(&mut self, window: Duration) -> Result<Vec<Sample>, TransportError> {
        self.need_connected("stream_poll")?;
        let Some(cfg) = self.stream.clone() else {
            return Err(TransportError::Remote { op: "stream_poll".into(), message: "stream not started".into() });
        };
        let dt = 1.0 / cfg.rate_hz;
        let t_end = self.t + window.as_secs_f64();
        let mut out = Vec::new();
        let mut t = self.stream_t + dt;
        while t <= t_end + 1e-12 {
            // advance the PLL physics in <= 1 ms chunks, evaluate the samples in between
            if t - self.t > 1e-3 {
                let step = t - self.t;
                self.advance(step);
            } else if t > self.t {
                self.step(t - self.t);
            }
            out.push(self.sample_at(t, true));
            t += dt;
        }
        if t_end > self.t {
            self.advance(t_end - self.t);
        }
        self.stream_t = t - dt;
        self.pace(window);
        Ok(out)
    }

    fn stream_stop(&mut self) -> Result<(), TransportError> {
        self.stream = None;
        Ok(())
    }

    fn gated_acquire(&mut self, req: &GatedRequest, ctl: &mut dyn JobCtl) -> Result<GatedResult, TransportError> {
        self.need_connected("gated_acquire")?;
        let n = req.points;
        let a = self.drive_target();
        let tc = {
            let d = self.cfg.roles.meas_demod;
            self.nd(&format!("demods/{d}/timeconstant")).max(1e-9)
        };
        let gh = self.gamma_hz();
        let f_res = self.f_res(self.t);
        let dt = req.window_s / n as f64;
        let alpha = 1.0 - (-dt / tc).exp();
        let start_us = req.hint_window_start_s * 1e6;
        let sig = self.cfg.sim.noise_rel * a * GAIN / (req.pulses.max(1) as f64).sqrt();
        // Demod filter state starts at the steady state of the signal at the window start.
        let shift0 = self.pulse_shift_hz(start_us);
        let (mut fx, mut fy) = self.response(f_res, f_res + shift0, a, gh);
        let mut out = GatedResult { pulses: req.pulses, ..Default::default() };
        for k in 0..n {
            if ctl.cancelled() {
                return Err(TransportError::Cancelled);
            }
            let t_us = start_us + k as f64 * dt * 1e6;
            let shift = self.pulse_shift_hz(t_us);
            let (x, y) = self.response(f_res, f_res + shift, a, gh);
            fx += (x - fx) * alpha;
            fy += (y - fy) * alpha;
            out.t_s.push(k as f64 * dt);
            out.x.push(fx + sig * self.rng.gauss());
            out.y.push(fy + sig * self.rng.gauss());
            if k % 64 == 0 {
                ctl.progress(k as f64 / n as f64, "gated acquisition");
            }
        }
        // acquisition takes `pulses` repetition periods of the generator (assume 100 Hz)
        self.advance(req.pulses as f64 * 0.01);
        Ok(out)
    }

    fn pid_advisor(&mut self, req: &AdvisorRequest, _ctl: &mut dyn JobCtl) -> Result<AdvisorResult, TransportError> {
        self.need_connected("pid_advisor")?;
        let mut a = crate::measure::pll::local_advisor(req.slope_deg_per_hz, req.target_bw_hz)
            .map_err(|m| TransportError::Remote { op: "pid_advisor".into(), message: m })?;
        a.from_device = false;
        Ok(a)
    }

    fn load_settings(&mut self, path: &str) -> Result<(), TransportError> {
        self.need_connected("load_settings")?;
        let text = std::fs::read_to_string(path).map_err(|e| TransportError::Io { op: "load_settings".into(), message: e.to_string() })?;
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                if let Ok(i) = v.parse::<i64>() {
                    self.nodes.insert(k.to_string(), NodeValue::Int(i));
                } else if let Ok(d) = v.parse::<f64>() {
                    self.nodes.insert(k.to_string(), NodeValue::Double(d));
                }
            }
        }
        Ok(())
    }

    fn save_settings(&mut self, path: &str) -> Result<(), TransportError> {
        self.need_connected("save_settings")?;
        let mut keys: Vec<_> = self.nodes.keys().cloned().collect();
        keys.sort();
        let text: String = keys
            .iter()
            .map(|k| match self.nodes[k] {
                NodeValue::Int(i) => format!("{k} = {i}\n"),
                NodeValue::Double(d) => format!("{k} = {d:?}\n"),
            })
            .collect();
        std::fs::write(path, text).map_err(|e| TransportError::Io { op: "save_settings".into(), message: e.to_string() })
    }

    fn close(&mut self) {
        self.connected = false;
        self.stream = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::fit;
    use crate::instrument::uhf::{Loop, PllSetup, Uhf};
    use crate::measure::{pll, qcontrol, sweep};
    use crate::units::*;

    fn cfg() -> Arc<Config> {
        let mut c = Config::default();
        c.sim.realtime = false;
        Arc::new(c)
    }

    fn uhf_with(cfg: Arc<Config>) -> Uhf {
        Uhf::connect(Box::new(SimUhf::new(cfg.clone())), cfg).unwrap()
    }

    fn sweep_params() -> sweep::SweepParams {
        sweep::SweepParams {
            start_hz: 5.9970e6,
            stop_hz: 5.9998e6,
            points: 280,
            drive_v: 0.05,
            compensate_divider: true,
            bandwidth_hz: None,
        }
    }

    #[test]
    fn sweep_and_fit_recover_resonator() {
        let mut u = uhf_with(cfg());
        let r = sweep::run(&mut u, &sweep_params(), &mut NoCtl).unwrap();
        let e = r.estimate(fit::FitMethod::Lorentz).unwrap();
        assert!((e.f0_hz - 5.9985e6).abs() < 3.0, "f0 {}", e.f0_hz);
        assert!((e.q / 50_000.0 - 1.0).abs() < 0.08, "Q {}", e.q);
        let c = r.estimate(fit::FitMethod::Circle).unwrap();
        assert!((c.q / 50_000.0 - 1.0).abs() < 0.08, "Q circle {}", c.q);
        assert_eq!(e.polarity, fit::Polarity::Normal);
    }

    #[test]
    fn inverted_slope_is_detected_and_pll_locks_only_with_right_sign() {
        let mut c = Config::default();
        c.sim.realtime = false;
        c.sim.inverted_slope = true;
        let c = Arc::new(c);
        let mut u = uhf_with(c.clone());
        let r = sweep::run(&mut u, &sweep_params(), &mut NoCtl).unwrap();
        let est = r.estimate(fit::FitMethod::Lorentz).unwrap().clone();
        assert_eq!(est.polarity, fit::Polarity::Inverted);
        let status = pll::start(&mut u, &est, &pll::PllParams::default(), &mut NoCtl).unwrap();
        assert_eq!(status.state, pll::LockState::Locked);
        assert!(status.p > 0.0 && status.i > 0.0);
        // Wrong sign: manual negative gains on an inverted resonance cannot lock.
        pll::stop(&mut u).unwrap();
        let bad = pll::PllParams { manual_gains: Some([-1.0, -300.0, 0.0]), ..Default::default() };
        let err = pll::start(&mut u, &est, &bad, &mut NoCtl).unwrap_err();
        assert!(matches!(err, crate::error::MeasureError::Precondition(_)), "{err}");
        assert!(!u.loop_enabled(Loop::Pll).unwrap());
    }

    #[test]
    fn pll_tracks_deposition_drift() {
        let c = cfg();
        let mut u = uhf_with(c.clone());
        let r = sweep::run(&mut u, &sweep_params(), &mut NoCtl).unwrap();
        let est = r.estimate(fit::FitMethod::Circle).unwrap().clone();
        pll::start(&mut u, &est, &pll::PllParams::default(), &mut NoCtl).unwrap();
        u.start_stream(c.roles.pll_demod, Hertz(1000.0)).unwrap();
        let _ = u.poll_stream(Second(1.0)).unwrap();
        let s = u.poll_stream(Second(20.0)).unwrap();
        u.stop_stream().unwrap();
        let first = s.first().unwrap().freq_hz;
        let last = s.last().unwrap().freq_hz;
        // drift -0.8 Hz/s over ~20 s
        assert!((last - first + 16.0).abs() < 2.0, "df = {}", last - first);
    }

    #[test]
    fn q_calibration_recovers_gamma_slope_and_engages_target() {
        let c = cfg();
        let mut u = uhf_with(c.clone());
        let r = sweep::run(&mut u, &sweep_params(), &mut NoCtl).unwrap();
        let est = r.estimate(fit::FitMethod::Lorentz).unwrap().clone();
        let params = qcontrol::QCalParams { gains: vec![-6.0, -3.0, 0.0, 3.0, 6.0, 9.0], ..Default::default() };
        let cal = qcontrol::calibrate(&mut u, &est, &params, &mut NoCtl).unwrap();
        let gn = std::f64::consts::PI * 5.9985e6 / 50_000.0;
        assert!((cal.gamma_native / gn - 1.0).abs() < 0.05, "Gn {} vs {}", cal.gamma_native, gn);
        assert!((cal.slope / -15.0 - 1.0).abs() < 0.08, "slope {}", cal.slope);
        assert!(cal.r2 > 0.999);
        assert!(cal.gain_lasing.unwrap() > 0.0);
        // enhancement narrows the effective resonance and raises the peak
        let hi = cal.points.iter().find(|p| p.gain == 9.0).unwrap();
        let lo = cal.points.iter().find(|p| p.gain == 0.0).unwrap();
        // gain 9: Gamma = 377 - 15 * 9 = 242, i.e. BW x 0.64 and peak x 1.56
        let bw_ratio = hi.bw_hz.unwrap() / lo.bw_hz.unwrap();
        let peak_ratio = hi.peak_v.unwrap() / lo.peak_v.unwrap();
        assert!((bw_ratio - 0.642).abs() < 0.03 && (peak_ratio - 1.558).abs() < 0.1, "{bw_ratio} {peak_ratio}");
        // the phase-slope Q of the blog (Q = f0/2 dtheta/df) agrees with the amplitude fit
        assert!((hi.q_phase.unwrap() / hi.q.unwrap() - 1.0).abs() < 0.06, "{:?} {:?}", hi.q_phase, hi.q);
        // Everything is back to a safe state after the calibration.
        assert!(!u.q_loops_enabled().unwrap());
        assert_eq!(u.q_gain().unwrap(), 0.0);
        assert!((u.drive_amplitude().unwrap().get() - 0.1).abs() < 1e-12);
        let g = qcontrol::gain_for_target(&cal, 80_000.0, 50.0, 0.9).unwrap();
        assert!(g > 0.0 && g < cal.gain_lasing.unwrap());
        let eng = qcontrol::engage(&mut u, &cal, &qcontrol::EngageParams { q_target: 80_000.0, ..Default::default() }, &mut NoCtl).unwrap();
        assert!((eng.gain - g).abs() < 1e-12);
        assert!(u.q_loops_enabled().unwrap());
        assert!((u.q_gain().unwrap() - g).abs() < 1e-12, "P must be -gain on both PIDs");
        assert!((u.gains(Loop::QX).unwrap().0.get() + g).abs() < 1e-12 && (u.gains(Loop::QY).unwrap().0.get() + g).abs() < 1e-12);
        // the engaged resonance really has the requested Q
        let r2 = sweep::run(&mut u, &sweep_params(), &mut NoCtl).unwrap();
        let q_eff = r2.estimate(fit::FitMethod::Lorentz).unwrap().q;
        assert!((q_eff / 80_000.0 - 1.0).abs() < 0.1, "Q_eff {q_eff}");
        qcontrol::disengage(&mut u).unwrap();
        assert!(!u.q_loops_enabled().unwrap());
    }

    #[test]
    fn calibration_aborts_when_lasing() {
        let c = cfg();
        let mut u = uhf_with(c.clone());
        let r = sweep::run(&mut u, &sweep_params(), &mut NoCtl).unwrap();
        let est = r.estimate(fit::FitMethod::Lorentz).unwrap().clone();
        // gain 40 is far beyond the lasing threshold (25) of the simulated crystal
        let params = qcontrol::QCalParams { gains: vec![0.0, 2.0, 40.0], ..Default::default() };
        let err = qcontrol::calibrate(&mut u, &est, &params, &mut NoCtl).unwrap_err();
        assert!(matches!(err, crate::error::MeasureError::SafetyAbort(_)), "{err}");
        assert!(!u.q_loops_enabled().unwrap(), "Q loops must be off after an abort");
        assert_eq!(u.q_gain().unwrap(), 0.0, "the gain must be zeroed after an abort");
    }

    #[test]
    fn calibration_refuses_while_the_pll_runs() {
        let c = cfg();
        let mut u = uhf_with(c.clone());
        let r = sweep::run(&mut u, &sweep_params(), &mut NoCtl).unwrap();
        let est = r.estimate(fit::FitMethod::Lorentz).unwrap().clone();
        pll::start(&mut u, &est, &pll::PllParams::default(), &mut NoCtl).unwrap();
        let err = qcontrol::calibrate(&mut u, &est, &qcontrol::QCalParams::default(), &mut NoCtl).unwrap_err();
        assert!(matches!(err, crate::error::MeasureError::Precondition(_)), "{err}");
    }

    #[test]
    fn q_loop_wiring_follows_the_blog_layout() {
        let c = cfg();
        let mut u = uhf_with(c.clone());
        u.enable_drive(Volt(0.1)).unwrap();
        u.configure_q_loops().unwrap();
        u.set_q_gain(5.0).unwrap();
        u.enable_q_loops(true).unwrap();
        let path = std::env::temp_dir().join(format!("hqcm_wiring_{}.txt", std::process::id()));
        u.save_settings(&path.to_string_lossy()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let has = |k: &str| text.lines().any(|l| l.trim() == k);
        // PID X (index 2): input X of the measured demod, output = amplitude channel 2, P = -gain
        assert!(has("/dev2091/pids/2/input = 0") && has("/dev2091/pids/2/outputchannel = 2") && has("/dev2091/pids/2/p = -5.0"));
        // PID Y (index 3): input Y, output = amplitude channel 3 at 90 deg
        assert!(has("/dev2091/pids/3/input = 1") && has("/dev2091/pids/3/outputchannel = 3") && has("/dev2091/pids/3/p = -5.0"));
        assert!(has("/dev2091/demods/3/phaseshift = 90.0") && has("/dev2091/demods/2/phaseshift = 0.0"));
        assert!(has("/dev2091/pids/2/limitupper = 0.25") && has("/dev2091/pids/2/setpoint = 0.0"));
        // the three sines share one output: drive channel 1 plus Q channels 2 and 3
        assert!(has("/dev2091/sigouts/0/enables/2 = 1") && has("/dev2091/sigouts/0/enables/3 = 1"));
    }

    #[test]
    fn limits_block_unsafe_writes_before_reaching_the_instrument() {
        let c = cfg();
        let mut u = uhf_with(c.clone());
        assert!(u.set_drive_amplitude(Volt(5.0)).is_err());
        assert!(u.set_osc_freq(0, Hertz(50e6)).is_err());
        assert!(u.set_q_gain(1e3).is_err());
        assert!(u.set_q_gain(f64::NAN).is_err());
        assert!(u.set_gains(Loop::QX, PidGain(1.0), PidGain(1.0), PidGain(0.0)).is_err());
        let setup = PllSetup {
            center: Hertz(5.9985e6),
            phase_setpoint: Degree(0.0),
            p: PidGain(-1.0),
            i: PidGain(-1.0),
            d: PidGain(0.0),
            limit: Hertz(1e6),
        };
        assert!(u.configure_pll(&setup).is_err());
    }

    #[test]
    fn safe_state_turns_everything_off_and_runs_on_drop() {
        let c = cfg();
        let mut u = uhf_with(c.clone());
        u.enable_drive(Volt(0.1)).unwrap();
        u.configure_q_loops().unwrap();
        u.set_q_gain(3.0).unwrap();
        u.enable_q_loops(true).unwrap();
        u.enable_loop(Loop::Agc, true).unwrap();
        u.safe_state().unwrap();
        assert!(!u.loop_enabled(Loop::Agc).unwrap());
        assert!(!u.q_loops_enabled().unwrap());
        assert_eq!(u.q_gain().unwrap(), 0.0);
        assert!(!u.drive_is_on().unwrap());
    }

    #[test]
    fn settings_roundtrip() {
        let c = cfg();
        let mut u = uhf_with(c.clone());
        u.set_osc_freq(0, Hertz(5.99e6)).unwrap();
        let path = std::env::temp_dir().join("hipims_sim_settings.txt");
        let p = path.to_string_lossy().to_string();
        u.save_settings(&p).unwrap();
        u.set_osc_freq(0, Hertz(5.0e6)).unwrap();
        u.load_settings(&p).unwrap();
        assert_eq!(u.osc_freq(0).unwrap().get(), 5.99e6);
        let _ = std::fs::remove_file(path);
    }
}
