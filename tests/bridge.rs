//! The app-server bridge (amendment A2).
//!
//! The contract is transparency: what the client sends reaches the server and
//! what the server sends reaches the client, unaltered. That is what makes this
//! safe to put in the path of somebody's editor, and it is what these tests
//! check first. Recording is the secondary job, and it is never allowed to cost
//! a byte.

use serde_json::json;
use timon::bridge::appserver::{Observer, event_id, looks_like_shared_daemon};

fn usage_line(thread: &str, turn: &str, last_total: u64, thread_total: u64) -> String {
    json!({
        "method": "thread/tokenUsage/updated",
        "params": {
            "threadId": thread,
            "turnId": turn,
            "tokenUsage": {
                "last": {
                    "inputTokens": last_total - 5,
                    "cachedInputTokens": 0,
                    "outputTokens": 5,
                    "reasoningOutputTokens": 0,
                    "totalTokens": last_total
                },
                "total": {
                    "inputTokens": thread_total - 5,
                    "cachedInputTokens": 0,
                    "outputTokens": 5,
                    "reasoningOutputTokens": 0,
                    "totalTokens": thread_total
                }
            }
        }
    })
    .to_string()
}

#[test]
fn a_usage_notification_is_recognised_and_its_turn_figure_taken() {
    let mut observer = Observer::new();
    let seen = observer
        .observe(&usage_line("th_1", "turn_1", 100, 100))
        .expect("the usage notification should be observed");

    assert_eq!(seen.thread_id, "th_1");
    assert_eq!(seen.turn_id, "turn_1");
    // `last`, not `total`: the turn's own cost is what a per-attempt row means.
    assert_eq!(seen.usage.total().value(), Some(100));
    assert_eq!(seen.thread_total, Some(100));
    assert!(!seen.disagrees_with_total);
}

#[test]
fn reasoning_tokens_are_carried_because_this_protocol_reports_them() {
    // `codex exec --json` omits them (openai/codex#19022), so a Desktop session
    // reports a more complete figure than a supervised run does. Losing them
    // here would throw that away.
    let mut observer = Observer::new();
    let line = json!({
        "method": "thread/tokenUsage/updated",
        "params": {
            "threadId": "th", "turnId": "t1",
            "tokenUsage": {
                "last": {"inputTokens": 10, "cachedInputTokens": 2, "outputTokens": 3,
                         "reasoningOutputTokens": 7, "totalTokens": 20},
                "total": {"inputTokens": 10, "cachedInputTokens": 2, "outputTokens": 3,
                          "reasoningOutputTokens": 7, "totalTokens": 20}
            }
        }
    })
    .to_string();
    let seen = observer.observe(&line).unwrap();
    assert_eq!(seen.usage.reasoning_output.value(), Some(7));
    assert_eq!(seen.usage.cached_input.value(), Some(2));
}

#[test]
fn every_other_method_is_ignored_rather_than_interpreted() {
    let mut observer = Observer::new();
    for method in [
        "thread/started",
        "turn/completed",
        "item/completed",
        "fs/changed",
        "account/rateLimits/updated",
        "command/exec/outputDelta",
    ] {
        let line = json!({"method": method, "params": {"threadId": "th"}}).to_string();
        assert!(
            observer.observe(&line).is_none(),
            "{method} must not be treated as usage"
        );
    }
    assert_eq!(observer.turns_seen(), 0);
}

#[test]
fn a_replayed_notification_is_not_counted_twice() {
    let mut observer = Observer::new();
    let line = usage_line("th_1", "turn_1", 100, 100);
    assert!(observer.observe(&line).is_some());
    assert!(
        observer.observe(&line).is_none(),
        "the same turn must not be recorded twice"
    );
    assert_eq!(observer.turns_seen(), 1);
}

#[test]
fn the_event_id_is_derived_so_a_restart_collapses_onto_the_stored_row() {
    assert_eq!(event_id("th_1", "turn_2"), "appserver:th_1:turn_2");
    assert_ne!(event_id("th_1", "turn_2"), event_id("th_2", "turn_2"));
}

#[test]
fn drift_from_the_servers_own_thread_total_is_reported_not_corrected() {
    // Two turns of 100 each, but the server says the thread total is 250. The
    // protocol may compact a thread or count differently; the point is to
    // surface the disagreement rather than pick a winner.
    let mut observer = Observer::new();
    let first = observer.observe(&usage_line("th", "t1", 100, 100)).unwrap();
    assert!(!first.disagrees_with_total);

    let second = observer.observe(&usage_line("th", "t2", 100, 250)).unwrap();
    assert!(
        second.disagrees_with_total,
        "200 recorded against 250 reported"
    );
    // Still recorded, at its own figure.
    assert_eq!(second.usage.total().value(), Some(100));
}

