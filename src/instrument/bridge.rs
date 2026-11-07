//! Transport to a real UHF through the official Zurich Instruments Python API.
//!
//! Why a bridge: there is no official Rust binding for ziAPI; the C structs of the
//! streaming interface are version dependent, and the Sweeper / DAQ / PID Advisor
//! *modules* are only conveniently exposed by `zhinst.core`. The bridge
//! (`bridge/uhf_bridge.py`) is a thin process speaking JSON lines on stdin/stdout:
//!
//! ```text
//! -> {"id": 7, "op": "set", "args": {"node": "/dev2091/oscs/0/freq", "type": "d", "value": 6e6}}
//! <- {"id": 7, "ok": true, "result": null}
//! ```
//!
//! Every call has a strict timeout. A timeout poisons the session (the stream may
//! be out of sync); the next call then fails fast with `Closed`.

use super::*;
use crate::config::Config;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::time::Instant;

/// The bridge script is compiled into the executable, so the binary is self-contained
/// on the lab PC. A script found at the configured path takes precedence (for edits).
const EMBEDDED_BRIDGE: &str = include_str!("../../bridge/uhf_bridge.py");

pub struct PyBridge {
    cfg: Arc<Config>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    rx: Option<flume::Receiver<String>>,
    next_id: u64,
    poisoned: bool,
}

fn io_err(op: &str, e: impl std::fmt::Display) -> TransportError {
    TransportError::Io { op: op.to_string(), message: e.to_string() }
}

