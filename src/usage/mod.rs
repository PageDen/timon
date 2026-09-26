//! Normalized token usage for one attempt.
//!
//! Usage is a *report* from the model harness, not a measurement Timon can
//! verify. Two rules follow from that and are enforced by the types here:
//!
//! 1. A count that was not reported is [`TokenCount::Unknown`]. It never
//!    becomes zero, and a total built from a partial stream is labelled
//!    [`UsageStatus::Partial`].
//! 2. Nothing is counted twice. `cached_input` is a subset of `input` and
//!    `reasoning_output` a subset of `output`, so neither is added into a
//!    total. Per-turn events and an attempt-level total are separate
//!    authorities; when both arrive, one is chosen and the choice is recorded.

pub mod codex;

use serde::{Serialize, Serializer};
use std::collections::HashSet;

/// Upper bound on recorded notes, so a hostile or broken stream cannot grow
/// the report without limit.
pub const MAX_NOTES: usize = 64;

/// A reported token count.
///
/// `Unknown` means the harness did not report the number. It is not zero and
/// must not be rendered as zero.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TokenCount {
    #[default]
    Unknown,
    Known(u64),
}

impl TokenCount {
    /// The reported value, or `None` when it was not reported.
    pub fn value(self) -> Option<u64> {
        match self {
            TokenCount::Known(value) => Some(value),
            TokenCount::Unknown => None,
        }
    }

    pub fn is_known(self) -> bool {
        matches!(self, TokenCount::Known(_))
    }

    /// Adds two counts. Known values are summed; an unknown operand
    /// contributes nothing and leaves the sum a lower bound, which the caller
    /// records as [`UsageStatus::Partial`].
    fn add(self, other: Self) -> Self {
        match (self, other) {
            (TokenCount::Known(left), TokenCount::Known(right)) => {
                TokenCount::Known(left.saturating_add(right))
            }
            (TokenCount::Known(value), TokenCount::Unknown)
            | (TokenCount::Unknown, TokenCount::Known(value)) => TokenCount::Known(value),
            (TokenCount::Unknown, TokenCount::Unknown) => TokenCount::Unknown,
        }
    }

    /// Subtracts a subset count, saturating at zero. Unknown on either side
    /// makes the difference unknown, because a subset of an unknown total says
    /// nothing about the remainder.
    fn saturating_sub(self, other: Self) -> Self {
        match (self, other) {
            (TokenCount::Known(total), TokenCount::Known(part)) => {
                TokenCount::Known(total.saturating_sub(part))
            }
            _ => TokenCount::Unknown,
        }
    }
}

impl Serialize for TokenCount {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            TokenCount::Known(value) => serializer.serialize_u64(*value),
            TokenCount::Unknown => serializer.serialize_none(),
        }
    }
}

/// Token counts for one turn or one attempt.
///
/// `cached_input` is the part of `input` that the provider served from its
/// prompt cache, and `reasoning_output` is the part of `output` spent on hidden
/// reasoning. Both are subsets, so [`TokenUsage::total`] adds only `input` and
/// `output`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct TokenUsage {
    pub input: TokenCount,
    /// Part of `input` served from the provider's prompt cache.
    pub cached_input: TokenCount,
    pub output: TokenCount,
    /// Part of `output` spent on reasoning, when the harness reports it.
    pub reasoning_output: TokenCount,
}

impl TokenUsage {
    /// Input tokens that were not served from cache.
    pub fn billable_input(&self) -> TokenCount {
        self.input.saturating_sub(self.cached_input)
    }

    /// `input + output`. Cached input and reasoning output are subsets of those
    /// two and are deliberately not added again.
    pub fn total(&self) -> TokenCount {
        self.input.add(self.output)
    }

    /// True when the counts cannot hold: a subset larger than its superset.
    fn is_inconsistent(&self) -> bool {
        fn exceeds(part: TokenCount, whole: TokenCount) -> bool {
            matches!((part, whole), (TokenCount::Known(part), TokenCount::Known(whole)) if part > whole)
        }
        exceeds(self.cached_input, self.input) || exceeds(self.reasoning_output, self.output)
    }

    fn add(self, other: Self) -> Self {
        Self {
            input: self.input.add(other.input),
            cached_input: self.cached_input.add(other.cached_input),
            output: self.output.add(other.output),
            reasoning_output: self.reasoning_output.add(other.reasoning_output),
        }
    }

    fn any_known(&self) -> bool {
        self.input.is_known()
            || self.cached_input.is_known()
            || self.output.is_known()
            || self.reasoning_output.is_known()
    }

    /// True when a later snapshot reports fewer tokens than an earlier one,
    /// which a cumulative stream cannot legitimately do.
    fn regresses_from(&self, earlier: &Self) -> bool {
        fn shrinks(later: TokenCount, earlier: TokenCount) -> bool {
            matches!((later, earlier), (TokenCount::Known(later), TokenCount::Known(earlier)) if later < earlier)
        }
        shrinks(self.input, earlier.input)
            || shrinks(self.cached_input, earlier.cached_input)
            || shrinks(self.output, earlier.output)
            || shrinks(self.reasoning_output, earlier.reasoning_output)
    }
}

