//! Which model serves a request, and whether anyone can tell.
//!
//! Enforcement is the easy half. A broker that silently rewrites `model` leaves
//! a developer reading output from a model they do not know they are using, so
//! most of these tests are about the announcement rather than the substitution.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use timon::broker::policy::{
    Decision, EFFECTIVE_HEADER, ModelPolicy, POLICY_HEADER, REQUESTED_HEADER, decide, headers,
    rewrite_model,
};
use timon::broker::select::Pool;
use timon::broker::serve::{Config, Counters, serve};
use timon::broker::store::Store;

const ALICE: u32 = 1000;
const NOW: i64 = 1_790_000_000;

fn policy(assign: &str) -> ModelPolicy {
    ModelPolicy {
        assign: Some(assign.to_string()),
        allowed: Vec::new(),
        version: "v1".to_string(),
    }
}

#[test]
fn without_a_policy_the_broker_does_not_interfere() {
    // The default. Installing this release must change nothing until somebody
    // decides it should.
    let none = ModelPolicy::default();
    assert!(none.absent());
    let decision = decide(&none, Some("gpt-5.5"), None);
    assert_eq!(
        decision,
        Decision::PassThrough {
            effective: Some("gpt-5.5".to_string())
        }
    );
    assert!(!decision.rewrites());
    assert!(
        headers(&none, &decision)
            .iter()
            .all(|(n, _)| n != POLICY_HEADER)
    );
}

#[test]
fn a_request_for_another_model_is_substituted_and_said_so() {
    let policy = policy("gpt-5.6-luna");
    let decision = decide(&policy, Some("gpt-5.5"), None);
    assert_eq!(
        decision,
        Decision::Substituted {
            requested: "gpt-5.5".to_string(),
            effective: "gpt-5.6-luna".to_string()
        }
    );
    assert!(decision.rewrites());

    let announced = headers(&policy, &decision);
    let find = |name: &str| {
        announced
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(find(EFFECTIVE_HEADER), Some("gpt-5.6-luna"));
    assert_eq!(
        find(REQUESTED_HEADER),
        Some("gpt-5.5"),
        "the client must be able to see that what it asked for was not used"
    );
    assert_eq!(find(POLICY_HEADER), Some("v1"));
}

#[test]
fn a_request_that_already_complies_passes_through_untouched() {
    let policy = policy("gpt-5.6-luna");
    let decision = decide(&policy, Some("gpt-5.6-luna"), None);
    assert!(!decision.rewrites(), "no rewrite when nothing changes");
    let announced = headers(&policy, &decision);
    assert!(
        announced.iter().all(|(n, _)| n != REQUESTED_HEADER),
        "nothing was overridden, so nothing is reported as overridden"
    );
}

#[test]
fn a_request_naming_no_model_is_assigned_one() {
    let decision = decide(&policy("gpt-5.6-luna"), None, None);
    assert_eq!(
        decision,
        Decision::Assigned {
            effective: "gpt-5.6-luna".to_string()
        }
    );
    assert!(decision.rewrites());
}

#[test]
fn an_explicitly_allowed_model_is_honoured() {
    // The case where a developer legitimately needs a specific model and the
    // operator agreed in advance.
    let mut policy = policy("gpt-5.6-luna");
    policy.allowed = vec!["gpt-5.5".to_string()];
    let decision = decide(&policy, Some("gpt-5.5"), None);
    assert_eq!(
        decision,
        Decision::PassThrough {
            effective: Some("gpt-5.5".to_string())
        }
    );
}

#[test]
fn a_conversation_keeps_the_model_it_started_with() {
    // A policy change applies to new conversations. Switching models between
    // turns of a live one is untested and may break its state.
    let policy = policy("gpt-5.6-terra");
    let decision = decide(&policy, Some("gpt-5.6-luna"), Some("gpt-5.6-luna"));
    assert!(
        !decision.rewrites(),
        "the bound model wins over a changed assignment: {decision:?}"
    );
    assert_eq!(decision.effective(), Some("gpt-5.6-luna"));

    // Even an allowed model does not move a conversation already under way.
    let mut permissive = policy.clone();
    permissive.allowed = vec!["gpt-5.5".to_string()];
    let decision = decide(&permissive, Some("gpt-5.5"), Some("gpt-5.6-luna"));
    assert_eq!(
        decision,
        Decision::Substituted {
            requested: "gpt-5.5".to_string(),
            effective: "gpt-5.6-luna".to_string()
        }
    );
}

#[test]
fn rewriting_a_body_changes_the_model_and_nothing_else() {
    let body = br#"{"model":"gpt-5.5","stream":true,"input":[{"type":"text"}],"store":false}"#;
    let rewritten = rewrite_model(body, "gpt-5.6-luna").expect("should rewrite");
    let value: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
    assert_eq!(value["model"], "gpt-5.6-luna");
    assert_eq!(value["stream"], true);
    assert_eq!(value["store"], false);
    assert!(value["input"].is_array());
}

#[test]
fn a_body_needing_no_change_is_forwarded_as_it_arrived() {
    // Re-serialising every body would quietly reorder fields in something this
    // claims to forward transparently.
    let body = br#"{"model":"gpt-5.6-luna","stream":true}"#;
    assert!(rewrite_model(body, "gpt-5.6-luna").is_none());
    assert!(rewrite_model(b"not json", "gpt-5.6-luna").is_none());
    assert!(rewrite_model(b"[1,2,3]", "gpt-5.6-luna").is_none());
}

// --- end to end, through the listener ---

fn token(nick: &str) -> String {
    format!("eyJhbGciOiJub25lIn0.eyJleHAiOjQwMDAwMDAwMDB9.sig-{nick}")
}

fn account(root: &std::path::Path, name: &str) {
    let home = root.join(name);
    std::fs::create_dir_all(&home).unwrap();
    let body = serde_json::json!({
        "tokens": {
            "access_token": token(name),
            "refresh_token": format!("refresh-{name}"),
            "account_id": format!("account-id-{name}"),
        },
        "last_refresh": "2026-09-28T00:00:00Z",
    });
    std::fs::write(home.join("auth.json"), body.to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(
            home.join("auth.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
}

/// An upstream that reports back which model it was asked for.
fn echoing_stub() -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut stream) = incoming else { continue };
            let record = Arc::clone(&record);
            std::thread::spawn(move || {
                use std::io::{BufRead, BufReader};
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                let model = serde_json::from_slice::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string))
                    .unwrap_or_else(|| "<none>".to_string());
                record.lock().unwrap().push(model);
                let payload = b"data: SERVED\n\n";
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        payload.len()
                    )
                    .as_bytes(),
                );
                let _ = stream.write_all(payload);
                let _ = stream.flush();
            });
        }
    });
    (port, seen)
}

