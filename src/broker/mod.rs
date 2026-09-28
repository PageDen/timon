// Adapted for Timon. Not derived from Prodex source.
//! Pooled provider credentials, held so that users never see them (amendment A4).
//!
//! # What this is for
//!
//! Quota rotation across several accounts of one provider. A user's own process
//! cannot hold the credential it authenticates with — that is the whole point —
//! so the credential has to live somewhere the user cannot read, and something
//! else has to make the call on their behalf.
//!
//! # What it costs, said here because it should be hard to miss
//!
//! This is the most concentrated trust in the project. One service will hold
//! every pooled credential *and* see every prompt and response in plaintext; a
//! trivial turn measured 48,904 bytes of prompt, including system instructions
//! and workspace context. Nothing else in Timon sees content at all. The recorder
//! deliberately stores token counts and never text, and the bridge forwards bytes
//! without keeping them. Compromising this service yields both what people wrote
//! and the means to write more at their expense.
//!
//! That was put to Chris explicitly and accepted. It is written down here so a
//! later reader does not discover it by reading the proxy loop.
//!
//! # The invariant this module exists to keep
//!
//! **A credential value never leaves the store.** Not in a report, not in JSON,
//! not in an error message, not in a log line. The types here deliberately do not
//! hold token strings, so there is nothing to leak by accident: reading an account
//! yields its name, its mode, a short prefix of its account identifier and the age
//! of its last refresh, and nothing that could authenticate a request.
//!
//! Slice 1 is this module and nothing else. There is no proxy here, no forwarding,
//! and no rotation — only the store and the means to see what is in it.

pub mod grant;
pub mod health;
pub mod identity;
pub mod notify;
pub mod policy;
pub mod quota;
pub mod refresh;
pub mod select;
pub mod serve;
pub mod store;
