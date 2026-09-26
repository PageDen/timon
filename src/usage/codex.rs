//! Codex `exec --json` usage adapter.
//!
//! The adapter turns one harness's event stream into [`UsageNormalizer`] calls.
//! The accounting rules stay in the parent module, so a future harness cannot
//! reintroduce double counting or zero-for-unknown on its own.
//!
//! # Unverified
//!
//! The event shape below is taken from the Codex CLI's documented
//! non-interactive output and from public issue reports, not from a run against
//! the pinned executables. Qualifying it is PR3's job. Three known limits are
//! already encoded here:
//!
//! - `turn.completed` reports `input_tokens`, `cached_input_tokens` and
//!   `output_tokens`. Reasoning tokens are tracked by the harness but are not
//!   in the stream (openai/codex#19022), so `reasoning_output` stays
//!   [`TokenCount::Unknown`] unless a future version adds the field.
//! - Startup prewarm usage is reported outside `turn.completed`
//!   (openai/codex#46975), so a complete-looking total can still understate
//!   what the provider billed. Timon reports what the stream says and labels it
//!   a report, never a measurement.
//! - `--json` can be ignored when tools or MCP servers are active
//!   (openai/codex#15451). That shows up here as a stream with no usage events,
//!   which yields [`UsageStatus::Unknown`](super::UsageStatus::Unknown) rather
//!   than a zero total.
//!
//! Whether the counts are per-turn or cumulative is the caller's declaration,
//! not a guess made here; see [`Accumulation`].

use super::{
    Accumulation, TokenCount, TokenUsage, TurnKey, UsageNormalizer, UsageNote, UsageReport,
};
use serde_json::Value;
use std::io::BufRead;

/// Longest event line the adapter will parse. Longer lines are skipped with a
/// note so a single runaway line cannot exhaust memory.
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Parses a Codex `exec --json` stream into a normalized usage report.
///
/// Unparsable lines, unknown event types and missing fields are recorded as
/// notes and do not abort the parse: a usable partial report is more honest
/// than an error that discards what the stream did say. An I/O failure while
/// reading is returned, because the report would silently be a fragment.
pub fn parse_stream<R: BufRead>(
    reader: R,
    accumulation: Accumulation,
) -> std::io::Result<UsageReport> {
    let mut normalizer = UsageNormalizer::new(accumulation);
    let mut thread_id = None;
    let mut turns = TurnCounter::default();

    for (index, line) in reader.lines().enumerate() {
        let line_number = u32::try_from(index + 1).unwrap_or(u32::MAX);
        let line = line?;
        if line.len() > MAX_EVENT_BYTES {
            normalizer.note(UsageNote::UnparsableEvent { line: line_number });
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(trimmed) else {
            normalizer.note(UsageNote::UnparsableEvent { line: line_number });
            continue;
        };
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            normalizer.note(UsageNote::UnparsableEvent { line: line_number });
            continue;
        };

        match kind {
            "thread.started" => {
                thread_id = event
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            "turn.started" => turns.start(),
            "turn.completed" => {
                let key = TurnKey::new(thread_id.clone(), turns.close());
                match event.get("usage").and_then(read_usage) {
                    Some(usage) => normalizer.turn_usage(key, usage),
                    None => normalizer.turn_usage_missing(key),
                }
            }
            "turn.failed" => {
                let key = TurnKey::new(thread_id.clone(), turns.close());
                normalizer.note(UsageNote::TurnFailed {
                    turn: key.clone(),
                    message: failure_message(&event),
                });
                match event.get("usage").and_then(read_usage) {
                    Some(usage) => normalizer.turn_usage(key, usage),
                    None => normalizer.turn_usage_missing(key),
                }
            }
            _ => {}
        }
    }

    if let Some(open) = turns.open_turn() {
        let key = TurnKey::new(thread_id, open);
        normalizer.note(UsageNote::StreamEndedMidTurn { turn: key.clone() });
        normalizer.turn_usage_missing(key);
    }

    Ok(normalizer.finish())
}

/// Reads a `usage` object.
///
/// Returns `None` when the object carries none of the known counts, so a
/// `turn.completed` with an empty or unrecognised usage object is reported as
/// missing usage rather than as zeros.
fn read_usage(value: &Value) -> Option<TokenUsage> {
    let usage = TokenUsage {
        input: count(value, "input_tokens"),
        cached_input: count(value, "cached_input_tokens"),
        output: count(value, "output_tokens"),
        // Not present in the stream today; accepted if a future version adds it.
        reasoning_output: count(value, "reasoning_output_tokens"),
    };
    if usage.input.is_known()
        || usage.cached_input.is_known()
        || usage.output.is_known()
        || usage.reasoning_output.is_known()
    {
        Some(usage)
    } else {
        None
    }
}

/// Reads one count. A missing, null, negative or non-numeric value is unknown,
/// never zero.
fn count(value: &Value, field: &str) -> TokenCount {
    match value.get(field).and_then(Value::as_i64) {
        Some(number) if number >= 0 => TokenCount::Known(number.unsigned_abs()),
        _ => TokenCount::Unknown,
    }
}

const UNREPORTED_FAILURE: &str = "turn failed without a message";
/// Longest failure message kept, so a hostile stream cannot grow the report.
const MAX_MESSAGE_BYTES: usize = 512;

fn failure_message(event: &Value) -> String {
    let raw = event
        .get("error")
        .and_then(|error| {
            error
                .as_str()
                .or_else(|| error.get("message").and_then(Value::as_str))
        })
        .or_else(|| event.get("message").and_then(Value::as_str))
        .unwrap_or(UNREPORTED_FAILURE);
    truncate(raw, MAX_MESSAGE_BYTES)
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Assigns each turn an ordinal index within the attempt.
///
/// The Codex stream has no turn id, so position is the only identity available.
/// A `turn.completed` without a matching `turn.started` still gets its own
/// index, so an unbalanced stream cannot make two turns share a key and have
/// the second discarded as a duplicate.
#[derive(Debug, Default)]
struct TurnCounter {
    last_index: u32,
    open: Option<u32>,
}

impl TurnCounter {
    fn start(&mut self) {
        self.last_index = self.last_index.saturating_add(1);
        self.open = Some(self.last_index);
    }

    fn close(&mut self) -> u32 {
        match self.open.take() {
            Some(index) => index,
            None => {
                self.last_index = self.last_index.saturating_add(1);
                self.last_index
            }
        }
    }

    fn open_turn(&self) -> Option<u32> {
        self.open
    }
}
