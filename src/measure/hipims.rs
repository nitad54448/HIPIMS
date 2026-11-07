//! HiPIMS-gated acquisition (UHF Data Acquisition module with hardware trigger).
//!
//! The pulse generator produces the *plasma trigger* and the *measurement window*
//! at the same repetition frequency; only the window position relative to the pulse
//! is stepped (see `Principe_mesureQCM-ZI.pdf`). The window signal is wired to the
//! UHF trigger input; the DAQ module records `window_us` after each rising edge
//! and averages `pulses` windows. The operator sets the window start on the pulse
//! generator and records it here (`window_start_us`); the software never drives the
//! generator.

use crate::analysis::hipims::StepRecord;
use crate::error::MeasureError;
use crate::instrument::uhf::Uhf;
use crate::instrument::{GatedRequest, JobCtl};
use crate::units::{Hertz, Second};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StepParams {
    pub index: usize,
    /// Window start relative to the plasma-pulse start (us). Negative = earlier.
    pub window_start_us: f64,
    pub window_us: f64,
    /// Number of pulses averaged.
    pub pulses: usize,
    /// Points per window (grid of the DAQ module).
    pub points: usize,
    /// Fast demodulator used for the gated measurement.
    pub demod_tc_us: f64,
    pub demod_rate_hz: f64,
}

impl Default for StepParams {
    fn default() -> Self {
        Self { index: 0, window_start_us: -400.0, window_us: 300.0, pulses: 500, points: 300, demod_tc_us: 2.0, demod_rate_hz: 1.0e6 }
    }
}

pub fn validate(p: &StepParams) -> Result<(), MeasureError> {
    let bad = |m: &str| Err(MeasureError::InvalidParams(m.into()));
    if !(p.window_us.is_finite() && p.window_us > 0.0 && p.window_us <= 1.0e5) {
        return bad("window length must be in (0, 100000] us");
    }
    if !p.window_start_us.is_finite() {
        return bad("window start must be finite");
    }
    if p.pulses == 0 || p.pulses > 1_000_000 {
        return bad("pulses must be in 1..=1e6");
    }
    if p.points < 8 || p.points > 100_000 {
        return bad("points per window must be in 8..=1e5");
    }
    if !(p.demod_tc_us > 0.0) || !(p.demod_rate_hz > 0.0) {
        return bad("demodulator time constant and rate must be positive");
    }
    // The demod filter must resolve the grid spacing (notes: TC(demod 2) << pulse features).
    let dt_us = p.window_us / p.points as f64;
    if p.demod_tc_us > 10.0 * dt_us {
        return bad("demod time constant is much longer than the grid spacing: the window shape would be smeared");
    }
    Ok(())
}

pub fn acquire_step(uhf: &mut Uhf, p: &StepParams, ctl: &mut dyn JobCtl) -> Result<StepRecord, MeasureError> {
    validate(p)?;
    let cfg = uhf.config().clone();
    let roles = cfg.roles.clone();
    // The PID loops (PLL/Q/AGC) keep running; only the fast measurement demod is configured here.
    uhf.configure_demod(roles.meas_demod, roles.osc_main, Second::from_micros(p.demod_tc_us), 1, Hertz(p.demod_rate_hz))
        .map_err(MeasureError::from_instrument)?;
    uhf.sync().map_err(MeasureError::from_instrument)?;
    let req = GatedRequest {
        demod: roles.meas_demod,
        trigger_signal: cfg.routing.gate_trigger_signal.clone(),
        window_s: p.window_us * 1e-6,
        delay_s: 0.0,
        pulses: p.pulses,
        points: p.points,
        timeout_s: cfg.timing.gated_timeout_s,
        hint_window_start_s: p.window_start_us * 1e-6,
    };
    let r = uhf.gated(&req, ctl).map_err(MeasureError::from_instrument)?;
    let n = r.t_s.len();
    if n < 4 || r.x.len() != n || r.y.len() != n {
        return Err(MeasureError::Precondition(format!(
            "gated acquisition returned {} / {} / {} points (is the window signal connected to the trigger input?)",
            n,
            r.x.len(),
            r.y.len()
        )));
    }
    Ok(StepRecord {
        index: p.index,
        window_start_us: p.window_start_us,
        window_us: p.window_us,
        pulses: r.pulses,
        // Absolute time axis relative to the pulse start.
        t_us: r.t_s.iter().map(|t| p.window_start_us + t * 1e6).collect(),
        x: r.x,
        y: r.y,
        acquired_at: super::now_iso(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rules() {
        assert!(validate(&StepParams::default()).is_ok());
        assert!(validate(&StepParams { pulses: 0, ..Default::default() }).is_err());
        assert!(validate(&StepParams { demod_tc_us: 500.0, ..Default::default() }).is_err());
        assert!(validate(&StepParams { window_us: f64::NAN, ..Default::default() }).is_err());
    }
}