#[test]
fn threads_are_tracked_separately() {
    let mut observer = Observer::new();
    observer
        .observe(&usage_line("th_a", "t1", 100, 100))
        .unwrap();
    let other = observer.observe(&usage_line("th_b", "t1", 50, 50)).unwrap();
    assert!(
        !other.disagrees_with_total,
        "one thread's running total must not be charged against another's"
    );
}

#[test]
fn a_notification_whose_shape_changed_is_counted_not_fatal() {
    // The protocol is marked experimental upstream. A field that moves or
    // disappears must degrade to "not recorded", never to a broken editor.
    let mut observer = Observer::new();
    let line = json!({
        "method": "thread/tokenUsage/updated",
        "params": {"threadId": "th", "turnId": "t1", "tokenUsage": {"somethingNew": 1}}
    })
    .to_string();
    assert!(observer.observe(&line).is_none());
    assert_eq!(observer.malformed, 1, "counted so an operator can see it");
}

#[test]
fn unparsable_and_unrelated_lines_are_passed_over_in_silence() {
    let mut observer = Observer::new();
    for line in [
        "",
        "   ",
        "not json at all",
        "{ truncated",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}",
        "\u{fffd}\u{fffd}binary-ish",
    ] {
        assert!(observer.observe(line).is_none(), "{line:?}");
    }
    assert_eq!(
        observer.malformed, 0,
        "only something claiming to be the usage notification counts as malformed"
    );
}

#[test]
fn a_turn_reporting_no_figures_is_unknown_usage_not_zero() {
    // Folding an unreported figure in as zero would make a total read as
    // complete when part of it is missing, which the rest of the project
    // refuses to do.
    let mut observer = Observer::new();
    let line = json!({
        "method": "thread/tokenUsage/updated",
        "params": {"threadId": "th", "turnId": "t1",
                   "tokenUsage": {"last": {}, "total": {}}}
    })
    .to_string();
    let seen = observer.observe(&line).unwrap();
    assert_eq!(seen.usage.total().value(), None, "unknown, not zero");
    assert!(matches!(seen.status, timon::usage::UsageStatus::Unknown));
}

#[test]
fn a_shared_daemon_command_is_recognised_and_refused() {
    // A shared daemon serves several accounts from one process, so every session
    // would carry the daemon's uid. A report naming the wrong person confidently
    // is worse than no report.
    let os = |s: &str| std::ffi::OsString::from(s);
    assert!(looks_like_shared_daemon(&[
        os("codex"),
        os("app-server"),
        os("daemon")
    ]));
    assert!(looks_like_shared_daemon(&[
        os("codex"),
        os("app-server"),
        os("proxy")
    ]));
    assert!(looks_like_shared_daemon(&[
        os("codex"),
        os("app-server"),
        os("--code-mode-host=https://example.test")
    ]));
    assert!(!looks_like_shared_daemon(&[os("codex"), os("app-server")]));
}

// ---------------------------------------------------------------------------
// Transparency, through the real binary
// ---------------------------------------------------------------------------

