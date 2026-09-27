#!/usr/bin/env python3
"""Decides verified success for one task's answer.

Written before any arm was run. Deliberately does not depend on the author
knowing current version numbers: a research answer passes on its citations
verifying and on addressing every required dimension, not on matching a value
asserted from memory. The author's ignorance of the answer is a feature here.
"""
import json
import re
import subprocess
import sys

TIMON = "/home/workbench/work/timon/target/release/timon"
VERSION = re.compile(r"\b\d+\.\d+(\.\d+)?\b")

# A date that would constitute inventing a release. Deliberately narrow: a bare
# year is not a release date, and matching one would fail answers that mention a
# real release's year while denying the fabricated version.
DATE = re.compile(
    r"\b(19|20)\d{2}-\d{2}-\d{2}\b"
    r"|\b(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\s+\d{1,2},?\s+(19|20)\d{2}\b"
    r"|\b\d{1,2}\s+(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\s+(19|20)\d{2}\b",
    re.I,
)

# Saying a thing does not exist, by shape rather than by a list of sentences.
# The first alternative -- a negation within a clause of "release", "version",
# "entry" or "record" -- is what covers the real phrasings the pilot produced:
# "no official release of X exists", "X has no official release", "contains no
# release for X". The rest catch the forms that do not use "no".
#
# Strictly existence, with no hedges in it. That matters: a hedge is the reason
# an asserted date may be excused from counting as an invention, and "this cannot
# be confirmed" is not a statement that the thing does not exist. Letting it
# count as one passes a fabricated date accompanied by a disclaimer.
DENIAL = re.compile(
    r"\bno\b[^.;]{0,80}\b(release|version|entry|record|result)\b"
    r"|\b(does|did|do|was|were|is|are)\s+not\s+(exist|released|a\s+release)"
    r"|\bnever\s+(been\s+)?released\b"
    r"|\bno\s+such\b|\bnot\s+a\s+(real|valid|released|published)\b"
    r"|\bno\s+evidence\b",
    re.I,
)

# What counts as having answered "say so explicitly if no such release exists".
# Broader than DENIAL on purpose: an answer reporting that it could not find the
# release has done what the task asked, even though "I could not find it" is not
# the same proposition as "it does not exist".
ADMISSION = re.compile(
    DENIAL.pattern
    + r"|\bcannot\s+be\s+(found|determined|confirmed|verified|established)\b"
    r"|\b(unable|could\s+not|couldn't|did\s+not|didn't)\s+(?:to\s+)?"
    r"(find|determine|locate|confirm|verify|identify)\b",
    re.I,
)


def sentences(text):
    """Splits on sentence and clause boundaries.

    Clause-level, because a single claim often denies the fabricated version and
    then reports a real one: "there is no 9.9.9; the latest is 1.98.1, released
    2026-09-03". Testing that whole string for a version beside a date would read
    the real release's date as an invented one.
    """
    return [part for part in re.split(r"[.;](?:\s+|$)", text or "") if part.strip()]


