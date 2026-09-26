//! Usage normalization tests.
//!
//! The event streams here are fixtures, not recordings from a qualified
//! harness. They pin the accounting rules — unknown is not zero, subsets are
//! not added twice, one authority per attempt — so a later adapter change
//! cannot quietly break them.

use timon::usage::codex::parse_stream;
use timon::usage::{
    Accumulation, TokenCount, TokenUsage, TurnKey, UsageAuthority, UsageNormalizer, UsageNote,
    UsageReport, UsageStatus,
};

fn parse(stream: &str, accumulation: Accumulation) -> UsageReport {
    parse_stream(stream.as_bytes(), accumulation).expect("reading a fixture cannot fail")
}

fn turn(input: u64, cached: u64, output: u64) -> String {
    format!(
        r#"{{"type":"turn.completed","usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output}}}}}"#
    )
}

const THREAD: &str = r#"{"type":"thread.started","thread_id":"t-1"}"#;
const STARTED: &str = r#"{"type":"turn.started"}"#;

#[test]
fn sums_each_turn_when_the_stream_reports_deltas() {
    let stream = format!(
        "{THREAD}\n{STARTED}\n{}\n{STARTED}\n{}\n",
        turn(100, 20, 10),
        turn(200, 50, 30)
    );
    let report = parse(&stream, Accumulation::PerTurnDelta);

    assert_eq!(report.usage.input, TokenCount::Known(300));
    assert_eq!(report.usage.cached_input, TokenCount::Known(70));
    assert_eq!(report.usage.output, TokenCount::Known(40));
    assert_eq!(report.status, UsageStatus::Complete);
    assert_eq!(report.authority, UsageAuthority::Turns);
    assert_eq!(report.turns_seen, 2);
    assert_eq!(report.turns_with_usage, 2);
}

#[test]
fn keeps_the_last_total_when_the_stream_reports_snapshots() {
    // The same stream read as cumulative must not be summed.
    let stream = format!(
        "{THREAD}\n{STARTED}\n{}\n{STARTED}\n{}\n",
        turn(100, 20, 10),
        turn(300, 70, 40)
    );
    let report = parse(&stream, Accumulation::CumulativeSnapshot);

    assert_eq!(report.usage.input, TokenCount::Known(300));
    assert_eq!(report.usage.cached_input, TokenCount::Known(70));
    assert_eq!(report.usage.output, TokenCount::Known(40));
    assert_eq!(report.status, UsageStatus::Complete);
}

#[test]
fn notes_a_snapshot_that_reports_fewer_tokens_than_an_earlier_one() {
    // A delta stream read as cumulative looks like this, so the note is the
    // signal that the declared accumulation is wrong.
    let stream = format!(
        "{THREAD}\n{STARTED}\n{}\n{STARTED}\n{}\n",
        turn(300, 70, 40),
        turn(100, 20, 10)
    );
    let report = parse(&stream, Accumulation::CumulativeSnapshot);

    assert!(
        report
            .notes
            .iter()
            .any(|note| matches!(note, UsageNote::SnapshotWentBackwards { .. })),
        "expected a regression note, got {:?}",
        report.notes
    );
}

#[test]
fn cached_input_and_reasoning_are_subsets_and_are_never_added_twice() {
    let stream = format!(
        r#"{THREAD}
{STARTED}
{{"type":"turn.completed","usage":{{"input_tokens":1000,"cached_input_tokens":900,"output_tokens":100,"reasoning_output_tokens":80}}}}
"#
    );
    let report = parse(&stream, Accumulation::PerTurnDelta);

    // input + output only: 1000 + 100.
    assert_eq!(report.total, TokenCount::Known(1100));
    // Input that was not served from cache.
    assert_eq!(report.billable_input, TokenCount::Known(100));
    assert_eq!(report.usage.reasoning_output, TokenCount::Known(80));
}

