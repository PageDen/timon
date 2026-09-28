//! The pooled account store (amendment A4, slice 1).
//!
//! One invariant matters more than the rest and most of these tests are about it:
//! **a credential value never leaves the store.** The types carry no token, so
//! there should be nothing to leak — and these tests assert that by looking for
//! the token's own text in every rendering, rather than by trusting the design.

use serde_json::json;
use timon::broker::store::{Inventory, Store, render};

/// Distinctive strings, so finding one in any output is unambiguous.
const ACCESS: &str = "ACCESS-TOKEN-MUST-NEVER-APPEAR-a1b2c3";
const REFRESH: &str = "REFRESH-TOKEN-MUST-NEVER-APPEAR-d4e5f6";
const ID_TOKEN: &str = "ID-TOKEN-MUST-NEVER-APPEAR-g7h8i9";
const API_KEY: &str = "sk-API-KEY-MUST-NEVER-APPEAR-j1k2l3";

fn account(root: &std::path::Path, name: &str, body: serde_json::Value) -> std::path::PathBuf {
    let home = root.join(name);
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("auth.json");
    std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).unwrap();
    restrict(&home, 0o700);
    restrict(&path, 0o600);
    home
}

fn restrict(path: &std::path::Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
}

fn chatgpt(id: &str) -> serde_json::Value {
    json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": ID_TOKEN,
            "access_token": ACCESS,
            "refresh_token": REFRESH,
            "account_id": id
        },
        "last_refresh": "2026-09-27T12:00:00.000000Z"
    })
}

#[test]
fn a_well_formed_store_lists_its_accounts() {
    let dir = tempfile::tempdir().unwrap();
    account(dir.path(), "alpha", chatgpt("acct-alpha-0000000000"));
    account(dir.path(), "beta", chatgpt("acct-beta-11111111111"));

    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();

    assert_eq!(inventory.accounts.len(), 2);
    assert_eq!(inventory.usable, 2);
    assert!(
        inventory.rotation_possible,
        "two usable accounts can rotate"
    );
    // Alphabetical, so output is stable between runs.
    assert_eq!(inventory.accounts[0].name, "alpha");
    assert_eq!(inventory.accounts[1].name, "beta");
    assert_eq!(inventory.accounts[0].auth_mode.as_deref(), Some("chatgpt"));
    assert!(inventory.accounts[0].refreshable);
}

#[test]
fn no_credential_value_appears_in_any_rendering() {
    // The invariant. Asserted against the token's own text rather than inferred
    // from the types, because a future field could reintroduce one silently.
    let dir = tempfile::tempdir().unwrap();
    account(dir.path(), "alpha", chatgpt("acct-alpha-0000000000"));
    account(
        dir.path(),
        "keyed",
        json!({"auth_mode": "apikey", "OPENAI_API_KEY": API_KEY, "tokens": null}),
    );

    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    let text = render(&inventory);
    let as_json = serde_json::to_string(&inventory).unwrap();
    let debug = format!("{inventory:?}");

    for secret in [ACCESS, REFRESH, ID_TOKEN, API_KEY] {
        for (what, rendered) in [("text", &text), ("json", &as_json), ("debug", &debug)] {
            assert!(
                !rendered.contains(secret),
                "{what} rendering leaked a credential value"
            );
        }
    }
}

#[test]
fn the_account_identifier_is_shortened_rather_than_echoed() {
    let dir = tempfile::tempdir().unwrap();
    let full = "acct-alpha-0000000000";
    account(dir.path(), "alpha", chatgpt(full));

    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    let prefix = inventory.accounts[0].account_id_prefix.clone().unwrap();

    assert!(prefix.starts_with("acct-alp"), "got {prefix}");
    assert!(prefix.ends_with('…'));
    assert!(
        !render(&inventory).contains(full),
        "the whole identifier must not be printed"
    );
}

#[test]
fn one_broken_account_does_not_hide_the_others() {
    let dir = tempfile::tempdir().unwrap();
    account(dir.path(), "good", chatgpt("acct-good-000000000000"));
    // Present but unreadable as JSON.
    let broken = dir.path().join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("auth.json"), "{ not json").unwrap();
    restrict(&broken, 0o700);
    restrict(&broken.join("auth.json"), 0o600);
    // Present but never logged in.
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    restrict(&empty, 0o700);

    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();

    assert_eq!(
        inventory.accounts.len(),
        3,
        "the broken ones are listed too"
    );
    assert_eq!(inventory.usable, 1);
    assert!(
        !inventory.rotation_possible,
        "one usable account cannot rotate"
    );

    let by_name = |n: &str| {
        inventory
            .accounts
            .iter()
            .find(|a| a.name == n)
            .unwrap()
            .clone()
    };
    assert!(by_name("good").usable());
    assert!(
        by_name("broken")
            .faults
            .iter()
            .any(|f| f.contains("not valid JSON"))
    );
    assert!(
        by_name("empty")
            .faults
            .iter()
            .any(|f| f.contains("never been logged in"))
    );
}

