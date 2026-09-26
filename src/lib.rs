//! Timon: a lead model steering bounded workers on a shared server.

#[cfg(not(unix))]
compile_error!("Timon supports Unix-like systems only.");

pub mod worker;