#[test]
fn a_turn_without_usage_makes_the_total_a_lower_bound() {
    let stream = format!(
        "{THREAD}\n{STARTED}\n{}\n{STARTED}\n{{\"type\":\"turn.completed\"}}\n",
        turn(100, 0, 10)
    );
    let report = parse(&stream, Accumulation::PerTurnDelta);

    assert_eq!(report.usage.input, TokenCount::Known(100));
    assert_eq!(report.status, UsageStatus::Partial);
    assert_eq!(report.turns_seen, 2);
    assert_eq!(report.turns_with_usage, 1);
    assert!(
        report
            .notes
            .iter()
            .any(|note| matches!(note, UsageNote::TurnUsageMissing { .. }))
    );
}

#[test]
fn a_stream_with_no_usage_is_unknown_and_not_zero() {
    // What a run looks like when the harness ignores --json.
    let stream = format!("{THREAD}\n{STARTED}\n{{\"type\":\"item.completed\"}}\n");
    let report = parse(&stream, Accumulation::PerTurnDelta);

    assert_eq!(report.status, UsageStatus::Unknown);
    assert_eq!(report.authority, UsageAuthority::None);
    assert_eq!(report.usage.input, TokenCount::Unknown);
    assert_eq!(report.total, TokenCount::Unknown);
    assert_eq!(report.usage.input.value(), None);
}

#[test]
fn an_unknown_count_serializes_as_null_not_zero() {
    let stream = format!("{THREAD}\n{STARTED}\n{{\"type\":\"turn.completed\"}}\n");
    let report = parse(&stream, Accumulation::PerTurnDelta);
    let json = serde_json::to_value(&report).expect("report serializes");

    assert_eq!(json["usage"]["input"], serde_json::Value::Null);
    assert_eq!(json["total"], serde_json::Value::Null);
    assert_eq!(json["status"], "unknown");
}

#[test]
fn a_negative_or_non_numeric_count_is_unknown() {
    let stream = format!(
        r#"{THREAD}
{STARTED}
{{"type":"turn.completed","usage":{{"input_tokens":-5,"cached_input_tokens":"many","output_tokens":10}}}}
"#
    );
    let report = parse(&stream, Accumulation::PerTurnDelta);

    assert_eq!(report.usage.input, TokenCount::Unknown);
    assert_eq!(report.usage.cached_input, TokenCount::Unknown);
    assert_eq!(report.usage.output, TokenCount::Known(10));
    // An unknown input with a known output is still only a lower bound.
    assert_eq!(report.status, UsageStatus::Partial);
}

#[test]
fn an_unparsable_line_is_noted_and_the_rest_of_the_stream_still_counts() {
    let stream = format!(
        "{THREAD}\nnot json at all\n{STARTED}\n{}\n{{\"missing\":\"type\"}}\n",
        turn(100, 0, 10)
    );
    let report = parse(&stream, Accumulation::PerTurnDelta);

    assert_eq!(report.usage.input, TokenCount::Known(100));
    let unparsable = report
        .notes
        .iter()
        .filter(|note| matches!(note, UsageNote::UnparsableEvent { .. }))
        .count();
    assert_eq!(unparsable, 2);
}

#[test]
fn a_failed_turn_still_contributes_the_tokens_it_reported() {
    let stream = format!(
        r#"{THREAD}
{STARTED}
{{"type":"turn.failed","error":{{"message":"stream disconnected"}},"usage":{{"input_tokens":40,"cached_input_tokens":0,"output_tokens":5}}}}
"#
    );
    let report = parse(&stream, Accumulation::PerTurnDelta);

    assert_eq!(report.usage.input, TokenCount::Known(40));
    assert!(report.notes.iter().any(
        |note| matches!(note, UsageNote::TurnFailed { message, .. } if message == "stream disconnected")
    ));
}

