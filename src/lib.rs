//! Timon: a lead model steering bounded workers on a shared server.

#[cfg(not(unix))]
compile_error!("Timon supports Unix-like systems only.");

pub mod admission;
pub mod attempt;
pub mod bridge;
pub mod broker;
pub mod dag;
pub mod dag_inputs;
pub mod dag_run;
pub mod mcp;
pub mod orchestrate;
pub mod recorder;
pub mod research;
pub mod result;
pub mod run;
pub mod triage;
pub mod usage;
pub mod worker;
