//! Typed result tests.
//!
//! The cases that matter are the ones a process exit code cannot express: a
//! model that exited 0 and wrote nothing, wrote prose where a schema was
//! required, or satisfied every keyword Timon happens to implement while
//! ignoring one it does not.

use std::path::Path;
use timon::result::{ResultStatus, load};

fn write(dir: &Path, name: &str, contents: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).expect("writing a fixture cannot fail");
    path
}

#[test]
fn no_result_is_requested_when_no_path_is_given() {
    let status = load(None, None);

    assert_eq!(status, ResultStatus::NotRequested);
    assert!(!status.is_failure());
    assert!(!status.is_usable());
}

#[test]
fn a_result_the_model_never_wrote_is_missing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir.path().join("result.json");

    let status = load(Some(&missing), None);

    assert!(matches!(status, ResultStatus::Missing { .. }));
    assert!(status.is_failure());
}

#[test]
fn an_empty_result_is_not_a_result() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(dir.path(), "result.md", "   \n\t\n");

    let status = load(Some(&path), None);

    assert!(matches!(status, ResultStatus::Empty { .. }));
    assert!(status.is_failure());
}

#[test]
fn a_free_form_result_needs_no_schema_and_is_not_claimed_as_validated() {
    let dir = tempfile::tempdir().expect("temp dir");
    let contents = "The release notes mention two fixes.";
    let path = write(dir.path(), "result.md", contents);

    let status = load(Some(&path), None);

    match status {
        ResultStatus::Parsed {
            schema_validated,
            bytes,
            ..
        } => {
            assert!(!schema_validated);
            assert_eq!(bytes, contents.len() as u64);
        }
        other => panic!("expected a parsed result, got {other:?}"),
    }
}

#[test]
fn a_result_that_satisfies_its_schema_is_validated() {
    let dir = tempfile::tempdir().expect("temp dir");
    let schema = write(
        dir.path(),
        "schema.json",
        r#"{
          "type": "object",
          "required": ["summary", "claims"],
          "additionalProperties": false,
          "properties": {
            "summary": {"type": "string"},
            "claims": {
              "type": "array",
              "items": {
                "type": "object",
                "required": ["text", "kind"],
                "properties": {
                  "text": {"type": "string"},
                  "kind": {"enum": ["sourced", "inference"]}
                }
              }
            }
          }
        }"#,
    );
    let path = write(
        dir.path(),
        "result.json",
        r#"{"summary":"two fixes","claims":[{"text":"fix a","kind":"sourced"}]}"#,
    );

    let status = load(Some(&path), Some(&schema));

    match status {
        ResultStatus::Parsed {
            schema_validated, ..
        } => assert!(schema_validated),
        other => panic!("expected a validated result, got {other:?}"),
    }
}

#[test]
fn a_missing_required_property_is_a_violation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let schema = write(
        dir.path(),
        "schema.json",
        r#"{"type":"object","required":["summary"],"properties":{"summary":{"type":"string"}}}"#,
    );
    let path = write(dir.path(), "result.json", r#"{"other":"value"}"#);

    match load(Some(&path), Some(&schema)) {
        ResultStatus::SchemaViolation { reason, .. } => {
            assert!(reason.contains("summary"), "unexpected reason: {reason}");
        }
        other => panic!("expected a violation, got {other:?}"),
    }
}

