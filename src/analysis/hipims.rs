//! HiPIMS gated-window analysis (principle of `Principe_mesureQCM-ZI.pdf`).
//!
//! The ZI measurement window (duration `T_w`, same repetition frequency as the
//! plasma trigger) is moved step by step against the plasma pulse `[0, W]`:
//!
//! 1. window entirely before the pulse: no overlap, must show no mass change
//!    (baseline);
//! 2. small overlap with the pulse start;
//! 3. growing overlap: gradual integration of the mass change over the pulse;
//! 4. complete overlap: equivalent to a continuous measurement of the total change.
//!
//! The pulse-averaged demod phase `theta` inside the window is converted into a
//! resonance shift. For a Lorentzian resonance with the drive held at `f0`:
//! `delta_f0 = s * gamma * tan(delta_theta)`, `gamma = f0 / (2Q)`, where `s = +1`
//! for a normal phase slope and `-1` for an inverted one.

use super::fit::Polarity;
use serde::{Deserialize, Serialize};

/// One acquired step (pulse-averaged window).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub index: usize,
    /// Window start relative to the pulse start (us); negative = before the pulse.
    pub window_start_us: f64,
    pub window_us: f64,
    pub pulses: usize,
    pub t_us: Vec<f64>,
    pub x: Vec<f64>,
    pub y: Vec<f64>,
    /// Wall-clock acquisition time (ISO 8601).
    pub acquired_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HipimsParams {
    pub pulse_width_us: f64,
    pub f0_hz: f64,
    pub q: f64,
    pub polarity: Polarity,
    /// Fraction of the window used for the end-of-window phase (last part).
    pub end_fraction: f64,
    /// Unloaded crystal frequency and film data for the thickness estimate.
    pub f_unloaded_hz: f64,
    pub rho_g_cm3: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilePoint {
    pub index: usize,
    pub overlap_us: f64,
    pub window_start_us: f64,
    pub delta_theta_deg: f64,
    pub delta_f_hz: f64,
    pub thickness_nm: f64,
    /// Mean amplitude R at the end of the window relative to the baseline (damping).
    pub amplitude_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub points: Vec<ProfilePoint>,
    /// Incremental resonance shift between consecutive overlaps (Hz/us).
    pub rate_hz_per_us: Vec<f64>,
    pub warnings: Vec<String>,
}

/// Overlap (us) of the window `[start, start + len]` with the pulse `[0, width]`.
pub fn overlap_us(window_start_us: f64, window_us: f64, pulse_width_us: f64) -> f64 {
    let lo = window_start_us.clamp(0.0, pulse_width_us);
    let hi = (window_start_us + window_us).clamp(0.0, pulse_width_us);
    (hi - lo).max(0.0)
}

fn wrap_pi(a: f64) -> f64 {
    let two = 2.0 * std::f64::consts::PI;
    let mut v = (a + std::f64::consts::PI).rem_euclid(two) - std::f64::consts::PI;
    if v <= -std::f64::consts::PI {
        v += two
    }
    v
}

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len().max(1) as f64
}

fn phases(s: &StepRecord) -> Vec<f64> {
    s.x.iter().zip(&s.y).map(|(x, y)| y.atan2(*x)).collect()
}

/// Circular mean of the phases (robust to the +-pi wrap).
fn circ_mean(p: &[f64]) -> f64 {
    let (s, c) = p.iter().fold((0.0, 0.0), |a, v| (a.0 + v.sin(), a.1 + v.cos()));
    s.atan2(c)
}

fn validate_step(s: &StepRecord) -> Result<(), String> {
    let n = s.x.len();
    if n < 4 || s.y.len() != n || s.t_us.len() != n {
        return Err(format!("step {}: trace needs >= 4 matching points", s.index));
    }
    if s.x.iter().chain(&s.y).chain(&s.t_us).any(|v| !v.is_finite()) {
        return Err(format!("step {}: non-finite samples", s.index));
    }
    Ok(())
}