#[test]
fn a_credential_others_can_read_is_a_fault() {
    // The reason the store exists is that a user cannot read what authenticates
    // their request. A group-readable file undoes that silently, so it is a fault
    // rather than a note.
    let dir = tempfile::tempdir().unwrap();
    let home = account(dir.path(), "leaky", chatgpt("acct-leaky-00000000000"));
    restrict(&home.join("auth.json"), 0o644);

    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    let leaky = &inventory.accounts[0];

    assert!(!leaky.usable());
    assert!(
        leaky
            .faults
            .iter()
            .any(|f| f.contains("readable beyond its owner")),
        "got {:?}",
        leaky.faults
    );
    assert_eq!(inventory.usable, 0);
}

#[test]
fn a_world_readable_account_directory_is_a_fault_too() {
    let dir = tempfile::tempdir().unwrap();
    let home = account(dir.path(), "open", chatgpt("acct-open-0000000000"));
    restrict(&home, 0o755);

    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    assert!(
        inventory.accounts[0]
            .faults
            .iter()
            .any(|f| f.contains("directory is readable beyond its owner")),
        "got {:?}",
        inventory.accounts[0].faults
    );
}

#[test]
fn a_chatgpt_account_without_a_refresh_token_is_a_fault() {
    // It works until the access token expires and then fails in the middle of
    // somebody's session, which is the worst time to find out.
    let dir = tempfile::tempdir().unwrap();
    account(
        dir.path(),
        "stale",
        json!({
            "auth_mode": "chatgpt",
            "tokens": {"access_token": ACCESS, "account_id": "acct-stale-0000000000"},
            "last_refresh": "2026-09-01T00:00:00Z"
        }),
    );
    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    assert!(!inventory.accounts[0].refreshable);
    assert!(
        inventory.accounts[0]
            .faults
            .iter()
            .any(|f| f.contains("no refresh token")),
        "got {:?}",
        inventory.accounts[0].faults
    );
}

#[test]
fn an_account_with_no_usable_credential_is_a_fault() {
    let dir = tempfile::tempdir().unwrap();
    account(
        dir.path(),
        "hollow",
        json!({"auth_mode": "chatgpt", "tokens": {"account_id": "acct-x"}}),
    );
    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    assert!(
        inventory.accounts[0]
            .faults
            .iter()
            .any(|f| f.contains("no usable credential")),
        "got {:?}",
        inventory.accounts[0].faults
    );
}

#[test]
fn an_empty_store_says_so_rather_than_failing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    assert!(inventory.accounts.is_empty());
    assert!(!inventory.rotation_possible);
    assert!(render(&inventory).contains("none: no account directory"));
}

#[test]
fn a_missing_store_is_an_error_with_a_way_forward() {
    let dir = tempfile::tempdir().unwrap();
    let error = Store::open(dir.path().join("absent")).expect_err("a missing store is an error");
    assert!(format!("{error}").contains("does not exist"));
}

#[test]
fn dot_directories_are_not_accounts() {
    // A partially written account, or an editor's leftovers, should not be
    // mistaken for something that can serve a request.
    let dir = tempfile::tempdir().unwrap();
    account(dir.path(), "real", chatgpt("acct-real-0000000000"));
    std::fs::create_dir_all(dir.path().join(".partial")).unwrap();

    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    assert_eq!(inventory.accounts.len(), 1);
    assert_eq!(inventory.accounts[0].name, "real");
}

#[test]
fn rotation_needs_more_than_one_usable_account() {
    // The arithmetic that closed this whole question once already: rotation
    // across one account does nothing.
    let dir = tempfile::tempdir().unwrap();
    account(dir.path(), "only", chatgpt("acct-only-0000000000"));
    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    assert_eq!(inventory.usable, 1);
    assert!(!inventory.rotation_possible);
    assert!(render(&inventory).contains("not possible"));
}

// ---------------------------------------------------------------------------
// Caller identity (slice 2)
// ---------------------------------------------------------------------------

use timon::broker::identity::{find, peer_uid};

