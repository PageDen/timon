// Adapted for Timon. Not derived from Prodex source.
//! Which model serves a request, and saying so.
//!
//! Timon chooses the model, not the developer. Codex lets a user pick one in the
//! app or with a flag, so the only place that choice can actually be enforced is
//! the broker — it is the single path to a provider.
//!
//! Enforcement is the easy half. The half that matters is **visibility**: a
//! broker that silently rewrites `model` leaves a developer reading output from
//! a model they do not know they are using, debugging a difference they cannot
//! see. Every substitution here is announced on the response, counted, and
//! logged. Codex's review asked for exactly this and it was right to.
//!
//! One model per conversation. The first request on a thread fixes it, and a
//! later policy change applies to new conversations rather than reaching into
//! one already going. Switching models between turns of a live conversation is
//! untested and may break a session's state, so the plan does not do it.

use serde::Serialize;

/// The header a client can read to learn what actually served its request.
pub const EFFECTIVE_HEADER: &str = "x-timon-model";
/// Present only when the request asked for something else.
pub const REQUESTED_HEADER: &str = "x-timon-model-requested";
/// Which policy made the decision, so two hosts can be told apart.
pub const POLICY_HEADER: &str = "x-timon-model-policy";

/// What the operator configured.
///
/// An absent `assign` means no policy: requests pass through untouched. That is
/// the default deliberately, so installing this release changes nothing until
/// somebody decides it should.
#[derive(Clone, Debug, Default)]
pub struct ModelPolicy {
    /// The model interactive sessions get.
    pub assign: Option<String>,
    /// Models a caller may keep if it asks for them, beyond the assigned one.
    ///
    /// For the case where a developer legitimately needs a specific model and
    /// the operator agrees in advance. Empty means only the assigned model.
    pub allowed: Vec<String>,
    /// Names this policy, so a substitution can say which rule produced it.
    pub version: String,
}

impl ModelPolicy {
    /// True when nothing is configured and the broker should not interfere.
    pub fn absent(&self) -> bool {
        self.assign.is_none()
    }

    fn permits(&self, model: &str) -> bool {
        self.assign.as_deref() == Some(model) || self.allowed.iter().any(|m| m == model)
    }
}

/// What the broker decided about one request's model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    /// Forward unchanged: no policy, or the request already complies.
    PassThrough { effective: Option<String> },
    /// The request asked for something policy does not permit.
    Substituted {
        requested: String,
        effective: String,
    },
    /// The request named no model and policy supplied one.
    Assigned { effective: String },
}

impl Decision {
    /// The model that will actually serve, when one is known.
    pub fn effective(&self) -> Option<&str> {
        match self {
            Decision::PassThrough { effective } => effective.as_deref(),
            Decision::Substituted { effective, .. } | Decision::Assigned { effective } => {
                Some(effective)
            }
        }
    }

    /// Whether the body must be rewritten before forwarding.
    pub fn rewrites(&self) -> bool {
        matches!(
            self,
            Decision::Substituted { .. } | Decision::Assigned { .. }
        )
    }
}

/// Applies policy to one request.
///
/// `bound` is the model this conversation is already using, if it has one. It
/// wins over the policy's current assignment, because a policy change applies to
/// new conversations rather than reaching into one already going.
pub fn decide(policy: &ModelPolicy, requested: Option<&str>, bound: Option<&str>) -> Decision {
    if policy.absent() {
        return Decision::PassThrough {
            effective: requested.map(str::to_string),
        };
    }

    // A conversation keeps the model it started with.
    let target = bound
        .map(str::to_string)
        .or_else(|| policy.assign.clone())
        .unwrap_or_default();

    match requested {
        None => Decision::Assigned { effective: target },
        Some(asked) if asked == target => Decision::PassThrough {
            effective: Some(target),
        },
        // Explicitly permitted, and not already bound to something else.
        Some(asked) if bound.is_none() && policy.permits(asked) => Decision::PassThrough {
            effective: Some(asked.to_string()),
        },
        Some(asked) => Decision::Substituted {
            requested: asked.to_string(),
            effective: target,
        },
    }
}

/// Rewrites the `model` field of a request body.
///
/// Returns `None` when the body is not JSON or has nothing to change, so the
/// caller forwards the original bytes rather than a reconstruction of them. A
/// broker that re-serialises every body would quietly reorder fields and change
/// what it claims to be a transparent proxy.
pub fn rewrite_model(body: &[u8], effective: &str) -> Option<Vec<u8>> {
    let mut value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object_mut()?;
    let current = object.get("model").and_then(|m| m.as_str());
    if current == Some(effective) {
        return None;
    }
    object.insert(
        "model".to_string(),
        serde_json::Value::String(effective.to_string()),
    );
    serde_json::to_vec(&value).ok()
}

/// The headers announcing what served this request.
///
/// Added to the response so the client learns the effective model without having
/// to ask. A substitution the developer cannot see is the thing this exists to
/// prevent.
pub fn headers(policy: &ModelPolicy, decision: &Decision) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(effective) = decision.effective() {
        out.push((EFFECTIVE_HEADER.to_string(), effective.to_string()));
    }
    if let Decision::Substituted { requested, .. } = decision {
        out.push((REQUESTED_HEADER.to_string(), requested.clone()));
    }
    if !policy.absent() && !policy.version.is_empty() {
        out.push((POLICY_HEADER.to_string(), policy.version.clone()));
    }
    out
}
