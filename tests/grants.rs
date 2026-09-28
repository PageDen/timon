//! Run authority, as distinct from caller identity.
//!
//! The broker already knows who is calling — the uid comes from the kernel. It
//! does not follow that it knows what a request may do: a developer's
//! interactive session and their pipeline workers share a uid. These tests are
//! about the difference, and about the honest limit of it.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use timon::broker::grant::{GRANT_HEADER, GrantError, Grants, MAX_LIFETIME_SECS};
use timon::broker::policy::ModelPolicy;
use timon::broker::select::Pool;
use timon::broker::serve::{Config, Counters, serve};
use timon::broker::store::Store;

const ALICE: u32 = 1000;
const BOB: u32 = 1001;
const NOW: i64 = 1_790_000_000;

#[test]
fn a_token_the_broker_did_not_issue_is_not_honoured() {
    // Which is also what a forged token looks like, and the reason forging is
    // not worth attempting: the broker recognises only what it minted.
    let grants = Grants::new();
    assert_eq!(
        grants.check("deadbeef", ALICE, NOW).unwrap_err(),
        GrantError::Unknown
    );
}

#[test]
fn a_grant_is_bound_to_the_principal_it_was_issued_to() {
    let mut grants = Grants::new();
    let (token, _) = grants
        .issue(ALICE, "run-1", vec![], None, 600, NOW)
        .unwrap();

    assert!(grants.check(&token, ALICE, NOW).is_ok());
    assert_eq!(
        grants.check(&token, BOB, NOW).unwrap_err(),
        GrantError::WrongPrincipal,
        "a token taken from another user stays bound to the account it was minted for"
    );
}

#[test]
fn a_grant_expires_and_says_so() {
    let mut grants = Grants::new();
    let (token, grant) = grants
        .issue(ALICE, "run-1", vec![], None, 600, NOW)
        .unwrap();
    assert_eq!(grant.expires_at, NOW + 600);
    assert!(grants.check(&token, ALICE, NOW + 599).is_ok());
    assert_eq!(
        grants.check(&token, ALICE, NOW + 600).unwrap_err(),
        GrantError::Expired
    );
}

#[test]
fn a_lifetime_is_capped_however_much_was_asked_for() {
    // A leaked token should stop working without anyone having to notice.
    let mut grants = Grants::new();
    let (_, grant) = grants
        .issue(ALICE, "run-1", vec![], None, 365 * 24 * 3600, NOW)
        .unwrap();
    assert_eq!(grant.expires_at, NOW + MAX_LIFETIME_SECS);
}

#[test]
fn cancelling_a_run_ends_its_authority() {
    let mut grants = Grants::new();
    let (token, _) = grants
        .issue(ALICE, "run-7", vec![], None, 3600, NOW)
        .unwrap();
    assert_eq!(grants.revoke_run("run-7", ALICE), 1);
    assert_eq!(
        grants.check(&token, ALICE, NOW + 1).unwrap_err(),
        GrantError::Revoked,
        "a caller still presenting it should learn the run was cancelled"
    );
}

#[test]
fn one_principal_cannot_revoke_another_s_run() {
    let mut grants = Grants::new();
    let (token, _) = grants
        .issue(ALICE, "run-7", vec![], None, 3600, NOW)
        .unwrap();
    assert_eq!(grants.revoke_run("run-7", BOB), 0);
    assert!(grants.check(&token, ALICE, NOW + 1).is_ok());
}

#[test]
fn expired_grants_are_forgotten_but_revoked_ones_survive_until_they_expire() {
    let mut grants = Grants::new();
    let (revoked, _) = grants
        .issue(ALICE, "run-a", vec![], None, 600, NOW)
        .unwrap();
    let (expired, _) = grants.issue(ALICE, "run-b", vec![], None, 10, NOW).unwrap();
    grants.revoke_run("run-a", ALICE);

    grants.forget_stale(NOW + 100);

    // The revoked one is kept so the answer stays "cancelled" rather than
    // degrading into "never existed", which sends an operator looking in the
    // wrong place.
    assert_eq!(
        grants.check(&revoked, ALICE, NOW + 100).unwrap_err(),
        GrantError::Revoked
    );
    assert_eq!(
        grants.check(&expired, ALICE, NOW + 100).unwrap_err(),
        GrantError::Unknown
    );
    assert_eq!(grants.live_count(NOW + 100), 0);
}

#[test]
fn two_grants_never_share_a_token() {
    let mut grants = Grants::new();
    let (one, _) = grants
        .issue(ALICE, "run-1", vec![], None, 600, NOW)
        .unwrap();
    let (two, _) = grants
        .issue(ALICE, "run-2", vec![], None, 600, NOW)
        .unwrap();
    assert_ne!(one, two);
    assert_eq!(one.len(), 64, "32 bytes of randomness, hex encoded");
    assert!(one.chars().all(|c| c.is_ascii_hexdigit()));
}

// --- through the listener ---

fn token_for(nick: &str) -> String {
    format!("eyJhbGciOiJub25lIn0.eyJleHAiOjQwMDAwMDAwMDB9.sig-{nick}")
}

