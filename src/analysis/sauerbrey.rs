//! Sauerbrey / Z-match (Lu & Lewis) analysis and ion-flux fraction (IFF).
//!
//! Port of the equations of `nitad54448/Sauerbrey` (`sauerbrey.html`):
//!
//! * Sauerbrey: `sigma = -df sqrt(rho_q mu_q) / (2 f_q^2)`
//! * Z-match:   `sigma = sqrt(rho_q mu_q)/(2 pi z f) * atan(z tan(pi (f_q - f)/f_q))`
//!
//! Units: CGS as in the reference tool (rho in g/cm^3, mu in g/(cm s^2),
//! thickness returned in nm, mass in g, diameter in mm).

use serde::{Deserialize, Serialize};

pub const RHO_Q: f64 = 2.648;
pub const MU_Q: f64 = 2.947e11;
/// Above this relative frequency shift the linear Sauerbrey equation is flagged.
pub const SAUERBREY_LIMIT: f64 = 0.02;
/// Beyond this the Z-match equation is not trusted.
pub const ZMATCH_LIMIT: f64 = 0.40;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Material {
    pub name: String,
    pub rho_g_cm3: f64,
    pub z_ratio: f64,
}

pub fn presets() -> Vec<Material> {
    let m = |n: &str, rho: f64, z: f64| Material { name: n.into(), rho_g_cm3: rho, z_ratio: z };
    vec![
        m("Au", 19.30, 0.381),
        m("Ag", 10.50, 0.529),
        m("Cr", 7.19, 0.305),
        m("Ti", 4.506, 0.628),
        m("Pt", 21.45, 0.245),
        m("Cu", 8.96, 0.437),
        m("Al", 2.70, 1.080),
        m("SiO2", 2.20, 1.000),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Model {
    /// Linear Sauerbrey.
    Sauerbrey,
    /// Cumulative Z-match (reduces to Sauerbrey for small loading).
    ZMatch,
    /// Z-match everywhere, labelled "Auto" like the reference tool.
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrystalParams {
    /// Unloaded resonance frequency f_q (Hz).
    pub f_unloaded_hz: f64,
    /// Electrode/film diameter in mm (mass = areal mass x area).
    pub diameter_mm: f64,
    pub rho_g_cm3: f64,
    pub z_ratio: f64,
    pub model: Model,
}

impl Default for CrystalParams {
    fn default() -> Self {
        Self { f_unloaded_hz: 6.0e6, diameter_mm: 10.0, rho_g_cm3: 4.506, z_ratio: 0.628, model: Model::Auto }
    }
}

/// Absolute frequency versus time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FreqSeries {
    pub t_s: Vec<f64>,
    pub f_hz: Vec<f64>,
}

impl FreqSeries {
    /// Copy with time measured from the first sample (the file keeps the raw instrument clock).
    pub fn rebased(&self) -> FreqSeries {
        let t0 = self.t_s.first().copied().unwrap_or(0.0);
        FreqSeries { t_s: self.t_s.iter().map(|t| t - t0).collect(), f_hz: self.f_hz.clone() }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.t_s.len() != self.f_hz.len() {
            return Err("time/frequency length mismatch".into());
        }
        if self.t_s.len() < 2 {
            return Err("need at least two points".into());
        }
        if self.t_s.iter().chain(&self.f_hz).any(|v| !v.is_finite()) {
            return Err("non-finite values in the series".into());
        }
        if self.t_s.windows(2).any(|w| w[1] < w[0]) {
            return Err("time must be non-decreasing".into());
        }
        Ok(())
    }

    /// Linear interpolation; `None` outside the recorded range.
    pub fn freq_at(&self, t: f64) -> Option<f64> {
        let n = self.t_s.len();
        if n == 0 || !t.is_finite() || t < self.t_s[0] || t > self.t_s[n - 1] {
            return None;
        }
        let hi = self.t_s.partition_point(|&q| q <= t).clamp(1, n - 1);
        let lo = hi - 1;
        let dt = self.t_s[hi] - self.t_s[lo];
        if dt <= 0.0 {
            return Some(self.f_hz[lo]);
        }
        Some(self.f_hz[lo] + (t - self.t_s[lo]) / dt * (self.f_hz[hi] - self.f_hz[lo]))
    }
}

/// Cumulative thickness (nm) of the Z-match equation for loaded frequency `f`.
pub fn zmatch_thickness_nm(f: f64, fq: f64, rho: f64, z: f64) -> Option<f64> {
    if ![f, fq, rho, z].iter().all(|v| v.is_finite() && *v > 0.0) {
        return None;
    }
    let loading = (fq - f) / fq;
    if loading.abs() >= ZMATCH_LIMIT {
        return None;
    }
    let t = (RHO_Q * MU_Q).sqrt() / (2.0 * std::f64::consts::PI * z * f * rho) * (z * (std::f64::consts::PI * loading).tan()).atan() * 1e7;
    t.is_finite().then_some(t)
}

/// Linear Sauerbrey thickness change (nm) for a shift `df = f - f_ref` (Hz).
pub fn sauerbrey_thickness_nm(df: f64, fq: f64, rho: f64) -> Option<f64> {
    if !(fq > 0.0 && rho > 0.0 && df.is_finite()) {
        return None;
    }
    Some(-df * (RHO_Q * MU_Q).sqrt() / (2.0 * fq * fq * rho) * 1e7)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowResult {
    pub ta_s: f64,
    pub tb_s: f64,
    pub dt_s: f64,
    pub fa_hz: f64,
    pub fb_hz: f64,
    pub df_hz: f64,
    /// max |f_q - f| / f_q over the two cursors.
    pub loading: f64,
    pub thickness_nm: f64,
    pub rate_nm_s: Option<f64>,
    pub mass_g: f64,
    pub model: String,
    pub warning: Option<String>,
}

/// Thickness and rate between two cursors `ta`, `tb` of a frequency series.
pub fn analyze_window(series: &FreqSeries, p: &CrystalParams, ta: f64, tb: f64) -> Result<WindowResult, String> {
    series.validate()?;
    for (n, v) in [("f_unloaded", p.f_unloaded_hz), ("diameter", p.diameter_mm), ("density", p.rho_g_cm3), ("Z ratio", p.z_ratio)] {
        if !(v.is_finite() && v > 0.0) {
            return Err(format!("crystal/material parameter `{n}` must be positive"));
        }
    }
    if !(ta.is_finite() && tb.is_finite()) {
        return Err("cursor positions must be finite".into());
    }
    let (ta, tb) = if ta <= tb { (ta, tb) } else { (tb, ta) };
    // Cursors snap to the recorded range (like the reference tool); a window that lies
    // entirely outside the data is an error. The result reports the window actually used.
    let (first, last) = (series.t_s[0], series.t_s[series.t_s.len() - 1]);
    if tb < first || ta > last {
        return Err("the window lies entirely outside the recorded data".into());
    }
    let (ta, tb) = (ta.max(first), tb.min(last));
    let fa = series.freq_at(ta).ok_or("window start is outside the recorded data")?;
    let fb = series.freq_at(tb).ok_or("window end is outside the recorded data")?;
    if fa <= 0.0 || fb <= 0.0 {
        return Err("cursors require valid positive frequencies".into());
    }
    let fq = p.f_unloaded_hz;
    let loading = ((fq - fa) / fq).abs().max(((fq - fb) / fq).abs());
    if loading >= ZMATCH_LIMIT {
        return Err("outside supported loading range (|f_q - f| / f_q >= 40 %)".into());
    }
    let dt = tb - ta;
    let (thickness, model) = match p.model {
        Model::Sauerbrey => (sauerbrey_thickness_nm(fb - fa, fq, p.rho_g_cm3).ok_or("Sauerbrey calculation out of range")?, "Sauerbrey"),
        m => {
            let t = zmatch_thickness_nm(fb, fq, p.rho_g_cm3, p.z_ratio)
                .zip(zmatch_thickness_nm(fa, fq, p.rho_g_cm3, p.z_ratio))
                .map(|(b, a)| b - a)
                .ok_or("Z-match calculation out of range")?;
            (t, if m == Model::Auto { "Z-match (Auto)" } else { "Z-match" })
        }
    };
    let rate = (dt > 0.0).then_some(thickness / dt);
    let radius_cm = p.diameter_mm / 20.0;
    let mass = thickness * 1e-7 * p.rho_g_cm3 * std::f64::consts::PI * radius_cm * radius_cm;
    let warning = (p.model == Model::Sauerbrey && loading >= SAUERBREY_LIMIT)
        .then(|| "Linear approximation: loading is at least 2 %; consider Z-match".to_string());
    Ok(WindowResult {
        ta_s: ta,
        tb_s: tb,
        dt_s: dt,
        fa_hz: fa,
        fb_hz: fb,
        df_hz: fb - fa,
        loading,
        thickness_nm: thickness,
        rate_nm_s: rate,
        mass_g: mass,
        model: model.into(),
        warning,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IffResult {
    pub reference_nm_s: f64,
    pub rate_nm_s: f64,
    /// Ion-flux fraction in percent: `100 (1 - v_neutral / v_total)`.
    pub percent: f64,
    pub note: Option<String>,
}

/// IFF from the reference (ions + neutrals) and neutral-only deposition rates.
pub fn iff(reference_nm_s: f64, neutral_nm_s: f64) -> Result<IffResult, String> {
    if !(reference_nm_s.is_finite() && reference_nm_s > 0.0) || !neutral_nm_s.is_finite() {
        return Err("IFF needs a positive reference rate and a finite neutral rate".into());
    }
    let percent = (1.0 - neutral_nm_s / reference_nm_s) * 100.0;
    let note = (!(0.0..=100.0).contains(&percent)).then(|| "Outside 0-100 %: check reference and measurement conditions.".into());
    Ok(IffResult { reference_nm_s, rate_nm_s: neutral_nm_s, percent, note })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(df_per_s: f64) -> FreqSeries {
        let t: Vec<f64> = (0..=100).map(|i| i as f64).collect();
        FreqSeries { f_hz: t.iter().map(|ti| 6.0e6 + df_per_s * ti).collect(), t_s: t }
    }

    #[test]
    fn sauerbrey_matches_hand_calculation() {
        // 1 Hz down on a 6 MHz Ti-loaded crystal: d = 1*sqrt(2.648*2.947e11)/(2*36e12*4.506)*1e7 nm
        let t = sauerbrey_thickness_nm(-1.0, 6.0e6, 4.506).unwrap();
        let expect = (2.648_f64 * 2.947e11).sqrt() / (2.0 * 6.0e6_f64.powi(2) * 4.506) * 1e7;
        assert!((t - expect).abs() < 1e-9);
        assert!((t - 0.02723).abs() < 5e-5, "{t}");
    }

    #[test]
    fn zmatch_agrees_with_sauerbrey_for_small_loading() {
        let p = CrystalParams { model: Model::ZMatch, ..Default::default() };
        let s = ramp(-10.0); // 1 kHz total over 100 s (0.017 %)
        let z = analyze_window(&s, &p, 0.0, 100.0).unwrap();
        let sp = CrystalParams { model: Model::Sauerbrey, ..Default::default() };
        let l = analyze_window(&s, &sp, 0.0, 100.0).unwrap();
        assert!((z.thickness_nm / l.thickness_nm - 1.0).abs() < 1e-2);
        assert!(z.rate_nm_s.unwrap() > 0.0);
        assert!(l.warning.is_none());
    }

    #[test]
    fn heavy_loading_warns_and_limits() {
        let s = ramp(-2000.0); // 200 kHz => 3.3 %
        let sp = CrystalParams { model: Model::Sauerbrey, ..Default::default() };
        assert!(analyze_window(&s, &sp, 0.0, 100.0).unwrap().warning.is_some());
        let s = ramp(-30_000.0); // 3 MHz => 50 %
        assert!(analyze_window(&s, &CrystalParams::default(), 0.0, 100.0).is_err());
    }

    #[test]
    fn window_outside_data_is_rejected_but_overhang_is_clamped() {
        assert!(analyze_window(&ramp(-1.0), &CrystalParams::default(), -20.0, -10.0).is_err());
        assert!(analyze_window(&ramp(-1.0), &CrystalParams::default(), 200.0, 300.0).is_err());
        let w = analyze_window(&ramp(-1.0), &CrystalParams::default(), -5.0, 150.0).unwrap();
        assert_eq!((w.ta_s, w.tb_s), (0.0, 100.0));
        assert!(analyze_window(&ramp(-1.0), &CrystalParams::default(), f64::NAN, 10.0).is_err());
    }

    #[test]
    fn swapped_cursors_are_ordered() {
        let w = analyze_window(&ramp(-5.0), &CrystalParams::default(), 80.0, 20.0).unwrap();
        assert!(w.dt_s > 0.0 && w.ta_s < w.tb_s);
    }

    #[test]
    fn iff_formula() {
        let r = iff(10.0, 6.0).unwrap();
        assert!((r.percent - 40.0).abs() < 1e-12 && r.note.is_none());
        assert!(iff(10.0, 12.0).unwrap().note.is_some());
        assert!(iff(0.0, 1.0).is_err());
    }
}
