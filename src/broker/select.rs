// Adapted for Timon. Not derived from Prodex source.
//! Which pooled account serves the next request.
//!
//! Three things decide it, in this order.
//!
//! *Continuity.* A conversation that started on one account stays there. The
//! provider holds server-side state against a thread, so moving a thread between
//! accounts mid-conversation is not load balancing, it is losing the thread.
//! Affinity is keyed on the `thread-id` header, which Codex sends on every turn
//! and which was confirmed present against a live client.
//!
//! The thread id alone is not the key. It arrives in a header, so it is whatever
//! the caller says it is; two callers sending the same value would otherwise
//! share one binding, and one of them could park a conversation on an account by
//! naming a thread belonging to somebody else. The key is the caller's uid, taken
//! from the kernel, together with the thread. A caller can only ever collide with
//! itself.
//!
//! *Capability.* Accounts are not interchangeable. Testing found an account whose
//! catalog advertised a model the account could not actually be served, so a
//! refusal for a model is remembered against that account and that model, and
//! excluded from later choices until it expires.
//!
//! *Headroom.* Among what is left, the account with the most of its weekly window
//! remaining goes first. Read from the provider rather than counted locally,
//! because usage from outside this broker is otherwise invisible.
//!
//! This module holds no credentials and performs no I/O. Everything it needs is
//! passed in, so the decision can be tested exactly as it will be made.

use std::collections::HashMap;

use crate::broker::quota::Standing;

/// How long an account is left out for a model it refused.
///
/// Long enough that a run of requests does not keep retrying an account that
/// cannot serve the model; short enough that an entitlement added to the account
/// is picked up the same hour.
pub const REFUSAL_SECS: i64 = 3600;

/// How long a thread stays bound to its account after its last turn.
///
/// A conversation resumed the next day has no server-side state worth preserving,
/// and holding every thread forever would grow without bound.
pub const AFFINITY_SECS: i64 = 12 * 3600;

/// Why no account could be chosen.
///
/// Separate cases because the operator's next action differs: an empty pool needs
/// an account added, an exhausted pool needs to wait for a reset, and a pool that
/// refused a particular model needs either a different model or an entitlement.
#[derive(Debug, PartialEq, Eq)]
pub enum NoAccount {
    PoolEmpty,
    AllExhausted,
    NoneServesModel { model: String },
}

impl std::fmt::Display for NoAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoAccount::PoolEmpty => write!(f, "the broker has no usable pooled account"),
            NoAccount::AllExhausted => write!(
                f,
                "every pooled account has reached its limit; the request was not forwarded"
            ),
            NoAccount::NoneServesModel { model } => write!(
                f,
                "no pooled account can serve {model}; it was refused on every account tried"
            ),
        }
    }
}

/// What the broker knows about the pool right now.
#[derive(Debug, Default)]
pub struct Pool {
    /// Latest usage reading per account name. Absent means never read.
    standings: HashMap<String, Standing>,
    /// `(account, model)` to the time the exclusion lapses.
    refusals: HashMap<(String, String), i64>,
    /// `(uid, thread-id)` to the account serving it, and when it was last seen.
    ///
    /// The uid is in the key because the thread is not trustworthy on its own:
    /// it is a header value, and headers come from the caller.
    affinity: HashMap<(u32, String), (String, i64)>,
}