fn account(root: &std::path::Path, name: &str) {
    let home = root.join(name);
    std::fs::create_dir_all(&home).unwrap();
    let body = serde_json::json!({
        "tokens": {
            "access_token": token_for(name),
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

fn started(root: &std::path::Path, serving: Vec<String>) -> (u16, Arc<Config>, Arc<Counters>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = listener.local_addr().unwrap();
    let config = Arc::new(Config {
        listen,
        upstream: "http://127.0.0.1:1".to_string(),
        store: Store::open(root).unwrap(),
        serving,
        pool: std::sync::Mutex::new(Pool::new()),
        models: ModelPolicy::default(),
        grants: std::sync::Mutex::new(Grants::new()),
        read_timeout: std::time::Duration::from_secs(10),
    });
    let counters = Arc::new(Counters::default());
    let running = Arc::clone(&config);
    let served = Arc::clone(&counters);
    std::thread::spawn(move || {
        let _ = serve(running, served, listener, Arc::new(|| false));
    });
    (listen.port(), config, counters)
}

fn post(port: u16, path: &str, body: &str, grant: Option<&str>) -> String {
    let header = grant
        .map(|g| format!("{GRANT_HEADER}: {g}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST {path} HTTP/1.1\r\nhost: 127.0.0.1\r\n{header}\
         content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    String::from_utf8_lossy(&response).to_string()
}

#[test]
fn a_grant_can_be_minted_and_then_used() {
    let root = tempfile::tempdir().unwrap();
    account(root.path(), "acct2");
    let (port, _config, counters) = started(root.path(), vec!["acct2".to_string()]);

    let minted = post(port, "/_timon/grant", r#"{"run_id":"run-1"}"#, None);
    assert!(minted.starts_with("HTTP/1.1 200 OK"), "{minted}");
    let body = minted.split("\r\n\r\n").nth(1).unwrap();
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    let token = json["grant"].as_str().unwrap();
    assert_eq!(json["header"], GRANT_HEADER);
    assert_eq!(counters.grants_issued.load(Ordering::Relaxed), 1);

    // Used on a real request: the upstream here is a dead port, so a 502 proves
    // the grant was accepted and the request went on to be forwarded.
    let used = post(
        port,
        "/responses",
        r#"{"model":"gpt-5.6-luna"}"#,
        Some(token),
    );
    // The upstream here is a dead port, so the request being attempted at all
    // is the proof: it got past the grant check and on to an account.
    assert!(used.contains("were tried for this request"), "{used}");
    assert!(!used.contains("grant"), "no grant complaint: {used}");
}

#[test]
fn a_request_presenting_an_unknown_grant_is_refused_before_anything_is_spent() {
    let root = tempfile::tempdir().unwrap();
    account(root.path(), "acct2");
    let (port, _config, counters) = started(root.path(), vec!["acct2".to_string()]);

    let refused = post(
        port,
        "/responses",
        r#"{"model":"gpt-5.6-luna"}"#,
        Some("not-a-real-token"),
    );
    assert!(refused.contains("403"), "{refused}");
    assert!(refused.contains("not one the broker issued"));
    assert_eq!(counters.grants_refused.load(Ordering::Relaxed), 1);
    assert_eq!(
        counters.requests_forwarded.load(Ordering::Relaxed),
        0,
        "a refused grant must stop the request before an account is chosen"
    );
}

#[test]
fn a_run_may_only_spend_the_accounts_it_was_granted() {
    let root = tempfile::tempdir().unwrap();
    account(root.path(), "acct2");
    account(root.path(), "acct3");
    let (port, _config, _counters) =
        started(root.path(), vec!["acct2".to_string(), "acct3".to_string()]);

    // A run granted an account that is not in this listener's pool has nothing
    // it may use, and the refusal says so rather than silently widening.
    let minted = post(
        port,
        "/_timon/grant",
        r#"{"run_id":"run-9","accounts":["acct-not-here"]}"#,
        None,
    );
    let body = minted.split("\r\n\r\n").nth(1).unwrap();
    let token = serde_json::from_str::<serde_json::Value>(body).unwrap()["grant"]
        .as_str()
        .unwrap()
        .to_string();

    let used = post(
        port,
        "/responses",
        r#"{"model":"gpt-5.6-luna"}"#,
        Some(&token),
    );
    assert!(used.contains("run-9"), "the refusal names the run: {used}");
    assert!(used.contains("acct-not-here"));
}

#[test]
fn revoking_a_run_stops_its_requests() {
    let root = tempfile::tempdir().unwrap();
    account(root.path(), "acct2");
    let (port, _config, _counters) = started(root.path(), vec!["acct2".to_string()]);

    let minted = post(port, "/_timon/grant", r#"{"run_id":"run-5"}"#, None);
    let body = minted.split("\r\n\r\n").nth(1).unwrap();
    let token = serde_json::from_str::<serde_json::Value>(body).unwrap()["grant"]
        .as_str()
        .unwrap()
        .to_string();

    let revoked = post(
        port,
        "/_timon/grant",
        r#"{"run_id":"run-5","revoke":true}"#,
        None,
    );
    assert!(revoked.contains("\"revoked\": 1"), "{revoked}");

    let after = post(
        port,
        "/responses",
        r#"{"model":"gpt-5.6-luna"}"#,
        Some(&token),
    );
    assert!(after.contains("403"));
    assert!(after.contains("cancelled"), "{after}");
}

#[test]
fn the_grant_never_reaches_the_provider() {
    // It is between the caller and this broker. Forwarding it would put a live
    // secret into somebody else's logs.
    assert!(
        timon::broker::serve::upstream_request(
            &timon::broker::serve::Request {
                method: "POST".to_string(),
                target: "/responses".to_string(),
                headers: vec![
                    (GRANT_HEADER.to_string(), "secret-token".to_string()),
                    ("accept".to_string(), "text/event-stream".to_string()),
                ],
                body: b"{}".to_vec(),
            },
            "/backend-api/codex",
            "chatgpt.com",
            "bearer-value",
            None,
        )
        .windows(12)
        .all(|w| w != b"secret-token"),
        "the grant header must be stripped"
    );
}