#[test]
fn a_turn_left_open_by_a_truncated_stream_is_reported_as_missing() {
    let stream = format!("{THREAD}\n{STARTED}\n{}\n{STARTED}\n", turn(100, 0, 10));
    let report = parse(&stream, Accumulation::PerTurnDelta);

    assert!(
        report
            .notes
            .iter()
            .any(|note| matches!(note, UsageNote::StreamEndedMidTurn { .. }))
    );
    assert_eq!(report.status, UsageStatus::Partial);
    assert_eq!(report.usage.input, TokenCount::Known(100));
}

#[test]
fn a_subset_larger_than_its_superset_is_noted() {
    let stream = format!("{THREAD}\n{STARTED}\n{}\n", turn(10, 99, 5));
    let report = parse(&stream, Accumulation::PerTurnDelta);

    assert!(
        report
            .notes
            .iter()
            .any(|note| matches!(note, UsageNote::InconsistentSubset { .. }))
    );
    // Reported as received; billable input cannot go negative.
    assert_eq!(report.billable_input, TokenCount::Known(0));
}

#[test]
fn two_turn_completions_without_a_start_between_them_are_separate_turns() {
    // The stream carries no turn id, so position is the only identity. Two
    // completions must not collapse into one key and lose the second turn.
    let stream = format!("{THREAD}\n{}\n{}\n", turn(100, 0, 10), turn(200, 0, 20));
    let report = parse(&stream, Accumulation::PerTurnDelta);

    assert_eq!(report.turns_seen, 2);
    assert_eq!(report.usage.input, TokenCount::Known(300));
    assert_eq!(report.duplicate_turns_ignored, 0);
}

#[test]
fn a_repeated_turn_key_is_counted_once() {
    let mut normalizer = UsageNormalizer::new(Accumulation::PerTurnDelta);
    let key = TurnKey::new(Some("t-1".to_owned()), 1);
    let usage = TokenUsage {
        input: TokenCount::Known(100),
        cached_input: TokenCount::Known(0),
        output: TokenCount::Known(10),
        reasoning_output: TokenCount::Unknown,
    };
    normalizer.turn_usage(key.clone(), usage);
    normalizer.turn_usage(key, usage);
    let report = normalizer.finish();

    assert_eq!(report.usage.input, TokenCount::Known(100));
    assert_eq!(report.duplicate_turns_ignored, 1);
    assert_eq!(report.turns_with_usage, 1);
}

#[test]
fn an_attempt_total_replaces_the_per_turn_sum_instead_of_adding_to_it() {
    let mut normalizer = UsageNormalizer::new(Accumulation::PerTurnDelta);
    normalizer.turn_usage(
        TurnKey::new(None, 1),
        TokenUsage {
            input: TokenCount::Known(100),
            cached_input: TokenCount::Known(0),
            output: TokenCount::Known(10),
            reasoning_output: TokenCount::Unknown,
        },
    );
    normalizer.attempt_total(TokenUsage {
        input: TokenCount::Known(100),
        cached_input: TokenCount::Known(0),
        output: TokenCount::Known(10),
        reasoning_output: TokenCount::Unknown,
    });
    let report = normalizer.finish();

    assert_eq!(report.usage.input, TokenCount::Known(100));
    assert_eq!(report.authority, UsageAuthority::AttemptTotal);
    assert!(
        report
            .notes
            .iter()
            .any(|note| matches!(note, UsageNote::AttemptTotalPreferred))
    );
}

#[test]
fn notes_stop_growing_at_the_cap() {
    let mut normalizer = UsageNormalizer::new(Accumulation::PerTurnDelta);
    for line in 0..(timon::usage::MAX_NOTES as u32 + 20) {
        normalizer.note(UsageNote::UnparsableEvent { line });
    }
    let report = normalizer.finish();

    assert_eq!(report.notes.len(), timon::usage::MAX_NOTES + 1);
    assert_eq!(report.notes.last(), Some(&UsageNote::NotesTruncated));
}

#[test]
fn an_empty_stream_reports_unknown_usage() {
    let report = parse("", Accumulation::PerTurnDelta);

    assert_eq!(report.status, UsageStatus::Unknown);
    assert_eq!(report.turns_seen, 0);
}
