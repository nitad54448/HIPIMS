//! Typed UHF driver on top of [`NodeIo`].
//!
//! Every write is validated against [`Limits`] *before* it reaches the instrument.
//! Node paths and semantics refer to the *LabOne Programming Manual*, chapter
//! "UHF nodes" (`/devN/oscs`, `/demods`, `/sigouts`, `/pids`), and to the
//! Q-control notes of this project.
//!
//! `safe_state()` (also run on `Drop`) switches off both signal outputs, all four
//! PIDs and zeroes the Q-feedback; it attempts every step even if one fails.

use super::*;
use crate::config::{Config, Limits};
use crate::error::{InstrumentError, TransportError};
use crate::units::{Degree, Hertz, PidGain, Second, Volt};
use std::sync::Arc;

pub type Result<T> = std::result::Result<T, InstrumentError>;

/// Settings of the internal PLL (PID in phase -> oscillator-frequency mode).
#[derive(Debug, Clone, Copy)]
pub struct PllSetup {
    pub center: Hertz,
    pub phase_setpoint: Degree,
    pub p: PidGain,
    pub i: PidGain,
    pub d: PidGain,
    /// Maximum excursion of the oscillator around `center` (both directions).
    pub limit: Hertz,
}

/// Role of one of the three auxiliary PIDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loop {
    Pll,
    /// Q-control feedback on the X (in-phase) component of the measured response.
    QX,
    /// Q-control feedback on the Y (quadrature) component.
    QY,
    Agc,
}

pub struct Uhf {
    io: Box<dyn NodeIo>,
    dev: String,
    cfg: Arc<Config>,
    info: DeviceInfo,
    streaming: bool,
    closed: bool,
}

fn map_t(dev: &str, node: &str, e: TransportError) -> InstrumentError {
    match e {
        TransportError::Cancelled => InstrumentError::Cancelled,
        source => InstrumentError::Node { instrument: dev.to_string(), node: node.to_string(), source: Box::new(source) },
    }
}

fn map_op(dev: &str, op: &str, e: TransportError) -> InstrumentError {
    match e {
        TransportError::Cancelled => InstrumentError::Cancelled,
        source => InstrumentError::Op { instrument: dev.to_string(), op: op.to_string(), source: Box::new(source) },
    }
}

fn check(what: &str, v: f64, min: f64, max: f64) -> Result<()> {
    if v.is_finite() && v >= min && v <= max {
        Ok(())
    } else {
        Err(InstrumentError::LimitViolation { what: what.to_string(), value: v, min, max })
    }
}

impl Uhf {
    /// Connect through `io` (the transport is consumed and owned by the driver).
    pub fn connect(mut io: Box<dyn NodeIo>, cfg: Arc<Config>) -> Result<Self> {
        let dev = cfg.device.id.clone();
        let info = io.connect().map_err(|e| map_op(&dev, "connect", e))?;
        tracing::info!(device = %info.device, devtype = %info.devtype, "connected");
        Ok(Self { io, dev, cfg, info, streaming: false, closed: false })
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }
    pub fn limits(&self) -> &Limits {
        &self.cfg.limits
    }
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    fn node(&self, suffix: &str) -> String {
        format!("/{}/{}", self.dev, suffix)
    }

    fn set_d(&mut self, suffix: &str, v: f64) -> Result<()> {
        let node = self.node(suffix);
        tracing::debug!(%node, value = v, "set");
        self.io.set(&node, NodeValue::Double(v)).map_err(|e| map_t(&self.dev, &node, e))
    }

    fn set_i(&mut self, suffix: &str, v: i64) -> Result<()> {
        let node = self.node(suffix);
        tracing::debug!(%node, value = v, "set");
        self.io.set(&node, NodeValue::Int(v)).map_err(|e| map_t(&self.dev, &node, e))
    }

    /// Best-effort write for nodes whose existence depends on options/firmware.
    fn set_d_optional(&mut self, suffix: &str, v: f64) {
        if let Err(e) = self.set_d(suffix, v) {
            tracing::warn!(error = %e, "optional node not written");
        }
    }

    fn set_i_optional(&mut self, suffix: &str, v: i64) {
        if let Err(e) = self.set_i(suffix, v) {
            tracing::warn!(error = %e, "optional node not written");
        }
    }

    fn get_d(&mut self, suffix: &str) -> Result<f64> {
        let node = self.node(suffix);
        let v = self.io.get(&node).map_err(|e| map_t(&self.dev, &node, e))?;
        let x = v.as_f64();
        if x.is_finite() {
            Ok(x)
        } else {
            Err(InstrumentError::UnexpectedValue { node, raw: format!("{v:?}") })
        }
    }

