//! Measurement routines (sweeps, PLL, Q-control, HiPIMS-gated acquisition).
//! They use only the typed [`crate::instrument::uhf::Uhf`] driver and the
//! configuration; settling times and parameters are never hard-coded.

pub mod hipims;
pub mod pll;
pub mod qcontrol;
pub mod sweep;

/// Current wall-clock time, ISO 8601 (UTC).
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
