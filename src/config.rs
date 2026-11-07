//! Configuration (TOML). Nothing physical is hard-coded in the routines: settling
//! times, limits, node roles and sweep defaults all come from here.
//!
//! Role/PID/routing defaults follow the 4-demodulator "mnemonic" setup of the
//! Q-control notes (1 = PLL, 2 = measurement, 3 = Q, 4 = AGC).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerCfg,
    pub device: DeviceCfg,
    pub bridge: BridgeCfg,
    pub roles: Roles,
    pub routing: Routing,
    pub pid: PidWiring,
    pub limits: Limits,
    pub timing: Timing,
    pub sim: SimCfg,
    pub data_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerCfg {
    /// TCP port of the local web UI/API (bound to 127.0.0.1 only).
    pub port: u16,
    pub open_browser: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeviceCfg {
    /// Device serial, e.g. `dev2091`.
    pub id: String,
    /// LabOne Data Server host (same PC as LabOne => `localhost`).
    pub host: String,
    /// Data Server port (8004 for UHF).
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BridgeCfg {
    /// Python interpreter that has `zhinst-core` + `zhinst-utils` + `numpy`.
    pub python: String,
    /// Path of the bridge script (relative to the working directory).
    pub script: PathBuf,
    /// Timeout of one short bridge call (seconds).
    pub call_timeout_s: f64,
    /// Timeout of the connection/discovery call (seconds).
    pub connect_timeout_s: f64,
}

/// Which demodulator/oscillator plays which role (0-based, as in the node tree).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Roles {
    pub pll_demod: u8,
    pub meas_demod: u8,
    /// Q-feedback demodulators (same oscillator): X component at 0 deg, Y component at 90 deg.
    pub q_demod_x: u8,
    pub q_demod_y: u8,
    pub agc_demod: u8,
    pub osc_main: u8,
    /// Oscillator used for the Q-feedback output (MF-MD option).
    pub osc_q: u8,
}

/// Signal output routing (UHF mixer channel indices of `sigouts/n/amplitudes/m`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Routing {
    pub sigout_drive: u8,
    pub drive_mixer: u8,
    pub drive_range_v: f64,
    /// Output-amplitude channels carrying the Q-feedback X (0 deg) and Y (90 deg) components;
    /// they sum with the drive on the SAME signal output (no second output, no divider).
    pub q_mixer_x: u8,
    pub q_mixer_y: u8,
    pub sigin: u8,
    pub sigin_range_v: f64,
    /// DAQ hardware-trigger signal used for HiPIMS gating (`sample.<name>`).
    pub gate_trigger_signal: String,
}

/// PID node wiring. Enumerations are those of the UHF-PID option: verify them
/// in the LabOne node tree (`/devN/pids/0/input`) on the target PC.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PidWiring {
    pub pll_index: u8,
    /// Q-control: PID acting on X and PID acting on Y of the measured response.
    pub q_x_index: u8,
    pub q_y_index: u8,
    pub agc_index: u8,
    /// `pids/n/input` enum: 0 = Demod X, 1 = Y, 2 = R, 3 = Theta.
    pub input_x: i64,
    pub input_y: i64,
    pub input_r: i64,
    pub input_theta: i64,
    /// `pids/n/output` enum: 0 = Signal output amplitude, 2 = Oscillator frequency.
    pub output_sigout_amplitude: i64,
    pub output_osc_freq: i64,
    /// Optional `pids/n/mode` value that selects PLL operation (node skipped if absent).
    pub pll_mode_value: Option<i64>,
}

