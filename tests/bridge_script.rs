//! Runs the REAL `bridge/uhf_bridge.py` through the REAL Rust `PyBridge` + typed `Uhf`
//! driver against a fake `zhinst` package (bridge/fake_zhinst). This verifies the JSON
//! contract (field names, shapes, error mapping) between the two halves without LabOne.
//! It does not replace a test on the real instrument.
//!
//! Needs Python with numpy; set HQCM_TEST_PYTHON to choose the interpreter. Skipped otherwise.

use hipims_qcm::config::Config;
use hipims_qcm::error::{InstrumentError, TransportError};
use hipims_qcm::instrument::bridge::PyBridge;
use hipims_qcm::instrument::uhf::{Loop, Uhf};
use hipims_qcm::instrument::{AdvisorRequest, GatedRequest, JobCtl, NoCtl, SweepRequest};
use hipims_qcm::units::{Hertz, PidGain, Volt};
use std::sync::Arc;

fn python() -> Option<String> {
    let py = std::env::var("HQCM_TEST_PYTHON").unwrap_or_else(|_| "python".into());
    let ok = std::process::Command::new(&py).args(["-c", "import numpy"]).output().map(|o| o.status.success()).unwrap_or(false);
    ok.then_some(py)
}

struct CancelledCtl;
impl JobCtl for CancelledCtl {
    fn progress(&mut self, _f: f64, _m: &str) {}
    fn cancelled(&self) -> bool {
        true
    }
}

#[test]
fn real_bridge_script_against_fake_zhinst() {
    let Some(py) = python() else {
        eprintln!("skipped: no Python with numpy (set HQCM_TEST_PYTHON)");
        return;
    };
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    std::env::set_var("PYTHONPATH", root.join("bridge/fake_zhinst"));
    let mut c = Config::default();
    c.bridge.python = py;
    c.bridge.script = root.join("bridge/uhf_bridge.py");
    c.bridge.call_timeout_s = 20.0;
    c.bridge.connect_timeout_s = 60.0;
    let cfg = Arc::new(c);

    let mut u = Uhf::connect(Box::new(PyBridge::new(cfg.clone())), cfg.clone()).expect("connect through the bridge");
    assert_eq!(u.info().devtype, "UHFLI");
    assert!(u.info().options.iter().any(|o| o == "PID") && !u.info().simulated);

    // node round-trips with the right types
    u.set_osc_freq(0, Hertz(5.9985e6)).unwrap();
    assert_eq!(u.osc_freq(0).unwrap().get(), 5.9985e6);
    u.enable_drive(Volt(0.1)).unwrap();
    assert!(u.drive_is_on().unwrap());
    assert!((u.drive_amplitude().unwrap().get() - 0.1).abs() < 1e-12);
    u.set_gains(Loop::Pll, PidGain(-0.5), PidGain(-300.0), PidGain(0.0)).unwrap();
    assert_eq!(u.gains(Loop::Pll).unwrap().0.get(), -0.5);

    // single sample
    let s = u.read_sample(0).unwrap();
    assert!((s.t_s - 1.0).abs() < 1e-9 && (s.phase_rad - 1.1).abs() < 1e-12 && s.freq_hz == 6.0e6);

    // sweeper module run
    let req = SweepRequest {
        demod: 1,
        osc: 0,
        start_hz: 5.99e6,
        stop_hz: 6.01e6,
        points: 50,
        bandwidth_hz: 50.0,
        settle_inaccuracy: 0.001,
        averaging_tc: 5.0,
    };
    let d = u.sweep(&req, &mut NoCtl).unwrap();
    assert_eq!(d.freq_hz.len(), 50);
    assert!(d.x.iter().all(|v| v.is_finite()));
    // ... and its cancellation
    assert!(matches!(u.sweep(&req, &mut CancelledCtl), Err(InstrumentError::Cancelled)));

    // data streaming (subscribe / poll / unsubscribe), timestamps converted with the clockbase
    u.start_stream(0, Hertz(1000.0)).unwrap();
    let batch = u.poll_stream(hipims_qcm::units::Second(0.05)).unwrap();
    assert_eq!(batch.len(), 100);
    assert!((batch[1].t_s - batch[0].t_s - 600.0 / 60e6).abs() < 1e-12);
    u.stop_stream().unwrap();

    // hardware-triggered DAQ module
    let g = GatedRequest {
        demod: 1,
        trigger_signal: "TrigIn1".into(),
        window_s: 3e-4,
        delay_s: 0.0,
        pulses: 20,
        points: 30,
        timeout_s: 20.0,
        hint_window_start_s: 0.0,
    };
    let r = u.gated(&g, &mut NoCtl).unwrap();
    assert_eq!((r.pulses, r.t_s.len()), (20, 30));
    assert!((r.x[0] - 1e-3).abs() < 1e-12);

    // PID advisor module
    let a = u
        .advisor(
            &AdvisorRequest { pid_index: 0, target_bw_hz: 90.0, dut_source: 3, slope_deg_per_hz: 0.5, q_factor: 5e4, f0_hz: 6e6 },
            &mut NoCtl,
        )
        .unwrap();
    assert!(a.from_device && a.p == -0.5 && a.bandwidth_hz == 90.0);

    // settings XML via the deviceSettings module
    let path = std::env::temp_dir().join("hqcm_bridge_settings.xml");
    u.save_settings(&path.to_string_lossy()).unwrap();
    u.load_settings(&path.to_string_lossy()).unwrap();

    // safe state through the bridge, read back
    u.safe_state().unwrap();
    assert!(!u.drive_is_on().unwrap());

    // a clean disconnect ends the child process
    u.disconnect().unwrap();
    let err = u.read_sample(0).unwrap_err();
    assert!(
        matches!(err, InstrumentError::Op { ref source, .. } if matches!(**source, TransportError::Closed | TransportError::Io { .. })),
        "{err}"
    );
}