/// Convert a phase change at constant drive frequency into a resonance shift.
pub fn phase_to_shift_hz(delta_theta_rad: f64, f0_hz: f64, q: f64, polarity: Polarity) -> Result<f64, String> {
    if !(f0_hz > 0.0 && q > 0.0 && delta_theta_rad.is_finite()) {
        return Err("invalid f0/Q/phase for the phase-to-frequency conversion".into());
    }
    if delta_theta_rad.abs() > 80f64.to_radians() {
        return Err("phase change too large for the linear-resonator conversion (|delta theta| > 80 deg)".into());
    }
    let gamma = f0_hz / (2.0 * q);
    let s = match polarity {
        Polarity::Normal => 1.0,
        Polarity::Inverted => -1.0,
    };
    Ok(s * gamma * delta_theta_rad.tan())
}

/// Build the cumulative profile from the recorded steps.
pub fn profile(steps: &[StepRecord], p: &HipimsParams) -> Result<Profile, String> {
    if steps.is_empty() {
        return Err("no steps recorded".into());
    }
    if !(p.pulse_width_us > 0.0 && p.end_fraction > 0.0 && p.end_fraction <= 0.5) {
        return Err("pulse width must be > 0 and end fraction in (0, 0.5]".into());
    }
    for s in steps {
        validate_step(s)?;
    }
    let mut warnings = Vec::new();
    // Baseline from the zero-overlap step(s).
    let base_steps: Vec<&StepRecord> =
        steps.iter().filter(|s| overlap_us(s.window_start_us, s.window_us, p.pulse_width_us) <= 0.0).collect();
    if base_steps.is_empty() {
        return Err("a step with zero overlap (window before the pulse) is required as baseline".into());
    }
    let base_theta = circ_mean(&base_steps.iter().map(|s| circ_mean(&phases(s))).collect::<Vec<_>>());
    let base_r =
        mean(&base_steps.iter().map(|s| mean(&s.x.iter().zip(&s.y).map(|(x, y)| x.hypot(*y)).collect::<Vec<_>>())).collect::<Vec<_>>());
    // Spread of the baseline steps = noise floor of the method.
    if base_steps.len() > 1 {
        let ths: Vec<f64> = base_steps.iter().map(|s| circ_mean(&phases(s))).collect();
        let spread = ths.iter().map(|t| wrap_pi(t - base_theta).abs()).fold(0.0, f64::max).to_degrees();
        if spread > 0.5 {
            warnings.push(format!("baseline steps differ by up to {spread:.2} deg: drift or insufficient averaging"));
        }
    }
    let mut points = Vec::new();
    for s in steps {
        let th = phases(s);
        let k = ((th.len() as f64 * p.end_fraction).ceil() as usize).clamp(1, th.len());
        let end_theta = circ_mean(&th[th.len() - k..]);
        let end_r = mean(&s.x[s.x.len() - k..].iter().zip(&s.y[s.y.len() - k..]).map(|(x, y)| x.hypot(*y)).collect::<Vec<_>>());
        let d_theta = wrap_pi(end_theta - base_theta);
        let d_f = phase_to_shift_hz(d_theta, p.f0_hz, p.q, p.polarity).unwrap_or(f64::NAN);
        if !d_f.is_finite() {
            warnings.push(format!("step {}: phase change outside the linear range", s.index));
        }
        let thickness = super::sauerbrey::sauerbrey_thickness_nm(d_f, p.f_unloaded_hz, p.rho_g_cm3).unwrap_or(f64::NAN);
        points.push(ProfilePoint {
            index: s.index,
            overlap_us: overlap_us(s.window_start_us, s.window_us, p.pulse_width_us),
            window_start_us: s.window_start_us,
            delta_theta_deg: d_theta.to_degrees(),
            delta_f_hz: d_f,
            thickness_nm: thickness,
            amplitude_ratio: if base_r > 0.0 { end_r / base_r } else { f64::NAN },
        });
    }
    points.sort_by(|a, b| a.overlap_us.partial_cmp(&b.overlap_us).unwrap_or(std::cmp::Ordering::Equal));
    let rate = points
        .windows(2)
        .map(|w| {
            let d = w[1].overlap_us - w[0].overlap_us;
            if d > 0.0 {
                (w[1].delta_f_hz - w[0].delta_f_hz) / d
            } else {
                f64::NAN
            }
        })
        .collect();
    Ok(Profile { points, rate_hz_per_us: rate, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(index: usize, start: f64, win: f64, shift_phase_end: f64, pulse: f64) -> StepRecord {
        // Phase ramps linearly from 0 to `shift_phase_end` over the overlap part.
        let n = 200;
        let t: Vec<f64> = (0..n).map(|i| start + win * i as f64 / (n - 1) as f64).collect();
        let (mut x, mut y) = (vec![], vec![]);
        for ti in &t {
            let frac = (ti.clamp(0.0, pulse)) / pulse;
            let th = shift_phase_end * frac;
            x.push(1e-3 * th.cos());
            y.push(1e-3 * th.sin());
        }
        StepRecord { index, window_start_us: start, window_us: win, pulses: 100, t_us: t, x, y, acquired_at: String::new() }
    }

    fn params() -> HipimsParams {
        HipimsParams {
            pulse_width_us: 100.0,
            f0_hz: 6.0e6,
            q: 50_000.0,
            polarity: Polarity::Normal,
            end_fraction: 0.05,
            f_unloaded_hz: 6.0e6,
            rho_g_cm3: 4.5,
        }
    }

    #[test]
    fn overlap_geometry() {
        assert_eq!(overlap_us(-300.0, 200.0, 100.0), 0.0);
        assert_eq!(overlap_us(-190.0, 200.0, 100.0), 10.0);
        assert_eq!(overlap_us(-50.0, 200.0, 100.0), 100.0);
        assert_eq!(overlap_us(0.0, 400.0, 100.0), 100.0);
    }

    #[test]
    fn phase_shift_conversion_sign_and_size() {
        // gamma = 60 Hz; 10 deg => 60 * tan(10 deg) = 10.58 Hz resonance shift magnitude
        let d = phase_to_shift_hz(-10f64.to_radians(), 6.0e6, 50_000.0, Polarity::Normal).unwrap();
        assert!((d + 10.58).abs() < 0.01, "{d}");
        let d2 = phase_to_shift_hz(-10f64.to_radians(), 6.0e6, 50_000.0, Polarity::Inverted).unwrap();
        assert!((d2 - 10.58).abs() < 0.01);
        assert!(phase_to_shift_hz(2.0, 6.0e6, 50_000.0, Polarity::Normal).is_err());
    }

    #[test]
    fn profile_grows_with_overlap_and_needs_baseline() {
        let end = -20f64.to_radians();
        let steps = vec![
            step(0, -400.0, 200.0, end, 100.0),
            step(1, -190.0, 200.0, end, 100.0),
            step(2, -150.0, 200.0, end, 100.0),
            step(3, -100.0, 200.0, end, 100.0),
            step(4, 0.0, 200.0, end, 100.0),
        ];
        let pr = profile(&steps, &params()).unwrap();
        assert_eq!(pr.points.len(), 5);
        assert!(pr.points[0].delta_f_hz.abs() < 1e-6);
        // end-of-window phase for step with overlap o is end * o/pulse
        let p3 = pr.points.iter().find(|p| p.index == 3).unwrap();
        assert!((p3.overlap_us - 100.0).abs() < 1e-9);
        assert!(p3.delta_f_hz < -20.0 && p3.delta_f_hz > -22.0, "{}", p3.delta_f_hz);
        assert!(p3.thickness_nm > 0.0);
        assert!(pr.points.windows(2).all(|w| w[1].delta_f_hz <= w[0].delta_f_hz + 1e-9));
        assert!(profile(&steps[1..], &params()).is_err());
    }
}
