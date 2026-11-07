//! Numerical fitting: Levenberg-Marquardt, resonance fits (Lorentzian magnitude,
//! geometric circle + phase-angle fit) and exponential-decay fits for ring-down.
//!
//! All routines validate finiteness and sample counts and return `Err` instead of
//! panicking. Tested against synthetic data with known parameters.

use serde::{Deserialize, Serialize};

pub type FitResult<T> = Result<T, String>;

// ------------------------------------------------------------------ linear algebra

/// Solve `a x = b` by Gaussian elimination with partial pivoting. `None` if singular.
pub fn solve_linear(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    if a.len() != n || a.iter().any(|r| r.len() != n) {
        return None;
    }
    for col in 0..n {
        let mut piv = col;
        for r in col + 1..n {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        if a[piv][col].abs().partial_cmp(&1e-300) != Some(std::cmp::Ordering::Greater) {
            return None;
        }
        a.swap(col, piv);
        b.swap(col, piv);
        for r in col + 1..n {
            let f = a[r][col] / a[col][col];
            if f != 0.0 {
                let (top, bottom) = a.split_at_mut(r);
                for (dst, src) in bottom[0][col..].iter_mut().zip(&top[col][col..]) {
                    *dst -= f * src;
                }
                b[r] -= f * b[col];
            }
        }
    }
    let mut x = vec![0.0; n];
    for r in (0..n).rev() {
        let mut s = b[r];
        for c in r + 1..n {
            s -= a[r][c] * x[c];
        }
        x[r] = s / a[r][r];
    }
    if x.iter().all(|v| v.is_finite()) {
        Some(x)
    } else {
        None
    }
}

fn invert(a: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n = a.len();
    let mut cols = Vec::with_capacity(n);
    for j in 0..n {
        let mut e = vec![0.0; n];
        e[j] = 1.0;
        cols.push(solve_linear(a.to_vec(), e)?);
    }
    // cols[j][i] = inv[i][j]
    Some((0..n).map(|i| (0..n).map(|j| cols[j][i]).collect()).collect())
}

// ------------------------------------------------------------------ Levenberg-Marquardt

#[derive(Debug, Clone)]
pub struct LmOutcome {
    pub params: Vec<f64>,
    /// Sum of squared residuals.
    pub cost: f64,
    pub iterations: usize,
    /// Parameter standard errors (None if the normal matrix is singular).
    pub sigma: Option<Vec<f64>>,
}

/// Minimise `sum(residual(p)^2)` starting from `p0`. `scales` are typical magnitudes
/// of each parameter (used for the finite-difference step).
pub fn levenberg_marquardt(p0: &[f64], scales: &[f64], residual: &dyn Fn(&[f64]) -> Vec<f64>) -> FitResult<LmOutcome> {
    let n = p0.len();
    if n == 0 || scales.len() != n {
        return Err("LM: parameter/scale length mismatch".into());
    }
    let mut p = p0.to_vec();
    let mut r = residual(&p);
    if r.is_empty() || r.len() < n {
        return Err(format!("LM: {} residuals for {} parameters", r.len(), n));
    }
    if r.iter().any(|v| !v.is_finite()) {
        return Err("LM: non-finite residual at the starting point".into());
    }
    let mut cost: f64 = r.iter().map(|v| v * v).sum();
    let mut lambda = 1e-3;
    let mut iters = 0;
    let jac = |p: &[f64], r0: &[f64]| -> Vec<Vec<f64>> {
        // columns: derivative wrt parameter j
        (0..n)
            .map(|j| {
                let h = 1e-6 * p[j].abs().max(scales[j].abs()).max(1e-300);
                let mut pj = p.to_vec();
                pj[j] += h;
                let rj = residual(&pj);
                rj.iter().zip(r0).map(|(a, b)| (a - b) / h).collect()
            })
            .collect()
    };
    for it in 0..300 {
        iters = it + 1;
        let jc = jac(&p, &r);
        let mut jtj = vec![vec![0.0; n]; n];
        let mut jtr = vec![0.0; n];
        for a in 0..n {
            for b in a..n {
                let s: f64 = jc[a].iter().zip(&jc[b]).map(|(x, y)| x * y).sum();
                jtj[a][b] = s;
                jtj[b][a] = s;
            }
            jtr[a] = jc[a].iter().zip(&r).map(|(x, y)| x * y).sum();
        }
        let mut improved = false;
        for _ in 0..30 {
            let mut m = jtj.clone();
            for a in 0..n {
                m[a][a] += lambda * jtj[a][a].max(1e-30);
            }
            let rhs: Vec<f64> = jtr.iter().map(|v| -v).collect();
            let Some(delta) = solve_linear(m, rhs) else {
                lambda *= 10.0;
                continue;
            };
            let trial: Vec<f64> = p.iter().zip(&delta).map(|(a, d)| a + d).collect();
            let rt = residual(&trial);
            let ct: f64 = rt.iter().map(|v| v * v).sum();
            if ct.is_finite() && ct < cost {
                let rel = (cost - ct) / cost.max(1e-300);
                p = trial;
                r = rt;
                cost = ct;
                lambda = (lambda / 3.0).max(1e-12);
                improved = true;
                if rel < 1e-12 {
                    let jc = jac(&p, &r);
                    return Ok(finish(p, cost, iters, &jc, r.len()));
                }
                break;
            }
            lambda *= 4.0;
        }
        if !improved {
            break;
        }
    }
    Ok(finish(p.clone(), cost, iters, &jac(&p, &r), r.len()))
}

fn finish(params: Vec<f64>, cost: f64, iterations: usize, jc: &[Vec<f64>], m: usize) -> LmOutcome {
    let n = params.len();
    let mut jtj = vec![vec![0.0; n]; n];
    for a in 0..n {
        for b in 0..n {
            jtj[a][b] = jc[a].iter().zip(&jc[b]).map(|(x, y)| x * y).sum();
        }
    }
    let dof = m.saturating_sub(n).max(1) as f64;
    let s2 = cost / dof;
    let sigma = invert(&jtj).map(|inv| (0..n).map(|i| (inv[i][i] * s2).max(0.0).sqrt()).collect());
    LmOutcome { params, cost, iterations, sigma }
}

// ------------------------------------------------------------------ helpers

pub fn median(v: &[f64]) -> f64 {
    let mut s: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if s.is_empty() {
        return f64::NAN;
    }
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        0.5 * (s[n / 2 - 1] + s[n / 2])
    }
}