    fn get_i(&mut self, suffix: &str) -> Result<i64> {
        let node = self.node(suffix);
        self.io.get(&node).map(NodeValue::as_i64).map_err(|e| map_t(&self.dev, &node, e))
    }

    /// Wait (real time).
    pub fn wait(&mut self, d: Second) {
        self.io.wait(d.to_duration());
    }

    pub fn sync(&mut self) -> Result<()> {
        self.io.sync().map_err(|e| map_op(&self.dev, "sync", e))
    }

    // ---------------------------------------------------------------- oscillators / demods

    /// `/devN/oscs/<osc>/freq` (manual: Oscillators).
    pub fn set_osc_freq(&mut self, osc: u8, f: Hertz) -> Result<()> {
        let l = self.cfg.limits.clone();
        check("oscillator frequency [Hz]", f.get(), l.freq_min_hz, l.freq_max_hz)?;
        self.set_d(&format!("oscs/{osc}/freq"), f.get())
    }

    pub fn osc_freq(&mut self, osc: u8) -> Result<Hertz> {
        Ok(Hertz(self.get_d(&format!("oscs/{osc}/freq"))?))
    }

    /// Demodulator: oscillator, filter order, time constant, output rate, enable.
    /// Nodes: `demods/n/{oscselect,order,timeconstant,rate,enable,adcselect,harmonic}`.
    pub fn configure_demod(&mut self, demod: u8, osc: u8, tc: Second, order: u8, rate: Hertz) -> Result<()> {
        let l = self.cfg.limits.clone();
        check("demod time constant [s]", tc.get(), l.tc_min_s, l.tc_max_s)?;
        check("demod filter order", f64::from(order), 1.0, 8.0)?;
        check("demod rate [Sa/s]", rate.get(), 1.0, l.stream_rate_max_hz)?;
        let sigin = i64::from(self.cfg.routing.sigin);
        self.set_i(&format!("demods/{demod}/adcselect"), sigin)?;
        self.set_i(&format!("demods/{demod}/oscselect"), i64::from(osc))?;
        self.set_i(&format!("demods/{demod}/harmonic"), 1)?;
        self.set_i(&format!("demods/{demod}/order"), i64::from(order))?;
        self.set_d(&format!("demods/{demod}/timeconstant"), tc.get())?;
        self.set_d(&format!("demods/{demod}/rate"), rate.get())?;
        self.set_i(&format!("demods/{demod}/enable"), 1)
    }

    pub fn set_demod_phaseshift(&mut self, demod: u8, phase: Degree) -> Result<()> {
        check("demod phase shift [deg]", phase.get(), -360.0, 360.0)?;
        self.set_d(&format!("demods/{demod}/phaseshift"), phase.get())
    }

    pub fn demod_timeconstant(&mut self, demod: u8) -> Result<Second> {
        Ok(Second(self.get_d(&format!("demods/{demod}/timeconstant"))?))
    }

    /// Signal input range/AC/impedance (`sigins/n/*`).
    pub fn configure_input(&mut self) -> Result<()> {
        let n = self.cfg.routing.sigin;
        let range = self.cfg.routing.sigin_range_v;
        self.set_i(&format!("sigins/{n}/ac"), 0)?;
        self.set_d(&format!("sigins/{n}/range"), range)
    }

    // ---------------------------------------------------------------- outputs

    /// Set the amplitude of the main drive (`sigouts/<drive>/amplitudes/<mixer>`).
    pub fn set_drive_amplitude(&mut self, a: Volt) -> Result<()> {
        let max = self.cfg.limits.drive_max_v;
        check("drive amplitude [V]", a.get(), 0.0, max)?;
        let (s, m) = (self.cfg.routing.sigout_drive, self.cfg.routing.drive_mixer);
        self.set_d(&format!("sigouts/{s}/amplitudes/{m}"), a.get())
    }

    pub fn drive_amplitude(&mut self) -> Result<Volt> {
        let (s, m) = (self.cfg.routing.sigout_drive, self.cfg.routing.drive_mixer);
        Ok(Volt(self.get_d(&format!("sigouts/{s}/amplitudes/{m}"))?))
    }