impl PyBridge {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg, child: None, stdin: None, rx: None, next_id: 1, poisoned: false }
    }

    /// Path of the bridge script: the configured file if it exists, otherwise the
    /// embedded copy written to a per-version temp directory.
    fn script_path(&self) -> Result<std::path::PathBuf, TransportError> {
        let configured = &self.cfg.bridge.script;
        if configured.is_file() {
            return Ok(configured.clone());
        }
        let dir = std::env::temp_dir().join(format!("hipims-qcm-bridge-{}", env!("CARGO_PKG_VERSION")));
        std::fs::create_dir_all(&dir).map_err(|e| io_err("extract bridge", e))?;
        let path = dir.join("uhf_bridge.py");
        let current = std::fs::read_to_string(&path).ok();
        if current.as_deref() != Some(EMBEDDED_BRIDGE) {
            std::fs::write(&path, EMBEDDED_BRIDGE).map_err(|e| io_err("extract bridge", e))?;
        }
        Ok(path)
    }

    fn spawn(&mut self) -> Result<(), TransportError> {
        let script = self.script_path()?;
        let b = &self.cfg.bridge;
        let mut cmd = Command::new(&b.python);
        cmd.arg("-u").arg(&script).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().map_err(|e| TransportError::Spawn {
            command: format!("{} {}", b.python, script.display()),
            message: format!("{e} (is Python installed, with `pip install zhinst numpy`? set bridge.python in the config)"),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| io_err("spawn", "no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| io_err("spawn", "no stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| io_err("spawn", "no stderr"))?;
        let (tx, rx) = flume::unbounded::<String>();
        std::thread::Builder::new()
            .name("bridge-stdout".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(l) => {
                            if tx.send(l).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .map_err(|e| io_err("spawn", e))?;
        std::thread::Builder::new()
            .name("bridge-stderr".into())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    tracing::warn!(target: "bridge", "{line}");
                }
            })
            .map_err(|e| io_err("spawn", e))?;
        self.child = Some(child);
        self.stdin = Some(stdin);
        self.rx = Some(rx);
        self.poisoned = false;
        Ok(())
    }

    /// One request/response exchange with a strict timeout.
    fn call(&mut self, op: &str, args: Value, timeout: Duration) -> Result<Value, TransportError> {
        if self.poisoned {
            return Err(TransportError::Closed);
        }
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"id": id, "op": op, "args": args}).to_string();
        let started = Instant::now();
        {
            let stdin = self.stdin.as_mut().ok_or(TransportError::Closed)?;
            stdin.write_all(line.as_bytes()).and_then(|_| stdin.write_all(b"\n")).and_then(|_| stdin.flush()).map_err(|e| {
                self.poisoned = true;
                io_err(op, e)
            })?;
        }
        tracing::debug!(op, %line, "bridge request");
        let rx = self.rx.as_ref().ok_or(TransportError::Closed)?.clone();
        loop {
            let left = timeout.checked_sub(started.elapsed()).unwrap_or(Duration::ZERO);
            let raw = match rx.recv_timeout(left) {
                Ok(r) => r,
                Err(flume::RecvTimeoutError::Timeout) => {
                    self.poisoned = true;
                    return Err(TransportError::Timeout { op: op.into(), after: timeout });
                }
                Err(flume::RecvTimeoutError::Disconnected) => {
                    self.poisoned = true;
                    return Err(TransportError::Closed);
                }
            };
            let v: Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(e) => {
                    // Non-JSON output of the bridge/zhinst (banner, warnings): ignore, log.
                    tracing::debug!(%raw, error = %e, "bridge: non-JSON line ignored");
                    continue;
                }
            };
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                tracing::warn!(%raw, "bridge: stale response ignored");
                continue;
            }
            tracing::debug!(op, elapsed_ms = started.elapsed().as_millis() as u64, "bridge response");
            return if v.get("ok").and_then(Value::as_bool) == Some(true) {
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            } else {
                Err(TransportError::Remote {
                    op: op.into(),
                    message: v.get("error").and_then(Value::as_str).unwrap_or("unknown error").to_string(),
                })
            };
        }
    }

    fn short(&self) -> Duration {
        Duration::from_secs_f64(self.cfg.bridge.call_timeout_s)
    }

    fn call_short(&mut self, op: &str, args: Value) -> Result<Value, TransportError> {
        let t = self.short();
        self.call(op, args, t)
    }

    fn field_f64s(op: &str, v: &Value, key: &str) -> Result<Vec<f64>, TransportError> {
        serde_json::from_value::<Vec<f64>>(v.get(key).cloned().unwrap_or(Value::Null)).map_err(|e| TransportError::Protocol {
            op: op.into(),
            message: format!("field `{key}`: {e}"),
            raw: v.to_string().chars().take(200).collect(),
        })
    }

    /// Poll a start/progress/read/stop module run until finished, honouring cancel.
    fn run_module(&mut self, name: &str, args: Value, timeout_s: f64, ctl: &mut dyn JobCtl) -> Result<Value, TransportError> {
        self.call_short(&format!("{name}_start"), args)?;
        let started = Instant::now();
        loop {
            if ctl.cancelled() {
                if let Err(e) = self.call_short(&format!("{name}_stop"), Value::Null) {
                    tracing::error!(error = %e, module = name, "could not stop the module after cancel");
                }
                return Err(TransportError::Cancelled);
            }
            if started.elapsed().as_secs_f64() > timeout_s {
                let _ = self.call_short(&format!("{name}_stop"), Value::Null);
                return Err(TransportError::Timeout { op: name.into(), after: Duration::from_secs_f64(timeout_s) });
            }
            let p = self.call_short(&format!("{name}_progress"), Value::Null)?;
            let frac = p.get("progress").and_then(Value::as_f64).unwrap_or(0.0);
            ctl.progress(frac.clamp(0.0, 1.0), name);
            if p.get("finished").and_then(Value::as_bool) == Some(true) {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        self.call_short(&format!("{name}_read"), Value::Null)
    }
}

impl NodeIo for PyBridge {
    fn connect(&mut self) -> Result<DeviceInfo, TransportError> {
        if self.child.is_none() {
            self.spawn()?;
        }
        let cfg = Arc::clone(&self.cfg);
        let d = &cfg.device;
        let args = json!({"device": d.id, "host": d.host, "port": d.port});
        let t = Duration::from_secs_f64(self.cfg.bridge.connect_timeout_s);
        let v = self.call("connect", args, t)?;
        Ok(DeviceInfo {
            device: v.get("device").and_then(Value::as_str).unwrap_or(&d.id).to_string(),
            devtype: v.get("devtype").and_then(Value::as_str).unwrap_or("").to_string(),
            serial: v.get("serial").and_then(Value::as_str).unwrap_or("").to_string(),
            options: v
                .get("options")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|o| o.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            firmware: v.get("firmware").map(|f| f.to_string().trim_matches('"').to_string()).unwrap_or_default(),
            simulated: false,
        })
    }

    fn set(&mut self, node: &str, value: NodeValue) -> Result<(), TransportError> {
        let (ty, val) = match value {
            NodeValue::Int(i) => ("i", json!(i)),
            NodeValue::Double(d) => ("d", json!(d)),
        };
        self.call_short("set", json!({"node": node, "type": ty, "value": val})).map(|_| ())
    }

    fn get(&mut self, node: &str) -> Result<NodeValue, TransportError> {
        let v = self.call_short("get", json!({"node": node}))?;
        let val = v.get("value").and_then(Value::as_f64).ok_or_else(|| TransportError::Protocol {
            op: "get".into(),
            message: format!("no numeric value for `{node}`"),
            raw: v.to_string(),
        })?;
        Ok(if v.get("type").and_then(Value::as_str) == Some("i") { NodeValue::Int(val.round() as i64) } else { NodeValue::Double(val) })
    }

    fn sync(&mut self) -> Result<(), TransportError> {
        self.call_short("sync", Value::Null).map(|_| ())
    }

    fn read_sample(&mut self, demod: u8) -> Result<Sample, TransportError> {
        let v = self.call_short("read_sample", json!({"demod": demod}))?;
        let g = |k: &str| {
            v.get(k).and_then(Value::as_f64).ok_or_else(|| TransportError::Protocol {
                op: "read_sample".into(),
                message: format!("missing `{k}`"),
                raw: v.to_string(),
            })
        };
        Ok(Sample { t_s: g("t")?, x: g("x")?, y: g("y")?, freq_hz: g("frequency")?, phase_rad: g("phase")? })
    }

    fn wait(&mut self, d: Duration) {
        std::thread::sleep(d);
    }

    fn sweep(&mut self, req: &SweepRequest, ctl: &mut dyn JobCtl) -> Result<SweepData, TransportError> {
        let args = serde_json::to_value(req).map_err(|e| io_err("sweep", e))?;
        let timeout = self.cfg.timing.sweep_timeout_s;
        let v = self.run_module("sweep", args, timeout, ctl)?;
        Ok(SweepData {
            freq_hz: Self::field_f64s("sweep", &v, "freq")?,
            x: Self::field_f64s("sweep", &v, "x")?,
            y: Self::field_f64s("sweep", &v, "y")?,
        })
    }

    fn stream_start(&mut self, cfg: &StreamConfig) -> Result<(), TransportError> {
        self.call_short("stream_start", json!({"demod": cfg.demod, "rate": cfg.rate_hz})).map(|_| ())
    }

    fn stream_poll(&mut self, window: Duration) -> Result<Vec<Sample>, TransportError> {
        let t = window + self.short();
        let v = self.call("stream_poll", json!({"window_s": window.as_secs_f64()}), t)?;
        let (ts, x, y, f, p) = (
            Self::field_f64s("stream_poll", &v, "t")?,
            Self::field_f64s("stream_poll", &v, "x")?,
            Self::field_f64s("stream_poll", &v, "y")?,
            Self::field_f64s("stream_poll", &v, "frequency")?,
            Self::field_f64s("stream_poll", &v, "phase")?,
        );
        let n = ts.len();
        if [x.len(), y.len(), f.len(), p.len()].iter().any(|l| *l != n) {
            return Err(TransportError::Protocol {
                op: "stream_poll".into(),
                message: "column length mismatch".into(),
                raw: String::new(),
            });
        }
        Ok((0..n).map(|i| Sample { t_s: ts[i], x: x[i], y: y[i], freq_hz: f[i], phase_rad: p[i] }).collect())
    }

    fn stream_stop(&mut self) -> Result<(), TransportError> {
        self.call_short("stream_stop", Value::Null).map(|_| ())
    }

    fn gated_acquire(&mut self, req: &GatedRequest, ctl: &mut dyn JobCtl) -> Result<GatedResult, TransportError> {
        let args = serde_json::to_value(req).map_err(|e| io_err("gated", e))?;
        let v = self.run_module("gated", args, req.timeout_s, ctl)?;
        Ok(GatedResult {
            t_s: Self::field_f64s("gated", &v, "t")?,
            x: Self::field_f64s("gated", &v, "x")?,
            y: Self::field_f64s("gated", &v, "y")?,
            pulses: v.get("pulses").and_then(Value::as_u64).unwrap_or(0) as usize,
        })
    }

    fn pid_advisor(&mut self, req: &AdvisorRequest, ctl: &mut dyn JobCtl) -> Result<AdvisorResult, TransportError> {
        let args = serde_json::to_value(req).map_err(|e| io_err("advisor", e))?;
        let v = self.run_module("advisor", args, 120.0, ctl)?;
        let g = |k: &str| {
            v.get(k).and_then(Value::as_f64).ok_or_else(|| TransportError::Protocol {
                op: "advisor".into(),
                message: format!("missing `{k}`"),
                raw: v.to_string(),
            })
        };
        Ok(AdvisorResult { p: g("p")?, i: g("i")?, d: g("d")?, bandwidth_hz: g("bw")?, from_device: true })
    }

    fn load_settings(&mut self, path: &str) -> Result<(), TransportError> {
        let t = Duration::from_secs_f64(self.cfg.bridge.connect_timeout_s);
        self.call("load_settings", json!({"path": path}), t).map(|_| ())
    }

    fn save_settings(&mut self, path: &str) -> Result<(), TransportError> {
        let t = Duration::from_secs_f64(self.cfg.bridge.connect_timeout_s);
        self.call("save_settings", json!({"path": path}), t).map(|_| ())
    }

    fn close(&mut self) {
        if !self.poisoned {
            let _ = self.call("close", Value::Null, Duration::from_secs(2));
        }
        self.stdin = None; // EOF ends the bridge
        if let Some(mut c) = self.child.take() {
            let started = Instant::now();
            loop {
                match c.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if started.elapsed() < Duration::from_secs(2) => std::thread::sleep(Duration::from_millis(50)),
                    _ => {
                        let _ = c.kill();
                        let _ = c.wait();
                        break;
                    }
                }
            }
        }
        self.rx = None;
    }
}