/// Unwrap a phase sequence (radians) by removing 2*pi jumps.
pub fn unwrap_phase(p: &[f64]) -> Vec<f64> {
    let mut out = Vec::with_capacity(p.len());
    let mut offset = 0.0;
    for (i, &v) in p.iter().enumerate() {
        if i > 0 {
            let d = v - p[i - 1];
            if d > std::f64::consts::PI {
                offset -= 2.0 * std::f64::consts::PI * ((d / (2.0 * std::f64::consts::PI)).round().max(1.0));
            } else if d < -std::f64::consts::PI {
                offset += 2.0 * std::f64::consts::PI * ((-d / (2.0 * std::f64::consts::PI)).round().max(1.0));
            }
        }
        out.push(v + offset);
    }
    out
}

/// Ordinary least squares `y = slope x + intercept`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LineFit {
    pub slope: f64,
    pub intercept: f64,
    pub r2: f64,
    pub slope_sigma: f64,
}

pub fn linear_fit(x: &[f64], y: &[f64]) -> FitResult<LineFit> {
    let n = x.len();
    if n != y.len() || n < 2 {
        return Err("linear fit needs >= 2 matching points".into());
    }
    if x.iter().chain(y).any(|v| !v.is_finite()) {
        return Err("linear fit: non-finite input".into());
    }
    let nf = n as f64;
    let mx = x.iter().sum::<f64>() / nf;
    let my = y.iter().sum::<f64>() / nf;
    let sxx: f64 = x.iter().map(|v| (v - mx).powi(2)).sum();
    let sxy: f64 = x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)).sum();
    if sxx <= 0.0 {
        return Err("linear fit: x values are all identical".into());
    }
    let slope = sxy / sxx;
    let intercept = my - slope * mx;
    let ss_res: f64 = x.iter().zip(y).map(|(a, b)| (b - (slope * a + intercept)).powi(2)).sum();
    let ss_tot: f64 = y.iter().map(|b| (b - my).powi(2)).sum();
    let r2 = if ss_tot > 0.0 { 1.0 - ss_res / ss_tot } else { 1.0 };
    let slope_sigma = if n > 2 { (ss_res / (nf - 2.0) / sxx).sqrt() } else { f64::NAN };
    Ok(LineFit { slope, intercept, r2, slope_sigma })
}