impl Pool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a usage reading.
    pub fn observe(&mut self, account: &str, standing: Standing) {
        self.standings.insert(account.to_string(), standing);
    }

    pub fn standing(&self, account: &str) -> Option<&Standing> {
        self.standings.get(account)
    }

    /// Notes that an account could not serve a model, so it is not tried again
    /// for that model until the exclusion lapses.
    pub fn refused(&mut self, account: &str, model: &str, now: i64) {
        self.refusals
            .insert((account.to_string(), model.to_string()), now + REFUSAL_SECS);
    }

    /// True when this account is currently excluded for this model.
    pub fn refuses(&self, account: &str, model: &str, now: i64) -> bool {
        self.refusals
            .get(&(account.to_string(), model.to_string()))
            .is_some_and(|until| *until > now)
    }

    /// Binds one caller's thread to the account that served it.
    pub fn bind(&mut self, uid: u32, thread: &str, account: &str, now: i64) {
        self.affinity
            .insert((uid, thread.to_string()), (account.to_string(), now));
    }

    /// The account this caller's thread is bound to, if the binding is still live.
    pub fn bound(&self, uid: u32, thread: &str, now: i64) -> Option<&str> {
        self.affinity
            .get(&(uid, thread.to_string()))
            .filter(|(_, seen)| now - *seen <= AFFINITY_SECS)
            .map(|(account, _)| account.as_str())
    }

    /// Drops bindings and refusals that have lapsed.
    ///
    /// Called on selection rather than on a timer: the map only grows when a
    /// request arrives, so that is the only time it needs pruning.
    pub fn forget_stale(&mut self, now: i64) {
        self.affinity
            .retain(|_, (_, seen)| now - *seen <= AFFINITY_SECS);
        self.refusals.retain(|_, until| *until > now);
    }

    /// Chooses the account for one request.
    ///
    /// `candidates` are the pooled accounts with no fault of their own, in store
    /// order. `model` is what the request asked for, when it said.
    ///
    /// An account with no usage reading is eligible. A missing reading means the
    /// quota endpoint has not been asked yet, which is not evidence of exhaustion,
    /// and refusing to serve until a reading exists would make the broker depend
    /// on an undocumented endpoint being up.
    pub fn choose(
        &mut self,
        candidates: &[String],
        model: Option<&str>,
        caller: Option<(u32, &str)>,
        exclude: &[String],
        now: i64,
    ) -> Result<String, NoAccount> {
        self.forget_stale(now);
        if candidates.is_empty() {
            return Err(NoAccount::PoolEmpty);
        }

        let eligible = |account: &String| -> bool {
            if exclude.contains(account) {
                return false;
            }
            if let Some(model) = model
                && self.refuses(account, model, now)
            {
                return false;
            }
            self.standings
                .get(account)
                .map(|standing| standing.usable())
                .unwrap_or(true)
        };

        // Continuity first: an account that is already carrying this caller's
        // thread wins even when another has more headroom.
        if let Some((uid, thread)) = caller
            && let Some(bound) = self.bound(uid, thread, now).map(str::to_string)
            && candidates.contains(&bound)
            && eligible(&bound)
        {
            return Ok(bound);
        }

        let chosen = candidates
            .iter()
            .filter(|account| eligible(account))
            .enumerate()
            // Highest headroom wins. Ties break on the earlier position in the
            // store, which is why the position is part of the key: `max_by_key`
            // keeps the *last* maximum, so without the reversed index two
            // accounts with equal headroom would resolve backwards.
            .max_by_key(|(position, account)| {
                let headroom = self
                    .standings
                    .get(*account)
                    .map(Standing::headroom)
                    .unwrap_or(50);
                (headroom, std::cmp::Reverse(*position))
            })
            .map(|(_, account)| account.clone());

        match chosen {
            Some(account) => Ok(account),
            None => Err(self.why_none(candidates, model, exclude, now)),
        }
    }

    /// Explains an empty selection in terms the operator can act on.
    fn why_none(
        &self,
        candidates: &[String],
        model: Option<&str>,
        exclude: &[String],
        now: i64,
    ) -> NoAccount {
        let remaining: Vec<&String> = candidates
            .iter()
            .filter(|account| !exclude.contains(account))
            .collect();
        if remaining.is_empty() {
            // Everything was tried on this request. If a model was named and any
            // excluded account refused it, that is the specific cause.
            if let Some(model) = model
                && candidates
                    .iter()
                    .any(|account| self.refuses(account, model, now))
            {
                return NoAccount::NoneServesModel {
                    model: model.to_string(),
                };
            }
            return NoAccount::AllExhausted;
        }
        if let Some(model) = model
            && remaining
                .iter()
                .all(|account| self.refuses(account, model, now))
        {
            return NoAccount::NoneServesModel {
                model: model.to_string(),
            };
        }
        NoAccount::AllExhausted
    }
}

/// The model a request asked for, read from its JSON body.
///
/// Lenient on purpose. A body that is not JSON, or has no `model`, yields `None`
/// and the request is still served: selection then falls back to headroom alone.
/// Refusing a request because the broker could not parse its body would make the
/// broker stricter than the provider it fronts.
pub fn model_of(body: &[u8]) -> Option<String> {
    /// Bodies above this are not parsed. A turn's prompt measured about 49 KB in
    /// testing; this leaves generous room while keeping the parse cost bounded.
    const PARSE_LIMIT: usize = 4 * 1024 * 1024;

    if body.len() > PARSE_LIMIT || !body.first().is_some_and(|byte| *byte == b'{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value
        .get("model")?
        .as_str()
        .filter(|model| !model.is_empty())
        .map(str::to_string)
}

/// The thread a request belongs to, read from its headers.
///
/// `thread-id` is what Codex sends and what is preferred. `session-id` carried the
/// same value in testing and is accepted as a fallback; `x-codex-window-id` is
/// `thread:window` and its thread part is the last resort.
pub fn thread_of(headers: &[(String, String)]) -> Option<String> {
    let find = |want: &str| {
        headers
            .iter()
            .find(|(name, _)| name == want)
            .map(|(_, value)| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    find("thread-id")
        .or_else(|| find("session-id"))
        .or_else(|| {
            find("x-codex-window-id").map(|window| match window.split_once(':') {
                Some((thread, _)) => thread.to_string(),
                None => window,
            })
        })
}