fn ask(port: u16, thread: &str, model: &str) -> String {
    let body = format!(r#"{{"model":"{model}","stream":true}}"#);
    let request = format!(
        "POST /responses HTTP/1.1\r\nhost: 127.0.0.1\r\nthread-id: {thread}\r\n\
         content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    String::from_utf8_lossy(&response).to_string()
}

#[test]
fn the_client_is_told_on_the_response_which_model_actually_served_it() {
    let root = tempfile::tempdir().unwrap();
    account(root.path(), "acct2");
    let (upstream, seen) = echoing_stub();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = listener.local_addr().unwrap();
    let config = Arc::new(Config {
        listen,
        upstream: format!("http://127.0.0.1:{upstream}"),
        store: Store::open(root.path()).unwrap(),
        serving: vec!["acct2".to_string()],
        pool: std::sync::Mutex::new(Pool::new()),
        models: policy("gpt-5.6-luna"),
        grants: std::sync::Mutex::new(timon::broker::grant::Grants::new()),
        read_timeout: std::time::Duration::from_secs(20),
    });
    let counters = Arc::new(Counters::default());
    let running = Arc::clone(&config);
    let served = Arc::clone(&counters);
    std::thread::spawn(move || {
        let _ = serve(running, served, listener, Arc::new(|| false));
    });

    let response = ask(listen.port(), "thread-one", "gpt-5.5");

    assert!(response.starts_with("HTTP/1.1 200 OK"));
    let lower = response.to_lowercase();
    assert!(
        lower.contains(&format!("{EFFECTIVE_HEADER}: gpt-5.6-luna")),
        "the response must name the model that served: {response}"
    );
    assert!(
        lower.contains(&format!("{REQUESTED_HEADER}: gpt-5.5")),
        "and must say what was asked for instead"
    );
    assert!(response.contains("SERVED"), "the body still arrives");

    assert_eq!(
        *seen.lock().unwrap(),
        vec!["gpt-5.6-luna".to_string()],
        "the provider was asked for the assigned model, not the requested one"
    );
    assert_eq!(counters.model_substitutions.load(Ordering::Relaxed), 1);

    // The conversation is now bound, so a later turn stays put.
    let _ = ask(listen.port(), "thread-one", "gpt-5.5");
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["gpt-5.6-luna".to_string(), "gpt-5.6-luna".to_string()]
    );
    let _ = ALICE;
    let _ = NOW;
}