// ------------------------------------------------------------------ resonance estimates

/// Sign of the phase slope d(theta)/df through resonance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Polarity {
    /// Phase decreases through resonance (no parasitic capacitance problem).
    Normal,
    /// Phase increases through resonance: PLL gains must be inverted.
    Inverted,
}

impl Polarity {
    /// Sign to apply to the (positive) advisor gains: Normal => negative P and I.
    pub fn gain_sign(self) -> f64 {
        match self {
            Polarity::Normal => -1.0,
            Polarity::Inverted => 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FitMethod {
    Lorentz,
    Circle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResonanceEstimate {
    pub method: FitMethod,
    pub f0_hz: f64,
    /// Full width at half power (R = Rpeak / sqrt(2)): 2 gamma.
    pub bw_hz: f64,
    pub q: f64,
    /// Peak response above the off-resonance baseline (V).
    pub amplitude_v: f64,
    pub offset_v: f64,
    /// Measured demod phase at f0 (degrees): the PLL setpoint.
    pub phase_deg: f64,
    pub polarity: Polarity,
    /// |d theta / df| at f0 in deg/Hz.
    pub slope_deg_per_hz: f64,
    pub rms_residual: f64,
    pub f0_sigma_hz: Option<f64>,
    pub bw_sigma_hz: Option<f64>,
    /// Circle fit only: centre and radius in the (x, y) plane.
    pub circle: Option<[f64; 3]>,
}

fn check_spectrum(freq: &[f64], x: &[f64], y: &[f64]) -> FitResult<()> {
    let n = freq.len();
    if n < 8 {
        return Err(format!("need at least 8 sweep points, got {n}"));
    }
    if x.len() != n || y.len() != n {
        return Err("frequency/x/y length mismatch".into());
    }
    if freq.iter().chain(x).chain(y).any(|v| !v.is_finite()) {
        return Err("non-finite values in sweep data".into());
    }
    if freq.windows(2).any(|w| w[1] <= w[0]) {
        return Err("frequencies must be strictly increasing".into());
    }
    Ok(())
}

/// Phase (rad, unwrapped) of the raw demod signal.
pub fn raw_phase(x: &[f64], y: &[f64]) -> Vec<f64> {
    let wrapped: Vec<f64> = x.iter().zip(y).map(|(a, b)| b.atan2(*a)).collect();
    unwrap_phase(&wrapped)
}

fn interp(freq: &[f64], v: &[f64], f: f64) -> f64 {
    if f <= freq[0] {
        return v[0];
    }
    let n = freq.len();
    if f >= freq[n - 1] {
        return v[n - 1];
    }
    let hi = freq.partition_point(|&q| q <= f).clamp(1, n - 1);
    let lo = hi - 1;
    let t = (f - freq[lo]) / (freq[hi] - freq[lo]);
    v[lo] + t * (v[hi] - v[lo])
}

/// Slope of the unwrapped raw phase around `f0` within +-`halfwidth` (>= 3 points).
fn local_phase_slope(freq: &[f64], theta: &[f64], f0: f64, halfwidth: f64) -> (f64, f64) {
    let idx: Vec<usize> = (0..freq.len()).filter(|&i| (freq[i] - f0).abs() <= halfwidth).collect();
    let (xs, ys): (Vec<f64>, Vec<f64>) = if idx.len() >= 3 {
        (idx.iter().map(|&i| freq[i] - f0).collect(), idx.iter().map(|&i| theta[i]).collect())
    } else {
        // Fall back to the three nearest points.
        let mut order: Vec<usize> = (0..freq.len()).collect();
        order.sort_by(|&a, &b| (freq[a] - f0).abs().partial_cmp(&(freq[b] - f0).abs()).unwrap_or(std::cmp::Ordering::Equal));
        let sel = &order[..3.min(order.len())];
        (sel.iter().map(|&i| freq[i] - f0).collect(), sel.iter().map(|&i| theta[i]).collect())
    };
    match linear_fit(&xs, &ys) {
        Ok(l) => (l.slope, interp(freq, theta, f0)),
        Err(_) => (0.0, interp(freq, theta, f0)),
    }
}

/// Fit `R(f) = c + A / sqrt(1 + ((f - f0)/gamma)^2)` to the magnitude (dip: A < 0).
pub fn fit_lorentzian(freq: &[f64], x: &[f64], y: &[f64]) -> FitResult<ResonanceEstimate> {
    check_spectrum(freq, x, y)?;
    let n = freq.len();
    let r: Vec<f64> = x.iter().zip(y).map(|(a, b)| a.hypot(*b)).collect();
    let base = median(&r);
    // Peak or dip: the extremum furthest from the median.
    let (imax, _) = r.iter().enumerate().fold((0, f64::MIN), |acc, (i, v)| if *v > acc.1 { (i, *v) } else { acc });
    let (imin, _) = r.iter().enumerate().fold((0, f64::MAX), |acc, (i, v)| if *v < acc.1 { (i, *v) } else { acc });
    let peak = (r[imax] - base).abs() >= (r[imin] - base).abs();
    let ipk = if peak { imax } else { imin };
    let c0 = if peak { r[..n / 10 + 1].iter().chain(&r[n - n / 10 - 1..]).sum::<f64>() / (2 * (n / 10 + 1)) as f64 } else { base };
    let a0 = r[ipk] - c0;
    // Half-power width estimate.
    let target = c0 + a0 / std::f64::consts::SQRT_2;
    let above = |v: f64| if a0 > 0.0 { v >= target } else { v <= target };
    let (mut lo, mut hi) = (ipk, ipk);
    while lo > 0 && above(r[lo - 1]) {
        lo -= 1;
    }
    while hi + 1 < n && above(r[hi + 1]) {
        hi += 1;
    }
    let span = (freq[hi] - freq[lo]).max((freq[n - 1] - freq[0]) / n as f64);
    let gamma0 = (span / 2.0).max(1e-9);
    let fmid = freq[ipk];
    let scales = [a0.abs().max(1e-12), gamma0, gamma0, c0.abs().max(a0.abs() * 1e-3).max(1e-12)];
    let model = |p: &[f64], f: f64| p[3] + p[0] / (1.0 + ((f - fmid - p[1]) / p[2]).powi(2)).sqrt();
    let res = |p: &[f64]| -> Vec<f64> {
        if p[2].abs() < 1e-12 {
            return vec![1e30; n];
        }
        freq.iter().zip(&r).map(|(f, v)| model(p, *f) - v).collect()
    };
    let out = levenberg_marquardt(&[a0, 0.0, gamma0, c0], &scales, &res)?;
    let p = &out.params;
    let gamma = p[2].abs();
    let f0 = fmid + p[1];
    if !f0.is_finite() || gamma <= 0.0 || f0 < freq[0] || f0 > freq[n - 1] {
        return Err(format!("Lorentzian fit left the sweep range (f0 = {f0:.1} Hz)"));
    }
    let bw = 2.0 * gamma;
    let theta = raw_phase(x, y);
    let (slope, th0) = local_phase_slope(freq, &theta, f0, (0.5 * gamma).max(span * 0.005));
    let rms = (out.cost / n as f64).sqrt();
    Ok(ResonanceEstimate {
        method: FitMethod::Lorentz,
        f0_hz: f0,
        bw_hz: bw,
        q: f0 / bw,
        amplitude_v: p[0],
        offset_v: p[3],
        phase_deg: wrap_deg(th0.to_degrees()),
        polarity: if slope <= 0.0 { Polarity::Normal } else { Polarity::Inverted },
        slope_deg_per_hz: slope.to_degrees().abs(),
        rms_residual: rms,
        f0_sigma_hz: out.sigma.as_ref().map(|s| s[1]),
        bw_sigma_hz: out.sigma.as_ref().map(|s| 2.0 * s[2]),
        circle: None,
    })
}

fn wrap_deg(d: f64) -> f64 {
    let mut v = d % 360.0;
    if v > 180.0 {
        v -= 360.0
    }
    if v <= -180.0 {
        v += 360.0
    }
    v
}

/// Geometric circle fit (Kasa initialisation, LM on orthogonal distances).
pub fn fit_circle(x: &[f64], y: &[f64]) -> FitResult<[f64; 3]> {
    let n = x.len();
    if n < 5 || y.len() != n {
        return Err("circle fit needs >= 5 points".into());
    }
    let nf = n as f64;
    let (mx, my) = (x.iter().sum::<f64>() / nf, y.iter().sum::<f64>() / nf);
    // Kasa: solve  [u v 1] [A B C]^T = u^2+v^2  on centred data.
    let mut m = vec![vec![0.0; 3]; 3];
    let mut b = vec![0.0; 3];
    for (xi, yi) in x.iter().zip(y) {
        let (u, v) = (xi - mx, yi - my);
        let row = [u, v, 1.0];
        let z = u * u + v * v;
        for a in 0..3 {
            for c in 0..3 {
                m[a][c] += row[a] * row[c];
            }
            b[a] += row[a] * z;
        }
    }
    let sol = solve_linear(m, b).ok_or("circle fit: degenerate (collinear) points")?;
    let (cu, cv) = (sol[0] / 2.0, sol[1] / 2.0);
    let r2 = sol[2] + cu * cu + cv * cv;
    if !(r2 > 0.0) {
        return Err("circle fit: no real circle".into());
    }
    let p0 = [cu + mx, cv + my, r2.sqrt()];
    let scale = p0[2].max(1e-12);
    let res = |p: &[f64]| -> Vec<f64> { x.iter().zip(y).map(|(a, b)| (a - p[0]).hypot(b - p[1]) - p[2]).collect() };
    let out = levenberg_marquardt(&p0, &[scale, scale, scale], &res)?;
    Ok([out.params[0], out.params[1], out.params[2].abs()])
}

/// Circle fit of the Nyquist plot, then phase-angle fit `phi(f) = phi0 + 2 s atan((f-f0)/gamma)`.
pub fn fit_circle_resonance(freq: &[f64], x: &[f64], y: &[f64]) -> FitResult<ResonanceEstimate> {
    check_spectrum(freq, x, y)?;
    let n = freq.len();
    let [xc, yc, rad] = fit_circle(x, y)?;
    let wrapped: Vec<f64> = x.iter().zip(y).map(|(a, b)| (b - yc).atan2(a - xc)).collect();
    let phi = unwrap_phase(&wrapped);
    let span = freq[n - 1] - freq[0];
    // Steepest point of the smoothed angle(f): initial f0 and gamma.
    let k = (n / 25).max(2);
    let (mut best_i, mut best_s) = (n / 2, 0.0_f64);
    for i in k..n - k {
        let s = (phi[i + k] - phi[i - k]) / (freq[i + k] - freq[i - k]);
        if s.abs() > best_s.abs() {
            best_s = s;
            best_i = i;
        }
    }
    if best_s == 0.0 {
        return Err("circle fit: phase angle does not change with frequency".into());
    }
    let s = best_s.signum();
    let gamma0 = (2.0 / best_s.abs()).clamp(span / 1e4, span);
    let (f_ref, phi0) = (freq[best_i], phi[best_i]);
    let res = |p: &[f64]| -> Vec<f64> {
        if p[2].abs() < 1e-12 {
            return vec![1e30; n];
        }
        freq.iter().zip(&phi).map(|(f, v)| p[0] + 2.0 * s * ((f - f_ref - p[1]) / p[2]).atan() - v).collect()
    };
    let out = levenberg_marquardt(&[phi0, 0.0, gamma0], &[1.0, gamma0, gamma0], &res)?;
    let gamma = out.params[2].abs();
    let f0 = f_ref + out.params[1];
    if !f0.is_finite() || f0 < freq[0] || f0 > freq[n - 1] {
        return Err(format!("circle phase fit left the sweep range (f0 = {f0:.1} Hz)"));
    }
    let r: Vec<f64> = x.iter().zip(y).map(|(a, b)| a.hypot(*b)).collect();
    let theta = raw_phase(x, y);
    let (slope, th0) = local_phase_slope(freq, &theta, f0, (0.5 * gamma).max(span * 0.005));
    let r0 = interp(freq, &r, f0);
    Ok(ResonanceEstimate {
        method: FitMethod::Circle,
        f0_hz: f0,
        bw_hz: 2.0 * gamma,
        q: f0 / (2.0 * gamma),
        amplitude_v: r0,
        offset_v: 0.0,
        phase_deg: wrap_deg(th0.to_degrees()),
        polarity: if slope <= 0.0 { Polarity::Normal } else { Polarity::Inverted },
        slope_deg_per_hz: slope.to_degrees().abs(),
        rms_residual: (out.cost / n as f64).sqrt() * rad,
        f0_sigma_hz: out.sigma.as_ref().map(|s| s[1]),
        bw_sigma_hz: out.sigma.as_ref().map(|s| 2.0 * s[2]),
        circle: Some([xc, yc, rad]),
    })
}

// ------------------------------------------------------------------ exponential decays
// (not used by the blog-style Q-control; kept for optional ring-down analysis)

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DecayFit {
    pub amplitude: f64,
    /// Time constant of the (slow) resonator decay, seconds.
    pub tau_s: f64,
    pub offset: f64,
    pub rms: f64,
    /// Fast parasitic time constant if a two-exponential model was fitted.
    pub tau_fast_s: Option<f64>,
}

/// How the pure decay is isolated before fitting (see Q-control notes, part 3A).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SliceMethod {
    None,
    /// Index of the largest slope change between adjacent windows of `window` points.
    Knee {
        window: usize,
    },
    /// Start at the maximum of a `window`-point moving average.
    Max {
        window: usize,
    },
}

/// Index where the decay starts, according to `method`.
pub fn decay_start(a: &[f64], method: SliceMethod) -> usize {
    match method {
        SliceMethod::None => 0,
        SliceMethod::Max { window } => {
            let w = window.clamp(1, a.len().max(1));
            if a.len() < w {
                return 0;
            }
            let (mut best, mut bi) = (f64::MIN, 0);
            for i in 0..=a.len() - w {
                let m = a[i..i + w].iter().sum::<f64>() / w as f64;
                if m > best {
                    best = m;
                    bi = i;
                }
            }
            bi
        }
        SliceMethod::Knee { window } => {
            let w = window.max(3);
            if a.len() < 2 * w + 1 {
                return 0;
            }
            let t: Vec<f64> = (0..w).map(|i| i as f64).collect();
            let slope = |s: &[f64]| linear_fit(&t, s).map(|l| l.slope).unwrap_or(0.0);
            let (mut best, mut bi) = (0.0_f64, 0);
            for i in w..=a.len() - w {
                let d = slope(&a[i..i + w]) - slope(&a[i - w..i]);
                if d < best {
                    best = d;
                    bi = i;
                }
            }
            bi
        }
    }
}

fn tau_guess(t: &[f64], a: &[f64], c: f64) -> f64 {
    let (mut xs, mut ys) = (Vec::new(), Vec::new());
    for (ti, ai) in t.iter().zip(a) {
        if ai - c > 0.0 {
            xs.push(*ti);
            ys.push((ai - c).ln());
        }
    }
    linear_fit(&xs, &ys).ok().filter(|l| l.slope < 0.0).map(|l| -1.0 / l.slope).unwrap_or((t[t.len() - 1] - t[0]) / 3.0)
}

/// Single exponential `A exp(-t/tau) + C` (t measured from the start of the slice).
pub fn fit_decay_single(t: &[f64], a: &[f64]) -> FitResult<DecayFit> {
    if t.len() != a.len() || t.len() < 8 {
        return Err(format!("decay fit needs >= 8 points, got {}", t.len()));
    }
    if t.iter().chain(a).any(|v| !v.is_finite()) {
        return Err("decay fit: non-finite data".into());
    }
    let t0 = t[0];
    let tt: Vec<f64> = t.iter().map(|v| v - t0).collect();
    let tail = (a.len() / 10).max(1);
    let c0 = a[a.len() - tail..].iter().sum::<f64>() / tail as f64;
    let a0 = a[0] - c0;
    let tau0 = tau_guess(&tt, a, c0).max(1e-9);
    let res = |p: &[f64]| -> Vec<f64> { tt.iter().zip(a).map(|(ti, ai)| p[0] * (-ti / p[1].exp()).exp() + p[2] - ai).collect() };
    let out = levenberg_marquardt(&[a0, tau0.ln(), c0], &[a0.abs().max(1e-12), 1.0, c0.abs().max(1e-12)], &res)?;
    Ok(DecayFit {
        amplitude: out.params[0],
        tau_s: out.params[1].exp(),
        offset: out.params[2],
        rms: (out.cost / a.len() as f64).sqrt(),
        tau_fast_s: None,
    })
}

/// Two exponentials with the ordering constraint tau_slow > tau_fast (parameterised
/// as `tau_slow = tau_fast + exp(d)`). The slow component is the resonator.
pub fn fit_decay_double(t: &[f64], a: &[f64]) -> FitResult<DecayFit> {
    let single = fit_decay_single(t, a)?;
    let t0 = t[0];
    let tt: Vec<f64> = t.iter().map(|v| v - t0).collect();
    let tf0 = (single.tau_s / 20.0).max(1e-9);
    let d0 = (single.tau_s - tf0).max(1e-9).ln();
    let a_s = single.amplitude;
    let res = |p: &[f64]| -> Vec<f64> {
        let tf = p[1].exp();
        let ts = tf + p[3].exp();
        tt.iter().zip(a).map(|(ti, ai)| p[0] * (-ti / tf).exp() + p[2] * (-ti / ts).exp() + p[4] - ai).collect()
    };
    let sc = [a_s.abs().max(1e-12) * 0.1, 1.0, a_s.abs().max(1e-12), 1.0, single.offset.abs().max(1e-12)];
    let out = levenberg_marquardt(&[a_s * 0.1, tf0.ln(), a_s * 0.9, d0, single.offset], &sc, &res)?;
    let tf = out.params[1].exp();
    let ts = tf + out.params[3].exp();
    Ok(DecayFit {
        amplitude: out.params[2],
        tau_s: ts,
        offset: out.params[4],
        rms: (out.cost / a.len() as f64).sqrt(),
        tau_fast_s: Some(tf),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 11) as f64) / ((1u64 << 53) as f64) - 0.5
        }
    }

    fn synth(f0: f64, q: f64, noise: f64, inverted: bool, n: usize, span: f64) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
        let gamma = f0 / (2.0 * q);
        let mut rng = Lcg(42);
        let freq: Vec<f64> = (0..n).map(|i| f0 - span / 2.0 + span * i as f64 / (n - 1) as f64).collect();
        let (mut xs, mut ys) = (Vec::new(), Vec::new());
        let rot = 0.7_f64; // arbitrary electrical phase
        for f in &freq {
            let xx = (f - f0) / gamma;
            // H = 1/(1 + i x); inverted => conjugate (phase increases through resonance)
            let (re, im) = (1.0 / (1.0 + xx * xx), -xx / (1.0 + xx * xx));
            let im = if inverted { -im } else { im };
            let a = 2e-3;
            let (c, s) = (rot.cos(), rot.sin());
            xs.push(a * (re * c - im * s) + noise * rng.next());
            ys.push(a * (re * s + im * c) + noise * rng.next());
        }
        (freq, xs, ys)
    }