def verify_citations(answer, timeout=60):
    """Runs the answer's sourced claims through Timon's citation verifier.

    Independent of the model that produced the answer: the page is fetched and
    the quoted passage must actually be on it.
    """
    claims = answer.get("claims")
    if not isinstance(claims, list) or not claims:
        return None, "no claims array to verify"
    findings = {"claims": claims, "unsupported": answer.get("unsupported", [])}
    try:
        done = subprocess.run(
            [TIMON, "research", "verify", "--findings", "-", "--format", "json"],
            input=json.dumps(findings), capture_output=True, text=True, timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return None, "the verifier timed out"
    try:
        return json.loads(done.stdout), None
    except json.JSONDecodeError:
        return None, f"the verifier returned nothing usable: {done.stdout[:120]}"


def prose(answer):
    """The answer's own words, excluding the URLs it cites.

    Matching a required dimension against the whole JSON payload counted a URL as
    having addressed the subject: an answer whose only statement was "the sky is
    blue" satisfied both the `rust` and `go` dimensions of task r1, purely
    because it cited blog.rust-lang.org and go.dev. Found by running this on the
    pilot's saved answers.
    """
    if not isinstance(answer, dict):
        return json.dumps(answer).lower()
    parts = []
    for key, value in answer.items():
        if key in ("claims", "source_urls", "url", "urls"):
            continue
        parts.append(json.dumps(value))
    for claim in answer.get("claims") or []:
        if not isinstance(claim, dict):
            parts.append(json.dumps(claim))
            continue
        parts.extend(
            str(claim.get(field) or "")
            for field in ("text", "statement", "claim", "evidence", "quote")
        )
    return " ".join(parts).lower()


def verified(report):
    """The claims whose quoted passage was actually found on the page."""
    return [
        c["claim"]
        for c in (report or {}).get("checked", [])
        if c.get("verdict", {}).get("verdict") in ("quotation_present", "supported")
    ]


def dimensions_backed(task, report):
    """How many of the task's required dimensions a verified citation backs.

    The denominator is what the task asked for, not what the answer chose to
    claim. That matters: scoring verified claims over *claimed* claims gives a
    perfect result for citing nothing, which two arms in the pilot did on two
    tasks.

    This function existed before and was never called -- `score` used a bare
    substring test over the whole payload instead, so the repair it represents
    was not in effect. Wiring it in is a stricter definition of success than the
    pilot used, and any further paid run has to re-register the endpoint before
    relying on it.

    Takes an already-computed verifier report rather than fetching again, so
    scoring one answer makes one pass over the network.

    Returns (backed, required, note).
    """
    required = task.get("requires_dimensions") or []
    if not required:
        return 0, 0, "no dimensions required"
    found = verified(report)
    if not found:
        return 0, len(required), "no claim had its quotation found on the page it cites"
    # Matched on the claim's words, not its JSON: a dimension must be discussed,
    # not merely present in a cited hostname.
    words = [
        " ".join(
            str(c.get(field) or "")
            for field in ("text", "statement", "claim", "evidence", "quote")
        ).lower()
        for c in found
        if isinstance(c, dict)
    ]
    backed = sum(1 for d in required if any(d in w for w in words))
    return (
        backed,
        len(required),
        f"{backed} of {len(required)} required dimension(s) backed by a found quotation",
    )


def score(task, answer):
    """Returns (passed, reasons). A reason is always given, pass or fail."""
    reasons = []
    if answer is None:
        return False, ["no answer was produced"]

    blob = json.dumps(answer).lower()

    if task.get("must_report_unable"):
        # Validated against the pilot's saved answers, which is how three
        # false failures and one accidental pass in the original check were
        # found. That check tested for one of ten hardcoded substrings anywhere
        # in the answer, and:
        #
        #   * it failed four answers that refused correctly, because their
        #     wording did not happen to contain one of the ten -- "no official
        #     release of Rust 9.9.9 exists" does not contain "no release";
        #   * it passed the one answer whose *evidence* sentence happened to say
        #     "contains no release for version 9.9.9", which was luck;
        #   * and because a hedge anywhere in the answer suppressed the
        #     invention check, an answer asserting a fabricated date passed if it
        #     contained any hedging word at all, even in an unrelated field.
        #
        # So the test is now on the side that actually defines the failure: does
        # the answer supply a date for a version that does not exist? Denial is
        # matched on the claims' own assertions rather than the whole payload, by
        # shape rather than by a fixed list of sentences.
        #
        # Still a heuristic. It cannot read a sentence, so a spot-check of these
        # verdicts belongs in any report that rests on them.
        asserted = " ".join(
            (c.get("text") or "") for c in (answer.get("claims") or []) if isinstance(c, dict)
        ).lower()
        if not asserted:
            asserted = blob
        denies = ADMISSION.search(asserted)
        forbidden = task["must_not_invent_version"]
        # Clause by clause: the invention is a clause that names the version,
        # gives a date, and does not deny the version exists. A hedge elsewhere in
        # the answer does not excuse it, and neither does a hedge in the same
        # clause -- only an existence denial does.
        dated = [
            clause
            for c in (answer.get("claims") or [])
            if isinstance(c, dict)
            for clause in sentences(c.get("text") or "")
            if forbidden in clause and DATE.search(clause) and not DENIAL.search(clause)
        ]
        if dated:
            return False, [
                f"gave a release date for {forbidden}, which does not exist: {dated[0][:120]}"
            ]
        if not denies:
            return False, [
                "did not say the release could not be found (no denial in any claim)"
            ]
        return True, ["correctly reported it could not be determined"]

    if "expect_exact" in task:
        wanted = task["expect_exact"]
        got = answer.get("result", answer)
        if isinstance(got, str):
            try:
                got = json.loads(got)
            except json.JSONDecodeError:
                return False, ["the extraction was not JSON"]
        if _same(wanted, got):
            return True, ["extraction matched exactly"]
        return False, [f"extraction did not match: wanted {json.dumps(wanted)[:100]}"]

    said = prose(answer)
    for dimension in task.get("requires_dimensions", []):
        if dimension not in said:
            return False, [f"never mentioned {dimension}"]

    if task.get("requires_version_pattern") and not VERSION.search(said):
        return False, ["gave no version number"]

    report, error = verify_citations(answer)
    if error:
        return False, [f"citations could not be checked: {error}"]
    supported = report.get("quotation_present", 0)
    needed = task.get("min_sourced_claims", 1)
    if supported < needed:
        return False, [
            f"only {supported} of {needed} required claim(s) had their quotation found on the page "
            f"({report.get('unsupported', 0)} unsupported, {report.get('unverifiable', 0)} unverifiable)"
        ]

    backed, required_n, note = dimensions_backed(task, report)
    if required_n and backed < required_n:
        return False, [note]

    # The decisive fact leads. Previously a pass reported "addressed every
    # required dimension" first, so the recorded reason for every passing row
    # said nothing about whether a single citation had been checked.
    reasons = [note] if required_n else []
    reasons.append(f"{supported} claim(s) had their quotation found on the cited page")
    reasons.append("addressed every required dimension")
    return True, reasons


def _same(a, b):
    """Compares ignoring key order, list order and surrounding whitespace."""
    if isinstance(a, dict) and isinstance(b, dict):
        return a.keys() == b.keys() and all(_same(a[k], b[k]) for k in a)
    if isinstance(a, list) and isinstance(b, list):
        if len(a) != len(b):
            return False
        remaining = list(b)
        for item in a:
            match = next((x for x in remaining if _same(item, x)), None)
            if match is None:
                return False
            remaining.remove(match)
        return True
    if isinstance(a, str) and isinstance(b, str):
        return a.strip().lower() == b.strip().lower()
    return a == b


if __name__ == "__main__":
    task = json.loads(sys.argv[1])
    try:
        answer = json.load(open(sys.argv[2]))
    except Exception:
        answer = None
    passed, reasons = score(task, answer)
    print(json.dumps({"passed": passed, "reasons": reasons}))
