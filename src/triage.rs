// Adapted for Timon. Not derived from Prodex source.
//! Choosing how a goal is run, before anything is spent.
//!
//! The pilot measured orchestrating *everything* at 1.365x the tokens of asking
//! the strong model directly, slower on every task, because the lead's two calls
//! cost about 30,000 tokens whatever the work was. The saving therefore does not
//! come from orchestrating better. It comes from **not orchestrating work that
//! does not need it**, which is what this module decides.
//!
//! Three routes, all first-class:
//!
//! 1. **One cheap worker** — small, self-contained work.
//! 2. **One strong worker** — hard work that is still a single piece. The pilot
//!    was skipping past this route, at a cost of ~30,000 lead tokens per run.
//! 3. **Planner and task graph** — genuinely separable work.
//!
//! Deterministic rules, not a model call. A model asked to route would spend
//! tokens on every request, including the ones being routed to save tokens.
//!
//! **Weak signals gate entry; they never force a route.** Length and deliverable
//! count are poor evidence of difficulty — a short migration request can be
//! brutal and a long extraction request trivial — so they can only keep work
//! *out* of the fast path, never push it up to the planner. Anything uncertain
//! goes to a single strong call, which is the cheapest route that can plausibly
//! handle whatever it turns out to be.
//!
//! Every decision records which rules fired, because "was this routed well" is a
//! question about the alternative that was not run. The verifier cannot answer
//! it — a weak answer passes a schema and a citation check perfectly well — so
//! misrouting is measured in evaluation, and only if the reasons were kept.

use serde::{Deserialize, Serialize};

/// How a goal will be run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// One cheap-model worker session.
    CheapWorker,
    /// One strong-model worker session. The default when anything is unclear.
    StrongWorker,
    /// A strong planner, a validated task graph, and bounded workers.
    Planner,
}

impl Route {
    pub fn as_str(self) -> &'static str {
        match self {
            Route::CheapWorker => "cheap_worker",
            Route::StrongWorker => "strong_worker",
            Route::Planner => "planner",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "cheap_worker" | "cheap" => Route::CheapWorker,
            "strong_worker" | "strong" => Route::StrongWorker,
            "planner" => Route::Planner,
            _ => return None,
        })
    }

    /// What this costs, in the unit budgets are stated in.
    ///
    /// "One cheap-model call" is **one worker session, which may take several
    /// model and tool turns inside it** — not one provider request. Leaving that
    /// ambiguous would make every cost comparison in the evaluation unreadable,
    /// which is why it is written down here rather than assumed.
    pub fn sessions(self) -> &'static str {
        match self {
            Route::CheapWorker => "one cheap worker session",
            Route::StrongWorker => "one strong worker session",
            Route::Planner => "a lead session, plus one worker session per task",
        }
    }
}

/// What the caller said about how the work may be run.
///
/// None of it is the model's to choose. A developer's complexity hint is an
/// input to this decision, not authority over it.
#[derive(Clone, Debug, Default)]
pub struct Allowances {
    /// The caller insists on a route. Honoured, and recorded as their choice.
    pub route: Option<Route>,
    /// Whether the planner path may be used at all. Off means the most that can
    /// happen is a single strong call.
    pub allow_planner: bool,
}

/// A rule that fired, in the words an operator would use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reason {
    pub rule: String,
    pub detail: String,
}

impl Reason {
    fn new(rule: &str, detail: impl Into<String>) -> Self {
        Reason {
            rule: rule.to_string(),
            detail: detail.into(),
        }
    }
}

/// What triage decided, and why.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decision {
    pub route: Route,
    /// Every rule that fired, in the order they were considered.
    pub reasons: Vec<Reason>,
    /// Measured inputs, kept so a later evaluation can re-examine the decision
    /// without re-deriving it from the goal text.
    pub signals: Signals,
}

impl Decision {
    /// One line for a person.
    pub fn summary(&self) -> String {
        let why = self
            .reasons
            .first()
            .map(|r| r.detail.clone())
            .unwrap_or_else(|| "no rule fired".to_string());
        format!("{} — {why}", self.route.sessions())
    }
}

/// What was measured about the request.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Signals {
    pub goal_chars: usize,
    /// Deliverables the goal appears to name separately.
    pub deliverables: usize,
    /// Phrases suggesting work the fast path has no tools for.
    pub needs_tools: Vec<String>,
}

/// Longest goal admitted to the fast path.
///
/// Not a measure of difficulty — nothing here is. It keeps obviously large
/// requests out of the cheapest route, and nothing more.
pub const FAST_PATH_MAX_CHARS: usize = 280;

