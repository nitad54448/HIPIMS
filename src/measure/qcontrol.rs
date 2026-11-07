//! Q-control after the Zurich Instruments blog *Resonance engineering of quality
//! factor: the Q-control method* (R. Stomp, 2024) and its `mfli_qcontrol.py`.
//!
//! Principle: the measured response is demodulated; its X and Y components are scaled
//! by a P gain and re-injected into the drive at 0 deg and 90 deg (two extra
//! demodulators on the same oscillator, two PIDs, one signal output). A negative P
//! (`P = -gain`, gain > 0) adds the response back in phase and enhances Q; the opposite
//! sign adds loss and lowers Q. The *effective* resonance is measured with an ordinary
//! frequency sweep (blog: `Q = f0/2 * dtheta/df`), so no ring-down is needed.
//!
//! Calibration: sweep the effective resonance at several gains. For a Lorentzian
//! resonator the feedback only rescales the width, `BW_eff = BW_native (1 - k g)`, so the
//! damping rate `Gamma = pi * BW` is linear in the gain: `Gamma(g) = Gamma_native + m g`
//! with the signed slope `m` *fitted*. Then
//!
//! * `g_target = (pi f0/Q_target - Gamma_native) / m`
//! * lasing threshold `g_las = -Gamma_native / m` (effective bandwidth reaches zero)
//!
//! With a PLL the blog requires the PLL closed-loop bandwidth to exceed the Q-control
//! bandwidth (set by the demodulator filter in front of the PIDs).

use crate::analysis::fit::{self, ResonanceEstimate};
use crate::error::MeasureError;
use crate::instrument::uhf::{Loop, Uhf};
use crate::instrument::JobCtl;
use crate::measure::sweep::{self, SweepParams};
use crate::units::{Hertz, PidGain, Second, Volt};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct QCalParams {
    /// Feedback gains to scan (include 0 for the native resonance). Run in order of increasing |gain|.
    pub gains: Vec<f64>,
    /// Centre of the sweeps. None: the fitted resonance. The resonance drifts during a deposition, so the
    /// service passes the frequency where the PLL last was.
    pub center_hz: Option<f64>,
    /// Sweep span as a multiple of the native bandwidth.
    pub span_bw_factor: f64,
    pub points: usize,
    /// Demodulator bandwidth of the sweeps (None: a quarter of the native bandwidth).
    pub bandwidth_hz: Option<f64>,
    /// Fraction of the lasing threshold that must not be exceeded (0.8 - 0.9).
    pub lasing_margin: f64,
}

