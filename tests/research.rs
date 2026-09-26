//! Citation checking: the guards, and the verdicts.
//!
//! Every cited URL here is treated as hostile input, because in production it
//! is: a model chose it, influenced by pages the model read, and pointed it at
//! this host.

use std::net::{IpAddr, TcpListener};
use std::time::Duration;

use serde_json::json;
use timon::research::fetch::{self, FetchError, is_blocked, meta_refresh};
use timon::research::verify::{self, Claim, Findings, Verdict};

const QUICK: Duration = Duration::from_secs(3);

fn claim(value: serde_json::Value) -> Claim {
    serde_json::from_value(value).unwrap()
}

fn sourced(url: &str, evidence: &str) -> Claim {
    claim(json!({
        "text": "some claim about the world",
        "kind": "sourced",
        "source_urls": [url],
        "evidence": evidence
    }))
}

// --- the guards -----------------------------------------------------------

#[test]
fn every_address_this_host_should_not_be_made_to_reach_is_blocked() {
    for address in [
        "127.0.0.1",        // loopback
        "127.5.5.5",        // the rest of loopback
        "10.0.0.1",         // private
        "172.16.0.1",       // private
        "192.168.1.1",      // private
        "169.254.169.254",  // link-local, the cloud metadata address
        "100.64.0.1",       // carrier-grade NAT
        "198.18.0.1",       // benchmarking
        "192.0.0.1",        // IETF protocol assignments
        "0.0.0.0",          // unspecified
        "224.0.0.1",        // multicast
        "255.255.255.255",  // broadcast
        "::1",              // v6 loopback
        "::",               // v6 unspecified
        "fd00::1",          // v6 unique local
        "fe80::1",          // v6 link-local
        "::ffff:127.0.0.1", // v4 loopback wearing a v6 hat
        "::ffff:169.254.169.254",
    ] {
        let ip: IpAddr = address.parse().unwrap();
        assert!(is_blocked(ip), "{address} must be blocked");
    }
}

#[test]
fn ordinary_public_addresses_are_not_blocked() {
    // A guard that blocks everything is not a guard, it is an outage.
    for address in ["93.184.216.34", "1.1.1.1", "2606:4700::1111"] {
        let ip: IpAddr = address.parse().unwrap();
        assert!(!is_blocked(ip), "{address} must not be blocked");
    }
}

#[test]
fn a_citation_aimed_at_this_host_is_refused_before_anything_connects() {
    // A listener that would answer if we let it. Nothing must reach it.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || listener.accept().is_ok());

    let result = fetch::get(&format!("http://127.0.0.1:{port}/secret"), QUICK);

    assert!(
        matches!(result, Err(FetchError::BlockedAddress(_))),
        "expected the address to be blocked, got {result:?}"
    );
    drop(handle); // never accepted
}

#[test]
fn a_name_that_resolves_to_loopback_is_blocked_too() {
    // The check is on the resolved address, not the name, because a name can
    // point anywhere and an attacker controls the name.
    let result = fetch::get("http://localhost/", QUICK);
    assert!(
        matches!(result, Err(FetchError::BlockedAddress(_))),
        "got {result:?}"
    );
}

#[test]
fn only_http_and_https_are_fetched() {
    for url in [
        "file:///etc/passwd",
        "gopher://example.com/",
        "ftp://example.com/",
        "data:text/html,hello",
        "jar:http://example.com!/",
    ] {
        assert!(
            matches!(fetch::get(url, QUICK), Err(FetchError::Rejected(_))),
            "{url} must be refused"
        );
    }
}

#[test]
fn a_url_carrying_credentials_is_refused() {
    let result = fetch::get("http://user:password@example.com/", QUICK);
    assert!(
        matches!(result, Err(FetchError::Rejected(_))),
        "got {result:?}"
    );
}

#[test]
fn a_malformed_url_is_refused_rather_than_guessed_at() {
    for url in ["", "notaurl", "http://", "http:///path"] {
        assert!(
            matches!(fetch::get(url, QUICK), Err(FetchError::Rejected(_))),
            "{url:?} must be refused"
        );
    }
}

#[test]
fn a_meta_refresh_target_is_found_so_a_redirect_stub_is_followed() {
    // PR3 found a genuine citation behind one of these. A verifier that stopped
    // at the stub would have called a true claim unsupported.
    let stub = r#"<!DOCTYPE html><meta charset="utf-8"><title>Redirect</title>
        <meta http-equiv="refresh" content="0; url=/2026/09/03/Rust-1.98.1/">"#;
    assert_eq!(
        meta_refresh(stub).as_deref(),
        Some("/2026/09/03/Rust-1.98.1/")
    );
    assert_eq!(
        meta_refresh("<html><body>no refresh here</body></html>"),
        None
    );
    // A refresh with a delay and single quotes is still a refresh.
    assert!(meta_refresh("<meta http-equiv='refresh' content='5; url=/next'>").is_some());
}

// --- the verdicts ---------------------------------------------------------

#[test]
fn a_sourced_claim_with_no_citation_is_unsupported_not_unverifiable() {
    let claim = claim(json!({
        "text": "a bare assertion", "kind": "sourced", "source_urls": [], "evidence": "something"
    }));
    match verify::check_claim(&claim, QUICK) {
        Verdict::Unsupported { reason } => assert!(reason.contains("no citation")),
        other => panic!("expected unsupported, got {other:?}"),
    }
}

