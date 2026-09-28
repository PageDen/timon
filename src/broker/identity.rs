// Adapted for Timon. Not derived from Prodex source.
//! Who is on the other end of a loopback connection, according to the kernel.
//!
//! The recorder gets this for free: `SO_PEERCRED` on a Unix socket returns the
//! peer's uid and cannot be spoofed. The broker cannot use that, because Codex
//! reaches it through an HTTP base URL and so the transport is TCP.
//!
//! What TCP does offer is `/proc/net/tcp`, which lists the uid owning each
//! socket. Matching the connection's port pair finds the client's socket and
//! therefore the uid that opened it. That is still the kernel's answer rather
//! than the client's claim, so it cannot be forged by a request header — but it
//! is weaker than `SO_PEERCRED` in two ways worth stating plainly rather than
//! discovering later:
//!
//! 1. **It is a lookup, not a property of the connection.** The socket must still
//!    be established when the lookup happens. A closed socket's uid field reads
//!    `0`, so a late lookup does not fail — it silently says root. Everything here
//!    is arranged so that cannot be mistaken for an answer.
//! 2. **It parses a text interface.** The format is stable in practice but is not
//!    a syscall contract.
//!
//! A failed lookup is an error, never a default. Attributing an unidentifiable
//! request to root would be the same fault as the shared app-server daemon this
//! project already refuses: a confident answer naming the wrong person.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

/// Why the peer could not be identified.
#[derive(Debug)]
pub enum IdentityError {
    /// No established socket matched the port pair. Usually the client closed
    /// before the lookup; never treated as "root".
    NoMatch {
        local: u16,
        peer: u16,
    },
    /// The connection is not from this machine, so no local uid owns it.
    NotLoopback(IpAddr),
    Io(PathBuf, std::io::Error),
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdentityError::NoMatch { local, peer } => write!(
                f,
                "no established local socket owns the connection from port {peer} to port {local}; \
                 refusing rather than guessing whose request this is"
            ),
            IdentityError::NotLoopback(ip) => {
                write!(
                    f,
                    "{ip} is not a loopback address, so no local account owns it"
                )
            }
            IdentityError::Io(path, error) => write!(f, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for IdentityError {}

/// The tables the kernel exposes, IPv4 and IPv6.
const TABLES: [&str; 2] = ["/proc/net/tcp", "/proc/net/tcp6"];

/// Hex for an established connection in the state column.
const ESTABLISHED: &str = "01";

/// The uid that owns the other end of this connection.
///
/// `local` is this side's address, `peer` the address `accept` reported. Both are
/// needed because the client's socket is the mirror of ours: its local port is our
/// remote port.
pub fn peer_uid(local: SocketAddr, peer: SocketAddr) -> Result<u32, IdentityError> {
    if !peer.ip().is_loopback() {
        return Err(IdentityError::NotLoopback(peer.ip()));
    }
    for table in TABLES {
        let path = Path::new(table);
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            // A kernel without IPv6 has no tcp6 table; that is not an error.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(IdentityError::Io(path.to_path_buf(), error)),
        };
        if let Some(uid) = find(&text, local.port(), peer.port()) {
            return Ok(uid);
        }
    }
    Err(IdentityError::NoMatch {
        local: local.port(),
        peer: peer.port(),
    })
}

/// Scans one table for the client's established socket.
///
/// Separated from the file reading so it can be tested against captured table
/// text, including the shapes that must *not* match.
pub fn find(table: &str, local_port: u16, peer_port: u16) -> Option<u32> {
    let want_local = format!("{peer_port:04X}");
    let want_remote = format!("{local_port:04X}");
    for line in table.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // sl local rem st tx rx tr when retrnsmt uid ...
        if fields.len() < 8 {
            continue;
        }
        // Only an established socket carries a meaningful uid. A closed or
        // TIME_WAIT entry reports 0, which would read as root.
        if fields[3] != ESTABLISHED {
            continue;
        }
        let (Some(local), Some(remote)) = (port_of(fields[1]), port_of(fields[2])) else {
            continue;
        };
        if local == want_local && remote == want_remote {
            return fields[7].parse().ok();
        }
    }
    None
}

/// The port half of an `address:port` column, uppercased for comparison.
fn port_of(column: &str) -> Option<String> {
    column.split(':').nth(1).map(|port| port.to_uppercase())
}
