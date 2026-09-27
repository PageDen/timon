// Adapted for Timon. Not derived from Prodex source.
//! Checking whether a cited claim is actually supported.
//!
//! The route Timon uses returns no source content of its own: a captured
//! research stream carries the search *query* and nothing else, so the only
//! evidence for a citation is what the model wrote. Taking that at face value
//! would be circular, since a model can invent a URL and an excerpt together.
//!
//! So a claim is checked against the page it cites. That means fetching, which
//! means treating every cited URL as hostile input: it was chosen by a model,
//! influenced by pages the model read, and pointed at this host.

pub mod fetch;
pub mod verify;
