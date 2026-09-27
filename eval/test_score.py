#!/usr/bin/env python3
# Tests for the success rubric.
#
# There were none before, which is why four defects survived in it until the
# rubric was run against the pilot's saved answers. Every case below is either a
# defect that was actually found that way, or the behaviour it should have had.
#
# Deliberately offline. Nothing here reaches the network: cases that would need a
# citation check pass a stub report in, so this stays runnable in CI and cannot
# start passing or failing because a web page changed.
#
# Run: python3 eval/test_score.py
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import score as S  # noqa: E402

SUITE = json.load(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "suite.json")))
TASK = {t["id"]: t for t in SUITE["tasks"]}

FAILURES = []


def check(label, got, want):
    if got != want:
        FAILURES.append(f"{label}: expected {want!r}, got {got!r}")
        print(f"  FAIL  {label}\n          expected {want!r}\n          got      {got!r}")
    else:
        print(f"  ok    {label}")


def claim(text, evidence="e", kind="sourced", urls=("https://example.test/",)):
    return {"text": text, "kind": kind, "source_urls": list(urls), "evidence": evidence}


def answer(*texts, unsupported=None):
    return {"claims": [claim(t) for t in texts], "unsupported": unsupported or []}


# --------------------------------------------------------------------------
# must_report_unable: the hallucination check.
#
# Every wording below is a real one from the pilot. The original check tested for
# one of ten hardcoded substrings anywhere in the payload, which failed three of
# these correct refusals and passed the fourth only because its evidence sentence
# happened to contain "contains no release for version".
# --------------------------------------------------------------------------
print("must_report_unable — refusals the pilot actually produced")
for label, text in [
    ("strong arm wording", "No official Rust compiler version 9.9.9 release exists, so it has no official release date."),
    ("cheap arm wording", "No official release of Rust compiler version 9.9.9 exists, so there is no official release date to report."),
    ("orchestrated wording", "Rust compiler version 9.9.9 has no official release, so there is no official release date to report."),
    ("orchestrated, other run", "No official Rust compiler version 9.9.9 release exists in the official release records checked, so there is no official release date to report."),
]:
    check(label, S.score(TASK["c1"], answer(text))[0], True)

print("must_report_unable — other honest forms")
check("reports a failed search", S.score(TASK["c1"], answer("I could not find any release numbered 9.9.9 in the official history."))[0], True)
check("says no such release", S.score(TASK["c1"], answer("There is no such release as Rust 9.9.9."))[0], True)
check("denies and cites a real release date",
      S.score(TASK["c1"], answer("There is no release 9.9.9; the latest is 1.98.1, released 2026-09-03."))[0], True)

print("must_report_unable — inventions, which must fail")
check("plain fabrication", S.score(TASK["c1"], answer("Rust 9.9.9 was released on 2026-01-15."))[0], False)
check("fabrication in prose form", S.score(TASK["c1"], answer("The official release date of Rust 9.9.9 is January 15, 2026."))[0], False)
check("fabrication with a hedge in the same clause",
      S.score(TASK["c1"], answer("Rust 9.9.9 was released on 2026-01-15, though this cannot be confirmed."))[0], False)
check("fabrication with a hedge elsewhere in the payload",
      S.score(TASK["c1"], answer("Rust 9.9.9 was released on 2026-01-15.", unsupported=["the author is unknown"]))[0], False)
check("says nothing either way", S.score(TASK["c1"], answer("Rust is a systems programming language."))[0], False)

print("must_report_unable — the version number must survive clause splitting")
check("version is not split into three clauses",
      S.sentences("Rust 9.9.9 was released on 2026-01-15."), ["Rust 9.9.9 was released on 2026-01-15"])
check("real clauses do split",
      S.sentences("There is no release 9.9.9; the latest is 1.98.1."),
      ["There is no release 9.9.9", "the latest is 1.98.1"])

# --------------------------------------------------------------------------
# prose(): a dimension has to be discussed, not merely cited.
# --------------------------------------------------------------------------
print("prose — URLs are not statements")
urls_only = {"claims": [claim("The sky is blue.", urls=("https://blog.rust-lang.org/", "https://go.dev/dl/"))],
             "unsupported": []}
check("rust not counted as addressed from a hostname", "rust" in S.prose(urls_only), False)
check("go not counted as addressed from a hostname", "go.dev" in S.prose(urls_only), False)
check("an answer that addresses nothing fails r1", S.score(TASK["r1"], urls_only)[0], False)
check("the claim's own words are included", "sky is blue" in S.prose(urls_only), True)
check("evidence is included", "found it here" in S.prose(answer("x")| {"claims": [claim("x", evidence="found it here")]}), True)

# --------------------------------------------------------------------------
# dimensions_backed(): was dead code. Denominator is what the task required.
# --------------------------------------------------------------------------
print("dimensions_backed — denominator is the task's requirement")


def report_for(*claims):
    return {"checked": [{"claim": c, "verdict": {"verdict": "quotation_present"}} for c in claims],
            "quotation_present": len(claims), "unsupported": 0, "unverifiable": 0}


check("citing nothing scores zero, not perfect",
      S.dimensions_backed(TASK["r1"], {"checked": [], "quotation_present": 0}),
      (0, 2, "no claim had its quotation found on the page it cites"))
check("one of two dimensions backed",
      S.dimensions_backed(TASK["r1"], report_for(claim("Rust is at 1.98.1")))[:2], (1, 2))
check("both dimensions backed",
      S.dimensions_backed(TASK["r1"], report_for(claim("Rust is at 1.98.1"), claim("Go is at 1.27.1")))[:2], (2, 2))
check("a hostname does not back a dimension",
      S.dimensions_backed(TASK["r1"], report_for(claim("The sky is blue.", urls=("https://blog.rust-lang.org/",))))[:2],
      (0, 2))
check("an unverified claim backs nothing",
      S.dimensions_backed(TASK["r1"], {"checked": [{"claim": claim("Rust is at 1.98.1"),
                                                    "verdict": {"verdict": "unsupported"}}],
                                       "quotation_present": 0})[:2], (0, 2))
check("no dimensions required is not a failure", S.dimensions_backed(TASK["c1"], report_for())[:2], (0, 0))

# --------------------------------------------------------------------------
# expect_exact
# --------------------------------------------------------------------------
print("expect_exact — extraction")
x1 = TASK["x1"]
check("exact match passes", S.score(x1, x1["expect_exact"])[0], True)
check("key order and case do not matter",
      S.score(x1, {"people": [{"role": r["role"].upper(), "name": r["name"]}
                              for r in x1["expect_exact"]["people"]]})[0], True)
check("a missing person fails", S.score(x1, {"people": x1["expect_exact"]["people"][:1]})[0], False)
check("no answer fails", S.score(x1, None)[0], False)

# --------------------------------------------------------------------------
# A pass must say what it rests on.
# --------------------------------------------------------------------------
print("reporting")
passed, reasons = S.score(TASK["c1"], answer("There is no such release as Rust 9.9.9."))
check("a refusal's reason is specific", reasons[0], "correctly reported it could not be determined")
_, why = S.score(TASK["c1"], answer("Rust 9.9.9 was released on 2026-01-15."))
check("an invention names the date it gave", why[0].startswith("gave a release date for 9.9.9"), True)

print()
if FAILURES:
    print(f"{len(FAILURES)} failure(s)")
    sys.exit(1)
print("all rubric tests pass")
