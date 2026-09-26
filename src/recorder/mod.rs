// Adapted for Timon. Not derived from Prodex source.
//! Per-user usage recording (accepted amendment A1).
//!
//! What this provides is *reported-usage visibility*, not billing. A producer
//! reports what it observed; the daemon fixes the identity and the receipt time
//! and stores the row durably. It cannot prove completeness: an account may run
//! a model client directly, or under-report, and nothing here would know.

pub mod client;
pub mod db;
pub mod event;
pub mod producer;
pub mod protocol;
pub mod render;
pub mod server;