#[test]
fn a_url_with_no_quoted_passage_is_unsupported_because_a_link_is_not_evidence() {
    // The round-3 rule, in code: if the route returns only a plausible URL, the
    // claim does not pass.
    let claim = sourced("https://example.com/page", "");
    match verify::check_claim(&claim, QUICK) {
        Verdict::Unsupported { reason } => {
            assert!(reason.contains("plausible link is not"), "got {reason}");
        }
        other => panic!("expected unsupported, got {other:?}"),
    }
}

#[test]
fn an_inference_is_not_checked_because_it_cites_nothing() {
    let claim = claim(json!({
        "text": "probably true", "kind": "inference", "source_urls": [], "evidence": ""
    }));
    assert_eq!(verify::check_claim(&claim, QUICK), Verdict::NotChecked);
}

#[test]
fn a_passage_too_short_to_mean_anything_is_unverifiable_not_supported() {
    // A three-word excerpt would match almost any page, so a pass would be
    // meaningless. Refusing to judge is not the same as disproving it.
    let claim = sourced("https://example.com/", "the rust");
    match verify::check_claim(&claim, QUICK) {
        Verdict::Unverifiable { reason } => assert!(reason.contains("too short")),
        other => panic!("expected unverifiable, got {other:?}"),
    }
}

#[test]
fn a_citation_pointing_at_the_host_network_is_unsupported_not_unverifiable() {
    // A blocked address is a decision about the citation, not a failure to
    // check it, so it counts against the claim.
    let claim = sourced(
        "http://169.254.169.254/latest/meta-data/",
        "a passage long enough to be checked against a page",
    );
    match verify::check_claim(&claim, QUICK) {
        Verdict::Unsupported { reason } => assert!(reason.contains("non-public")),
        other => panic!("expected unsupported, got {other:?}"),
    }
}

#[test]
fn unverifiable_is_never_counted_as_a_pass() {
    // The property that matters: a route that cannot be checked must not be able
    // to launder claims through by being unavailable.
    let findings = Findings {
        claims: vec![sourced(
            "https://example.invalid/nothing-here",
            &"x".repeat(40),
        )],
        unsupported: vec![],
    };
    let report = verify::check(&findings, Duration::from_millis(600));

    assert_eq!(report.supported, 0);
    assert!(!report.all_sourced_claims_supported);
    assert!(
        report.unverifiable + report.unsupported == 1,
        "one claim, not verified: {report:?}"
    );
}

#[test]
fn findings_with_nothing_to_check_do_not_pass() {
    let findings = Findings {
        claims: vec![claim(json!({
            "text": "no sources at all", "kind": "inference", "source_urls": [], "evidence": ""
        }))],
        unsupported: vec![],
    };
    let report = verify::check(&findings, QUICK);
    assert!(
        !report.all_sourced_claims_supported,
        "demonstrating nothing is not the same as demonstrating support"
    );
}

#[test]
fn the_report_says_what_a_check_does_and_does_not_establish() {
    let report = verify::check(
        &Findings {
            claims: vec![],
            unsupported: vec![],
        },
        QUICK,
    );
    assert!(report.basis.contains("not that the page is correct"));
    assert!(report.basis.contains("nor that the claim follows from it"));
}

#[test]
fn a_citation_to_a_page_that_does_not_exist_is_unsupported_not_unverifiable() {
    // A 404 is evidence about the citation: the page it names is not there. Only
    // a problem that says nothing about the claim -- a paywall, a bot block, a
    // server fault -- earns "could not check".
    for (status, expected_unsupported) in [(404u16, true), (410, true), (403, false), (503, false)]
    {
        let verdict = timon::research::verify::verdict_for_status(status);
        assert_eq!(
            matches!(verdict, Verdict::Unsupported { .. }),
            expected_unsupported,
            "HTTP {status} mapped wrongly: {verdict:?}"
        );
    }
}

// --- the report as a whole -------------------------------------------------

#[test]
fn one_unsupported_claim_is_enough_to_fail_a_set() {
    // A set passes only if every sourced claim was shown to be supported, so a
    // single fabricated citation cannot ride along with good ones.
    let findings = Findings {
        claims: vec![
            claim(json!({
                "text": "an inference", "kind": "inference",
                "source_urls": [], "evidence": ""
            })),
            sourced("https://example.com/x", ""),
        ],
        unsupported: vec![],
    };
    let report = verify::check(&findings, QUICK);

    assert!(!report.all_sourced_claims_supported);
    assert_eq!(report.unsupported, 1);
}

#[test]
fn an_inference_alongside_a_supported_claim_does_not_drag_the_set_down() {
    // Inferences are not checked, so they must not count as failures either.
    let findings = Findings {
        claims: vec![claim(json!({
            "text": "an inference", "kind": "inference", "source_urls": [], "evidence": ""
        }))],
        unsupported: vec![],
    };
    let report = verify::check(&findings, QUICK);
    assert_eq!(report.unsupported, 0);
    assert_eq!(report.supported, 0);
}

#[test]
fn a_status_says_something_about_the_citation_only_when_it_is_about_the_page() {
    use timon::research::verify::verdict_for_status;
    // The page is not there: that is the citation's problem.
    for gone in [404u16, 410] {
        assert!(matches!(
            verdict_for_status(gone),
            Verdict::Unsupported { .. }
        ));
    }
    // A paywall, a bot block, rate limiting or a server fault say nothing about
    // whether the claim is true.
    for elsewhere in [401u16, 403, 429, 500, 503] {
        assert!(
            matches!(verdict_for_status(elsewhere), Verdict::Unverifiable { .. }),
            "HTTP {elsewhere} must not count against the claim"
        );
    }
}
