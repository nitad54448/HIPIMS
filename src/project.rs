//! Projects: save/load, incremental track recording, provenance.
//!
//! Layout: `<data_dir>/<project>/project.json` (atomic write: tmp + rename) and
//! `<data_dir>/<project>/tracks/track_<timestamp>.csv` (open format, `# key: value`
//! metadata header, flushed after every batch so a crash loses at most one batch).
//!
//! Raw measurements are immutable once stored; analyses are appended as revisions
//! (undo = append a new revision), and the history is append-only.

use crate::analysis::fit::ResonanceEstimate;
use crate::analysis::hipims::{HipimsParams, StepRecord};
use crate::analysis::sauerbrey::FreqSeries;
use crate::error::ProjectError;
use crate::instrument::{DeviceInfo, Sample};
use crate::measure::pll::PllStatus;
use crate::measure::qcontrol::QCalibration;
use crate::measure::sweep::SweepResult;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub t: String,
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackMeta {
    pub file: String,
    pub started: String,
    pub samples: u64,
    pub rate_hz: f64,
    /// `false` for a track that was never closed properly (crash / recovery).
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisRevision {
    pub id: usize,
    pub created: String,
    pub kind: String,
    pub params: serde_json::Value,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub format_version: u32,
    pub name: String,
    pub sample_id: String,
    pub operator: String,
    pub notes: String,
    pub created: String,
    pub modified: String,
    pub software_version: String,
    pub git_hash: String,
    pub config_toml: String,
    pub device: Option<DeviceInfo>,
    pub sweeps: Vec<SweepResult>,
    pub estimate: Option<ResonanceEstimate>,
    pub pll: Option<PllStatus>,
    pub qcal: Vec<QCalibration>,
    pub steps: Vec<StepRecord>,
    /// Index in `steps` where the current HiPIMS series starts (older steps stay stored).
    #[serde(default)]
    pub series_start: usize,
    pub hipims: Option<HipimsParams>,
    pub tracks: Vec<TrackMeta>,
    pub analyses: Vec<AnalysisRevision>,
    pub history: Vec<HistoryEntry>,
    #[serde(skip)]
    pub dir: PathBuf,
    #[serde(skip)]
    pub dirty: bool,
}

/// Names become directory names: letters, digits, `-`, `_`, `.`, space; no traversal.
pub fn sanitize_name(name: &str) -> Result<String, ProjectError> {
    let n = name.trim();
    let ok = !n.is_empty()
        && n.len() <= 80
        && !n.starts_with('.')
        && n.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '));
    if ok {
        Ok(n.to_string())
    } else {
        Err(ProjectError::BadName(name.to_string()))
    }
}

fn io_err(op: &'static str, path: &Path, e: impl std::fmt::Display) -> ProjectError {
    ProjectError::Io { op, path: path.to_path_buf(), message: e.to_string() }
}

impl Project {
    pub fn new(data_dir: &Path, name: &str, config_toml: String) -> Result<Self, ProjectError> {
        let name = sanitize_name(name)?;
        let now = crate::measure::now_iso();
        Ok(Self {
            format_version: FORMAT_VERSION,
            dir: data_dir.join(&name),
            name,
            sample_id: String::new(),
            operator: String::new(),
            notes: String::new(),
            created: now.clone(),
            modified: now,
            software_version: env!("CARGO_PKG_VERSION").to_string(),
            git_hash: option_env!("HQCM_GIT_HASH").unwrap_or("unknown").to_string(),
            config_toml,
            device: None,
            sweeps: vec![],
            estimate: None,
            pll: None,
            qcal: vec![],
            steps: vec![],
            series_start: 0,
            hipims: None,
            tracks: vec![],
            analyses: vec![],
            history: vec![],
            dirty: true,
        })
    }

    /// Default name of an unnamed session.
    pub fn default_name() -> String {
        format!("session-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"))
    }

    pub fn log(&mut self, kind: &str, text: impl Into<String>) {
        self.history.push(HistoryEntry { t: crate::measure::now_iso(), kind: kind.into(), text: text.into() });
        self.dirty = true;
    }

    /// Steps of the series being acquired now.
    pub fn current_series(&self) -> &[StepRecord] {
        &self.steps[self.series_start.min(self.steps.len())..]
    }

    /// Start a new series; the raw steps of the previous one remain in the project.
    pub fn steps_series_start(&mut self) {
        self.series_start = self.steps.len();
        self.dirty = true;
    }

    pub fn touch(&mut self) {
        self.modified = crate::measure::now_iso();
        self.dirty = true;
    }

    pub fn file(&self) -> PathBuf {
        self.dir.join("project.json")
    }
    pub fn tracks_dir(&self) -> PathBuf {
        self.dir.join("tracks")
    }

    /// Atomic save: a failed write leaves the previous file intact and keeps `dirty` set.
    pub fn save(&mut self) -> Result<(), ProjectError> {
        self.modified = crate::measure::now_iso();
        fs::create_dir_all(&self.dir).map_err(|e| io_err("create", &self.dir, e))?;
        let json = serde_json::to_vec_pretty(self).map_err(|e| ProjectError::Format { path: self.file(), message: e.to_string() })?;
        let tmp = self.dir.join("project.json.tmp");
        {
            let mut f = File::create(&tmp).map_err(|e| io_err("create", &tmp, e))?;
            f.write_all(&json).map_err(|e| io_err("write", &tmp, e))?;
            f.sync_all().map_err(|e| io_err("sync", &tmp, e))?;
        }
        fs::rename(&tmp, self.file()).map_err(|e| io_err("rename", &tmp, e))?;
        self.dirty = false;
        Ok(())
    }

    pub fn load(data_dir: &Path, name: &str) -> Result<Self, ProjectError> {
        let name = sanitize_name(name)?;
        let dir = data_dir.join(&name);
        let path = dir.join("project.json");
        let text = fs::read_to_string(&path).map_err(|e| io_err("read", &path, e))?;
        let mut p: Project =
            serde_json::from_str(&text).map_err(|e| ProjectError::Format { path: path.clone(), message: e.to_string() })?;
        if p.format_version > FORMAT_VERSION {
            return Err(ProjectError::Version { found: p.format_version, supported: FORMAT_VERSION });
        }
        p.dir = dir;
        p.dirty = false;
        // A track that is not marked complete was interrupted: keep it, label it partial.
        for t in &mut p.tracks {
            if !t.complete {
                tracing::warn!(file = %t.file, "track was not closed properly; treated as partial");
            }
        }
        Ok(p)
    }

    /// Names of the projects below `data_dir` (those with a `project.json`).
    pub fn list(data_dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(data_dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter(|e| e.path().join("project.json").is_file())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    pub fn push_analysis(&mut self, kind: &str, params: serde_json::Value, result: serde_json::Value) -> usize {
        let id = self.analyses.len();
        self.analyses.push(AnalysisRevision { id, created: crate::measure::now_iso(), kind: kind.into(), params, result });
        self.dirty = true;
        id
    }

    /// Resolve a track file name inside this project (no path traversal).
    pub fn track_path(&self, file: &str) -> Result<PathBuf, ProjectError> {
        let f = Path::new(file);
        if f.components().count() != 1 || file.contains("..") {
            return Err(ProjectError::BadName(file.to_string()));
        }
        Ok(self.tracks_dir().join(f))
    }
}

/// Incremental CSV writer of streamed samples.
pub struct TrackWriter {
    path: PathBuf,
    w: BufWriter<File>,
    pub samples: u64,
}

impl TrackWriter {
    pub const COLUMNS: &'static str = "t_s,wall_unix_s,freq_hz,x_v,y_v,r_v,phase_deg";

    pub fn create(path: &Path, header: &[(String, String)]) -> Result<Self, ProjectError> {
        if let Some(d) = path.parent() {
            fs::create_dir_all(d).map_err(|e| io_err("create", d, e))?;
        }
        let f = File::create(path).map_err(|e| io_err("create", path, e))?;
        let mut w = BufWriter::new(f);
        let mut head = String::new();
        for (k, v) in header {
            head.push_str(&format!("# {}: {}\n", k, v.replace('\n', " ")));
        }
        head.push_str(Self::COLUMNS);
        head.push('\n');
        w.write_all(head.as_bytes()).map_err(|e| io_err("write", path, e))?;
        w.flush().map_err(|e| io_err("flush", path, e))?;
        Ok(Self { path: path.to_path_buf(), w, samples: 0 })
    }

    /// Append a batch and flush. `wall_end_unix_s` is the wall clock of the last sample.
    pub fn write_batch(&mut self, batch: &[Sample], wall_end_unix_s: f64) -> Result<(), ProjectError> {
        let Some(last) = batch.last() else { return Ok(()) };
        let mut out = String::with_capacity(batch.len() * 80);
        for s in batch {
            let wall = wall_end_unix_s - (last.t_s - s.t_s);
            out.push_str(&format!(
                "{:.9},{:.6},{:.6},{:.9e},{:.9e},{:.9e},{:.6}\n",
                s.t_s,
                wall,
                s.freq_hz,
                s.x,
                s.y,
                s.r(),
                s.phase_rad.to_degrees()
            ));
        }
        self.w.write_all(out.as_bytes()).map_err(|e| io_err("write", &self.path, e))?;
        self.w.flush().map_err(|e| io_err("flush", &self.path, e))?;
        self.samples += batch.len() as u64;
        Ok(())
    }

    pub fn finish(mut self) -> Result<u64, ProjectError> {
        self.w.flush().map_err(|e| io_err("flush", &self.path, e))?;
        Ok(self.samples)
    }
}

/// Read `(time, frequency)` from a track CSV, or from any 2+-column numeric text file
/// (`time freq` separated by comma/tab/space; `#` comments skipped), as used by the
/// reference Sauerbrey tool.
pub fn load_series(path: &Path, max_points: Option<usize>) -> Result<FreqSeries, ProjectError> {
    let text = fs::read_to_string(path).map_err(|e| io_err("read", path, e))?;
    let (mut t_col, mut f_col) = (0usize, 1usize);
    let (mut t, mut f) = (Vec::new(), Vec::new());
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = l.split(|c: char| c == ',' || c == '\t' || c == ';' || c.is_whitespace()).filter(|c| !c.is_empty()).collect();
        if cols.iter().any(|c| c.parse::<f64>().is_err()) {
            // header line
            if let Some(i) = cols.iter().position(|c| c.eq_ignore_ascii_case("t_s")) {
                t_col = i
            }
            if let Some(i) = cols.iter().position(|c| c.eq_ignore_ascii_case("freq_hz")) {
                f_col = i
            }
            continue;
        }
        if let (Some(a), Some(b)) =
            (cols.get(t_col).and_then(|c| c.parse::<f64>().ok()), cols.get(f_col).and_then(|c| c.parse::<f64>().ok()))
        {
            if a.is_finite() && b.is_finite() {
                t.push(a);
                f.push(b);
            }
        }
    }
    if t.len() < 2 {
        return Err(ProjectError::Analysis(format!("`{}` contains fewer than two valid (time, frequency) rows", path.display())));
    }
    if let Some(m) = max_points {
        if t.len() > m && m >= 2 {
            let step = (t.len() as f64 / m as f64).ceil() as usize;
            let keep = |v: &Vec<f64>| v.iter().step_by(step).copied().collect::<Vec<_>>();
            let last = (t[t.len() - 1], f[f.len() - 1]);
            let (mut t2, mut f2) = (keep(&t), keep(&f));
            if t2.last() != Some(&last.0) {
                t2.push(last.0);
                f2.push(last.1);
            }
            return Ok(FreqSeries { t_s: t2, f_hz: f2 });
        }
    }
    Ok(FreqSeries { t_s: t, f_hz: f })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hqcm_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn names_are_sanitised() {
        assert!(sanitize_name("run 1_a-b.c").is_ok());
        for bad in ["", "..", "../x", "a/b", "a\\b", ".hidden"] {
            assert!(sanitize_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn save_load_roundtrip_and_history_is_append_only() {
        let d = tmp("proj");
        let mut p = Project::new(&d, "p1", "cfg".into()).unwrap();
        p.sample_id = "Ti-001".into();
        p.log("test", "hello");
        let id0 = p.push_analysis("window", serde_json::json!({"a": 1}), serde_json::json!({"b": 2}));
        let id1 = p.push_analysis("window", serde_json::json!({"a": 2}), serde_json::json!({"b": 3}));
        assert_eq!((id0, id1), (0, 1));
        p.save().unwrap();
        assert!(!p.dirty);
        let q = Project::load(&d, "p1").unwrap();
        assert_eq!(q.sample_id, "Ti-001");
        assert_eq!(q.analyses.len(), 2);
        assert_eq!(q.history[0].text, "hello");
        assert!(Project::list(&d).contains(&"p1".to_string()));
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn newer_format_is_refused() {
        let d = tmp("ver");
        let mut p = Project::new(&d, "p", String::new()).unwrap();
        p.format_version = FORMAT_VERSION + 1;
        p.save().unwrap();
        assert!(matches!(Project::load(&d, "p"), Err(ProjectError::Version { .. })));
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn failed_save_keeps_dirty_flag() {
        let d = tmp("fail");
        let mut p = Project::new(&d, "p", String::new()).unwrap();
        // a regular file where the directory should be created
        fs::write(&p.dir, "x").unwrap();
        assert!(p.save().is_err());
        assert!(p.dirty);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn track_roundtrip_and_two_column_files() {
        let d = tmp("trk");
        let path = d.join("t.csv");
        let mut w = TrackWriter::create(&path, &[("sample".into(), "Ti".into())]).unwrap();
        let batch: Vec<Sample> =
            (0..10).map(|i| Sample { t_s: i as f64 * 0.1, x: 1e-3, y: 0.0, freq_hz: 6.0e6 - i as f64, phase_rad: 0.1 }).collect();
        w.write_batch(&batch, 1.7e9).unwrap();
        assert_eq!(w.finish().unwrap(), 10);
        let s = load_series(&path, None).unwrap();
        assert_eq!(s.t_s.len(), 10);
        assert!((s.f_hz[9] - (6.0e6 - 9.0)).abs() < 1e-3);
        let two = d.join("two.txt");
        fs::write(&two, "0\t6000000\n1\t5999990\n2\t5999980\n").unwrap();
        assert_eq!(load_series(&two, None).unwrap().t_s.len(), 3);
        let dec = load_series(&path, Some(4)).unwrap();
        assert!(dec.t_s.len() <= 6 && *dec.t_s.last().unwrap() == 0.9);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn track_path_rejects_traversal() {
        let d = tmp("tp");
        let p = Project::new(&d, "p", String::new()).unwrap();
        assert!(p.track_path("../x.csv").is_err());
        assert!(p.track_path("a/b.csv").is_err());
        assert!(p.track_path("ok.csv").is_ok());
        let _ = fs::remove_dir_all(d);
    }
}
