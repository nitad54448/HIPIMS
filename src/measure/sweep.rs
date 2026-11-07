//! Frequency sweep around the resonance (6 MHz QCM) + Lorentzian / circle fits.
//!
//! With the Q-control of the Zurich Instruments blog there is a single signal output
//! (drive + Q-feedback components summed inside the instrument), so no divider is
//! involved. `compensate_divider` only exists for an external two-output set-up.
//! The Q-control loops may be active during a sweep: the sweep then measures the
//! *effective* resonance, which is exactly what the Q calibration needs.

use crate::analysis::fit::{self, FitMethod, ResonanceEstimate};
use crate::error::MeasureError;
use crate::instrument::uhf::Uhf;
use crate::instrument::{JobCtl, SweepData, SweepRequest};
use crate::units::{Hertz, Volt};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepParams {
    pub start_hz: f64,
    pub stop_hz: f64,
    pub points: usize,
    /// Desired drive voltage at the resonator.
    pub drive_v: f64,
    /// Double the output amplitude (only for an external 50 ohm divider set-up).
    pub compensate_divider: bool,
    /// Demodulator bandwidth during the sweep (None => `timing.sweep_bandwidth_hz`).
    pub bandwidth_hz: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepResult {
    pub params: SweepParams,
    pub freq_hz: Vec<f64>,
    pub x: Vec<f64>,
    pub y: Vec<f64>,
    /// Magnitude R (V).
    pub r: Vec<f64>,
    /// Unwrapped phase (deg).
    pub theta_deg: Vec<f64>,
    pub lorentz: Option<ResonanceEstimate>,
    pub circle: Option<ResonanceEstimate>,
    pub fit_errors: Vec<String>,
    pub acquired_at: String,
}

impl SweepResult {
    /// Estimate selected by `method`, falling back to the other fit if missing.
    pub fn estimate(&self, method: FitMethod) -> Option<&ResonanceEstimate> {
        match method {
            FitMethod::Lorentz => self.lorentz.as_ref().or(self.circle.as_ref()),
            FitMethod::Circle => self.circle.as_ref().or(self.lorentz.as_ref()),
        }
    }
}

pub fn build_result(params: SweepParams, data: SweepData) -> SweepResult {
    let r: Vec<f64> = data.x.iter().zip(&data.y).map(|(a, b)| a.hypot(*b)).collect();
    let theta_deg: Vec<f64> = fit::raw_phase(&data.x, &data.y).iter().map(|t| t.to_degrees()).collect();
    let mut fit_errors = Vec::new();
    let lorentz = match fit::fit_lorentzian(&data.freq_hz, &data.x, &data.y) {
        Ok(e) => Some(e),
        Err(e) => {
            fit_errors.push(format!("Lorentz: {e}"));
            None
        }
    };
    let circle = match fit::fit_circle_resonance(&data.freq_hz, &data.x, &data.y) {
        Ok(e) => Some(e),
        Err(e) => {
            fit_errors.push(format!("Circle: {e}"));
            None
        }
    };
    SweepResult {
        params,
        freq_hz: data.freq_hz,
        x: data.x,
        y: data.y,
        r,
        theta_deg,
        lorentz,
        circle,
        fit_errors,
        acquired_at: super::now_iso(),
    }
}

/// Configure the instrument, run the sweep, fit. The drive is switched off again
/// if the sweep fails or is cancelled.
pub fn run(uhf: &mut Uhf, p: &SweepParams, ctl: &mut dyn JobCtl) -> Result<SweepResult, MeasureError> {
    if !(p.drive_v.is_finite() && p.drive_v > 0.0) {
        return Err(MeasureError::InvalidParams("drive voltage must be positive".into()));
    }
    let cfg = uhf.config().clone();
    let bw = p.bandwidth_hz.unwrap_or(cfg.timing.sweep_bandwidth_hz);
    let out_amp = if p.compensate_divider { 2.0 * p.drive_v } else { p.drive_v };
    let roles = cfg.roles.clone();
    let setup = |uhf: &mut Uhf| -> Result<(), crate::error::InstrumentError> {
        uhf.configure_input()?;
        uhf.configure_demod(
            roles.meas_demod,
            roles.osc_main,
            crate::units::Second(1.0 / (2.0 * std::f64::consts::PI * bw.max(0.01))),
            4,
            Hertz(1.0e3),
        )?;
        uhf.set_osc_freq(roles.osc_main, Hertz(p.start_hz))?;
        uhf.enable_drive(Volt(out_amp))?;
        uhf.sync()
    };
    setup(uhf).map_err(MeasureError::from_instrument)?;
    ctl.progress(0.0, "sweep started");
    let req = SweepRequest {
        demod: roles.meas_demod,
        osc: roles.osc_main,
        start_hz: p.start_hz,
        stop_hz: p.stop_hz,
        points: p.points,
        bandwidth_hz: bw,
        settle_inaccuracy: cfg.timing.sweep_settle_inaccuracy,
        averaging_tc: cfg.timing.sweep_averaging_tc,
    };
    match uhf.sweep(&req, ctl) {
        Ok(data) => {
            ctl.progress(1.0, "sweep finished, fitting");
            Ok(build_result(p.clone(), data))
        }
        Err(e) => {
            if let Err(e2) = uhf.disable_drive() {
                tracing::error!(error = %e2, "could not disable the drive after a failed sweep");
            }
            Err(MeasureError::from_instrument(e))
        }
    }
}