impl Default for QCalParams {
    fn default() -> Self {
        Self {
            gains: vec![-4.0, -2.0, 0.0, 2.0, 4.0, 8.0],
            center_hz: None,
            span_bw_factor: 8.0,
            points: 400,
            bandwidth_hz: None,
            lasing_margin: 0.9,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QCalPoint {
    pub gain: f64,
    pub f0_hz: Option<f64>,
    pub bw_hz: Option<f64>,
    pub q: Option<f64>,
    /// Q from the phase slope at resonance (blog formula `Q = f0/2 dtheta/df`), as a cross-check.
    pub q_phase: Option<f64>,
    /// Damping rate `pi * BW` (1/s).
    pub gamma: Option<f64>,
    pub peak_v: Option<f64>,
    pub note: String,
    /// Decimated sweep (the effective resonance) for display.
    pub freq_hz: Vec<f64>,
    pub r: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QCalibration {
    pub f0_hz: f64,
    pub q_native_sweep: f64,
    /// Intercept of Gamma(gain) at gain = 0 (1/s).
    pub gamma_native: f64,
    pub q_native: f64,
    /// dGamma/dgain (1/s per unit gain), signed.
    pub slope: f64,
    pub r2: f64,
    pub gain_lasing: Option<f64>,
    pub points: Vec<QCalPoint>,
    pub acquired_at: String,
}

fn decimate(f: &[f64], r: &[f64], max: usize) -> (Vec<f64>, Vec<f64>) {
    let step = (f.len() / max.max(1)).max(1);
    (f.iter().step_by(step).copied().collect(), r.iter().step_by(step).copied().collect())
}

/// Linear fit of Gamma against the gain from the valid points: (intercept, slope, r2, gain at Gamma = 0).
pub fn fit_gamma_vs_gain(points: &[QCalPoint]) -> Result<(f64, f64, f64, Option<f64>), MeasureError> {
    let (g, gamma): (Vec<f64>, Vec<f64>) = points.iter().filter_map(|p| p.gamma.map(|v| (p.gain, v))).unzip();
    if g.len() < 3 {
        return Err(MeasureError::Fit(format!("need >= 3 valid effective-resonance sweeps, have {}", g.len())));
    }
    let l = fit::linear_fit(&g, &gamma).map_err(MeasureError::Fit)?;
    if l.slope == 0.0 {
        return Err(MeasureError::Fit("the bandwidth does not depend on the gain (zero slope): check the Q-loop wiring/phases".into()));
    }
    let las = (l.intercept > 0.0).then(|| -l.intercept / l.slope);
    Ok((l.intercept, l.slope, l.r2, las))
}

/// `true` if `gain` lies on the unstable side beyond `margin` of the lasing threshold.
fn skip_for_lasing(gain: f64, las: Option<f64>, margin: f64) -> bool {
    match las {
        Some(k) if k != 0.0 && gain != 0.0 && gain.signum() == k.signum() => gain.abs() > margin * k.abs(),
        _ => false,
    }
}

/// Everything back to zero feedback; best effort, every step attempted and logged.
fn safe_off(uhf: &mut Uhf, f0: Hertz) {
    let osc = uhf.config().roles.osc_main;
    for (what, r) in [
        ("gain := 0", uhf.set_q_gain(0.0)),
        ("Q loops off", uhf.enable_q_loops(false)),
        ("Q feedback channels := 0", uhf.zero_q_outputs()),
        ("oscillator back to f0", uhf.set_osc_freq(osc, f0)),
    ] {
        if let Err(e) = r {
            tracing::error!(error = %e, step = what, "Q-calibration cleanup step failed");
        }
    }
}

/// A resonance peak must stand out from the baseline; a flat or saturated response means
/// the loop ran into its limits (lasing) or the resonance left the sweep range.
fn stands_out(r: &[f64]) -> bool {
    let med = fit::median(r);
    let max = r.iter().copied().fold(f64::MIN, f64::max);
    let min = r.iter().copied().fold(f64::MAX, f64::min);
    med.is_finite() && med > 0.0 && (max / med >= 1.5 || med / min.max(1e-30) >= 1.5)
}

/// Calibrate the gain-to-Q relation with sweeps of the effective resonance. The PLL must be
/// off (the sweep moves the oscillator) and the drive on.
pub fn calibrate(uhf: &mut Uhf, est: &ResonanceEstimate, p: &QCalParams, ctl: &mut dyn JobCtl) -> Result<QCalibration, MeasureError> {
    let inv = |m: &str| Err(MeasureError::InvalidParams(m.into()));
    if p.gains.len() < 3 || p.gains.iter().any(|g| !g.is_finite()) {
        return inv("provide at least three finite gains");
    }
    if !(p.lasing_margin > 0.0 && p.lasing_margin < 1.0) {
        return inv("lasing margin must be in (0, 1)");
    }
    if !(p.span_bw_factor >= 3.0 && p.span_bw_factor <= 100.0) || p.points < 50 {
        return inv("span must be 3-100 native bandwidths and at least 50 points");
    }
    let cfg = uhf.config().clone();
    let (roles, timing) = (cfg.roles.clone(), cfg.timing.clone());
    if uhf.loop_enabled(Loop::Pll).map_err(MeasureError::from_instrument)? {
        return Err(MeasureError::Precondition("switch the PLL off before the Q calibration: the sweeps move the oscillator".into()));
    }
    let a0 = uhf.drive_amplitude().map_err(MeasureError::from_instrument)?;
    if a0.get() <= 0.0 {
        return Err(MeasureError::Precondition("the drive output is off: run a sweep first".into()));
    }
    let span = p.span_bw_factor * est.bw_hz;
    let center = p.center_hz.unwrap_or(est.f0_hz);
    let (start, stop) = (center - span / 2.0, center + span / 2.0);
    let bw = p.bandwidth_hz.unwrap_or((est.bw_hz / 4.0).clamp(1.0, timing.sweep_bandwidth_hz.max(1.0)));

    // The PID inputs are X and Y of the PLL demodulator: it must run (PLL off => not yet configured).
    uhf.configure_demod(roles.pll_demod, roles.osc_main, Second(timing.q_demod_tc_s), timing.q_demod_order, Hertz(1.0e3))
        .map_err(MeasureError::from_instrument)?;
    uhf.configure_q_loops().map_err(MeasureError::from_instrument)?;
    uhf.enable_q_loops(true).map_err(MeasureError::from_instrument)?;

    let mut order: Vec<f64> = p.gains.clone();
    order.sort_by(|a, b| a.abs().partial_cmp(&b.abs()).unwrap_or(std::cmp::Ordering::Equal));
    let mut points: Vec<QCalPoint> = Vec::new();
    let mut lasing: Option<f64> = None;
    let n = order.len();
    let mut outcome: Result<(), MeasureError> = Ok(());
    for (idx, &g) in order.iter().enumerate() {
        if ctl.cancelled() {
            outcome = Err(MeasureError::Cancelled);
            break;
        }
        ctl.progress(idx as f64 / n as f64, &format!("effective resonance {}/{} (gain {g})", idx + 1, n));
        if skip_for_lasing(g, lasing, p.lasing_margin) {
            points.push(QCalPoint {
                gain: g,
                f0_hz: None,
                bw_hz: None,
                q: None,
                q_phase: None,
                gamma: None,
                peak_v: None,
                note: "skipped: beyond the lasing margin".into(),
                freq_hz: vec![],
                r: vec![],
            });
            continue;
        }
        let step = (|| -> Result<QCalPoint, MeasureError> {
            uhf.set_q_gain(g).map_err(MeasureError::from_instrument)?;
            uhf.wait(Second(timing.pid_settle_s));
            let sp = SweepParams {
                start_hz: start,
                stop_hz: stop,
                points: p.points,
                drive_v: a0.get(),
                compensate_divider: false,
                bandwidth_hz: Some(bw),
            };
            let res = sweep::run(uhf, &sp, ctl)?;
            if !stands_out(&res.r) {
                return Err(MeasureError::SafetyAbort(format!(
                    "no resonance peak at gain {g}: the feedback is saturated (lasing?) or the resonance left the sweep range"
                )));
            }
            let Some(e) = res.lorentz.as_ref().or(res.circle.as_ref()) else {
                let why = res.fit_errors.join("; ");
                return if g == 0.0 {
                    Err(MeasureError::Fit(why))
                } else {
                    Err(MeasureError::SafetyAbort(format!("fit failed at gain {g} ({why}): stopping for safety")))
                };
            };
            if !(e.bw_hz > 0.05 * est.bw_hz) {
                return Err(MeasureError::SafetyAbort(format!(
                    "effective bandwidth {:.3} Hz at gain {g} is below 5 % of the native one: too close to lasing",
                    e.bw_hz
                )));
            }
            let q_phase = e.f0_hz / 2.0 * e.slope_deg_per_hz.to_radians();
            let mut note = String::new();
            if q_phase.is_finite() && q_phase > 0.0 && (q_phase / e.q - 1.0).abs() > 0.25 {
                note = format!("Q from the phase slope ({q_phase:.0}) differs from the amplitude fit ({:.0})", e.q);
            }
            let (fd, rd) = decimate(&res.freq_hz, &res.r, 200);
            Ok(QCalPoint {
                gain: g,
                f0_hz: Some(e.f0_hz),
                bw_hz: Some(e.bw_hz),
                q: Some(e.q),
                q_phase: Some(q_phase),
                gamma: Some(std::f64::consts::PI * e.bw_hz),
                peak_v: Some(res.r.iter().copied().fold(f64::MIN, f64::max)),
                note,
                freq_hz: fd,
                r: rd,
            })
        })();
        match step {
            Ok(pt) => points.push(pt),
            Err(e) => {
                outcome = Err(e);
                break;
            }
        }
        if let Ok((_, _, _, las)) = fit_gamma_vs_gain(&points) {
            lasing = las;
        }
    }
    safe_off(uhf, Hertz(center));
    outcome?;
    let (g_native, slope, r2, las) = fit_gamma_vs_gain(&points)?;
    if g_native <= 0.0 {
        return Err(MeasureError::Fit("non-positive native damping: the sweeps are unreliable".into()));
    }
    let q_native = crate::units::DampingRate(g_native).quality_factor(Hertz(est.f0_hz)).get();
    ctl.progress(1.0, "calibration done");
    Ok(QCalibration {
        f0_hz: est.f0_hz,
        q_native_sweep: est.q,
        gamma_native: g_native,
        q_native,
        slope,
        r2,
        gain_lasing: las,
        points,
        acquired_at: super::now_iso(),
    })
}

/// Feedback gain for `q_target`, refusing values beyond the lasing margin or the hard limit.
pub fn gain_for_target(cal: &QCalibration, q_target: f64, gain_abs_max: f64, lasing_margin: f64) -> Result<f64, MeasureError> {
    if !(q_target.is_finite() && q_target > 0.0) {
        return Err(MeasureError::InvalidParams("target Q must be positive".into()));
    }
    let gamma_t = std::f64::consts::PI * cal.f0_hz / q_target;
    let g = (gamma_t - cal.gamma_native) / cal.slope;
    if !g.is_finite() {
        return Err(MeasureError::Fit("gain is not finite: the calibration slope is degenerate".into()));
    }
    if skip_for_lasing(g, cal.gain_lasing, lasing_margin) {
        return Err(MeasureError::SafetyAbort(format!(
            "Q target {q_target:.0} needs gain {g:.3}, beyond {:.0} % of the lasing threshold ({:.3})",
            lasing_margin * 100.0,
            cal.gain_lasing.unwrap_or(f64::NAN)
        )));
    }
    if g.abs() > gain_abs_max {
        return Err(MeasureError::SafetyAbort(format!("gain {g:.3} exceeds the configured limit {gain_abs_max}")));
    }
    Ok(g)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngageParams {
    pub q_target: f64,
    /// Native PLL gains (P, I) to be scaled by Q_native / Q_target (None: PLL not used).
    pub pll_native_gains: Option<[f64; 2]>,
    /// Closed-loop bandwidth of the PLL (Hz), to check the blog's rule PLL BW > Q-control BW.
    pub pll_bandwidth_hz: Option<f64>,
    pub enable_agc: bool,
    /// AGC gains (P, I); needs the PID advisor (bandwidth 0.5 - 2 Hz).
    pub agc_gains: [f64; 2],
    pub lasing_margin: f64,
}

impl Default for EngageParams {
    fn default() -> Self {
        Self {
            q_target: 50_000.0,
            pll_native_gains: None,
            pll_bandwidth_hz: None,
            enable_agc: false,
            agc_gains: [0.1, 1.0],
            lasing_margin: 0.9,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Engaged {
    pub q_target: f64,
    pub gain: f64,
    pub pll_p: Option<f64>,
    pub pll_i: Option<f64>,
    pub agc_setpoint_v: Option<f64>,
    pub warnings: Vec<String>,
}

/// -3 dB bandwidth of the demodulator filter in front of the Q-control PIDs (Hz).
pub fn q_control_bandwidth_hz(tc_s: f64, order: u8) -> f64 {
    ((2f64).powf(1.0 / f64::from(order.max(1))) - 1.0).sqrt() / (2.0 * std::f64::consts::PI * tc_s)
}

/// Apply the calibrated gain, rescale the PLL gains, then (optionally) enable the AGC.
pub fn engage(uhf: &mut Uhf, cal: &QCalibration, p: &EngageParams, ctl: &mut dyn JobCtl) -> Result<Engaged, MeasureError> {
    let cfg = uhf.config().clone();
    let gain = gain_for_target(cal, p.q_target, cfg.limits.q_gain_abs_max, p.lasing_margin)?;
    let mut warnings = Vec::new();
    let q_bw = q_control_bandwidth_hz(cfg.timing.q_demod_tc_s, cfg.timing.q_demod_order);
    match p.pll_bandwidth_hz {
        Some(b) if b < q_bw => warnings.push(format!(
            "PLL bandwidth ({b:.0} Hz) is below the Q-control bandwidth ({q_bw:.0} Hz): the PLL must be faster (raise its bandwidth or increase timing.q_demod_tc_s)"
        )),
        None if p.pll_native_gains.is_some() => warnings.push("PLL bandwidth unknown: cannot check PLL BW > Q-control BW".into()),
        _ => {}
    }
    ctl.progress(0.1, "writing the feedback gain");
    uhf.configure_q_loops().map_err(MeasureError::from_instrument)?;
    uhf.set_q_gain(gain).map_err(MeasureError::from_instrument)?;
    uhf.enable_q_loops(true).map_err(MeasureError::from_instrument)?;
    let mut out = Engaged { q_target: p.q_target, gain, pll_p: None, pll_i: None, agc_setpoint_v: None, warnings };
    if let Some([pn, in_]) = p.pll_native_gains {
        // Q changes the phase slope: scale the PLL gains to keep the tracking bandwidth.
        let ratio = cal.q_native / p.q_target;
        let (np, ni) = (pn * ratio, in_ * ratio);
        let (_, _, d) = uhf.gains(Loop::Pll).map_err(MeasureError::from_instrument)?;
        uhf.set_gains(Loop::Pll, PidGain(np), PidGain(ni), d).map_err(MeasureError::from_instrument)?;
        out.pll_p = Some(np);
        out.pll_i = Some(ni);
    }
    if p.enable_agc {
        ctl.progress(0.6, "settling, then enabling the AGC");
        let roles = cfg.roles.clone();
        // Slow demodulator for the amplitude regulation (must not follow single pulses).
        uhf.configure_demod(roles.agc_demod, roles.osc_main, Second(0.1), 4, Hertz(100.0)).map_err(MeasureError::from_instrument)?;
        uhf.wait(Second(cfg.timing.pid_settle_s));
        let amp = uhf.read_sample(roles.agc_demod).map_err(MeasureError::from_instrument)?.r();
        if !(amp.is_finite() && amp > 0.0) {
            return Err(MeasureError::Precondition(format!("AGC demod reads an implausible amplitude ({amp})")));
        }
        let setpoint = Volt(amp.min(uhf.limits().drive_max_v));
        uhf.configure_agc_loop(setpoint, PidGain(p.agc_gains[0]), PidGain(p.agc_gains[1])).map_err(MeasureError::from_instrument)?;
        uhf.enable_loop(Loop::Agc, true).map_err(MeasureError::from_instrument)?;
        out.agc_setpoint_v = Some(amp);
    }
    ctl.progress(1.0, "Q-control engaged");
    Ok(out)
}

/// Switch off AGC and Q-control (gain := 0 first), every step attempted.
pub fn disengage(uhf: &mut Uhf) -> Result<(), MeasureError> {
    let mut first: Option<crate::error::InstrumentError> = None;
    for r in [uhf.enable_loop(Loop::Agc, false), uhf.set_q_gain(0.0), uhf.enable_q_loops(false), uhf.zero_q_outputs()] {
        if let Err(e) = r {
            first.get_or_insert(e);
        }
    }
    first.map_or(Ok(()), |e| Err(MeasureError::Instrument(e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal(slope: f64) -> QCalibration {
        let g = std::f64::consts::PI * 6.0e6 / 50_000.0;
        QCalibration {
            f0_hz: 6.0e6,
            q_native_sweep: 50_000.0,
            gamma_native: g,
            q_native: 50_000.0,
            slope,
            r2: 1.0,
            gain_lasing: Some(-g / slope),
            points: vec![],
            acquired_at: String::new(),
        }
    }

    fn point(gain: f64, gamma: Option<f64>) -> QCalPoint {
        QCalPoint {
            gain,
            f0_hz: None,
            bw_hz: None,
            q: None,
            q_phase: None,
            gamma,
            peak_v: None,
            note: String::new(),
            freq_hz: vec![],
            r: vec![],
        }
    }

    #[test]
    fn target_gain_is_sign_convention_free() {
        // Gamma = Gn + m g with m < 0: g > 0 raises Q (the blog's convention: P = -g).
        let c = cal(-15.0);
        assert!(gain_for_target(&c, 100_000.0, 50.0, 0.9).unwrap() > 0.0);
        assert!(gain_for_target(&c, 25_000.0, 50.0, 0.9).unwrap() < 0.0);
        // The opposite wiring flips the sign of the gain, not the physics.
        assert!(gain_for_target(&cal(15.0), 100_000.0, 50.0, 0.9).unwrap() < 0.0);
    }

    #[test]
    fn lasing_margin_and_limit_are_enforced() {
        let c = cal(-15.0); // g_las = +25.1
        assert!(matches!(gain_for_target(&c, 1.0e9, 50.0, 0.9), Err(MeasureError::SafetyAbort(_))));
        assert!(matches!(gain_for_target(&c, 25_000.0, 5.0, 0.9), Err(MeasureError::SafetyAbort(_))));
        assert!(gain_for_target(&c, -1.0, 50.0, 0.9).is_err());
    }

    #[test]
    fn gamma_vs_gain_fit() {
        let g0 = 377.0;
        let pts: Vec<QCalPoint> = [-4.0, -2.0, 0.0, 2.0, 4.0].iter().map(|&g| point(g, Some(g0 - 15.0 * g))).collect();
        let (gn, m, r2, las) = fit_gamma_vs_gain(&pts).unwrap();
        assert!((gn - g0).abs() < 1e-9 && (m + 15.0).abs() < 1e-9 && r2 > 0.999999);
        assert!((las.unwrap() - g0 / 15.0).abs() < 1e-9);
        assert!(fit_gamma_vs_gain(&pts[..2]).is_err());
        let flat: Vec<QCalPoint> = [-1.0, 0.0, 1.0].iter().map(|&g| point(g, Some(g0))).collect();
        assert!(fit_gamma_vs_gain(&flat).is_err());
    }

    #[test]
    fn lasing_skip_only_on_the_unstable_side() {
        assert!(skip_for_lasing(24.0, Some(25.0), 0.9));
        assert!(!skip_for_lasing(-24.0, Some(25.0), 0.9));
        assert!(!skip_for_lasing(10.0, Some(25.0), 0.9));
        assert!(!skip_for_lasing(10.0, None, 0.9));
    }

    #[test]
    fn demod_filter_bandwidth_of_the_q_loops() {
        // 4th order, 0.5 ms: sqrt(2^(1/4) - 1) / (2 pi 0.5e-3) = 138.5 Hz
        assert!((q_control_bandwidth_hz(0.5e-3, 4) - 138.5).abs() < 0.5);
        assert!(q_control_bandwidth_hz(5e-3, 4) < 20.0);
    }

    #[test]
    fn peak_detection() {
        let lorentz: Vec<f64> = (0..200).map(|i| 1.0 / (1.0 + ((i as f64 - 100.0) / 5.0).powi(2))).collect();
        assert!(stands_out(&lorentz));
        assert!(!stands_out(&[0.5; 200]), "a flat (saturated) response is not a resonance");
    }
}
