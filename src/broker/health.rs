// Adapted for Timon. Not derived from Prodex source.
//! Whether this broker can actually serve, as opposed to merely being alive.
//!
//! The distinction is the point. A broker whose pool lock has been poisoned by a
//! panicking thread keeps its listener open and answers every request with a 500
//! for the rest of the process's life. Anything that checks the socket reports a
//! healthy service. So health is defined as *the things a request needs*, and the
//! same answer drives both the endpoint an operator can curl and the watchdog
//! that decides whether to let this process keep running.
//!
//! Nothing here contacts the provider. A health check that spent quota would be
//! a health check nobody could afford to run often.

use std::sync::atomic::Ordering;

use crate::broker::serve::{Config, Counters};

/// What a health check found.
#[derive(Debug, serde::Serialize)]
pub struct Health {
    pub healthy: bool,
    /// Why not, in operator language. Empty when healthy.
    pub faults: Vec<String>,
    /// Accounts this listener may use that currently have no fault of their own.
    pub accounts_usable: usize,
    pub accounts_configured: usize,
    pub connections: u64,
    pub requests_forwarded: u64,
    pub requests_refused_no_account: u64,
}

/// Examines the broker's own state.
///
/// Two conditions, and they are the two that make a request fail for everyone
/// rather than for one caller:
///
/// 1. **The pool lock is usable.** A poisoned mutex is permanent and invisible
///    from outside, which is exactly the failure a socket probe misses.
/// 2. **Some account can serve.** A pool where every credential has broken is a
///    listener that can only refuse.
pub fn check(config: &Config, counters: &Counters) -> Health {
    let mut faults = Vec::new();

    // Taken and dropped immediately. This is a test of whether the lock can be
    // held at all, not an inspection of what it guards.
    if config.pool.lock().is_err() {
        faults.push(
            "the account state is poisoned by an earlier panic; every request will \
             fail until this process is restarted"
                .to_string(),
        );
    }

    let configured = config.serving.len();
    let usable = config
        .serving
        .iter()
        .filter(|name| config.store.read(name).usable())
        .count();
    if usable == 0 {
        faults.push(format!(
            "none of the {configured} configured account(s) can serve requests; \
             `timon broker accounts` says what is wrong with each"
        ));
    }

    Health {
        healthy: faults.is_empty(),
        faults,
        accounts_usable: usable,
        accounts_configured: configured,
        connections: counters.connections.load(Ordering::Relaxed),
        requests_forwarded: counters.requests_forwarded.load(Ordering::Relaxed),
        requests_refused_no_account: counters.requests_refused_no_account.load(Ordering::Relaxed),
    }
}

/// The path that answers a health check instead of being forwarded upstream.
///
/// Prefixed and namespaced so it cannot collide with a provider path. A request
/// to it is still identified by uid first, like every other request: health is
/// not a hole in that rule.
pub const HEALTH_PATH: &str = "/_timon/health";

/// True when this request is asking about the broker rather than the provider.
pub fn is_health_request(target: &str) -> bool {
    let path = target.split('?').next().unwrap_or(target);
    path.trim_end_matches('/') == HEALTH_PATH
}

/// The health answer as an HTTP response.
///
/// `503` when unhealthy rather than `500`, because the condition is one a restart
/// resolves, and that is what a supervisor should infer.
pub fn response(health: &Health) -> Vec<u8> {
    let body = serde_json::to_string_pretty(health).unwrap_or_else(|_| "{}".to_string());
    let (status, reason) = if health.healthy {
        (200, "OK")
    } else {
        (503, "Service Unavailable")
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         content-type: application/json\r\n\
         cache-control: no-store\r\n\
         content-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}
