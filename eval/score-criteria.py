#!/usr/bin/env python3
"""Scores a result branch against the criteria registered in the P6 suite.

The pipeline's own verifier decides done/repairable/blocked with a model call
against criteria another model call wrote. This gate exists partly to test that
verifier, so scoring it with the verifier would be circular. Everything here is
path and string checks; nothing judges anything.

Four criterion shapes, the same four src/acceptance.rs supports:

  file_exists   the path is a file in the tree
  file_absent   the path is not present
  contains      the file exists and contains the value
  absent_text   the file exists and does not contain the value

`absent_text` requires the file to exist, which is not a detail. Read the other
way, "accounts-public.md does not contain acct2" is vacuously true of a file
nobody wrote, and a worker that skipped the deliverable entirely would score the
redaction criteria as passes. That is the shape of every instrument fault this
project has had: a check that is satisfied by the absence of the work.

`contains` folds case unless the criterion says `exact`, because the tasks ask
for prose and no gate here is about whether a sentence began with a capital.
Criteria naming a code identifier set `exact`.
"""
import argparse
import json
import shutil
import sys
import tempfile
from pathlib import Path


def check(criterion, root):
    """Returns (passed, note). Never raises on a missing or unreadable file."""
    kind = criterion["kind"]
    target = root / criterion["path"]

    if kind == "file_exists":
        return target.is_file(), "present" if target.is_file() else "missing"

    if kind == "file_absent":
        return not target.exists(), "absent" if not target.exists() else "still present"

    if kind not in ("contains", "absent_text"):
        raise ValueError(f"unknown criterion kind: {kind}")

    # Both text shapes require the file. See the module docstring.
    if not target.is_file():
        return False, "file missing"
    try:
        text = target.read_text(errors="replace")
    except OSError as exc:
        return False, f"unreadable: {exc}"

    value = criterion["value"]
    if not criterion.get("exact"):
        text, value = text.lower(), value.lower()
    found = value in text

    if kind == "contains":
        return found, "found" if found else "not found"
    return not found, "absent" if not found else "still present"


def score(suite, root, task_ids=None):
    rows = []
    for task in suite["tasks"]:
        if task_ids and task["id"] not in task_ids:
            continue
        results = [check(c, root) for c in task["criteria"]]
        rows.append({
            "id": task["id"],
            "kind": task["kind"],
            "established": sum(1 for passed, _ in results if passed),
            "total": len(results),
            "detail": [
                {"criterion": c, "passed": passed, "note": note}
                for c, (passed, note) in zip(task["criteria"], results)
            ],
        })
    return rows


SELF_TEST = [
    # (kind, files to create, criterion, expected)
    ("file_exists present", {"a.md": "x"}, {"kind": "file_exists", "path": "a.md"}, True),
    ("file_exists missing", {}, {"kind": "file_exists", "path": "a.md"}, False),
    ("file_absent when absent", {}, {"kind": "file_absent", "path": "a.md"}, True),
    ("file_absent when present", {"a.md": "x"}, {"kind": "file_absent", "path": "a.md"}, False),
    ("contains found", {"a.md": "uses headroom last"},
     {"kind": "contains", "path": "a.md", "value": "headroom"}, True),
    ("contains not found", {"a.md": "nothing relevant"},
     {"kind": "contains", "path": "a.md", "value": "headroom"}, False),
    ("contains folds case", {"a.md": "Continuity first"},
     {"kind": "contains", "path": "a.md", "value": "continuity"}, True),
    ("exact respects case", {"a.md": "headwithuncaptured"},
     {"kind": "contains", "path": "a.md", "value": "HeadWithUncaptured", "exact": True}, False),
    ("exact matches exactly", {"a.md": "Base::HeadWithUncaptured {"},
     {"kind": "contains", "path": "a.md", "value": "HeadWithUncaptured", "exact": True}, True),
    ("contains on missing file fails", {},
     {"kind": "contains", "path": "a.md", "value": "x"}, False),
    ("absent_text when redacted", {"a.md": "the two pooled accounts"},
     {"kind": "absent_text", "path": "a.md", "value": "acct2"}, True),
    ("absent_text when not redacted", {"a.md": "acct2 is live"},
     {"kind": "absent_text", "path": "a.md", "value": "acct2"}, False),
    # The trap this checker exists to avoid.
    ("absent_text on missing file FAILS, not vacuously passes", {},
     {"kind": "absent_text", "path": "a.md", "value": "acct2"}, False),
    ("absent_text folds case", {"a.md": "ACCT2 is live"},
     {"kind": "absent_text", "path": "a.md", "value": "acct2"}, False),
    ("numeral found as substring", {"a.md": "the value is 43200 seconds"},
     {"kind": "contains", "path": "a.md", "value": "43200"}, True),
]


def self_test():
    failures = []
    for name, files, criterion, expected in SELF_TEST:
        root = Path(tempfile.mkdtemp())
        try:
            for rel, body in files.items():
                (root / rel).write_text(body)
            got, note = check(criterion, root)
            mark = "ok  " if got == expected else "FAIL"
            if got != expected:
                failures.append(f"{name}: expected {expected}, got {got} ({note})")
            print(f"  {mark} {name}")
        finally:
            shutil.rmtree(root, ignore_errors=True)
    print(f"\n{len(SELF_TEST) - len(failures)}/{len(SELF_TEST)} self-tests pass")
    for failure in failures:
        print(f"  {failure}")
    return 1 if failures else 0


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--suite", default=str(Path(__file__).parent / "gate-p6-suite.json"))
    parser.add_argument("--tree", help="Checkout of the result branch to score")
    parser.add_argument("--task", action="append", help="Score only these task ids")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if not args.tree:
        parser.error("--tree is required unless --self-test")

    suite = json.load(open(args.suite))
    rows = score(suite, Path(args.tree), set(args.task) if args.task else None)

    if args.json:
        print(json.dumps(rows, indent=2))
        return 0

    for row in rows:
        print(f"{row['id']:4} {row['kind']:12} {row['established']}/{row['total']}")
        for item in row["detail"]:
            if not item["passed"]:
                c = item["criterion"]
                value = f" {c.get('value')!r}" if "value" in c else ""
                print(f"       miss  {c['kind']} {c['path']}{value} — {item['note']}")
    total = sum(r["established"] for r in rows)
    print(f"\nestablished {total}/{sum(r['total'] for r in rows)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