/// A `/proc/net/tcp` table, in the kernel's own column order.
fn table(rows: &[(&str, &str, &str, &str)]) -> String {
    let mut out = String::from(
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n",
    );
    for (i, (local, remote, state, uid)) in rows.iter().enumerate() {
        out.push_str(&format!(
            "  {i}: {local} {remote} {state} 00000000:00000000 00:00000000 00000000 {uid}  0 12345 1 0000 100 0 0 10 0\n"
        ));
    }
    out
}

#[test]
fn the_clients_uid_is_found_by_mirroring_the_port_pair() {
    // Our side is port 0x2000, the client's is 0xABCD. The client's row has them
    // the other way round, which is the whole basis of the lookup.
    let t = table(&[("0100007F:ABCD", "0100007F:2000", "01", "1002")]);
    assert_eq!(find(&t, 0x2000, 0xABCD), Some(1002));
}

#[test]
fn a_socket_that_is_not_established_is_never_matched() {
    // This is the important one. A closed or TIME_WAIT socket reports uid 0, so
    // matching it would silently attribute the request to root — the exact
    // "confidently names the wrong person" failure the shared daemon is refused
    // for. Found by testing: the first version of this lookup ignored the state
    // column and reported [0, 0] for two connections that had already closed.
    for state in ["06", "08", "0A", "02"] {
        let t = table(&[("0100007F:ABCD", "0100007F:2000", state, "0")]);
        assert_eq!(
            find(&t, 0x2000, 0xABCD),
            None,
            "state {state} must not be treated as an answer"
        );
    }
}

#[test]
fn the_listening_socket_is_not_mistaken_for_the_client() {
    // Our own listener appears in the same table. Matching it would attribute
    // every request to whoever runs the broker.
    let t = table(&[
        ("00000000:2000", "00000000:0000", "0A", "998"),
        ("0100007F:ABCD", "0100007F:2000", "01", "1002"),
    ]);
    assert_eq!(find(&t, 0x2000, 0xABCD), Some(1002));
}

#[test]
fn our_own_end_of_the_connection_is_not_matched() {
    // Both ends are in the table. Ours has the ports the other way up, and
    // matching it would report the broker's own uid for every caller.
    let t = table(&[
        ("0100007F:2000", "0100007F:ABCD", "01", "998"),
        ("0100007F:ABCD", "0100007F:2000", "01", "1002"),
    ]);
    assert_eq!(find(&t, 0x2000, 0xABCD), Some(1002));
}

#[test]
fn an_unrelated_connection_on_another_port_is_not_matched() {
    let t = table(&[("0100007F:BEEF", "0100007F:1F90", "01", "1003")]);
    assert_eq!(find(&t, 0x2000, 0xABCD), None);
}

#[test]
fn a_malformed_table_yields_no_answer_rather_than_a_wrong_one() {
    for text in ["", "header only\n", "header\ngarbage\n", "header\n1: x y\n"] {
        assert_eq!(find(text, 0x2000, 0xABCD), None, "{text:?}");
    }
}

#[test]
fn a_non_loopback_peer_is_refused() {
    let local = "127.0.0.1:9000".parse().unwrap();
    let remote = "203.0.113.5:40000".parse().unwrap();
    let error = peer_uid(local, remote).expect_err("a remote peer has no local uid");
    assert!(format!("{error}").contains("not a loopback address"));
}

#[test]
fn identifying_a_live_connection_returns_the_real_uid() {
    // End to end against the kernel rather than a fixture, with the connection
    // deliberately held open: this is the mechanism the broker depends on, and a
    // captured table cannot show that the real format still parses.
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let local = listener.local_addr().unwrap();

    let client = std::thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(local).unwrap();
        // Held open until the server has looked us up.
        let mut buf = [0u8; 1];
        let _ = stream.read(&mut buf);
    });

    let (stream, peer) = listener.accept().unwrap();
    let found = peer_uid(stream.local_addr().unwrap(), peer);
    drop(stream);
    client.join().unwrap();

    assert_eq!(
        found.expect("the kernel should own this connection"),
        // Safe: this is the uid the test process itself runs as.
        unsafe { libc::getuid() },
    );
}

#[test]
fn identifying_a_closed_connection_fails_rather_than_saying_root() {
    // The failure mode that would be worst in production: the client has gone, so
    // the lookup must refuse instead of reporting uid 0.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let local = listener.local_addr().unwrap();
    let client = std::net::TcpStream::connect(local).unwrap();
    let peer = client.local_addr().unwrap();
    drop(client);
    let (stream, _) = listener.accept().unwrap();
    drop(stream);
    // Give the kernel a moment to retire the socket.
    std::thread::sleep(std::time::Duration::from_millis(300));

    match peer_uid(local, peer) {
        Err(error) => assert!(
            format!("{error}").contains("refusing rather than guessing"),
            "got {error}"
        ),
        // Still established somehow: acceptable, but it must not be root by
        // default.
        Ok(uid) => assert_ne!(uid, 0, "a vanished client must never resolve to root"),
    }
}