/// Whether a stream reports each turn's own tokens or a running total.
///
/// Adding cumulative snapshots would multiply the count, and taking the last
/// delta would discard the rest, so the caller must state which shape the
/// stream has. Timon does not guess it from the numbers; it only reports when
/// the numbers contradict the declared shape
/// ([`UsageNote::SnapshotWentBackwards`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Accumulation {
    /// Each event reports the tokens of that turn alone. Events are summed.
    PerTurnDelta,
    /// Each event reports the running total for the attempt. The last event
    /// wins; events are never summed.
    CumulativeSnapshot,
}

/// How complete the reported usage is.
///
/// The judgment covers `input` and `output`, the two counts every reporting
/// harness is expected to supply. `reasoning_output` is deliberately excluded:
/// the Codex stream does not carry it today (openai/codex#19022), so requiring
/// it would mark every attempt partial and the label would stop meaning
/// anything. A missing reasoning count is still visible as `null` in the
/// report.
///
/// `Complete` describes the *stream*, not the provider's bill. Usage the
/// harness never emitted — startup prewarm, for one (openai/codex#46975) — is
/// invisible here by construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    /// Every turn the stream announced reported its input and output counts.
    Complete,
    /// Usage arrived, but at least one turn's counts are missing or partial.
    /// The totals are a lower bound.
    Partial,
    /// No usage was reported at all. The totals are unknown, not zero.
    Unknown,
}

/// Identity of one turn within an attempt.
///
/// The Codex event stream carries a `thread_id` but no turn id, so the index is
/// the turn's ordinal position within this attempt. The pair is what makes a
/// retransmitted or repeated event recognisable instead of double counted.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct TurnKey {
    pub thread_id: Option<String>,
    pub index: u32,
}

impl TurnKey {
    pub fn new(thread_id: Option<String>, index: u32) -> Self {
        Self { thread_id, index }
    }
}

/// Something worth saying about how the usage was derived.
///
/// Notes are the honest part of the report: they name what was missing,
/// ignored, or contradictory rather than smoothing it away.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "note", rename_all = "snake_case")]
pub enum UsageNote {
    /// A turn completed without reporting usage.
    TurnUsageMissing { turn: TurnKey },
    /// A turn reported usage twice. The repeat was ignored.
    DuplicateTurnIgnored { turn: TurnKey },
    /// A turn ended in failure. Any usage it reported is still counted.
    TurnFailed { turn: TurnKey, message: String },
    /// A subset count exceeded its superset, so the event is not internally
    /// consistent. It is still counted as reported.
    InconsistentSubset { turn: TurnKey },
    /// A cumulative snapshot reported fewer tokens than an earlier one. Either
    /// the stream is a delta stream or it lost state.
    SnapshotWentBackwards { turn: TurnKey },
    /// Both per-turn events and an attempt-level total arrived. The
    /// attempt-level total is used; the per-turn sum is reported separately and
    /// never added to it.
    AttemptTotalPreferred,
    /// A line could not be parsed as an event.
    UnparsableEvent { line: u32 },
    /// The usage stream could not be read, so no usage was recovered.
    StreamUnreadable { reason: String },
    /// The captured stream hit its size cap, so later events were never
    /// written and their usage is missing.
    StreamTruncated,
    /// The stream ended while a turn was still open, so that turn's usage was
    /// never reported.
    StreamEndedMidTurn { turn: TurnKey },
    /// Further notes were dropped at [`MAX_NOTES`].
    NotesTruncated,
}

/// Normalized usage for one attempt.
#[derive(Clone, Debug, Serialize)]
pub struct UsageReport {
    /// The accumulation shape the caller declared for the stream.
    pub accumulation: Accumulation,
    /// The usage Timon reports for this attempt.
    pub usage: TokenUsage,
    /// Input tokens not served from cache, derived from `usage`.
    pub billable_input: TokenCount,
    /// `input + output`, derived from `usage`.
    pub total: TokenCount,
    pub status: UsageStatus,
    /// Which events the totals came from.
    pub authority: UsageAuthority,
    /// Turns whose outcome the stream reported, with or without counts.
    pub turns_seen: u32,
    /// Turns that reported usage.
    pub turns_with_usage: u32,
    /// Repeated turn events that were ignored.
    pub duplicate_turns_ignored: u32,
    pub notes: Vec<UsageNote>,
}

/// Which events the reported totals were taken from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageAuthority {
    /// Summed or snapshotted per-turn events.
    Turns,
    /// An attempt-level total reported by the harness.
    AttemptTotal,
    /// Nothing usable was reported.
    None,
}