/// Hard safety limits, enforced by the driver before every write.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub freq_min_hz: f64,
    pub freq_max_hz: f64,
    pub drive_max_v: f64,
    pub q_amp_max_v: f64,
    /// Largest |feedback gain| of the Q-control (P = -gain on both PIDs).
    pub q_gain_abs_max: f64,
    /// Absolute limit of PLL/AGC P and I gains.
    pub pid_gain_abs_max: f64,
    pub pll_limit_hz_max: f64,
    pub sweep_points_max: usize,
    pub stream_rate_max_hz: f64,
    pub tc_min_s: f64,
    pub tc_max_s: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timing {
    pub sweep_bandwidth_hz: f64,
    pub sweep_settle_inaccuracy: f64,
    pub sweep_averaging_tc: f64,
    pub sweep_timeout_s: f64,
    pub pll_lock_timeout_s: f64,
    pub pll_lock_tol_deg: f64,
    /// Poll step of the PLL lock-wait loop (s).
    pub pll_lock_poll_s: f64,
    pub pid_settle_s: f64,
    /// Time constant / order of the demodulator filtering X and Y for the Q-feedback.
    /// The PLL bandwidth must exceed the resulting Q-control bandwidth.
    pub q_demod_tc_s: f64,
    pub q_demod_order: u8,
    pub stream_poll_s: f64,
    pub gated_timeout_s: f64,
    pub journal_flush_s: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SimCfg {
    pub f0_hz: f64,
    pub q: f64,
    pub inverted_slope: bool,
    /// Gamma(gain) = Gamma_native + gain_slope * gain  (negative => gain > 0 raises Q).
    pub gain_slope: f64,
    pub noise_rel: f64,
    pub realtime: bool,
    pub parasitic_decay: bool,
    /// Slow resonance drift (deposition), Hz per second.
    pub drift_hz_per_s: f64,
    /// Simulated plasma pulse: width (us), total resonance shift (Hz), rise time (us).
    pub pulse_width_us: f64,
    pub pulse_shift_hz: f64,
    pub pulse_rise_us: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerCfg::default(),
            device: DeviceCfg::default(),
            bridge: BridgeCfg::default(),
            roles: Roles::default(),
            routing: Routing::default(),
            pid: PidWiring::default(),
            limits: Limits::default(),
            timing: Timing::default(),
            sim: SimCfg::default(),
            data_dir: PathBuf::from("data"),
        }
    }
}
impl Default for ServerCfg {
    fn default() -> Self {
        Self { port: 8765, open_browser: true }
    }
}
impl Default for DeviceCfg {
    fn default() -> Self {
        Self { id: "dev2091".into(), host: "localhost".into(), port: 8004 }
    }
}
impl Default for BridgeCfg {
    fn default() -> Self {
        Self {
            python: if cfg!(windows) { "python".into() } else { "python3".into() },
            script: PathBuf::from("bridge/uhf_bridge.py"),
            call_timeout_s: 10.0,
            connect_timeout_s: 30.0,
        }
    }
}
impl Default for Roles {
    fn default() -> Self {
        Self { pll_demod: 0, meas_demod: 1, q_demod_x: 2, q_demod_y: 3, agc_demod: 4, osc_main: 0, osc_q: 1 }
    }
}
impl Default for Routing {
    fn default() -> Self {
        Self {
            sigout_drive: 0,
            drive_mixer: 1,
            drive_range_v: 1.0,
            q_mixer_x: 2,
            q_mixer_y: 3,
            sigin: 0,
            sigin_range_v: 1.0,
            gate_trigger_signal: "TrigIn1".into(),
        }
    }
}
impl Default for PidWiring {
    fn default() -> Self {
        Self {
            pll_index: 0,
            q_x_index: 2,
            q_y_index: 3,
            agc_index: 1,
            input_x: 0,
            input_y: 1,
            input_r: 2,
            input_theta: 3,
            output_sigout_amplitude: 0,
            output_osc_freq: 2,
            pll_mode_value: None,
        }
    }
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            freq_min_hz: 1.0e6,
            freq_max_hz: 10.0e6,
            drive_max_v: 1.0,
            q_amp_max_v: 0.25,
            q_gain_abs_max: 50.0,
            pid_gain_abs_max: 1.0e6,
            pll_limit_hz_max: 20_000.0,
            sweep_points_max: 20_000,
            stream_rate_max_hz: 2.0e6,
            tc_min_s: 1.0e-7,
            tc_max_s: 1.0,
        }
    }
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            sweep_bandwidth_hz: 50.0,
            sweep_settle_inaccuracy: 0.001,
            sweep_averaging_tc: 5.0,
            sweep_timeout_s: 900.0,
            pll_lock_timeout_s: 20.0,
            pll_lock_tol_deg: 5.0,
            pll_lock_poll_s: 0.1,
            pid_settle_s: 0.75,
            q_demod_tc_s: 0.5e-3,
            q_demod_order: 4,
            stream_poll_s: 0.05,
            gated_timeout_s: 120.0,
            journal_flush_s: 1.0,
        }
    }
}
impl Default for SimCfg {
    fn default() -> Self {
        Self {
            f0_hz: 5.998_5e6,
            q: 50_000.0,
            inverted_slope: false,
            gain_slope: -15.0,
            noise_rel: 2e-4,
            realtime: true,
            parasitic_decay: true,
            drift_hz_per_s: -0.8,
            pulse_width_us: 100.0,
            pulse_shift_hz: -25.0,
            pulse_rise_us: 30.0,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config `{path}`: {message}")]
    Io { path: PathBuf, message: String },
    #[error("invalid config `{path}`: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

impl DeviceCfg {
    /// Trim and validate a device address entered by the operator (UI) or read from the
    /// config. The serial becomes part of every node path (`/dev2091/...`), so only
    /// letters, digits, `-` and `_` are accepted; LabOne node paths are lower case.
    pub fn normalized(&self) -> Result<DeviceCfg, ConfigError> {
        let bad = |m: &str| Err(ConfigError::Invalid(m.to_string()));
        let id = self.id.trim().to_ascii_lowercase();
        if id.is_empty() || id.len() > 32 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return bad("device ID must be 1-32 letters/digits (e.g. dev2091)");
        }
        let host = self.host.trim().to_string();
        if host.is_empty() || host.len() > 253 || host.chars().any(|c| c.is_whitespace() || c == '/') {
            return bad("host must be a host name or IP address (e.g. localhost)");
        }
        if self.port == 0 {
            return bad("port must be 1-65535 (LabOne Data Server: 8004)");
        }
        Ok(DeviceCfg { id, host, port: self.port })
    }
}

impl Config {
    /// Load a TOML file; missing keys fall back to defaults.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io { path: path.into(), message: e.to_string() })?;
        let cfg: Config = toml::from_str(&text).map_err(|e| ConfigError::Parse { path: path.into(), message: e.to_string() })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject physically meaningless or unsafe settings at construction time.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let l = &self.limits;
        let bad = |m: &str| Err(ConfigError::Invalid(m.to_string()));
        if !(l.freq_min_hz > 0.0 && l.freq_max_hz > l.freq_min_hz) {
            return bad("limits.freq_min_hz/freq_max_hz must satisfy 0 < min < max");
        }
        if !(l.drive_max_v > 0.0 && l.drive_max_v <= 10.0) {
            return bad("limits.drive_max_v must be in (0, 10] V");
        }
        if !(l.q_amp_max_v > 0.0 && l.q_amp_max_v <= l.drive_max_v) {
            return bad("limits.q_amp_max_v must be in (0, drive_max_v]");
        }
        if !(l.q_gain_abs_max > 0.0) || !(l.tc_min_s > 0.0 && l.tc_max_s > l.tc_min_s) {
            return bad("limits.q_gain_abs_max/tc_min_s/tc_max_s invalid");
        }
        let r = &self.roles;
        let demods = [r.pll_demod, r.meas_demod, r.q_demod_x, r.q_demod_y, r.agc_demod];
        if demods.iter().any(|d| *d > 7) {
            return bad("roles.*_demod must be 0..=7");
        }
        let mut sorted = demods;
        sorted.sort_unstable();
        if sorted.windows(2).any(|w| w[0] == w[1]) {
            return bad("each role needs its own demodulator");
        }
        let t = &self.timing;
        if !(t.q_demod_tc_s >= l.tc_min_s && t.q_demod_tc_s <= l.tc_max_s) || !(1..=8).contains(&t.q_demod_order) {
            return bad("timing.q_demod_tc_s must be within limits.tc_min_s..tc_max_s and q_demod_order in 1..=8");
        }
        if r.q_demod_x == r.q_demod_y {
            return bad("roles.q_demod_x and q_demod_y must differ");
        }
        let m = &self.routing;
        if m.q_mixer_x == m.q_mixer_y || [m.q_mixer_x, m.q_mixer_y].contains(&m.drive_mixer) {
            return bad("routing: drive_mixer, q_mixer_x and q_mixer_y must be three different channels");
        }
        // Every value that becomes a wait or a timeout must stay finite and sane: they are
        // converted to `Duration`s, and a stray huge number must be rejected at load time,
        // not panic the actor thread later.
        const DAY_S: f64 = 86_400.0;
        for (name, v) in [
            ("timing.sweep_timeout_s", t.sweep_timeout_s),
            ("timing.pll_lock_timeout_s", t.pll_lock_timeout_s),
            ("timing.pll_lock_poll_s", t.pll_lock_poll_s),
            ("timing.pid_settle_s", t.pid_settle_s),
            ("timing.stream_poll_s", t.stream_poll_s),
            ("timing.gated_timeout_s", t.gated_timeout_s),
            ("bridge.call_timeout_s", self.bridge.call_timeout_s),
            ("bridge.connect_timeout_s", self.bridge.connect_timeout_s),
        ] {
            if !(v.is_finite() && v > 0.0 && v <= DAY_S) {
                return bad(&format!("{name} must be in (0, {DAY_S}] s"));
            }
        }
        if !(t.pll_lock_tol_deg.is_finite() && t.pll_lock_tol_deg > 0.0 && t.pll_lock_tol_deg <= 180.0) {
            return bad("timing.pll_lock_tol_deg must be in (0, 180]");
        }
        if !(t.sweep_bandwidth_hz >= 0.01 && t.sweep_bandwidth_hz <= 1.0e6) {
            return bad("timing.sweep_bandwidth_hz must be in [0.01, 1e6] Hz (the driver sweep limit)");
        }
        self.device.normalized()?;
        Ok(())
    }

    /// Serialise to TOML (used to embed the exact configuration in each project).
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_else(|e| format!("# config serialisation failed: {e}\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn shipped_default_toml_loads_and_matches_the_builtin_defaults() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config/default.toml");
        let shipped = Config::load(&path).expect("config/default.toml must be valid");
        assert_eq!(
            toml::to_string(&shipped).unwrap(),
            toml::to_string(&Config::default()).unwrap(),
            "default.toml drifted from the code defaults"
        );
    }

    #[test]
    fn device_address_is_validated_and_normalised() {
        let ok = DeviceCfg { id: "  DEV2091 ".into(), host: " 10.0.0.5 ".into(), port: 8004 }.normalized().unwrap();
        assert_eq!((ok.id.as_str(), ok.host.as_str()), ("dev2091", "10.0.0.5"));
        for bad in ["", "dev 1", "dev/../x", "a".repeat(33).as_str(), "dev2091\n/oscs"] {
            assert!(DeviceCfg { id: bad.into(), ..DeviceCfg::default() }.normalized().is_err(), "{bad:?}");
        }
        assert!(DeviceCfg { host: "a b".into(), ..DeviceCfg::default() }.normalized().is_err());
        assert!(DeviceCfg { port: 0, ..DeviceCfg::default() }.normalized().is_err());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>(
            "[roles]
q_demod = 2
"
        )
        .is_err());
        assert!(toml::from_str::<Config>(
            "[limitz]
"
        )
        .is_err());
    }

    #[test]
    fn partial_toml_uses_defaults() {
        let cfg: Config = toml::from_str("[device]\nid = \"dev1234\"\n").unwrap();
        assert_eq!(cfg.device.id, "dev1234");
        assert_eq!(cfg.device.port, 8004);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn duplicate_demod_roles_rejected() {
        let mut cfg = Config::default();
        cfg.roles.q_demod_x = cfg.roles.pll_demod;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn roundtrip_toml() {
        let cfg = Config::default();
        let back: Config = toml::from_str(&cfg.to_toml()).unwrap();
        assert_eq!(back.device.id, cfg.device.id);
        assert_eq!(back.limits.q_gain_abs_max, cfg.limits.q_gain_abs_max);
    }
}