    /// Enable the main drive. The amplitude is written *first*, so the output never
    /// turns on at a stale value.
    pub fn enable_drive(&mut self, amplitude: Volt) -> Result<()> {
        self.set_drive_amplitude(amplitude)?;
        let (s, m) = (self.cfg.routing.sigout_drive, self.cfg.routing.drive_mixer);
        let range = self.cfg.routing.drive_range_v;
        self.set_d(&format!("sigouts/{s}/range"), range)?;
        self.set_i(&format!("sigouts/{s}/enables/{m}"), 1)?;
        self.set_i(&format!("sigouts/{s}/on"), 1)
    }

    /// `true` if the main signal output is switched on (read back from the instrument).
    pub fn drive_is_on(&mut self) -> Result<bool> {
        let s = self.cfg.routing.sigout_drive;
        Ok(self.get_i(&format!("sigouts/{s}/on"))? != 0)
    }

    pub fn disable_drive(&mut self) -> Result<()> {
        let s = self.cfg.routing.sigout_drive;
        self.set_i(&format!("sigouts/{s}/on"), 0)
    }

    /// Zero the Q-feedback amplitude channels (they sum with the drive on the same output).
    pub fn zero_q_outputs(&mut self) -> Result<()> {
        let (s, mx, my) = (self.cfg.routing.sigout_drive, self.cfg.routing.q_mixer_x, self.cfg.routing.q_mixer_y);
        self.set_d(&format!("sigouts/{s}/amplitudes/{mx}"), 0.0)?;
        self.set_d(&format!("sigouts/{s}/amplitudes/{my}"), 0.0)
    }

    // ---------------------------------------------------------------- PIDs

    fn pid_index(&self, l: Loop) -> u8 {
        match l {
            Loop::Pll => self.cfg.pid.pll_index,
            Loop::QX => self.cfg.pid.q_x_index,
            Loop::QY => self.cfg.pid.q_y_index,
            Loop::Agc => self.cfg.pid.agc_index,
        }
    }

    /// Write P/I/D of a loop. Q-control loops are pure P controllers with |P| <= `q_gain_abs_max`.
    pub fn set_gains(&mut self, l: Loop, p: PidGain, i: PidGain, d: PidGain) -> Result<()> {
        let lim = self.cfg.limits.clone();
        let idx = self.pid_index(l);
        match l {
            Loop::QX | Loop::QY => {
                check("Q-control P", p.get(), -lim.q_gain_abs_max, lim.q_gain_abs_max)?;
                if i.get() != 0.0 || d.get() != 0.0 {
                    return Err(InstrumentError::InvalidParameter("Q-control is a pure P controller (I = D = 0)".into()));
                }
            }
            _ => {
                check("PID P gain", p.get(), -lim.pid_gain_abs_max, lim.pid_gain_abs_max)?;
                check("PID I gain", i.get(), -lim.pid_gain_abs_max, lim.pid_gain_abs_max)?;
                check("PID D gain", d.get(), -lim.pid_gain_abs_max, lim.pid_gain_abs_max)?;
            }
        }
        self.set_d(&format!("pids/{idx}/p"), p.get())?;
        self.set_d(&format!("pids/{idx}/i"), i.get())?;
        self.set_d(&format!("pids/{idx}/d"), d.get())
    }

    pub fn gains(&mut self, l: Loop) -> Result<(PidGain, PidGain, PidGain)> {
        let idx = self.pid_index(l);
        Ok((
            PidGain(self.get_d(&format!("pids/{idx}/p"))?),
            PidGain(self.get_d(&format!("pids/{idx}/i"))?),
            PidGain(self.get_d(&format!("pids/{idx}/d"))?),
        ))
    }

    pub fn enable_loop(&mut self, l: Loop, on: bool) -> Result<()> {
        let idx = self.pid_index(l);
        self.set_i(&format!("pids/{idx}/enable"), i64::from(on))
    }

    pub fn loop_enabled(&mut self, l: Loop) -> Result<bool> {
        let idx = self.pid_index(l);
        Ok(self.get_i(&format!("pids/{idx}/enable"))? != 0)
    }