/// Builds a [`UsageReport`] from events in stream order.
///
/// The normalizer is provider-agnostic: an adapter (see [`codex`]) turns one
/// harness's event stream into these calls. Keeping the accounting rules here
/// means a second harness cannot reintroduce double counting or zero-for-
/// unknown on its own.
#[derive(Debug)]
pub struct UsageNormalizer {
    accumulation: Accumulation,
    turn_sum: TokenUsage,
    last_snapshot: Option<TokenUsage>,
    attempt_total: Option<TokenUsage>,
    seen_turns: HashSet<TurnKey>,
    turns_with_usage: u32,
    turns_missing_usage: u32,
    duplicate_turns_ignored: u32,
    notes: Vec<UsageNote>,
    notes_truncated: bool,
}

impl UsageNormalizer {
    pub fn new(accumulation: Accumulation) -> Self {
        Self {
            accumulation,
            turn_sum: TokenUsage::default(),
            last_snapshot: None,
            attempt_total: None,
            seen_turns: HashSet::new(),
            turns_with_usage: 0,
            turns_missing_usage: 0,
            duplicate_turns_ignored: 0,
            notes: Vec::new(),
            notes_truncated: false,
        }
    }

    /// Records a turn's reported usage.
    ///
    /// A key already seen is a repeat: it is ignored, not added, so a
    /// retransmitted event cannot inflate the attempt.
    pub fn turn_usage(&mut self, turn: TurnKey, usage: TokenUsage) {
        if !self.seen_turns.insert(turn.clone()) {
            self.duplicate_turns_ignored += 1;
            self.note(UsageNote::DuplicateTurnIgnored { turn });
            return;
        }
        if usage.is_inconsistent() {
            self.note(UsageNote::InconsistentSubset { turn: turn.clone() });
        }
        if !usage.any_known() {
            self.turns_missing_usage += 1;
            self.note(UsageNote::TurnUsageMissing { turn });
            return;
        }
        self.turns_with_usage += 1;
        match self.accumulation {
            Accumulation::PerTurnDelta => self.turn_sum = self.turn_sum.add(usage),
            Accumulation::CumulativeSnapshot => {
                if let Some(previous) = self.last_snapshot
                    && usage.regresses_from(&previous)
                {
                    self.note(UsageNote::SnapshotWentBackwards { turn });
                }
                self.last_snapshot = Some(usage);
            }
        }
    }

    /// Records a turn that completed or failed without usable counts.
    pub fn turn_usage_missing(&mut self, turn: TurnKey) {
        if !self.seen_turns.insert(turn.clone()) {
            self.duplicate_turns_ignored += 1;
            self.note(UsageNote::DuplicateTurnIgnored { turn });
            return;
        }
        self.turns_missing_usage += 1;
        self.note(UsageNote::TurnUsageMissing { turn });
    }

    /// Records an attempt-level total reported by the harness.
    ///
    /// This is a separate authority from the per-turn events. It replaces them
    /// in the report; the two are never added together.
    pub fn attempt_total(&mut self, usage: TokenUsage) {
        if usage.is_inconsistent() {
            self.note(UsageNote::InconsistentSubset {
                turn: TurnKey::new(None, 0),
            });
        }
        self.attempt_total = Some(usage);
    }

    pub fn note(&mut self, note: UsageNote) {
        if self.notes.len() >= MAX_NOTES {
            self.notes_truncated = true;
            return;
        }
        self.notes.push(note);
    }

    pub fn finish(mut self) -> UsageReport {
        let per_turn = match self.accumulation {
            Accumulation::PerTurnDelta => {
                if self.turns_with_usage > 0 {
                    Some(self.turn_sum)
                } else {
                    None
                }
            }
            Accumulation::CumulativeSnapshot => self.last_snapshot,
        };

        let (usage, authority) = match (self.attempt_total, per_turn) {
            (Some(total), Some(_)) => {
                self.note(UsageNote::AttemptTotalPreferred);
                (total, UsageAuthority::AttemptTotal)
            }
            (Some(total), None) => (total, UsageAuthority::AttemptTotal),
            (None, Some(turns)) => (turns, UsageAuthority::Turns),
            (None, None) => (TokenUsage::default(), UsageAuthority::None),
        };

        let status = if authority == UsageAuthority::None {
            UsageStatus::Unknown
        } else if self.turns_missing_usage > 0
            || !usage.input.is_known()
            || !usage.output.is_known()
        {
            UsageStatus::Partial
        } else {
            UsageStatus::Complete
        };

        if self.notes_truncated {
            self.notes.push(UsageNote::NotesTruncated);
        }

        UsageReport {
            accumulation: self.accumulation,
            usage,
            billable_input: usage.billable_input(),
            total: usage.total(),
            status,
            authority,
            turns_seen: self.turns_with_usage + self.turns_missing_usage,
            turns_with_usage: self.turns_with_usage,
            duplicate_turns_ignored: self.duplicate_turns_ignored,
            notes: self.notes,
        }
    }
}

/// A report for an attempt whose usage stream was never read.
pub fn unread(accumulation: Accumulation) -> UsageReport {
    UsageNormalizer::new(accumulation).finish()
}
