// Adapted for Timon. Not derived from Prodex source.
//! Deciding whether a claim's citation actually supports it.
//!
//! Three verdicts, not two. "Supported" and "unsupported" are the interesting
//! ones, but most of the honesty lives in the third: a citation nobody could
//! check is *unverifiable*, and calling that either of the others would be a
//! lie in one direction or the other.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::research::fetch::{self, FetchError};

/// One claim as a research worker reports it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Claim {
    pub text: String,
    /// `sourced` or `inference`. Only a sourced claim is checked; an inference
    /// makes no claim about a page.
    pub kind: String,
    #[serde(default)]
    pub source_urls: Vec<String>,
    /// The passage the worker says supports the claim.
    #[serde(default)]
    pub evidence: String,
}

/// What a research worker returned.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Findings {
    #[serde(default)]
    pub claims: Vec<Claim>,
    #[serde(default)]
    pub unsupported: Vec<String>,
}

/// The verdict on one claim.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// The quoted passage is on the page it cites.
    Supported { url: String, final_url: String },
    /// It is not. Either the citation is wrong or the passage was invented.
    Unsupported { reason: String },
    /// It could not be checked, so nothing is claimed either way.
    Unverifiable { reason: String },
    /// An inference, which cites nothing and is not checked.
    NotChecked,
}

impl Verdict {
    /// True only for a claim actually shown to be supported.
    ///
    /// Unverifiable is deliberately not a pass. A route that cannot be checked
    /// must not be able to launder claims through by being unavailable.
    pub fn is_supported(&self) -> bool {
        matches!(self, Verdict::Supported { .. })
    }
}

/// One claim and what became of it.
#[derive(Clone, Debug, Serialize)]
pub struct Checked {
    pub claim: Claim,
    pub verdict: Verdict,
}

/// The result of checking a set of findings.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub checked: Vec<Checked>,
    pub supported: usize,
    pub unsupported: usize,
    pub unverifiable: usize,
    /// True when every sourced claim was shown to be supported. A set with
    /// nothing to check does not pass: it has demonstrated nothing.
    pub all_sourced_claims_supported: bool,
    pub basis: &'static str,
}

/// What a check does and does not establish.
pub const VERIFY_BASIS: &str = "Each sourced claim was checked by fetching the page it cites and \
looking for the passage it quotes. That establishes the passage exists on that page, not that the \
page is correct, nor that the claim follows from it.";

/// Checks every claim in a set of findings.
pub fn check(findings: &Findings, timeout: Duration) -> Report {
    let mut checked = Vec::new();
    for claim in &findings.claims {
        let verdict = check_claim(claim, timeout);
        checked.push(Checked {
            claim: claim.clone(),
            verdict,
        });
    }
    let supported = checked.iter().filter(|c| c.verdict.is_supported()).count();
    let unsupported = checked
        .iter()
        .filter(|c| matches!(c.verdict, Verdict::Unsupported { .. }))
        .count();
    let unverifiable = checked
        .iter()
        .filter(|c| matches!(c.verdict, Verdict::Unverifiable { .. }))
        .count();
    let sourced = checked
        .iter()
        .filter(|c| !matches!(c.verdict, Verdict::NotChecked))
        .count();
    Report {
        checked,
        supported,
        unsupported,
        unverifiable,
        all_sourced_claims_supported: sourced > 0 && supported == sourced,
        basis: VERIFY_BASIS,
    }
}

/// Checks one claim.
pub fn check_claim(claim: &Claim, timeout: Duration) -> Verdict {
    if claim.kind != "sourced" {
        return Verdict::NotChecked;
    }
    // Structural failures first, and they are failures rather than
    // unverifiable: a sourced claim that cites nothing, or quotes nothing, has
    // not been prevented from supporting itself. It simply does not.
    let Some(url) = claim.source_urls.iter().find(|u| !u.trim().is_empty()) else {
        return Verdict::Unsupported {
            reason: "a sourced claim with no citation".to_string(),
        };
    };
    let excerpt = normalise(&claim.evidence);
    if excerpt.is_empty() {
        return Verdict::Unsupported {
            reason: "a sourced claim with a URL but no quoted passage: a plausible link is not \
evidence"
                .to_string(),
        };
    }
    if excerpt.len() < MIN_EXCERPT_CHARS {
        // A three-word excerpt would match almost any page, so a pass would mean
        // nothing. Refusing is not the same as disproving it.
        return Verdict::Unverifiable {
            reason: format!(
                "the quoted passage is too short to check ({} characters, {MIN_EXCERPT_CHARS} \
needed)",
                excerpt.len()
            ),
        };
    }

    match fetch::get(url, timeout) {
        Ok(page) => {
            let text = normalise(&strip_markup(&page.body));
            if text.contains(&excerpt) {
                Verdict::Supported {
                    url: url.clone(),
                    final_url: page.final_url,
                }
            } else {
                Verdict::Unsupported {
                    reason: format!(
                        "the quoted passage is not on {}{}",
                        page.final_url,
                        if page.followed_meta_refresh {
                            " (after following a meta refresh)"
                        } else {
                            ""
                        }
                    ),
                }
            }
        }
        // A refusal to fetch, or an address aimed at this host's own network,
        // is a decision about the citation rather than a failure to check it.
        Err(error @ (FetchError::Rejected(_) | FetchError::BlockedAddress(_))) => {
            Verdict::Unsupported {
                reason: format!("{error}"),
            }
        }
        // 404 and 410 say the cited page does not exist. That is evidence
        // against the citation, not a transient problem, so the claim fails
        // rather than escaping into "could not check".
        Err(FetchError::Status(status)) => verdict_for_status(status),
        // Everything else genuinely says nothing about the claim: a paywall, a
        // bot block, rate limiting or a server fault are not the claim's fault,
        // and reporting them as unsupported would punish a true claim for a
        // network problem.
        Err(error) => Verdict::Unverifiable {
            reason: format!("{error}"),
        },
    }
}

/// The verdict a response status alone implies.
///
/// Exposed so the rule can be checked without standing up a server for each
/// status, which would otherwise be tested through the loopback guard that
/// refuses such a URL before any status exists.
pub fn verdict_for_status(status: u16) -> Verdict {
    match status {
        404 | 410 => Verdict::Unsupported {
            reason: format!("the cited page does not exist (HTTP {status})"),
        },
        other => Verdict::Unverifiable {
            reason: format!("the page returned HTTP {other}"),
        },
    }
}

/// Shortest passage worth trying to match.
pub const MIN_EXCERPT_CHARS: usize = 24;

/// Collapses text so quoting differences in whitespace do not matter.
fn normalise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        // Curly quotes and dashes travel badly between a page and a quotation.
        out.push(match ch {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201c}' | '\u{201d}' => '"',
            '\u{2013}' | '\u{2014}' => '-',
            '\u{00a0}' => ' ',
            other => other,
        });
    }
    out
}

/// Removes tags and decodes the handful of entities that matter, so a quotation
/// is compared against what a reader would see.
fn strip_markup(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut depth = 0usize;
    let mut chars = html.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            '&' => {
                let mut entity = String::new();
                while let Some(&next) = chars.peek() {
                    if next == ';' || entity.len() > 8 {
                        chars.next();
                        break;
                    }
                    entity.push(next);
                    chars.next();
                }
                out.push_str(match entity.as_str() {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    "apos" | "#39" => "'",
                    "nbsp" | "#160" => " ",
                    "middot" | "#183" => "·",
                    _ => " ",
                });
            }
            other => out.push(other),
        }
    }
    out
}
