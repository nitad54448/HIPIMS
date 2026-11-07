//! QCM resonance tracking, Q-control and HiPIMS-gated acquisition on a Zurich
//! Instruments UHF. Layers: `instrument` (transport + typed driver) -> `measure`
//! (routines) -> `analysis` (pure maths) -> application (actor, project, server).

// Validation deliberately uses `!(x > 0.0)` (instead of `x <= 0.0`) so that NaN is rejected too.
#![allow(clippy::neg_cmp_op_on_partial_ord)]

pub mod actor;
pub mod analysis;
pub mod config;
pub mod error;
pub mod instrument;
pub mod measure;
pub mod project;
pub mod report;
pub mod server;
pub mod units;
