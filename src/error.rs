//! One error type per layer: transport -> instrument -> measurement -> project -> app.
//! Every variant keeps the context needed to debug a lab session (instrument,
//! node/command, raw response).

use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

/// Failures talking to the LabOne Data Server (here: through the Python bridge).
#[derive(Debug, Error, Clone)]
pub enum TransportError {
    #[error("bridge could not be started ({command}): {message}")]
    Spawn { command: String, message: String },
    #[error("I/O error during `{op}`: {message}")]
    Io { op: String, message: String },
    #[error("timeout after {after:?} during `{op}`")]
    Timeout { op: String, after: Duration },
    #[error("protocol error during `{op}`: {message} (raw: {raw})")]
    Protocol { op: String, message: String, raw: String },
    #[error("device/data server reported an error for `{op}`: {message}")]
    Remote { op: String, message: String },
    #[error("connection closed")]
    Closed,
    #[error("operation cancelled")]
    Cancelled,
}

/// Failures of the typed instrument driver.
#[derive(Debug, Error, Clone)]
pub enum InstrumentError {
    #[error("not connected to an instrument")]
    NotConnected,
    #[error("instrument `{instrument}`, node `{node}`: {source}")]
    Node {
        instrument: String,
        node: String,
        #[source]
        source: Box<TransportError>,
    },
    #[error("instrument `{instrument}`, operation `{op}`: {source}")]
    Op {
        instrument: String,
        op: String,
        #[source]
        source: Box<TransportError>,
    },
    #[error("safety limit: {what} = {value} outside [{min}, {max}]")]
    LimitViolation { what: String, value: f64, min: f64, max: f64 },
    #[error("invalid parameter: {0}")]
    InvalidParameter(String),
    #[error("unexpected value for node `{node}`: {raw}")]
    UnexpectedValue { node: String, raw: String },
    #[error("operation cancelled")]
    Cancelled,
    #[error("safe-state shutdown incomplete, uncertain outputs: {0}")]
    SafeStateIncomplete(String),
}

/// Failures of measurement routines (sweep, PLL, Q-control, HiPIMS acquisition).
#[derive(Debug, Error, Clone)]
pub enum MeasureError {
    #[error(transparent)]
    Instrument(#[from] InstrumentError),
    #[error("fit failed: {0}")]
    Fit(String),
    #[error("invalid measurement parameters: {0}")]
    InvalidParams(String),
    #[error("precondition not met: {0}")]
    Precondition(String),
    #[error("aborted for safety: {0}")]
    SafetyAbort(String),
    #[error("cancelled by the operator")]
    Cancelled,
}

impl MeasureError {
    /// Collapse a cancellation raised by the driver into the measurement-level variant.
    pub fn from_instrument(e: InstrumentError) -> Self {
        match e {
            InstrumentError::Cancelled => MeasureError::Cancelled,
            other => MeasureError::Instrument(other),
        }
    }
}

/// Failures reading/writing projects and recorded data.
#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("{op} `{path}`: {message}")]
    Io { op: &'static str, path: PathBuf, message: String },
    #[error("project file `{path}` is not valid: {message}")]
    Format { path: PathBuf, message: String },
    #[error("project format version {found} is newer than supported {supported}")]
    Version { found: u32, supported: u32 },
    #[error("invalid project name `{0}`")]
    BadName(String),
    #[error("no project is open")]
    NoProject,
    #[error("analysis error: {0}")]
    Analysis(String),
}

/// Application-layer error surfaced to the HTTP API.
#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Measure(#[from] MeasureError),
    #[error(transparent)]
    Instrument(#[from] InstrumentError),
    #[error(transparent)]
    Project(#[from] ProjectError),
    #[error("busy: {0}")]
    Busy(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("internal: {0}")]
    Internal(String),
}
