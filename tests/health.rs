//! Whether the broker can serve, as distinct from whether it is running.
//!
//! The case these tests exist for: a panicking thread poisons the pool lock, and
//! from then on the process stays up, the port stays open, and every request
//! returns 500 forever. Anything that watches for a crash or probes the socket
//! reports a healthy service. So the test that matters here is the one that
//! poisons the lock on purpose.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use timon::broker::health::{self, HEALTH_PATH};
use timon::broker::select::Pool;
use timon::broker::serve::{Config, Counters, serve};
use timon::broker::store::Store;

/// Wall-clock seconds. `health::check` reads the real clock, so a test that
/// fabricates a timestamp is testing nothing.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

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

fn config(root: &std::path::Path, serving: &[&str]) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        upstream: "http://127.0.0.1:1".to_string(),
        store: Store::open(root).unwrap(),
        serving: serving.iter().map(|name| name.to_string()).collect(),
        pool: std::sync::Mutex::new(Pool::new()),
        models: timon::broker::policy::ModelPolicy::default(),
        read_timeout: std::time::Duration::from_secs(5),
    }
}

#[test]
fn a_broker_with_a_usable_account_reports_healthy() {
    let root = tempfile::tempdir().unwrap();
    account(root.path(), "acct2");
    let config = config(root.path(), &["acct2"]);
    let state = health::check(&config, &Counters::default());
    assert!(state.healthy, "faults: {:?}", state.faults);
    assert_eq!(state.accounts_usable, 1);
    assert_eq!(state.accounts_configured, 1);
}

#[test]
fn a_poisoned_pool_lock_is_reported_unhealthy_though_the_process_is_alive() {
    // The failure this whole module exists for. Nothing has crashed; the port
    // would still accept; every request would return 500 for the life of the
    // process.
    let root = tempfile::tempdir().unwrap();
    account(root.path(), "acct2");
    let config = Arc::new(config(root.path(), &["acct2"]));
    assert!(health::check(&config, &Counters::default()).healthy);

    // Poison it exactly as a panicking request thread would.
    let poisoner = Arc::clone(&config);
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.pool.lock().unwrap();
        panic!("a request thread died holding the pool lock");
    })
    .join();

    let state = health::check(&config, &Counters::default());
    assert!(!state.healthy, "a poisoned lock must not report healthy");
    assert!(
        state.faults.iter().any(|fault| fault.contains("poisoned")),
        "the fault should name the cause: {:?}",
        state.faults
    );
    assert!(
        state.faults.iter().any(|fault| fault.contains("restarted")),
        "and should say what resolves it: {:?}",
        state.faults
    );
}

#[test]
fn a_pool_with_no_usable_account_is_unhealthy() {
    let root = tempfile::tempdir().unwrap();
    // A configured account whose credential file does not exist at all.
    std::fs::create_dir_all(root.path().join("acct9")).unwrap();
    let config = config(root.path(), &["acct9"]);
    let state = health::check(&config, &Counters::default());
    assert!(!state.healthy);
    assert_eq!(state.accounts_usable, 0);
    assert_eq!(state.accounts_configured, 1);
}

#[test]
fn the_health_path_is_recognised_and_nothing_else_is() {
    assert!(health::is_health_request(HEALTH_PATH));
    assert!(health::is_health_request("/_timon/health/"));
    assert!(health::is_health_request("/_timon/health?verbose=1"));
    // A provider path must never be mistaken for a health check, or a real
    // request would be answered locally and silently never reach the model.
    assert!(!health::is_health_request("/responses"));
    assert!(!health::is_health_request("/models"));
    assert!(!health::is_health_request("/_timon/healthcheck"));
    assert!(!health::is_health_request("/"));
}