    /// Program (but do not enable) the PLL: PID 1, input = demod theta, output =
    /// oscillator frequency (manual: PID tab; example `example_pid_advisor_pll`).
    pub fn configure_pll(&mut self, s: &PllSetup) -> Result<()> {
        let lim = self.cfg.limits.clone();
        check("PLL centre [Hz]", s.center.get(), lim.freq_min_hz, lim.freq_max_hz)?;
        check("PLL limit [Hz]", s.limit.get(), 1.0, lim.pll_limit_hz_max)?;
        check("PLL phase setpoint [deg]", s.phase_setpoint.get(), -360.0, 360.0)?;
        let (idx, roles, wiring) = (self.cfg.pid.pll_index, self.cfg.roles.clone(), self.cfg.pid.clone());
        self.enable_loop(Loop::Pll, false)?;
        if let Some(m) = wiring.pll_mode_value {
            self.set_i_optional(&format!("pids/{idx}/mode"), m);
        }
        self.set_i(&format!("pids/{idx}/input"), wiring.input_theta)?;
        self.set_i(&format!("pids/{idx}/inputchannel"), i64::from(roles.pll_demod))?;
        self.set_d(&format!("pids/{idx}/setpoint"), s.phase_setpoint.get())?;
        self.set_i(&format!("pids/{idx}/output"), wiring.output_osc_freq)?;
        self.set_i(&format!("pids/{idx}/outputchannel"), i64::from(roles.osc_main))?;
        self.set_d(&format!("pids/{idx}/center"), s.center.get())?;
        self.set_i(&format!("pids/{idx}/phaseunwrap"), 1)?;
        self.set_d(&format!("pids/{idx}/limitlower"), -s.limit.get())?;
        self.set_d(&format!("pids/{idx}/limitupper"), s.limit.get())?;
        self.set_osc_freq(roles.osc_main, s.center)?;
        self.set_gains(Loop::Pll, s.p, s.i, s.d)
    }

    /// Both Q-control loops on or off.
    pub fn enable_q_loops(&mut self, on: bool) -> Result<()> {
        let rx = self.enable_loop(Loop::QX, on);
        let ry = self.enable_loop(Loop::QY, on);
        rx.and(ry)
    }

    pub fn q_loops_enabled(&mut self) -> Result<bool> {
        Ok(self.loop_enabled(Loop::QX)? && self.loop_enabled(Loop::QY)?)
    }

    /// Feedback gain `g` of the Q-control: `P = -g` on both PIDs (PID error = setpoint - input,
    /// so a negative P re-injects the measured X and Y in phase: g > 0 enhances Q, g < 0 damps).
    /// Validated against `q_gain_abs_max`.
    pub fn set_q_gain(&mut self, gain: f64) -> Result<()> {
        let max = self.cfg.limits.q_gain_abs_max;
        check("Q-control gain", gain, -max, max)?;
        let p = PidGain(-gain);
        // A zero gain is written as +0.0 (never -0.0).
        let p = if gain == 0.0 { PidGain(0.0) } else { p };
        self.set_gains(Loop::QX, p, PidGain(0.0), PidGain(0.0))?;
        self.set_gains(Loop::QY, p, PidGain(0.0), PidGain(0.0))
    }

    /// Present Q-control gain (`-P` of the X loop).
    pub fn q_gain(&mut self) -> Result<f64> {
        Ok(-self.gains(Loop::QX)?.0.get() + 0.0)
    }

    /// Program (and leave disabled, gain 0) the two Q-control loops after the
    /// Zurich Instruments blog "Resonance engineering of quality factor": two extra
    /// demodulators on the drive oscillator, the first at 0 deg, the second at 90 deg;
    /// PID X acts on the X of the measured response, PID Y on its Y; their outputs
    /// (limited to +-`q_amp_max_v`: fail-safe against lasing) drive the amplitude
    /// channels of those two demodulators on the *same* signal output as the drive.
    pub fn configure_q_loops(&mut self) -> Result<()> {
        let (roles, wiring, routing, timing) =
            (self.cfg.roles.clone(), self.cfg.pid.clone(), self.cfg.routing.clone(), self.cfg.timing.clone());
        let amp = self.cfg.limits.q_amp_max_v;
        self.enable_q_loops(false)?;
        let rate = Hertz(1.0e3);
        for (demod, phase) in [(roles.q_demod_x, 0.0), (roles.q_demod_y, 90.0)] {
            self.configure_demod(demod, roles.osc_main, Second(timing.q_demod_tc_s), timing.q_demod_order, rate)?;
            self.set_demod_phaseshift(demod, Degree(phase))?;
        }
        let s = routing.sigout_drive;
        for (idx, input, mixer) in
            [(wiring.q_x_index, wiring.input_x, routing.q_mixer_x), (wiring.q_y_index, wiring.input_y, routing.q_mixer_y)]
        {
            self.set_d(&format!("sigouts/{s}/amplitudes/{mixer}"), 0.0)?;
            self.set_i(&format!("sigouts/{s}/enables/{mixer}"), 1)?;
            self.set_i(&format!("pids/{idx}/input"), input)?;
            self.set_i(&format!("pids/{idx}/inputchannel"), i64::from(roles.pll_demod))?;
            self.set_d(&format!("pids/{idx}/setpoint"), 0.0)?;
            self.set_i(&format!("pids/{idx}/output"), wiring.output_sigout_amplitude)?;
            // For a signal-output-amplitude PID the output channel is the amplitude channel.
            self.set_i(&format!("pids/{idx}/outputchannel"), i64::from(mixer))?;
            self.set_d(&format!("pids/{idx}/center"), 0.0)?;
            self.set_d(&format!("pids/{idx}/limitlower"), -amp)?;
            self.set_d(&format!("pids/{idx}/limitupper"), amp)?;
            // The blog script also sets a PID-internal demodulator; absent on some firmware.
            self.set_d_optional(&format!("pids/{idx}/demod/timeconstant"), timing.q_demod_tc_s);
            self.set_i_optional(&format!("pids/{idx}/demod/order"), i64::from(timing.q_demod_order));
            self.set_i_optional(&format!("pids/{idx}/demod/harmonic"), 1);
            self.set_i_optional(&format!("pids/{idx}/keepint"), 0);
        }
        self.set_q_gain(0.0)
    }