// ---------------------------------------------------------------------------
// The forwarding proxy (slice 2)
// ---------------------------------------------------------------------------

use std::io::{BufReader, Read, Write};
use timon::broker::serve::{
    Request, read_request, refusal, relay, split_upstream, upstream_request,
};

fn parse(raw: &str) -> Request {
    let mut reader = BufReader::new(raw.as_bytes());
    read_request(&mut reader)
        .expect("should parse")
        .expect("should be a request")
}

#[test]
fn a_request_is_read_with_its_headers_lowercased_and_its_body_whole() {
    let request = parse(
        "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:9000\r\nContent-Length: 9\r\n\
         Content-Type: application/json\r\n\r\n{\"a\":1.0}",
    );
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/v1/responses");
    assert_eq!(request.body, b"{\"a\":1.0}");
    assert!(request.headers.iter().any(|(n, _)| n == "content-type"));
    assert!(
        request.headers.iter().all(|(n, _)| n.to_lowercase() == *n),
        "names are lowercased so lookups cannot miss a differently-cased header"
    );
}

#[test]
fn the_callers_own_auth_headers_are_removed_not_merged() {
    // The failure this prevents: a caller choosing which pooled account pays for
    // its request by sending its own authorization.
    let request = parse(
        "POST /v1/responses HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer CALLER-CHOSEN\r\n\
         ChatGPT-Account-Id: CALLER-ACCOUNT\r\nOpenAI-Organization: CALLER-ORG\r\n\
         Content-Length: 2\r\n\r\n{}",
    );
    let out = String::from_utf8(upstream_request(
        &request,
        "/backend-api/codex",
        "upstream.test",
        "POOLED-TOKEN",
        Some("POOLED-ACCOUNT"),
    ))
    .unwrap();

    assert!(out.contains("authorization: Bearer POOLED-TOKEN"));
    assert!(out.contains("chatgpt-account-id: POOLED-ACCOUNT"));
    for caller in ["CALLER-CHOSEN", "CALLER-ACCOUNT", "CALLER-ORG"] {
        assert!(!out.contains(caller), "{caller} must not reach upstream");
    }
    // Exactly one authorization header, not the pooled one appended to theirs.
    assert_eq!(out.matches("authorization:").count(), 1);
}

#[test]
fn the_upstream_path_prefix_is_applied_and_the_host_replaced() {
    let request = parse("POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:9000\r\n\r\n");
    let out = String::from_utf8(upstream_request(
        &request,
        "/backend-api/codex",
        "chat.openai.com",
        "T",
        None,
    ))
    .unwrap();
    assert!(out.starts_with("POST /backend-api/codex/v1/responses HTTP/1.1\r\n"));
    assert!(out.contains("host: chat.openai.com"));
    assert!(
        !out.contains("127.0.0.1:9000"),
        "the client's host must not travel"
    );
    assert!(
        !out.contains("chatgpt-account-id"),
        "omitted when there is none"
    );
}

#[test]
fn the_content_length_sent_upstream_matches_the_body_actually_forwarded() {
    // A mismatch here would make the upstream wait for bytes that never come, or
    // truncate a prompt silently.
    let body = "{\"model\":\"gpt-5.5\",\"input\":\"hello\"}";
    let raw = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: h\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let request = parse(&raw);
    let out = upstream_request(&request, "", "u", "T", None);
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains(&format!("content-length: {}", body.len())));
    assert!(out.ends_with(body.as_bytes()));
    assert_eq!(text.matches("content-length:").count(), 1);
}

#[test]
fn a_chunked_request_body_is_refused_rather_than_guessed_at() {
    let mut reader = BufReader::new(
        "POST /v1/responses HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"
            .as_bytes(),
    );
    let error = read_request(&mut reader).expect_err("chunked must be refused");
    assert!(format!("{error}").contains("chunked"));
}

#[test]
fn an_idle_connection_closing_is_not_an_error() {
    let mut reader = BufReader::new("".as_bytes());
    assert!(read_request(&mut reader).unwrap().is_none());
}

