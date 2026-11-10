//! Self-contained HTML report (inline SVG plots, no external resources). Open it in a
//! browser and use "Print -> Save as PDF". All text coming from the project is escaped.

use crate::analysis::hipims;
use crate::project::Project;

pub fn esc(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            c => c.to_string(),
        })
        .collect()
}

fn nice(v: f64) -> String {
    if v == 0.0 {
        "0".into()
    } else if v.abs() >= 1e4 || v.abs() < 1e-2 {
        format!("{v:.3e}")
    } else {
        format!("{v:.4}")
    }
}

/// Minimal line/marker plot as inline SVG. `series`: (x, y, css class, markers).
pub fn svg_plot(series: &[(&[f64], &[f64], &str, bool)], title: &str, xl: &str, yl: &str) -> String {
    let (w, h, l, r, t, b) = (640.0, 300.0, 70.0, 12.0, 28.0, 44.0);
    let pts = || series.iter().flat_map(|(x, y, _, _)| x.iter().zip(y.iter())).filter(|(a, b)| a.is_finite() && b.is_finite());
    let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for (x, y) in pts() {
        x0 = x0.min(*x);
        x1 = x1.max(*x);
        y0 = y0.min(*y);
        y1 = y1.max(*y);
    }
    if x0 >= x1 || y0 >= y1 {
        return format!("<p class=\"note\">{}: no data</p>", esc(title));
    }
    let pad = (y1 - y0) * 0.05;
    let (y0, y1) = (y0 - pad, y1 + pad);
    let sx = |v: f64| l + (v - x0) / (x1 - x0) * (w - l - r);
    let sy = |v: f64| h - b - (v - y0) / (y1 - y0) * (h - t - b);
    let mut s = format!(
        "<svg viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"{}\"><text x=\"{l}\" y=\"18\" class=\"ttl\">{}</text>",
        esc(title),
        esc(title)
    );
    for k in 0..=4 {
        let fx = x0 + (x1 - x0) * k as f64 / 4.0;
        let fy = y0 + (y1 - y0) * k as f64 / 4.0;
        s.push_str(&format!("<line x1=\"{l}\" x2=\"{}\" y1=\"{:.1}\" y2=\"{:.1}\" class=\"grid\"/>", w - r, sy(fy), sy(fy)));
        s.push_str(&format!("<text x=\"{:.1}\" y=\"{:.1}\" class=\"tick\" text-anchor=\"end\">{}</text>", l - 4.0, sy(fy) + 4.0, nice(fy)));
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" class=\"tick\" text-anchor=\"middle\">{}</text>",
            sx(fx),
            h - b + 16.0,
            nice(fx)
        ));
    }
    for (x, y, class, markers) in series {
        let pts: Vec<(f64, f64)> =
            x.iter().zip(y.iter()).filter(|(a, b)| a.is_finite() && b.is_finite()).map(|(a, b)| (sx(*a), sy(*b))).collect();
        if *markers {
            for (px, py) in &pts {
                s.push_str(&format!("<circle cx=\"{px:.1}\" cy=\"{py:.1}\" r=\"3\" class=\"{class}\"/>"));
            }
        } else {
            let d: String =
                pts.iter().enumerate().map(|(i, (px, py))| format!("{}{px:.1},{py:.1}", if i == 0 { "M" } else { "L" })).collect();
            s.push_str(&format!("<path d=\"{d}\" class=\"{class}\" fill=\"none\"/>"));
        }
    }
    s.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{}\" class=\"lab\" text-anchor=\"middle\">{}</text><text transform=\"translate(14 {:.1}) rotate(-90)\" class=\"lab\" text-anchor=\"middle\">{}</text></svg>",
        (l + w - r) / 2.0,
        h - 6.0,
        esc(xl),
        (t + h - b) / 2.0,
        esc(yl)
    ));
    s
}

fn row(k: &str, v: impl std::fmt::Display) -> String {
    format!("<tr><th>{}</th><td>{}</td></tr>", esc(k), esc(&v.to_string()))
}