#[test]
fn a_wrong_type_deep_in_the_document_reports_its_location() {
    let dir = tempfile::tempdir().expect("temp dir");
    let schema = write(
        dir.path(),
        "schema.json",
        r#"{"type":"object","properties":{"claims":{"type":"array","items":{"type":"object","properties":{"text":{"type":"string"}}}}}}"#,
    );
    let path = write(dir.path(), "result.json", r#"{"claims":[{"text":42}]}"#);

    match load(Some(&path), Some(&schema)) {
        ResultStatus::SchemaViolation { pointer, .. } => {
            assert_eq!(pointer, "/claims/0/text");
        }
        other => panic!("expected a violation, got {other:?}"),
    }
}

#[test]
fn an_unexpected_property_is_rejected_when_the_schema_closes_the_object() {
    let dir = tempfile::tempdir().expect("temp dir");
    let schema = write(
        dir.path(),
        "schema.json",
        r#"{"type":"object","additionalProperties":false,"properties":{"summary":{"type":"string"}}}"#,
    );
    let path = write(
        dir.path(),
        "result.json",
        r#"{"summary":"ok","extra":"sneaked in"}"#,
    );

    match load(Some(&path), Some(&schema)) {
        ResultStatus::SchemaViolation { reason, .. } => {
            assert!(reason.contains("extra"), "unexpected reason: {reason}");
        }
        other => panic!("expected a violation, got {other:?}"),
    }
}

#[test]
fn a_keyword_outside_the_supported_subset_fails_closed() {
    // minLength is not implemented. Skipping it would report this result as
    // schema-validated when the one constraint in the schema was never applied.
    let dir = tempfile::tempdir().expect("temp dir");
    let schema = write(
        dir.path(),
        "schema.json",
        r#"{"type":"object","properties":{"summary":{"type":"string","minLength":10}}}"#,
    );
    let path = write(dir.path(), "result.json", r#"{"summary":"short"}"#);

    match load(Some(&path), Some(&schema)) {
        ResultStatus::SchemaNotApplied {
            reason, pointer, ..
        } => {
            assert!(reason.contains("minLength"), "unexpected reason: {reason}");
            assert_eq!(pointer, "/summary");
        }
        other => panic!("expected the schema not to be applied, got {other:?}"),
    }
}

#[test]
fn annotation_keywords_do_not_block_validation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let schema = write(
        dir.path(),
        "schema.json",
        r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","title":"Result","description":"the model's answer","type":"object","properties":{"summary":{"type":"string"}}}"#,
    );
    let path = write(dir.path(), "result.json", r#"{"summary":"ok"}"#);

    match load(Some(&path), Some(&schema)) {
        ResultStatus::Parsed {
            schema_validated, ..
        } => assert!(schema_validated),
        other => panic!("expected a validated result, got {other:?}"),
    }
}

#[test]
fn prose_where_a_schema_was_required_is_not_json() {
    let dir = tempfile::tempdir().expect("temp dir");
    let schema = write(dir.path(), "schema.json", r#"{"type":"object"}"#);
    let path = write(dir.path(), "result.json", "Sure! Here is the summary:");

    assert!(matches!(
        load(Some(&path), Some(&schema)),
        ResultStatus::NotJson { .. }
    ));
}

#[test]
fn a_schema_that_cannot_be_read_leaves_the_result_unvalidated() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(dir.path(), "result.json", r#"{"summary":"ok"}"#);
    let schema = dir.path().join("absent.json");

    match load(Some(&path), Some(&schema)) {
        ResultStatus::SchemaNotApplied { reason, .. } => {
            assert!(reason.contains("unreadable"), "unexpected reason: {reason}");
        }
        other => panic!("expected the schema not to be applied, got {other:?}"),
    }
}

#[test]
fn an_integer_type_rejects_a_fractional_number() {
    let dir = tempfile::tempdir().expect("temp dir");
    let schema = write(
        dir.path(),
        "schema.json",
        r#"{"type":"object","properties":{"count":{"type":"integer"}}}"#,
    );
    let path = write(dir.path(), "result.json", r#"{"count":1.5}"#);

    assert!(matches!(
        load(Some(&path), Some(&schema)),
        ResultStatus::SchemaViolation { .. }
    ));
}

#[test]
fn a_union_type_accepts_either_member() {
    let dir = tempfile::tempdir().expect("temp dir");
    let schema = write(
        dir.path(),
        "schema.json",
        r#"{"type":"object","properties":{"note":{"type":["string","null"]}}}"#,
    );
    let present = write(dir.path(), "a.json", r#"{"note":"here"}"#);
    let absent = write(dir.path(), "b.json", r#"{"note":null}"#);

    assert!(load(Some(&present), Some(&schema)).is_usable());
    assert!(load(Some(&absent), Some(&schema)).is_usable());
}

#[test]
fn a_result_larger_than_the_limit_is_unreadable_rather_than_loaded() {
    let dir = tempfile::tempdir().expect("temp dir");
    let oversized = "x".repeat(timon::result::MAX_RESULT_BYTES as usize + 1);
    let path = write(dir.path(), "result.md", &oversized);

    match load(Some(&path), None) {
        ResultStatus::Unreadable { reason, .. } => {
            assert!(reason.contains("limit"), "unexpected reason: {reason}");
        }
        other => panic!("expected an unreadable result, got {other:?}"),
    }
}