    /// Program the AGC loop (PID 4): demod-4 R held at `target_amplitude` by the
    /// main drive amplitude. Gains from the PID Advisor (BW 0.5-2 Hz).
    pub fn configure_agc_loop(&mut self, target: Volt, p: PidGain, i: PidGain) -> Result<()> {
        let max = self.cfg.limits.drive_max_v;
        check("AGC setpoint [V]", target.get(), 0.0, max)?;
        let (idx, roles, wiring, routing) =
            (self.cfg.pid.agc_index, self.cfg.roles.clone(), self.cfg.pid.clone(), self.cfg.routing.clone());
        let drive = self.drive_amplitude()?;
        self.enable_loop(Loop::Agc, false)?;
        self.set_i(&format!("pids/{idx}/input"), wiring.input_r)?;
        self.set_i(&format!("pids/{idx}/inputchannel"), i64::from(roles.agc_demod))?;
        self.set_d(&format!("pids/{idx}/setpoint"), target.get())?;
        self.set_i(&format!("pids/{idx}/output"), wiring.output_sigout_amplitude)?;
        // Signal-output-amplitude PID: the output channel is the drive's amplitude channel.
        self.set_i(&format!("pids/{idx}/outputchannel"), i64::from(routing.drive_mixer))?;
        self.set_d(&format!("pids/{idx}/center"), drive.get())?;
        self.set_d(&format!("pids/{idx}/limitlower"), -drive.get())?;
        self.set_d(&format!("pids/{idx}/limitupper"), (max - drive.get()).max(0.0))?;
        self.set_gains(Loop::Agc, p, i, PidGain(0.0))
    }

    // ---------------------------------------------------------------- acquisition

    pub fn read_sample(&mut self, demod: u8) -> Result<Sample> {
        self.io.read_sample(demod).map_err(|e| map_op(&self.dev, "read_sample", e))
    }

    pub fn sweep(&mut self, req: &SweepRequest, ctl: &mut dyn JobCtl) -> Result<SweepData> {
        let l = self.cfg.limits.clone();
        check("sweep start [Hz]", req.start_hz, l.freq_min_hz, l.freq_max_hz)?;
        check("sweep stop [Hz]", req.stop_hz, l.freq_min_hz, l.freq_max_hz)?;
        if req.stop_hz <= req.start_hz {
            return Err(InstrumentError::InvalidParameter("sweep stop must exceed start".into()));
        }
        check("sweep points", req.points as f64, 3.0, l.sweep_points_max as f64)?;
        check("sweep bandwidth [Hz]", req.bandwidth_hz, 0.01, 1.0e6)?;
        let data = self.io.sweep(req, ctl).map_err(|e| map_op(&self.dev, "sweep", e))?;
        let n = data.freq_hz.len();
        if n == 0 || data.x.len() != n || data.y.len() != n {
            return Err(InstrumentError::UnexpectedValue {
                node: "sweeper".into(),
                raw: format!("{} freq / {} x / {} y points", n, data.x.len(), data.y.len()),
            });
        }
        Ok(data)
    }

    pub fn advisor(&mut self, req: &AdvisorRequest, ctl: &mut dyn JobCtl) -> Result<AdvisorResult> {
        check("advisor target bandwidth [Hz]", req.target_bw_hz, 0.01, 1.0e5)?;
        self.io.pid_advisor(req, ctl).map_err(|e| map_op(&self.dev, "pid_advisor", e))
    }

