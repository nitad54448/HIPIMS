//! PLL (PID 1) lock-in: PID Advisor gains, polarity handling, lock detection.
//!
//! Polarity rule (Q-control notes, step 2): the phase slope through resonance is
//! *normal* when theta decreases with frequency => negative P/I gains; if parasitic
//! capacitance inverts the slope the gains must be positive. The advisor supplies
//! magnitudes; the sign comes from the measured polarity.

use crate::analysis::fit::{Polarity, ResonanceEstimate};
use crate::error::MeasureError;
use crate::instrument::uhf::{Loop, PllSetup, Uhf};
use crate::instrument::{AdvisorRequest, AdvisorResult, JobCtl};
use crate::units::{Degree, Hertz, PidGain, Second};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PllParams {
    /// Target closed-loop bandwidth (Hz); typically 50-100 Hz for a 50 kQ crystal.
    pub target_bw_hz: f64,
    /// Multiplier on the advisor bandwidth (0.8-1 works well).
    pub bw_factor: f64,
    /// Demodulator time constant of the PLL demod (0.3-1 ms for HiPIMS).
    pub demod_tc_s: f64,
    pub demod_order: u8,
    /// Maximum oscillator excursion around the sweep f0 (Hz).
    pub limit_hz: f64,
    /// Use the LabOne PID Advisor module (false => local model).
    pub use_device_advisor: bool,
    /// Explicit gains (override the advisor); signs are applied as given.
    pub manual_gains: Option<[f64; 3]>,
}

