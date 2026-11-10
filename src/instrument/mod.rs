//! Instrument layers.
//!
//! * [`NodeIo`]  – transport: node get/set, module runs (sweeper, DAQ, PID advisor),
//!   streaming. Implemented by [`bridge::PyBridge`] (real UHF through the official
//!   `zhinst.core` Python API).
//! * [`uhf::Uhf`] – typed driver: unit-typed methods, hard limits validated before
//!   every write, safe-state shutdown.

pub mod bridge;
pub mod uhf;

use crate::error::TransportError;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Value of a LabOne node.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum NodeValue {
    Int(i64),
    Double(f64),
}

impl NodeValue {
    pub fn as_f64(self) -> f64 {
        match self {
            NodeValue::Int(i) => i as f64,
            NodeValue::Double(d) => d,
        }
    }
    pub fn as_i64(self) -> i64 {
        match self {
            NodeValue::Int(i) => i,
            NodeValue::Double(d) => d.round() as i64,
        }
    }
}

/// Identification of the connected device.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub device: String,
    pub devtype: String,
    pub serial: String,
    pub options: Vec<String>,
    pub firmware: String,
}

/// One demodulator sample. `t_s` is the instrument timestamp converted with the
/// device clockbase (seconds since device start).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Sample {
    pub t_s: f64,
    pub x: f64,
    pub y: f64,
    pub freq_hz: f64,
    pub phase_rad: f64,
}

impl Sample {
    pub fn r(&self) -> f64 {
        self.x.hypot(self.y)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamConfig {
    pub demod: u8,
    pub rate_hz: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepRequest {
    pub demod: u8,
    pub osc: u8,
    pub start_hz: f64,
    pub stop_hz: f64,
    pub points: usize,
    pub bandwidth_hz: f64,
    pub settle_inaccuracy: f64,
    pub averaging_tc: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SweepData {
    pub freq_hz: Vec<f64>,
    pub x: Vec<f64>,
    pub y: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorRequest {
    pub pid_index: u8,
    pub target_bw_hz: f64,
    /// 4 = internal PLL (phase -> oscillator frequency), 6 = resonator amplitude, ...
    pub dut_source: u8,
    /// Phase-slope magnitude |d(theta)/df| at resonance in deg/Hz (used by the
    /// local fallback; the device advisor measures its own model).
    pub slope_deg_per_hz: f64,
    pub q_factor: f64,
    pub f0_hz: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorResult {
    pub p: f64,
    pub i: f64,
    pub d: f64,
    pub bandwidth_hz: f64,
    /// `true` if computed by the LabOne PID Advisor module, `false` for the local model.
    pub from_device: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatedRequest {
    pub demod: u8,
    /// Gate signal (e.g. `TrigIn1`), hardware trigger on the rising edge.
    pub trigger_signal: String,
    /// Length of the recorded window after the trigger edge.
    pub window_s: f64,
    /// Trigger delay (negative = pre-trigger).
    pub delay_s: f64,
    pub pulses: usize,
    pub points: usize,
    pub timeout_s: f64,
    /// Informational: window start relative to the plasma-pulse start (s), as set on
    /// the pulse generator. Hardware ignores it.
    #[serde(default)]
    pub hint_window_start_s: f64,
}

/// Time-resolved, pulse-averaged response inside the measurement window.
/// `phase_rad` is the argument of the pulse-averaged complex demod signal.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatedResult {
    pub t_s: Vec<f64>,
    pub x: Vec<f64>,
    pub y: Vec<f64>,
    pub pulses: usize,
}

/// Progress/cancel channel handed to long-running operations.
pub trait JobCtl {
    fn progress(&mut self, fraction: f64, message: &str);
    fn cancelled(&self) -> bool;
}

/// Control handle that never cancels and ignores progress (tests, short calls).
pub struct NoCtl;
impl JobCtl for NoCtl {
    fn progress(&mut self, _fraction: f64, _message: &str) {}
    fn cancelled(&self) -> bool {
        false
    }
}

/// Cancellation flag shared with the actor so `stop`/`emergency_stop` work while
/// a long job holds the instrument.
#[derive(Debug, Clone, Default)]
pub struct AbortFlag(Arc<AtomicBool>);
impl AbortFlag {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn raise(&self) {
        self.0.store(true, Ordering::SeqCst)
    }
    pub fn clear(&self) {
        self.0.store(false, Ordering::SeqCst)
    }
    pub fn is_raised(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Transport + module layer of a UHF. One object = one connection; owned by exactly
/// one actor thread (never shared).
pub trait NodeIo: Send {
    /// Open the session and identify the device.
    fn connect(&mut self) -> Result<DeviceInfo, TransportError>;
    fn set(&mut self, node: &str, value: NodeValue) -> Result<(), TransportError>;
    fn get(&mut self, node: &str) -> Result<NodeValue, TransportError>;
    /// Global sync between Data Server and device (settings have taken effect).
    fn sync(&mut self) -> Result<(), TransportError>;
    /// Latest value of `/dev/demods/<n>/sample` (`getSample`).
    fn read_sample(&mut self, demod: u8) -> Result<Sample, TransportError>;
    /// Wait for `d`. Real sleep.
    fn wait(&mut self, d: Duration);

    /// Frequency sweep using the LabOne Sweeper module.
    fn sweep(&mut self, req: &SweepRequest, ctl: &mut dyn JobCtl) -> Result<SweepData, TransportError>;

    /// Data streaming (subscribe + poll) of one demodulator.
    fn stream_start(&mut self, cfg: &StreamConfig) -> Result<(), TransportError>;
    fn stream_poll(&mut self, window: Duration) -> Result<Vec<Sample>, TransportError>;
    fn stream_stop(&mut self) -> Result<(), TransportError>;

    /// Hardware-gated acquisition (DAQ module, trigger = HiPIMS measurement window).
    fn gated_acquire(&mut self, req: &GatedRequest, ctl: &mut dyn JobCtl) -> Result<GatedResult, TransportError>;

    /// PID Advisor module.
    fn pid_advisor(&mut self, req: &AdvisorRequest, ctl: &mut dyn JobCtl) -> Result<AdvisorResult, TransportError>;

    /// Device settings in LabOne XML format (as saved from the LabOne UI).
    fn load_settings(&mut self, path: &str) -> Result<(), TransportError>;
    fn save_settings(&mut self, path: &str) -> Result<(), TransportError>;

    fn close(&mut self);
}
