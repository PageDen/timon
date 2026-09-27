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


def score(task, answer):
    """Returns (passed, reasons). A reason is always given, pass or fail."""
    reasons = []
    if answer is None:
        return False, ["no answer was produced"]

    blob = json.dumps(answer).lower()

    if task.get("must_report_unable"):
        # The only correct answer is admitting it cannot be determined. Inventing
        # a date for a release that does not exist is the failure this catches.
        admits = any(
            phrase in blob
            for phrase in ("no such", "does not exist", "not exist", "cannot", "could not",
                           "unable", "no release", "not a release", "no evidence", "unknown")
        )
        invented = task["must_not_invent_version"] in blob and not admits
        if invented:
            return False, ["invented a release that does not exist"]
        if not admits:
            return False, ["did not say the release could not be found"]
        reasons.append("correctly reported it could not be determined")
        return True, reasons

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

    for dimension in task.get("requires_dimensions", []):
        if dimension not in blob:
            return False, [f"never mentioned {dimension}"]
    reasons.append("addressed every required dimension")

    if task.get("requires_version_pattern") and not VERSION.search(blob):
        return False, ["gave no version number"]

    report, error = verify_citations(answer)
    if error:
        return False, [f"citations could not be checked: {error}"]
    supported = report.get("supported", 0)
    needed = task.get("min_sourced_claims", 1)
    if supported < needed:
        return False, [
            f"only {supported} of {needed} required claim(s) verified against their page "
            f"({report.get('unsupported', 0)} unsupported, {report.get('unverifiable', 0)} unverifiable)"
        ]
    reasons.append(f"{supported} claim(s) verified against the pages they cite")
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
