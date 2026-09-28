//! Rotation through the whole listener, against a stub provider.
//!
//! The unit tests decide *which* account selection prefers. This one proves the
//! part that only shows up end to end: when the first account is refused the
//! model, the client still gets one clean answer, from the second account, with
//! nothing of the first attempt spliced into it — and the conversation then stays
//! where it was served.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use timon::broker::quota::Standing;
use timon::broker::select::Pool;
use timon::broker::serve::{Config, Counters, serve};
use timon::broker::store::Store;

/// A JWT-shaped token that expires far enough away that nothing tries to refresh
/// it. Only the payload is parsed; the suffix is what tells the accounts apart.
fn token(nick: &str) -> String {
    // {"exp":4000000000}
    format!("eyJhbGciOiJub25lIn0.eyJleHAiOjQwMDAwMDAwMDB9.sig-{nick}")
}

fn account(root: &std::path::Path, name: &str) {
    let home = root.join(name);
    std::fs::create_dir_all(&home).unwrap();
    let body = serde_json::json!({
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": token(&format!("{name}-id")),
            "access_token": token(name),
            "refresh_token": format!("refresh-{name}"),
            "account_id": format!("account-id-{name}"),
        },
        "last_refresh": "2026-09-28T00:00:00Z",
    });
    std::fs::write(
        home.join("auth.json"),
        serde_json::to_string_pretty(&body).unwrap(),
    )
    .unwrap();
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

/// A reading that is fresh, so nothing reaches for the real usage endpoint.
fn standing(account: &str, used_percent: u32, now: i64) -> Standing {
    Standing {
        account_id: account.to_string(),
        email: None,
        plan_type: Some("test".to_string()),
        allowed: true,
        limit_reached: false,
        used_percent: Some(used_percent),
        reset_at: None,
        read_at: now,
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Which account each stub request arrived as, in order.
type Seen = Arc<std::sync::Mutex<Vec<String>>>;

/// A provider that refuses `gpt-5.5` to acct3 and serves it to anyone else.
fn stub() -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(stream) = incoming else { continue };
            let record = Arc::clone(&record);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);

                let who = if head.contains("sig-acct3") {
                    "acct3"
                } else {
                    "acct2"
                };
                record.lock().unwrap().push(who.to_string());

                let mut stream = stream;
                if who == "acct3" {
                    let payload = br#"{"error":{"message":"The model `gpt-5.5` does not exist or you do not have access to it","code":"model_not_found"}}"#;
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 404 Not Found\r\ncontent-type: application/json\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n",
                            payload.len()
                        )
                        .as_bytes(),
                    );
                    let _ = stream.write_all(payload);
                } else {
                    let payload = b"data: SERVED-BY-ACCT2\n\n";
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n",
                            payload.len()
                        )
                        .as_bytes(),
                    );
                    let _ = stream.write_all(payload);
                }
                let _ = stream.flush();
            });
        }
    });
    (port, seen)
}

/// Sends one request through the broker and returns the whole response.
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
fn an_account_that_cannot_serve_the_model_is_rotated_past_before_anything_is_streamed() {
    let root = tempfile::tempdir().unwrap();
    account(root.path(), "acct2");
    account(root.path(), "acct3");
    let (upstream_port, seen) = stub();

    // acct3 has far more headroom, so selection prefers it and must discover the
    // refusal rather than predict it.
    let mut pool = Pool::new();
    pool.observe("acct3", standing("acct3", 1, now()));
    pool.observe("acct2", standing("acct2", 80, now()));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = listener.local_addr().unwrap();
    let config = Arc::new(Config {
        listen,
        upstream: format!("http://127.0.0.1:{upstream_port}"),
        store: Store::open(root.path()).unwrap(),
        serving: vec!["acct3".to_string(), "acct2".to_string()],
        pool: std::sync::Mutex::new(pool),
        read_timeout: std::time::Duration::from_secs(20),
    });
    let counters = Arc::new(Counters::default());
    let served = Arc::clone(&counters);
    let running = Arc::clone(&config);
    std::thread::spawn(move || {
        let _ = serve(running, served, listener, Arc::new(|| false));
    });

    let response = ask(listen.port(), "thread-one", "gpt-5.5");

    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "the client should see one clean success, got: {}",
        response.lines().next().unwrap_or_default()
    );
    assert!(response.contains("SERVED-BY-ACCT2"));
    assert!(
        !response.contains("model_not_found") && !response.contains("404"),
        "nothing from the refused attempt may reach the client: {response}"
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["acct3".to_string(), "acct2".to_string()],
        "the refused account should be tried first, then rotated past"
    );
    assert_eq!(counters.rotations.load(Ordering::Relaxed), 1);
    assert_eq!(counters.requests_forwarded.load(Ordering::Relaxed), 1);

    // The refusal is remembered: the next request for the same model skips acct3
    // entirely rather than paying for the discovery again.
    seen.lock().unwrap().clear();
    let response = ask(listen.port(), "thread-two", "gpt-5.5");
    assert!(response.contains("SERVED-BY-ACCT2"));
    assert_eq!(*seen.lock().unwrap(), vec!["acct2".to_string()]);
    assert_eq!(
        counters.rotations.load(Ordering::Relaxed),
        1,
        "skipping a known-refused account is not a rotation"
    );

    // And the first conversation stays where it was served, even though acct3
    // has more headroom and its refusal was only for this model.
    seen.lock().unwrap().clear();
    let response = ask(listen.port(), "thread-one", "gpt-5.6-luna");
    assert!(response.contains("SERVED-BY-ACCT2"));
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["acct2".to_string()],
        "affinity should hold the thread on acct2 for a model acct3 could serve"
    );
}

#[test]
fn a_request_no_account_can_serve_is_refused_in_terms_the_caller_can_act_on() {
    let root = tempfile::tempdir().unwrap();
    account(root.path(), "acct3");
    let (upstream_port, _seen) = stub();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = listener.local_addr().unwrap();
    let config = Arc::new(Config {
        listen,
        upstream: format!("http://127.0.0.1:{upstream_port}"),
        store: Store::open(root.path()).unwrap(),
        serving: vec!["acct3".to_string()],
        pool: std::sync::Mutex::new(Pool::new()),
        read_timeout: std::time::Duration::from_secs(20),
    });
    let counters = Arc::new(Counters::default());
    let served = Arc::clone(&counters);
    let running = Arc::clone(&config);
    std::thread::spawn(move || {
        let _ = serve(running, served, listener, Arc::new(|| false));
    });

    let response = ask(listen.port(), "thread-three", "gpt-5.5");
    assert!(
        response.contains("no pooled account can serve gpt-5.5"),
        "the refusal should name the model and not a credential: {response}"
    );
    assert!(
        !response.contains("sig-acct3"),
        "no token may appear: {response}"
    );
    assert_eq!(
        counters.requests_refused_no_account.load(Ordering::Relaxed),
        1
    );
}
