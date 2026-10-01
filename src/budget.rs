// Adapted for Timon. Not derived from Prodex source.
//! How long a run may take, as one number.
//!
//! A run's duration was governed by three settings that had to agree: when the
//! scheduler stops starting work, how long a single worker may run, and how many
//! run at once. Three knobs that must agree is three chances to get it wrong,
//! and the defaults got it wrong — the run deadline only stops work *starting*,
//! so a worker admitted just before it could run for its own deadline
//! afterwards. With a 240-second run deadline and a 600-second worker deadline
//! that is fourteen minutes for a run somebody thought was capped at four.
//!
//! So the budget is the input and the rest is derived. The arithmetic is here,
//! in one place, where it can be read:
//!
//! ```text
//! slices          = max_depth + 1        one for the planner, one per level
//! worker deadline = budget / slices
//! run deadline    = budget − worker deadline
//! worst case      = run deadline + worker deadline = budget
//! ```
//!
//! The last line is the point. Overshoot is impossible by construction rather
//! than unlikely in practice.
//!
//! **One pass.** A run does its work once, reports what it achieved and what was
//! checked, and stops. Whether to go again is the developer's decision, not the
//! host's — which is also the honest answer to a problem found on 2026-10-01: a
//! planner wrote a task saying "use clear link text" and a criterion demanding
//! the link text be the filename. A machine repairing against that criterion
//! would have made correct work worse, confidently, and spent tokens doing it.

use std::time::Duration;

use serde::Serialize;

/// What a run is allowed in wall-clock terms, and everything derived from it.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Budget {
    /// What the caller asked for.
    pub total: Duration,
    /// When the scheduler stops starting new work.
    pub run_deadline: Duration,
    /// How long any one worker may run, planner included.
    pub worker_deadline: Duration,
    /// How many workers may run at once.
    pub concurrency: usize,
    /// The deepest chain of dependencies that can still fit.
    pub max_depth: usize,
}

/// Shortest budget that can do anything useful.
///
/// Below this a worker's slice is too small to finish a turn, so the run would
/// spend tokens and be killed. Refusing is kinder than trying.
pub const MINIMUM: Duration = Duration::from_secs(60);

/// How many workers run at once when nothing says otherwise.
///
/// Width is nearly free — independent tasks overlap — so this is about how much
/// of the provider's attention one run should take on a shared host, not about
/// the budget.
pub const DEFAULT_CONCURRENCY: usize = 4;

impl Budget {
    /// Derives a coherent set of limits from one number.
    ///
    /// `depth` is how deep a plan may be; it decides how the budget is sliced,
    /// because depth is what costs time. A deeper plan means thinner slices, and
    /// at some point the slices are too thin to be worth starting — which is
    /// what [`MINIMUM`] and [`Budget::fits`] are for.
    pub fn from_total(total: Duration, depth: usize, concurrency: Option<usize>) -> Self {
        let depth = depth.max(1);
        // One slice for the planner, one for each level of the graph. The
        // planner is not free and pretending otherwise is how a budget gets
        // quietly exceeded on its first call.
        let slices = (depth + 1) as u32;
        let worker_deadline = total / slices;
        Budget {
            total,
            // Stop starting work early enough that the last worker admitted
            // cannot run past the budget.
            run_deadline: total.saturating_sub(worker_deadline),
            worker_deadline,
            concurrency: concurrency.unwrap_or(DEFAULT_CONCURRENCY).max(1),
            max_depth: depth,
        }
    }

    /// The worst case, stated so it can be checked against the budget.
    ///
    /// A worker admitted a moment before the run deadline runs for its own
    /// deadline afterwards, so this is the sum rather than the larger of the two.
    pub fn worst_case(&self) -> Duration {
        self.run_deadline + self.worker_deadline
    }

    /// Whether this budget can do anything worth starting.
    pub fn viable(&self) -> Result<(), String> {
        if self.total < MINIMUM {
            return Err(format!(
                "a budget of {}s is below the {}s minimum: a worker's share would be \
                 too small to finish a turn, so the run would spend tokens and then \
                 be stopped",
                self.total.as_secs(),
                MINIMUM.as_secs()
            ));
        }
        if self.worker_deadline < Duration::from_secs(15) {
            return Err(format!(
                "a depth of {} divides {}s into {}s per worker, which is not long \
                 enough for a model turn. Allow more time or plan shallower",
                self.max_depth,
                self.total.as_secs(),
                self.worker_deadline.as_secs()
            ));
        }
        Ok(())
    }

    /// Whether a plan of this depth can finish inside the budget.
    ///
    /// Checked against the worst case — every level taking its full slice —
    /// because a plan refused before anything runs costs nothing, and one
    /// discovered at minute four costs the whole run.
    pub fn fits(&self, depth: usize) -> Result<(), String> {
        let needed = self.worker_deadline * (depth as u32 + 1);
        if needed > self.total {
            return Err(format!(
                "a plan {depth} deep needs up to {}s ({} levels plus the planner, at \
                 {}s each) and the budget is {}s. Either allow more time or plan \
                 shallower; this was refused before anything was spent",
                needed.as_secs(),
                depth,
                self.worker_deadline.as_secs(),
                self.total.as_secs()
            ));
        }
        Ok(())
    }

    /// One line for a person, so the derivation is visible rather than implied.
    pub fn describe(&self) -> String {
        format!(
            "{}s total: planning and each level get up to {}s, new work stops at {}s, \
             {} worker(s) at once. Worst case {}s",
            self.total.as_secs(),
            self.worker_deadline.as_secs(),
            self.run_deadline.as_secs(),
            self.concurrency,
            self.worst_case().as_secs()
        )
    }
}

/// What a developer is told after one pass.
///
/// The decision about going again is theirs, so the report says what was
/// achieved and what was not, and does not recommend.
#[derive(Clone, Debug, Serialize)]
pub struct PassReport {
    pub pass: u32,
    /// What this pass used, against what it was allowed.
    pub elapsed_secs: u64,
    pub budget_secs: u64,
    /// True when the budget stopped the pass rather than the work finishing.
    pub stopped_by_budget: bool,
}

impl PassReport {
    pub fn describe(&self) -> String {
        if self.stopped_by_budget {
            format!(
                "pass {} used its whole {}s budget and was stopped with work \
                 outstanding",
                self.pass, self.budget_secs
            )
        } else {
            format!(
                "pass {} finished in {}s of a {}s budget",
                self.pass, self.elapsed_secs, self.budget_secs
            )
        }
    }
}