impl Drop for PyBridge {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake bridge in Python exercises the framing, timeouts and error mapping
    /// without LabOne. Skipped when no Python interpreter is available.
    fn python() -> Option<String> {
        for p in ["python", "python3", "py"] {
            if Command::new(p).arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
                return Some(p.to_string());
            }
        }
        None
    }

    #[test]
    fn echo_bridge_roundtrip_error_and_timeout() {
        let Some(py) = python() else { return };
        let dir = std::env::temp_dir().join("hipims_bridge_test");
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake_bridge.py");
        std::fs::write(
            &script,
            r#"
import sys, json, time
for line in sys.stdin:
    m = json.loads(line)
    op = m["op"]
    if op == "connect":
        print("banner noise that is not json", flush=True)
        r = {"id": m["id"], "ok": True, "result": {"device": "devX", "devtype": "UHFLI", "serial": "1", "options": ["PID"], "firmware": 1}}
    elif op == "get":
        r = {"id": m["id"], "ok": True, "result": {"type": "d", "value": 1.5}}
    elif op == "set":
        r = {"id": m["id"], "ok": False, "error": "node not found"}
    elif op == "sync":
        time.sleep(3); continue
    else:
        r = {"id": m["id"], "ok": True, "result": None}
    print(json.dumps(r), flush=True)
"#,
        )
        .unwrap();
        let mut c = Config::default();
        c.bridge.python = py;
        c.bridge.script = script;
        c.bridge.call_timeout_s = 1.0;
        let mut b = PyBridge::new(Arc::new(c));
        let info = b.connect().unwrap();
        assert_eq!(info.devtype, "UHFLI");
        assert_eq!(b.get("/devX/oscs/0/freq").unwrap().as_f64(), 1.5);
        let e = b.set("/devX/nope", NodeValue::Int(1)).unwrap_err();
        assert!(matches!(e, TransportError::Remote { ref message, .. } if message.contains("node not found")));
        let e = b.sync().unwrap_err();
        assert!(matches!(e, TransportError::Timeout { .. }), "{e}");
        // a timeout poisons the session: later calls fail fast
        assert!(matches!(b.get("/x").unwrap_err(), TransportError::Closed));
        b.close();
    }

    #[test]
    fn missing_interpreter_is_reported() {
        let mut c = Config::default();
        c.bridge.python = "definitely-not-a-python-binary".into();
        let mut b = PyBridge::new(Arc::new(c));
        assert!(matches!(b.connect().unwrap_err(), TransportError::Spawn { .. }));
    }
}