/// Work the cheap route has no tools for.
///
/// Deliberately about *capability*, not difficulty. Each of these describes
/// something a single cheap read-only call cannot do, so matching one is a fact
/// about the request rather than a guess about how hard it is.
const NEEDS_TOOLS: [(&str, &str); 10] = [
    ("refactor", "changing code across files"),
    ("migrate", "changing code across files"),
    ("implement", "writing code"),
    ("rewrite", "writing code"),
    ("fix the test", "running tests"),
    ("failing test", "running tests"),
    ("run the test", "running tests"),
    ("benchmark", "running the project"),
    ("profile", "running the project"),
    ("reproduce", "running the project"),
];

/// Markers that a goal names more than one deliverable.
const SEPARATORS: [&str; 6] = [" and also ", " then ", "; ", "\n- ", "\n* ", "\n1. "];

/// Decides the route for one goal.
pub fn decide(goal: &str, allowances: &Allowances) -> Decision {
    let signals = measure(goal);
    let mut reasons = Vec::new();

    // The caller's own choice wins, and is recorded as theirs so a later
    // evaluation does not credit or blame these rules for it.
    if let Some(route) = allowances.route {
        reasons.push(Reason::new(
            "caller_chose",
            format!("the caller asked for {}", route.as_str()),
        ));
        return Decision {
            route,
            reasons,
            signals,
        };
    }

    // The planner path, and only when the caller allowed it. Several
    // deliverables is the one signal that can *raise* a route, and it is still
    // not allowed to do so on its own.
    if signals.deliverables > 1 {
        if allowances.allow_planner {
            reasons.push(Reason::new(
                "several_deliverables",
                format!(
                    "the goal names {} separate deliverables, which is what the \
                     planner path is for",
                    signals.deliverables
                ),
            ));
            return Decision {
                route: Route::Planner,
                reasons,
                signals,
            };
        }
        reasons.push(Reason::new(
            "planner_not_allowed",
            format!(
                "the goal names {} deliverables, but the caller did not allow the \
                 planner path, so this is one strong call",
                signals.deliverables
            ),
        ));
        return Decision {
            route: Route::StrongWorker,
            reasons,
            signals,
        };
    }

    // Fast-path eligibility: narrow, and every disqualifier is a fact about
    // what the request needs rather than a judgement about its difficulty.
    let mut disqualified = false;
    if !signals.needs_tools.is_empty() {
        reasons.push(Reason::new(
            "needs_tools",
            format!(
                "the goal asks for {}, which one cheap call cannot do",
                signals.needs_tools.join(" and ")
            ),
        ));
        disqualified = true;
    }
    if signals.goal_chars > FAST_PATH_MAX_CHARS {
        reasons.push(Reason::new(
            "goal_too_long",
            format!(
                "the goal is {} characters, over the {FAST_PATH_MAX_CHARS} the fast \
                 path accepts",
                signals.goal_chars
            ),
        ));
        disqualified = true;
    }

    if disqualified {
        return Decision {
            route: Route::StrongWorker,
            reasons,
            signals,
        };
    }

    reasons.push(Reason::new(
        "fast_path_eligible",
        "one deliverable, short, and nothing it asks for needs tools the cheap \
         route lacks",
    ));
    Decision {
        route: Route::CheapWorker,
        reasons,
        signals,
    }
}

/// Measures a goal without judging it.
pub fn measure(goal: &str) -> Signals {
    let lower = goal.to_lowercase();
    let mut needs = Vec::new();
    for (phrase, why) in NEEDS_TOOLS {
        if lower.contains(phrase) && !needs.iter().any(|existing| existing == why) {
            needs.push(why.to_string());
        }
    }
    Signals {
        goal_chars: goal.chars().count(),
        deliverables: deliverables(&lower),
        needs_tools: needs,
    }
}

/// How many separate deliverables a goal appears to name.
///
/// Counted by splitting rather than by counting matches, because separators
/// overlap: "…this; then that" contains both `"; "` and `" then "`, and counting
/// matches makes two deliverables look like three. Over-counting is the
/// dangerous direction — it is the one signal allowed to raise a route, so an
/// inflated count sends single-piece work to the planner, which is the exact
/// cost the pilot measured.
fn deliverables(lower: &str) -> usize {
    let mut marked = lower.to_string();
    for separator in SEPARATORS {
        marked = marked.replace(separator, "\u{1}");
    }
    marked
        .split('\u{1}')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .count()
        .max(1)
}