#[test]
fn an_oversized_header_line_is_refused_before_it_is_stored() {
    let huge = "x".repeat(32 * 1024);
    let raw = format!("POST / HTTP/1.1\r\nHost: h\r\nX-Big: {huge}\r\n\r\n");
    let mut reader = BufReader::new(raw.as_bytes());
    let error = read_request(&mut reader).expect_err("an oversized header must be refused");
    assert!(format!("{error}").contains("exceeds the accepted size"));
}

#[test]
fn a_relay_forwards_every_byte_and_flushes_as_it_goes() {
    // Streaming is the contract: a client shows tokens as they arrive, so the
    // relay must not accumulate. `Flushing` records when each flush happened, so
    // buffering-until-the-end would be visible as a single flush at the end.
    struct Flushing {
        written: Vec<u8>,
        flushes: usize,
    }
    impl Write for Flushing {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }
    // Three SSE events arriving as separate reads.
    struct Trickle(Vec<Vec<u8>>);
    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            let next = self.0.remove(0);
            buf[..next.len()].copy_from_slice(&next);
            Ok(next.len())
        }
    }

    let events: Vec<Vec<u8>> = vec![
        b"event: response.created\ndata: {}\n\n".to_vec(),
        b"event: response.output_text.delta\ndata: {\"delta\":\"ok\"}\n\n".to_vec(),
        b"event: response.completed\ndata: {}\n\n".to_vec(),
    ];
    let expected: Vec<u8> = events.iter().flatten().copied().collect();

    let mut from = Trickle(events);
    let mut to = Flushing {
        written: Vec::new(),
        flushes: 0,
    };
    let total = relay(&mut from, &mut to).unwrap();

    assert_eq!(to.written, expected, "every byte, in order");
    assert_eq!(total, expected.len() as u64);
    assert_eq!(
        to.flushes, 3,
        "flushed once per chunk as it arrived, not once at the end"
    );
}

#[test]
fn an_upstream_that_closes_without_a_clean_shutdown_is_not_an_error() {
    struct Abrupt(bool);
    impl Read for Abrupt {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 {
                self.0 = false;
                buf[..4].copy_from_slice(b"data");
                return Ok(4);
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "no close_notify",
            ))
        }
    }
    let mut out = Vec::new();
    // The client keeps what arrived rather than losing the answer to a missing
    // shutdown, which is the same lesson the citation verifier learned.
    assert_eq!(relay(&mut Abrupt(true), &mut out).unwrap(), 4);
    assert_eq!(out, b"data");
}

#[test]
fn a_refusal_is_json_a_client_can_read_and_names_no_credential() {
    let bytes = refusal(
        403,
        "Forbidden",
        "the broker could not establish who is calling",
    );
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    assert!(text.contains("content-type: application/json"));
    let body = text.split("\r\n\r\n").nth(1).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(parsed["error"]["type"], "timon_broker_refused");
    assert!(
        parsed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("who is calling")
    );
}

#[test]
fn an_upstream_base_is_split_into_its_parts() {
    let (scheme, host, port, path) =
        split_upstream("https://chat.openai.com/backend-api/codex").unwrap();
    assert_eq!(
        (scheme.as_str(), host.as_str(), port),
        ("https", "chat.openai.com", 443)
    );
    assert_eq!(path, "/backend-api/codex");

    let (scheme, host, port, path) = split_upstream("http://127.0.0.1:8099").unwrap();
    assert_eq!(
        (scheme.as_str(), host.as_str(), port, path.as_str()),
        ("http", "127.0.0.1", 8099, "")
    );

    assert!(
        split_upstream("chat.openai.com").is_err(),
        "a scheme is required"
    );
    assert!(split_upstream("https://").is_err(), "a host is required");
}

#[test]
fn formatting_a_request_never_prints_the_prompt() {
    // The body is somebody's prompt. A derived Debug would print it the moment a
    // request appeared in an error, a panic or a log line, which is how content
    // escapes a process that was never supposed to keep any.
    let secret = "PROMPT-TEXT-MUST-NEVER-BE-PRINTED-xyz";
    let raw = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer TOKEN-abc\r\n\
         Content-Length: {}\r\n\r\n{secret}",
        secret.len()
    );
    let request = parse(&raw);
    let rendered = format!("{request:?}");

    assert!(!rendered.contains(secret), "the prompt leaked into Debug");
    assert!(
        !rendered.contains("TOKEN-abc"),
        "a header value leaked into Debug"
    );
    // Still useful for diagnosis.
    assert!(rendered.contains("authorization"), "header names are kept");
    assert!(rendered.contains(&format!("body_bytes: {}", secret.len())));
}