    #[test]
    fn lorentzian_recovers_f0_and_q() {
        let (f, x, y) = synth(5.9985e6, 50_000.0, 2e-6, false, 400, 1500.0);
        let e = fit_lorentzian(&f, &x, &y).unwrap();
        assert!((e.f0_hz - 5.9985e6).abs() < 1.0, "f0 {}", e.f0_hz);
        assert!((e.q / 50_000.0 - 1.0).abs() < 0.03, "Q {}", e.q);
        assert_eq!(e.polarity, Polarity::Normal);
        assert!(e.amplitude_v > 0.0);
    }

    #[test]
    fn circle_recovers_f0_and_q_and_inverted_polarity() {
        let (f, x, y) = synth(5.9985e6, 50_000.0, 2e-6, true, 400, 1500.0);
        let e = fit_circle_resonance(&f, &x, &y).unwrap();
        assert!((e.f0_hz - 5.9985e6).abs() < 1.0, "f0 {}", e.f0_hz);
        assert!((e.q / 50_000.0 - 1.0).abs() < 0.03, "Q {}", e.q);
        assert_eq!(e.polarity, Polarity::Inverted);
        assert_eq!(e.polarity.gain_sign(), 1.0);
        assert!(e.circle.is_some());
    }

    #[test]
    fn lorentzian_handles_dip() {
        let f0 = 6.0e6;
        let gamma = f0 / (2.0 * 20_000.0);
        let freq: Vec<f64> = (0..300).map(|i| f0 - 1500.0 + 3000.0 * i as f64 / 299.0).collect();
        let x: Vec<f64> = freq.iter().map(|f| 3e-3 - 2e-3 / (1.0 + ((f - f0) / gamma).powi(2)).sqrt()).collect();
        let y = [0.0; 300];
        let e = fit_lorentzian(&freq, &x, &y).unwrap();
        assert!(e.amplitude_v < 0.0);
        assert!((e.f0_hz - f0).abs() < 2.0, "f0 {}", e.f0_hz);
        assert!((e.q / 20_000.0 - 1.0).abs() < 0.03, "Q {}", e.q);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(fit_lorentzian(&[1.0, 2.0], &[1.0, 2.0], &[0.0, 0.0]).is_err());
        let f: Vec<f64> = (0..20).map(|i| i as f64).collect();
        let mut x = vec![1.0; 20];
        x[3] = f64::NAN;
        assert!(fit_lorentzian(&f, &x, &[0.0; 20]).is_err());
    }