impl Default for PllParams {
    fn default() -> Self {
        Self {
            target_bw_hz: 100.0,
            bw_factor: 0.9,
            demod_tc_s: 5e-4,
            demod_order: 4,
            limit_hz: 2_000.0,
            use_device_advisor: true,
            manual_gains: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LockState {
    Off,
    Seeking,
    Locked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PllStatus {
    pub state: LockState,
    pub freq_hz: f64,
    pub phase_error_deg: f64,
    pub at_limit: bool,
    pub p: f64,
    pub i: f64,
    pub d: f64,
    pub bandwidth_hz: f64,
    pub advisor_from_device: bool,
    pub polarity: Polarity,
}

/// Local PI model used when the device advisor is unavailable.
///
/// Loop: input = phase error (deg), output = frequency correction (Hz). The
/// resonator is a static phase-to-frequency slope `s` (deg/Hz); the closed-loop
/// pole of a PI loop is `I s / (1 + P s)`, so `I = 2 pi BW (1 + P s) / s` with
/// `P s = 0.25`.
pub fn local_advisor(slope_deg_per_hz: f64, target_bw_hz: f64) -> Result<AdvisorResult, String> {
    if !(slope_deg_per_hz.is_finite() && slope_deg_per_hz > 0.0 && target_bw_hz > 0.0) {
        return Err("advisor needs a positive phase slope and bandwidth".into());
    }
    let ps = 0.25;
    let p = ps / slope_deg_per_hz;
    let i = 2.0 * std::f64::consts::PI * target_bw_hz * (1.0 + ps) / slope_deg_per_hz;
    Ok(AdvisorResult { p, i, d: 0.0, bandwidth_hz: target_bw_hz, from_device: false })
}

/// Apply the polarity sign to advisor magnitudes.
pub fn signed_gains(advice: &AdvisorResult, polarity: Polarity) -> (f64, f64, f64) {
    let s = polarity.gain_sign();
    (s * advice.p.abs(), s * advice.i.abs(), 0.0)
}

/// Ask the advisor for PLL gains for the measured resonance.
pub fn advise(uhf: &mut Uhf, est: &ResonanceEstimate, p: &PllParams, ctl: &mut dyn JobCtl) -> Result<AdvisorResult, MeasureError> {
    let bw = p.target_bw_hz * p.bw_factor;
    if !(bw.is_finite() && bw > 0.0) {
        return Err(MeasureError::InvalidParams("target bandwidth x factor must be positive".into()));
    }
    if p.use_device_advisor {
        // The advisor reads the PID wiring (input/output/centre) from the device: program
        // it first (PLL disabled, zero gains) unless the loop is already running.
        let running = uhf.loop_enabled(Loop::Pll).map_err(MeasureError::from_instrument)?;
        if !running {
            let setup = PllSetup {
                center: Hertz(est.f0_hz),
                phase_setpoint: Degree(est.phase_deg),
                p: PidGain(0.0),
                i: PidGain(0.0),
                d: PidGain(0.0),
                limit: Hertz(p.limit_hz),
            };
            uhf.configure_pll(&setup).map_err(MeasureError::from_instrument)?;
            uhf.sync().map_err(MeasureError::from_instrument)?;
        }
        let req = AdvisorRequest {
            pid_index: uhf.config().pid.pll_index,
            target_bw_hz: bw,
            // 3 = "Resonator frequency" DUT model (Q-control notes: use the resonator model).
            dut_source: 3,
            slope_deg_per_hz: est.slope_deg_per_hz,
            q_factor: est.q,
            f0_hz: est.f0_hz,
        };
        match uhf.advisor(&req, ctl) {
            Ok(a) => return Ok(a),
            Err(crate::error::InstrumentError::Cancelled) => return Err(MeasureError::Cancelled),
            Err(e) => tracing::warn!(error = %e, "device PID advisor failed, using the local model"),
        }
    }
    local_advisor(est.slope_deg_per_hz, bw).map_err(MeasureError::Fit)
}

/// Phase error (deg, wrapped to +-180) of a sample relative to the setpoint.
pub fn phase_error_deg(phase_rad: f64, setpoint: Degree) -> f64 {
    let mut e = (phase_rad.to_degrees() - setpoint.get()) % 360.0;
    if e > 180.0 {
        e -= 360.0
    }
    if e < -180.0 {
        e += 360.0
    }
    e
}

/// One lock check: small phase error and oscillator away from the rails.
pub fn check_lock(uhf: &mut Uhf, setpoint: Degree, center: Hertz, limit: Hertz, tol_deg: f64) -> Result<PllStatus, MeasureError> {
    let demod = uhf.config().roles.pll_demod;
    let s = uhf.read_sample(demod).map_err(MeasureError::from_instrument)?;
    let err = phase_error_deg(s.phase_rad, setpoint);
    let at_limit = (s.freq_hz - center.get()).abs() >= 0.98 * limit.get();
    let (p, i, d) = uhf.gains(Loop::Pll).map_err(MeasureError::from_instrument)?;
    let state = if !at_limit && err.abs() <= tol_deg { LockState::Locked } else { LockState::Seeking };
    Ok(PllStatus {
        state,
        freq_hz: s.freq_hz,
        phase_error_deg: err,
        at_limit,
        p: p.get(),
        i: i.get(),
        d: d.get(),
        bandwidth_hz: 0.0,
        advisor_from_device: false,
        polarity: Polarity::Normal,
    })
}

/// Configure demod + PID, enable the PLL and wait for a stable lock.
pub fn start(uhf: &mut Uhf, est: &ResonanceEstimate, p: &PllParams, ctl: &mut dyn JobCtl) -> Result<PllStatus, MeasureError> {
    let cfg = uhf.config().clone();
    let roles = cfg.roles.clone();
    ctl.progress(0.05, "computing PLL gains");
    let (advice, (gp, gi, gd)) = match p.manual_gains {
        Some([a, b, c]) => (AdvisorResult { p: a, i: b, d: c, bandwidth_hz: p.target_bw_hz, from_device: false }, (a, b, c)),
        None => {
            let a = advise(uhf, est, p, ctl)?;
            let g = signed_gains(&a, est.polarity);
            (a, g)
        }
    };
    uhf.configure_demod(roles.pll_demod, roles.osc_main, Second(p.demod_tc_s), p.demod_order, Hertz(1.0e3))
        .map_err(MeasureError::from_instrument)?;
    let setup = PllSetup {
        center: Hertz(est.f0_hz),
        phase_setpoint: Degree(est.phase_deg),
        p: PidGain(gp),
        i: PidGain(gi),
        d: PidGain(gd),
        limit: Hertz(p.limit_hz),
    };
    uhf.configure_pll(&setup).map_err(MeasureError::from_instrument)?;
    uhf.sync().map_err(MeasureError::from_instrument)?;
    uhf.enable_loop(Loop::Pll, true).map_err(MeasureError::from_instrument)?;
    // Wait for 5 consecutive good readings.
    let deadline = cfg.timing.pll_lock_timeout_s;
    let step = cfg.timing.pll_lock_poll_s;
    let mut waited = 0.0;
    let mut good = 0;
    let mut last;
    loop {
        if ctl.cancelled() {
            if let Err(e) = uhf.enable_loop(Loop::Pll, false) {
                tracing::error!(error = %e, "PLL could not be disabled after cancel");
            }
            return Err(MeasureError::Cancelled);
        }
        uhf.wait(Second(step));
        waited += step;
        last = check_lock(uhf, setup.phase_setpoint, setup.center, setup.limit, cfg.timing.pll_lock_tol_deg)?;
        good = if last.state == LockState::Locked { good + 1 } else { 0 };
        ctl.progress(0.1 + 0.9 * (waited / deadline).min(1.0), "waiting for PLL lock");
        if good >= 5 || waited >= deadline {
            break;
        }
    }
    last.p = gp;
    last.i = gi;
    last.d = gd;
    last.bandwidth_hz = advice.bandwidth_hz;
    last.advisor_from_device = advice.from_device;
    last.polarity = est.polarity;
    if good < 5 {
        last.state = LockState::Seeking;
        if let Err(e) = uhf.enable_loop(Loop::Pll, false) {
            tracing::error!(error = %e, "PLL could not be disabled after a lock failure");
        }
        return Err(MeasureError::Precondition(format!(
            "PLL did not lock within {deadline:.0} s (phase error {:.1} deg, polarity {:?}); check slope polarity, bandwidth, cables",
            last.phase_error_deg, est.polarity
        )));
    }
    Ok(last)
}

pub fn stop(uhf: &mut Uhf) -> Result<(), MeasureError> {
    uhf.enable_loop(Loop::Pll, false).map_err(MeasureError::from_instrument)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advisor_signs_follow_polarity() {
        let a = local_advisor(0.5, 100.0).unwrap();
        let (p, i, d) = signed_gains(&a, Polarity::Normal);
        assert!(p < 0.0 && i < 0.0 && d == 0.0);
        let (p, i, _) = signed_gains(&a, Polarity::Inverted);
        assert!(p > 0.0 && i > 0.0);
    }

    #[test]
    fn advisor_pole_matches_target_bandwidth() {
        let s = 0.8;
        let a = local_advisor(s, 120.0).unwrap();
        let bw = a.i * s / (1.0 + a.p * s) / (2.0 * std::f64::consts::PI);
        assert!((bw - 120.0).abs() < 1e-9);
        assert!(local_advisor(0.0, 10.0).is_err());
    }

    #[test]
    fn phase_error_wraps() {
        assert!((phase_error_deg(179f64.to_radians(), Degree(-179.0)) + 2.0).abs() < 1e-9);
    }
}