pub fn render(p: &Project) -> String {
    let mut o = String::new();
    o.push_str(&format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>QCM report - {}</title><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">{}</head><body>",
        esc(&p.name),
        STYLE
    ));
    o.push_str(&format!("<h1>QCM / HiPIMS report: {}</h1>", esc(&p.name)));
    o.push_str("<h2>Provenance</h2><table>");
    for (k, v) in [
        ("Sample ID", p.sample_id.clone()),
        ("Operator", p.operator.clone()),
        ("Created", p.created.clone()),
        ("Modified", p.modified.clone()),
        ("Software", format!("hipims-qcm {} (git {})", p.software_version, p.git_hash)),
        ("Notes", p.notes.clone()),
    ] {
        o.push_str(&row(k, v));
    }
    match &p.device {
        Some(d) => {
            o.push_str(&row(
                "Instrument",
                format!(
                    "{} {} serial {} fw {} options {}",
                    d.device,
                    d.devtype,
                    d.serial,
                    d.firmware,
                    d.options.join("/")
                ),
            ));
        }
        None => o.push_str(&row("Instrument", "not connected during this project")),
    }
    o.push_str("</table>");

    if let Some(s) = p.sweeps.last() {
        o.push_str(&format!("<h2>Resonance sweep ({})</h2>", esc(&s.acquired_at)));
        o.push_str(&svg_plot(&[(&s.freq_hz, &s.r, "s1", false)], "Amplitude R", "frequency (Hz)", "R (V)"));
        o.push_str(&svg_plot(&[(&s.freq_hz, &s.theta_deg, "s1", false)], "Phase (unwrapped)", "frequency (Hz)", "theta (deg)"));
        o.push_str("<table><tr><th>Fit</th><th>f0 (Hz)</th><th>BW (Hz)</th><th>Q</th><th>phase (deg)</th><th>slope</th></tr>");
        for e in [s.lorentz.as_ref(), s.circle.as_ref()].into_iter().flatten() {
            o.push_str(&format!(
                "<tr><td>{:?}</td><td>{:.3}</td><td>{:.4}</td><td>{:.0}</td><td>{:.2}</td><td>{:?}</td></tr>",
                e.method, e.f0_hz, e.bw_hz, e.q, e.phase_deg, e.polarity
            ));
        }
        o.push_str("</table>");
        for m in &s.fit_errors {
            o.push_str(&format!("<p class=\"warn\">{}</p>", esc(m)));
        }
    }
    if let Some(pl) = &p.pll {
        o.push_str("<h2>PLL</h2><table>");
        o.push_str(&row("State", format!("{:?}", pl.state)));
        o.push_str(&row("Frequency (Hz)", format!("{:.3}", pl.freq_hz)));
        o.push_str(&row("P / I / D", format!("{:.5} / {:.5} / {:.5}", pl.p, pl.i, pl.d)));
        o.push_str(&row(
            "Bandwidth (Hz)",
            format!("{:.1} ({})", pl.bandwidth_hz, if pl.advisor_from_device { "LabOne PID Advisor" } else { "local model" }),
        ));
        o.push_str("</table>");
    }
    if let Some(c) = p.qcal.last() {
        o.push_str(&format!("<h2>Q-control calibration ({})</h2><table>", esc(&c.acquired_at)));
        o.push_str(&row("Native Q (gain calibration)", format!("{:.0}", c.q_native)));
        o.push_str(&row("Gamma native (1/s)", format!("{:.3}", c.gamma_native)));
        o.push_str(&row("dGamma/dgain (1/s)", format!("{:.4} (R2 = {:.4})", c.slope, c.r2)));
        o.push_str(&row("Lasing threshold (gain)", c.gain_lasing.map_or("n/a".into(), |k| format!("{k:.3}"))));
        o.push_str("</table>");
        let (kp, g): (Vec<f64>, Vec<f64>) = c.points.iter().filter_map(|q| q.gamma.map(|g| (q.gain, g))).unzip();
        o.push_str(&svg_plot(&[(&kp, &g, "s1", true)], "Damping rate vs feedback gain", "gain", "Gamma (1/s)"));
    }
    if let (Some(hp), false) = (&p.hipims, p.steps.is_empty()) {
        o.push_str("<h2>HiPIMS gated profile</h2>");
        match hipims::profile(&p.steps, hp) {
            Ok(pr) => {
                let ov: Vec<f64> = pr.points.iter().map(|q| q.overlap_us).collect();
                let df: Vec<f64> = pr.points.iter().map(|q| q.delta_f_hz).collect();
                o.push_str(&svg_plot(
                    &[(&ov, &df, "s1", true)],
                    "Cumulative resonance shift",
                    "overlap with the pulse (us)",
                    "delta f (Hz)",
                ));
                o.push_str("<table><tr><th>step</th><th>overlap (us)</th><th>delta theta (deg)</th><th>delta f (Hz)</th><th>thickness (nm)</th></tr>");
                for q in &pr.points {
                    o.push_str(&format!(
                        "<tr><td>{}</td><td>{:.1}</td><td>{:.3}</td><td>{:.3}</td><td>{:.4}</td></tr>",
                        q.index, q.overlap_us, q.delta_theta_deg, q.delta_f_hz, q.thickness_nm
                    ));
                }
                o.push_str("</table>");
                for w in &pr.warnings {
                    o.push_str(&format!("<p class=\"warn\">{}</p>", esc(w)));
                }
            }
            Err(e) => o.push_str(&format!("<p class=\"warn\">{}</p>", esc(&e))),
        }
    }
    if !p.tracks.is_empty() {
        o.push_str(
            "<h2>Recorded tracks</h2><table><tr><th>file</th><th>started</th><th>samples</th><th>rate (Sa/s)</th><th>status</th></tr>",
        );
        for t in &p.tracks {
            o.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&t.file),
                esc(&t.started),
                t.samples,
                t.rate_hz,
                if t.complete { "complete" } else { "PARTIAL (recovered)" }
            ));
        }
        o.push_str("</table>");
    }
    if !p.analyses.is_empty() {
        o.push_str("<h2>Analyses (append-only revisions)</h2><table><tr><th>#</th><th>time</th><th>kind</th><th>result</th></tr>");
        for a in &p.analyses {
            o.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td><code>{}</code></td></tr>",
                a.id,
                esc(&a.created),
                esc(&a.kind),
                esc(&a.result.to_string())
            ));
        }
        o.push_str("</table>");
    }
    o.push_str("<h2>History</h2><table>");
    for h in &p.history {
        o.push_str(&format!("<tr><td>{}</td><td>{}</td><td>{}</td></tr>", esc(&h.t), esc(&h.kind), esc(&h.text)));
    }
    o.push_str("</table><h2>Configuration</h2><pre>");
    o.push_str(&esc(&p.config_toml));
    o.push_str("</pre></body></html>");
    o
}