    #[test]
    fn single_decay_recovers_tau() {
        let t: Vec<f64> = (0..400).map(|i| i as f64 * 1e-4).collect();
        let a: Vec<f64> = t.iter().map(|ti| 0.5 * (-ti / 2.6e-3).exp() + 0.25).collect();
        let f = fit_decay_single(&t, &a).unwrap();
        assert!((f.tau_s / 2.6e-3 - 1.0).abs() < 1e-3, "tau {}", f.tau_s);
        assert!((f.offset - 0.25).abs() < 1e-3);
    }

    #[test]
    fn double_decay_isolates_slow_component() {
        let t: Vec<f64> = (0..800).map(|i| i as f64 * 5e-5).collect();
        let a: Vec<f64> = t.iter().map(|ti| 0.15 * (-ti / 1e-4).exp() + 0.5 * (-ti / 2.6e-3).exp() + 0.25).collect();
        let f = fit_decay_double(&t, &a).unwrap();
        assert!((f.tau_s / 2.6e-3 - 1.0).abs() < 0.02, "tau {}", f.tau_s);
        assert!(f.tau_fast_s.unwrap() < f.tau_s);
    }

    #[test]
    fn knee_finds_decay_start() {
        let mut a = vec![1.0; 60];
        a.extend((0..200).map(|i| 0.5 + 0.5 * (-(i as f64) / 40.0).exp()));
        let k = decay_start(&a, SliceMethod::Knee { window: 20 });
        assert!((50..=70).contains(&k), "knee {k}");
        let m = decay_start(&a, SliceMethod::Max { window: 3 });
        assert!(m <= 60);
    }

    #[test]
    fn linear_fit_exact() {
        let x = [0.0, 1.0, 2.0, 3.0];
        let y = [1.0, 3.0, 5.0, 7.0];
        let l = linear_fit(&x, &y).unwrap();
        assert!((l.slope - 2.0).abs() < 1e-12 && (l.intercept - 1.0).abs() < 1e-12);
    }

    #[test]
    fn unwrap_removes_jumps() {
        let w = [3.0, -3.0, -2.5, 3.1];
        let u = unwrap_phase(&w);
        assert!(u.windows(2).all(|p| (p[1] - p[0]).abs() < std::f64::consts::PI));
    }
}