#[test]
fn an_unhealthy_broker_answers_503_and_a_healthy_one_200() {
    let root = tempfile::tempdir().unwrap();
    account(root.path(), "acct2");
    let healthy = config(root.path(), &["acct2"]);
    let good = health::check(&healthy, &Counters::default());
    let response = String::from_utf8(health::response(&good)).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("\"healthy\": true"));

    let empty = config(root.path(), &[]);
    let bad = health::check(&empty, &Counters::default());
    let response = String::from_utf8(health::response(&bad)).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 503"),
        "503 rather than 500: a restart resolves this, and a supervisor should infer that"
    );
}

#[test]
fn a_health_check_is_answered_locally_and_never_forwarded() {
    // The upstream here is a port with nothing on it. If the health request were
    // forwarded, the answer would be a 502 rather than a health report.
    let root = tempfile::tempdir().unwrap();
    account(root.path(), "acct2");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = listener.local_addr().unwrap();
    let config = Arc::new(config(root.path(), &["acct2"]));
    let counters = Arc::new(Counters::default());
    let running = Arc::clone(&config);
    let served = Arc::clone(&counters);
    std::thread::spawn(move || {
        let _ = serve(running, served, listener, Arc::new(|| false));
    });

    let mut stream = TcpStream::connect(("127.0.0.1", listen.port())).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    stream
        .write_all(format!("GET {HEALTH_PATH} HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n").as_bytes())
        .unwrap();
    stream.flush().unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);

    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "expected a local health answer, got: {}",
        response.lines().next().unwrap_or_default()
    );
    assert!(response.contains("\"accounts_usable\": 1"));
    assert_eq!(
        counters
            .requests_forwarded
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a health check must not count as a forwarded request, or it would \
         pollute the numbers it reports"
    );
    assert!(!response.contains("sig-acct2"), "no token may appear");
}

#[test]
fn an_account_the_provider_refused_is_not_counted_as_usable() {
    // The failure that motivated this: a subscription change invalidated a token
    // that still had nine days left on it. Nothing in the credential file showed
    // it — permissions fine, refresh token present, expiry far away — so a health
    // check reading only the file called the account healthy while every request
    // to it was refused.
    let root = tempfile::tempdir().unwrap();
    account(root.path(), "acct2");
    account(root.path(), "acct3");
    let config = config(root.path(), &["acct2", "acct3"]);

    let before = health::check(&config, &Counters::default());
    assert_eq!(before.accounts_usable, 2);
    assert!(before.accounts_rejected.is_empty());

    config.pool.lock().unwrap().rejected("acct3", now());

    let after = health::check(&config, &Counters::default());
    assert_eq!(
        after.accounts_usable, 1,
        "a refused account must not be counted as able to serve"
    );
    assert_eq!(after.accounts_rejected, vec!["acct3".to_string()]);
    assert!(
        after.healthy,
        "one refused account of two is not a broker fault: the pool still works, \
         and restarting over it would help nobody"
    );
}

#[test]
fn every_account_refused_is_a_fault_that_names_the_recovery() {
    let root = tempfile::tempdir().unwrap();
    account(root.path(), "acct2");
    let config = config(root.path(), &["acct2"]);
    config.pool.lock().unwrap().rejected("acct2", now());

    let state = health::check(&config, &Counters::default());
    assert!(!state.healthy);
    assert_eq!(state.accounts_usable, 0);
    assert!(
        state.faults.iter().any(|f| f.contains("broker refresh")),
        "the fault should name the command that recovers it: {:?}",
        state.faults
    );
}

#[test]
fn a_rejection_lapses_so_a_recovered_account_returns() {
    let root = tempfile::tempdir().unwrap();
    account(root.path(), "acct2");
    let config = config(root.path(), &["acct2"]);
    config.pool.lock().unwrap().rejected("acct2", now());
    assert_eq!(
        health::check(&config, &Counters::default()).accounts_usable,
        0
    );

    // Fifteen minutes later it is tried again rather than staying out forever.
    {
        let mut pool = config.pool.lock().unwrap();
        pool.forget_stale(now() + 901);
    }
    assert_eq!(
        health::check(&config, &Counters::default()).accounts_usable,
        1
    );
}
