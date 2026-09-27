// Adapted for Timon. Not derived from Prodex source.
//! Making Codex Desktop and the IDE extensions visible to A1 (amendment A2).
//!
//! Desktop and the IDE extensions do not run `codex exec`. They speak the
//! app-server protocol over stdio, so nothing Timon supervises is involved and
//! their usage is invisible to the recorder. This module closes that gap.
//!
//! # Why this observes rather than bridges
//!
//! A2 was written to route Desktop through Timon's lead path, with bounded
//! workers and verification. Two things make that the wrong shape now.
//!
//! The first is a decision: orchestration is not a supported V1 capability
//! (`orchestration-experimental-not-v1`), so routing a Desktop session through
//! it would deliver the one thing V1 declines to claim.
//!
//! The second is arithmetic. The app-server protocol has **99 client-to-server
//! methods** — filesystem reads and writes, command execution, plugins, a
//! marketplace, MCP servers, skills, threads, turns, sandbox readiness — and
//! upstream marks it `[experimental]`. A shim that reimplemented that surface
//! would be a permanent chase after someone else's unstable protocol, and every
//! method it got subtly wrong would be a way to break a person's editor.
//!
//! So this does not implement the protocol. It copies it. The real app-server
//! runs as a child, bytes pass through unaltered in both directions, and the
//! stream is *read* on the way past for the one notification that carries token
//! counts. Conformance stops being a 99-method obligation and becomes a property
//! that can be stated and tested: what the client sent arrives unchanged, and
//! what the server said arrives unchanged.
//!
//! # What it records
//!
//! `thread/tokenUsage/updated` carries `threadId`, `turnId` and two breakdowns:
//! `last` for the turn that just finished and `total` for the thread so far.
//! Having both is unusually good luck — the `codex exec` adapter has to be told
//! whether counts are per-turn or cumulative, because the stream does not say,
//! and here the protocol supplies both so neither has to be assumed. `last` is
//! recorded, and `total` is used to check it.
//!
//! The breakdown is also richer than `codex exec --json`, which omits reasoning
//! tokens entirely (openai/codex#19022). Desktop sessions therefore report a
//! *more* complete figure than supervised runs do.
//!
//! # What it does not do
//!
//! It does not orchestrate, verify, bound, or interfere. It cannot make a
//! Desktop session cheaper or safer. A person who runs `codex` directly is still
//! invisible, and so is one who edits their client's configuration to bypass
//! this. Recording remains reported-usage visibility, not metering.

pub mod appserver;