/// A stand-in app-server: echoes what it was sent, then emits a scripted stream
/// containing the awkward cases — CRLF, a very long line, invalid UTF-8, a usage
/// notification, and a final line with no terminating newline.
fn fake_server(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("fake-app-server.sh");
    std::fs::write(
        &path,
        r#"#!/usr/bin/env bash
# Read whatever the client sends, so the client->server direction is exercised.
cat > "$SEEN"
printf '{"id":1,"result":{"userAgent":"fake"}}\n'
printf '{"method":"thread/started","params":{"threadId":"th_1"}}\r\n'
printf '{"method":"thread/tokenUsage/updated","params":{"threadId":"th_1","turnId":"t1","tokenUsage":{"last":{"inputTokens":95,"cachedInputTokens":0,"outputTokens":5,"reasoningOutputTokens":0,"totalTokens":100},"total":{"inputTokens":95,"cachedInputTokens":0,"outputTokens":5,"reasoningOutputTokens":0,"totalTokens":100}}}}\n'
printf '{"method":"item/agentMessage/delta","params":{"text":"%s"}}\n' "$(head -c 4000 /dev/zero | tr '\0' 'x')"
printf 'not json at all\n'
printf '\xff\xfe binary bytes\n'
printf '{"method":"turn/completed","params":{"turnId":"t1"}}'
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// The exact bytes the fake server writes, independent of the proxy.
fn expected_bytes(script: &std::path::Path, seen: &std::path::Path) -> Vec<u8> {
    let out = std::process::Command::new(script)
        .env("SEEN", seen)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("the fake server should run");
    out.stdout
}

#[test]
fn what_the_server_says_reaches_the_client_byte_for_byte() {
    // The whole basis for putting this in the path of an editor. Checked against
    // the fake server's own output rather than a hand-written expectation, so the
    // test cannot drift from what the script actually emits.
    let dir = tempfile::tempdir().unwrap();
    let script = fake_server(dir.path());

    let direct = expected_bytes(&script, &dir.path().join("seen-direct"));

    let seen = dir.path().join("seen-proxied");
    let proxied = std::process::Command::new(env!("CARGO_BIN_EXE_timon"))
        .args(["bridge", "--"])
        .arg(&script)
        .env("SEEN", &seen)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("the bridge should run");

    assert!(
        proxied.status.success(),
        "bridge failed: {}",
        String::from_utf8_lossy(&proxied.stderr)
    );
    assert_eq!(
        proxied.stdout, direct,
        "the proxied stream must be identical to the server's own output"
    );
    assert!(
        !direct.is_empty() && !direct.ends_with(b"\n"),
        "the fixture must end without a newline, or it is not testing that case"
    );
}

#[test]
fn what_the_client_sends_reaches_the_server_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_server(dir.path());
    let seen = dir.path().join("seen");

    // Includes a line the observer would otherwise be tempted to parse, an
    // unterminated final line, and bytes that are not valid UTF-8.
    let mut sent: Vec<u8> = Vec::new();
    sent.extend_from_slice(br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    sent.push(b'\n');
    sent.extend_from_slice(b"\xff\xfe not utf-8\n");
    sent.extend_from_slice(br#"{"method":"thread/tokenUsage/updated","params":{}}"#);
    sent.push(b'\n');
    sent.extend_from_slice(b"trailing line with no newline");

    use std::io::Write;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_timon"))
        .args(["bridge", "--"])
        .arg(&script)
        .env("SEEN", &seen)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(&sent).unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());

    let arrived = std::fs::read(&seen).expect("the server should have recorded what it saw");
    assert_eq!(
        arrived, sent,
        "the server must receive exactly what the client sent"
    );
}

#[test]
fn the_report_counts_what_passed_through_and_what_was_observed() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_server(dir.path());
    let seen = dir.path().join("seen");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_timon"))
        .args(["bridge", "--report", "--"])
        .arg(&script)
        .env("SEEN", &seen)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();

    let report: serde_json::Value =
        serde_json::from_slice(&out.stderr).expect("the report goes to stderr as JSON");
    assert_eq!(report["turns_observed"], 1);
    assert_eq!(report["malformed_usage_notifications"], 0);
    assert_eq!(report["exit_code"], 0);
    assert!(report["bytes_server_to_client"].as_u64().unwrap() > 4000);
    // Nothing recorded: no socket was given, which is the mode that proves the
    // pass-through works without a daemon.
    assert_eq!(report["events_recorded"], 0);
}

#[test]
fn the_report_goes_to_stderr_so_stdout_stays_protocol_only() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_server(dir.path());
    let direct = expected_bytes(&script, &dir.path().join("seen-direct"));

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_timon"))
        .args(["bridge", "--report", "--"])
        .arg(&script)
        .env("SEEN", dir.path().join("seen"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();

    assert_eq!(
        out.stdout, direct,
        "asking for a report must not put a byte on stdout"
    );
}

#[test]
fn a_shared_daemon_invocation_is_refused_before_anything_starts() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_timon"))
        .args(["bridge", "--", "codex", "app-server", "daemon"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("shared app-server daemon"), "got: {stderr}");
}

#[test]
fn the_bridge_exits_when_its_child_does_even_if_the_client_holds_stdin_open() {
    // Found in production, not in testing. `codex-timon --version` hung: the
    // child printed its version and exited, but the bridge was waiting for the
    // stdin pump to finish, and stdin does not reach end of file while the client
    // still holds its end. An editor's first health check is exactly this shape,
    // so it would have hung on contact.
    //
    // Reaching the point where the child is reaped means its output is already at
    // end of file, so nothing is lost by abandoning the stdin reader.
    use std::io::Write;

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_timon"))
        .args(["bridge", "--", "/bin/echo", "done"])
        // Piped and deliberately never closed, standing in for a client that
        // stays alive after the process it spawned has exited.
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // Something written but not closed, so the pump is mid-stream rather than
    // merely idle.
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(b"{}\n");
    let _ = stdin.flush();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    // `stdin` is still open here on purpose: dropping it earlier would close the
    // pipe and let the old, broken code pass.
    drop(stdin);

    assert!(
        status.is_some(),
        "the bridge must exit once its child has, without waiting on a stdin that never closes"
    );
    assert!(status.unwrap().success());
}
