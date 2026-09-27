//! Timon: a lead model steering bounded workers on a shared server.

#[cfg(not(unix))]
compile_error!("Timon supports Unix-like systems only.");

pub mod admission;
pub mod attempt;
pub mod bridge;
pub mod mcp;
pub mod orchestrate;
pub mod recorder;
pub mod research;
pub mod result;
pub mod usage;
pub mod worker;
