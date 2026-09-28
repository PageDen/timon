// Adapted for Timon. Not derived from Prodex source.
//! Grants: what a request is allowed to do, as distinct from who sent it.
//!
//! The broker already knows *who* is calling — it takes the uid from the kernel
//! and refuses anyone it cannot name. That is a real guarantee and it is not the
//! same as knowing what a request is entitled to. A developer's interactive
//! session and their pipeline workers arrive from the same uid. If a privileged
//! request were recognised by the shape of its headers, an interactive caller
//! could imitate one.
//!
//! So authority travels separately, as a grant the **broker itself mints**. The
//! orchestrator asks for one at the start of a run, naming what that run may
//! spend; the broker records it and returns an opaque token; workers present the
//! token on each request. Nothing a caller can invent is accepted, because the
//! broker only honours tokens it issued.
//!
//! Minting at the broker rather than signing at the host is deliberate. The
//! broker runs as its own account and cannot read a developer's run store, and a
//! shared signing key would be one more secret to distribute and rotate. This
//! way the authority and the thing checking it are the same process.
//!
//! **The limit, stated plainly.** A grant separates a pipeline worker's
//! authority from an interactive session's *by default*, not against a
//! determined process running as the same uid: a same-uid process can read a
//! worker's environment through `/proc` and take its token. Closing that needs
//! separate uids per role, which is a host change and not this module's to make.
//! What this does prevent is a session acquiring run authority by asking for it,
//! and it makes every privileged request attributable to a run.

use std::collections::HashMap;

use ring::rand::SecureRandom;
use serde::Serialize;

/// The header a caller presents its grant on.
pub const GRANT_HEADER: &str = "x-timon-grant";

/// Longest a grant may live, whatever was asked for.
///
/// A grant is for one run, and a run that has been going for a day has either
/// finished or is stuck. Bounding it means a leaked token stops working without
/// anyone having to notice.
pub const MAX_LIFETIME_SECS: i64 = 12 * 3600;

/// What a run may do.
#[derive(Clone, Debug, Serialize)]
pub struct Grant {
    /// Names this grant in reports. Not a secret, unlike the token.
    pub id: String,
    /// The uid this was issued to. A request presenting it from another uid is
    /// refused: a stolen token is still bound to the account it was minted for.
    pub principal_uid: u32,
    /// The run whose budget and accounts this request belongs to.
    pub run_id: String,
    /// Pooled accounts this run may spend. Empty means the broker's usual choice.
    pub accounts: Vec<String>,
    /// The model this run's requests get, when the run pins one.
    pub model: Option<String>,
    pub issued_at: i64,
    pub expires_at: i64,
    /// Set when the run was cancelled, so in-flight authority stops with it.
    pub revoked: bool,
}

impl Grant {
    pub fn live(&self, now: i64) -> bool {
        !self.revoked && now < self.expires_at
    }
}

/// Why a presented grant was not honoured.
///
/// Each case is something the caller or operator can act on, and none of them
/// says more about other principals' grants than the caller already knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantError {
    /// No grant with this token. Also what a forged token looks like.
    Unknown,
    Expired,
    Revoked,
    /// Presented by a uid other than the one it was issued to.
    WrongPrincipal,
}

impl std::fmt::Display for GrantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GrantError::Unknown => write!(
                f,
                "this grant is not one the broker issued, or the broker has restarted \
                 since it was; start the run again"
            ),
            GrantError::Expired => write!(f, "this grant has expired"),
            GrantError::Revoked => write!(f, "this run was cancelled"),
            GrantError::WrongPrincipal => write!(
                f,
                "this grant was issued to another principal and cannot be used from here"
            ),
        }
    }
}

/// The grants this broker has issued.
///
/// In memory, and deliberately. Grants are short-lived, and a broker restart
/// means the orchestrator that held them is no longer being served — the runs
/// are interrupted anyway, which P1.1 records honestly. Persisting them would
/// keep authority alive across a restart that lost everything else.
#[derive(Default)]
pub struct Grants {
    /// Keyed by the token's hash, never the token.
    by_hash: HashMap<[u8; 32], Grant>,
    issued: u64,
}

impl Grants {
    pub fn new() -> Self {
        Self::default()
    }

    /// Issues a grant and returns the token, which is the only time it exists in
    /// readable form.
    ///
    /// Only the hash is kept. A dump of this process's memory, or a future
    /// `Debug` on the store, cannot yield a usable token — the same reasoning
    /// that keeps provider credentials out of every rendering.
    pub fn issue(
        &mut self,
        principal_uid: u32,
        run_id: &str,
        accounts: Vec<String>,
        model: Option<String>,
        lifetime_secs: i64,
        now: i64,
    ) -> Result<(String, Grant), String> {
        let mut bytes = [0u8; 32];
        ring::rand::SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| "the system random source is unavailable".to_string())?;
        let token = hex(&bytes);
        let lifetime = lifetime_secs.clamp(1, MAX_LIFETIME_SECS);

        self.issued += 1;
        let grant = Grant {
            id: format!("grant-{}", self.issued),
            principal_uid,
            run_id: run_id.to_string(),
            accounts,
            model,
            issued_at: now,
            expires_at: now + lifetime,
            revoked: false,
        };
        self.by_hash.insert(hash(&token), grant.clone());
        Ok((token, grant))
    }

    /// Looks up a presented token.
    pub fn check(&self, token: &str, uid: u32, now: i64) -> Result<&Grant, GrantError> {
        let grant = self.by_hash.get(&hash(token)).ok_or(GrantError::Unknown)?;
        // Checked before expiry, so a caller using somebody else's token is told
        // the same thing whether or not that token happens to be current.
        if grant.principal_uid != uid {
            return Err(GrantError::WrongPrincipal);
        }
        if grant.revoked {
            return Err(GrantError::Revoked);
        }
        if now >= grant.expires_at {
            return Err(GrantError::Expired);
        }
        Ok(grant)
    }

    /// Ends a run's authority. Used when a run is cancelled.
    ///
    /// By run rather than by token, because the canceller has the run id and
    /// should not need to hold the token to stop it.
    pub fn revoke_run(&mut self, run_id: &str, uid: u32) -> usize {
        let mut revoked = 0;
        for grant in self.by_hash.values_mut() {
            if grant.run_id == run_id && grant.principal_uid == uid && !grant.revoked {
                grant.revoked = true;
                revoked += 1;
            }
        }
        revoked
    }

    /// Drops grants that have expired, so the map does not grow without bound.
    pub fn forget_stale(&mut self, now: i64) {
        // Revoked grants are kept until expiry on purpose: a caller still
        // presenting one should be told the run was cancelled, rather than that
        // its grant never existed.
        self.by_hash.retain(|_, grant| now < grant.expires_at);
    }

    pub fn live_count(&self, now: i64) -> usize {
        self.by_hash
            .values()
            .filter(|grant| grant.live(now))
            .count()
    }
}

fn hash(token: &str) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}
