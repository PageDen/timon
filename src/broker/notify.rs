// Adapted for Timon. Not derived from Prodex source.
//! Telling the supervisor this process is alive *and working*.
//!
//! systemd's watchdog is a dead-man's switch: the service promises to check in,
//! and stopping is what triggers a restart. That makes it the right mechanism for
//! the failure this broker actually has — a poisoned pool lock leaves the process
//! running and the port open while every request fails, so anything that watches
//! for a crash or probes the socket sees a healthy service forever.
//!
//! So the check-in is gated on [`crate::broker::health::check`]. A broker that
//! cannot serve stops saying it can, and systemd restarts it.
//!
//! Spoken directly over the notify socket rather than through libsystemd: the
//! protocol is a datagram containing a line of text, and a dependency for that
//! would cost more than it saves. Outside systemd there is no socket and every
//! function here does nothing, so the broker runs unchanged from a terminal.

use std::os::unix::net::UnixDatagram;
use std::path::Path;

/// Environment variable systemd sets for the notify socket.
const NOTIFY_SOCKET: &str = "NOTIFY_SOCKET";

/// Sends one notification. Absent socket means not under systemd: not an error.
fn send(message: &str) -> bool {
    let Ok(address) = std::env::var(NOTIFY_SOCKET) else {
        return false;
    };
    if address.is_empty() {
        return false;
    }
    let Ok(socket) = UnixDatagram::unbound() else {
        return false;
    };
    // A leading '@' means an abstract socket, whose name starts with a NUL byte.
    // Rust's UnixDatagram cannot address those by path, so this handles only the
    // filesystem form; systemd uses a filesystem socket for services by default.
    if address.starts_with('@') {
        return false;
    }
    socket
        .send_to(message.as_bytes(), Path::new(&address))
        .is_ok()
}

/// Announces that startup finished and the listener is accepting.
///
/// With `Type=notify` the supervisor blocks until this arrives, so anything
/// ordered after the broker starts only runs once the port is genuinely open.
pub fn ready() -> bool {
    send("READY=1")
}

/// Reports that the process is still able to serve.
pub fn watchdog() -> bool {
    send("WATCHDOG=1")
}

/// Explains a refusal to check in, so the journal says why a restart happened
/// rather than leaving an unexplained kill.
pub fn degraded(reason: &str) -> bool {
    send(&format!("STATUS=unhealthy: {reason}"))
}

/// Reports healthy status text.
pub fn status(text: &str) -> bool {
    send(&format!("STATUS={text}"))
}

/// How often to check in, derived from the deadline systemd gives us.
///
/// `WATCHDOG_USEC` is the interval after which systemd acts. Checking in at half
/// of it leaves room for one missed beat to be a delay rather than a restart.
pub fn interval() -> Option<std::time::Duration> {
    let micros: u64 = std::env::var("WATCHDOG_USEC").ok()?.parse().ok()?;
    if micros == 0 {
        return None;
    }
    Some(std::time::Duration::from_micros(micros / 2))
}
