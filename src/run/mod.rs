// Adapted for Timon. Not derived from Prodex source.
//! A run: the durable record of one hand-off.
//!
//! A hand-off outlives the terminal that started it. The developer closes the
//! laptop; the work continues; they want to know later what happened. So a run
//! is a row that survives, not a process that does not.
//!
//! Everything later in the pipeline hangs off this. P3's artifacts belong to a
//! run, P6's invalidation walks a run's tasks, the report describes a run, and
//! cancellation names one. Building it first is what stops each of those
//! inventing its own idea of identity.
//!
//! Two properties are load-bearing and both come from Codex's review:
//!
//! **A restart does not lose or duplicate a run.** On start-up the host either
//! recovers a run or marks it interrupted. It never silently drops one, and
//! never starts a second copy of one already going.
//!
//! **The starting state is recorded, not assumed.** A repository with
//! uncommitted changes is the normal case, so the run says which commit it
//! began from and whether a snapshot was taken, rather than leaving a reader to
//! guess what "the base" meant.

pub mod execute;
pub mod record;
pub mod start;