const STYLE: &str = "<style>:root{--fg:#1d232a;--mut:#5b6672;--line:#cfd6dd;--a:#1f5fbf;--warn:#a05a00}body{font:13px/1.45 system-ui,sans-serif;color:var(--fg);max-width:900px;margin:24px auto;padding:0 16px}h1{font-size:20px}h2{font-size:15px;border-bottom:1px solid var(--line);padding-bottom:3px;margin-top:26px}table{border-collapse:collapse;margin:8px 0;font-variant-numeric:tabular-nums}th,td{border:1px solid var(--line);padding:3px 8px;text-align:left;vertical-align:top}th{background:#f3f5f7}svg{width:100%;max-width:640px;height:auto;display:block;margin:8px 0}.s1{stroke:var(--a);stroke-width:1.4;fill:var(--a)}.grid{stroke:var(--line)}.tick,.lab{fill:var(--mut);font-size:11px}.ttl{font-weight:600;font-size:13px;fill:var(--fg)}.warn{color:var(--warn)}.note{color:var(--mut)}pre{white-space:pre-wrap;font-size:11px;background:#f3f5f7;padding:8px}@media print{body{margin:0}}</style>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_text_is_escaped() {
        let mut p = Project::new(&std::env::temp_dir(), "rep", String::new()).unwrap();
        p.notes = "<script>alert(1)</script>".into();
        p.log("x", "<img src=x onerror=alert(1)>");
        let html = render(&p);
        assert!(!html.contains("<script>alert"));
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn plot_handles_degenerate_data() {
        assert!(svg_plot(&[(&[1.0], &[1.0], "s1", false)], "t", "x", "y").contains("no data"));
        let x = [0.0, 1.0, 2.0];
        let y = [1.0, 3.0, 2.0];
        assert!(svg_plot(&[(&x, &y, "s1", false)], "t", "x", "y").contains("<path"));
    }
}