    pub fn start_stream(&mut self, demod: u8, rate: Hertz) -> Result<()> {
        let max = self.cfg.limits.stream_rate_max_hz;
        check("stream rate [Sa/s]", rate.get(), 1.0, max)?;
        self.io.stream_start(&StreamConfig { demod, rate_hz: rate.get() }).map_err(|e| map_op(&self.dev, "stream_start", e))?;
        self.streaming = true;
        Ok(())
    }

    pub fn poll_stream(&mut self, window: Second) -> Result<Vec<Sample>> {
        self.io.stream_poll(window.to_duration()).map_err(|e| map_op(&self.dev, "stream_poll", e))
    }

    pub fn stop_stream(&mut self) -> Result<()> {
        self.streaming = false;
        self.io.stream_stop().map_err(|e| map_op(&self.dev, "stream_stop", e))
    }

    pub fn gated(&mut self, req: &GatedRequest, ctl: &mut dyn JobCtl) -> Result<GatedResult> {
        check("gate window [s]", req.window_s, 1.0e-7, 1.0)?;
        check("gate delay [s]", req.delay_s, -1.0, 1.0)?;
        check("pulses", req.pulses as f64, 1.0, 1.0e6)?;
        check("gate points", req.points as f64, 2.0, 1.0e6)?;
        self.io.gated_acquire(req, ctl).map_err(|e| map_op(&self.dev, "gated_acquire", e))
    }

    pub fn load_settings(&mut self, path: &str) -> Result<()> {
        self.io.load_settings(path).map_err(|e| map_op(&self.dev, "load_settings", e))
    }

    pub fn save_settings(&mut self, path: &str) -> Result<()> {
        self.io.save_settings(path).map_err(|e| map_op(&self.dev, "save_settings", e))
    }

    // ---------------------------------------------------------------- safety

    /// Sources off, loops off, Q-feedback zeroed, streams stopped. Every step is
    /// attempted; failures are collected and reported (the state is then *uncertain*).
    pub fn safe_state(&mut self) -> Result<()> {
        let mut failures: Vec<String> = Vec::new();
        let mut note = |what: &str, r: Result<()>| {
            if let Err(e) = r {
                failures.push(format!("{what}: {e}"));
            }
        };
        if self.streaming {
            note("stream stop", self.stop_stream());
        }
        // Q loop first (lasing risk), then AGC, then PLL.
        note("Q-control gain := 0 (X)", self.set_gains_raw_zero(Loop::QX));
        note("Q-control gain := 0 (Y)", self.set_gains_raw_zero(Loop::QY));
        for l in [Loop::QX, Loop::QY, Loop::Agc, Loop::Pll] {
            note(&format!("{l:?} disable"), self.enable_loop(l, false));
        }
        note("Q feedback channels := 0", self.zero_q_outputs());
        note("drive off", self.disable_drive());
        if failures.is_empty() {
            // Read back the critical output state where supported.
            let s = self.cfg.routing.sigout_drive;
            match self.get_i(&format!("sigouts/{s}/on")) {
                Ok(0) => Ok(()),
                Ok(v) => Err(InstrumentError::SafeStateIncomplete(format!("drive output still on ({v})"))),
                Err(e) => Err(InstrumentError::SafeStateIncomplete(format!("read-back failed: {e}"))),
            }
        } else {
            Err(InstrumentError::SafeStateIncomplete(failures.join("; ")))
        }
    }

    fn set_gains_raw_zero(&mut self, l: Loop) -> Result<()> {
        let idx = self.pid_index(l);
        self.set_d(&format!("pids/{idx}/p"), 0.0)
    }

    /// Explicit emergency stop: same as [`Uhf::safe_state`], logged at error level.
    pub fn emergency_stop(&mut self) -> Result<()> {
        tracing::error!(device = %self.dev, "EMERGENCY STOP");
        self.safe_state()
    }

    /// Orderly disconnect: safe state, then close the transport.
    pub fn disconnect(&mut self) -> Result<()> {
        let r = self.safe_state();
        self.io.close();
        self.closed = true;
        r
    }
}

impl Drop for Uhf {
    fn drop(&mut self) {
        if !self.closed {
            if let Err(e) = self.safe_state() {
                tracing::error!(error = %e, "safe-state on drop failed");
            }
            self.io.close();
        }
    }
}
