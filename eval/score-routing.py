#!/usr/bin/env python3
"""Scores triage against the routing fixtures, without spending anything.

Triage is deterministic, so this measurement costs nothing and can be run on
every change. That is the point: the instrument exists before the thing it
measures is trusted, which is the rule this project adopted after four
instruments were wrong on first contact with real data.

What it will not do is pretend. A fixture whose expected route disagrees with
the rules is reported as a disagreement, never quietly reconciled. The suite
deliberately contains cases the current rules are expected to get wrong, so a
perfect score here would mean the suite was written to flatter them.
"""
import json
import subprocess
import sys
from collections import Counter

TIMON = "/home/workbench/work/timon/target/release/timon"


def route_of(goal, allow_planner):
    """Asks the real binary, so this measures what ships and not a copy of it."""
    command = [TIMON, "triage", "--goal", goal, "--format", "json"]
    if allow_planner:
        command.append("--allow-planner")
    done = subprocess.run(command, capture_output=True, text=True, timeout=30)
    if done.returncode != 0:
        raise SystemExit(f"timon triage failed: {done.stderr.strip()}")
    return json.loads(done.stdout)


def main():
    suite = json.load(open(sys.argv[1] if len(sys.argv) > 1 else "eval/triage-suite.json"))
    tasks = suite["tasks"]

    agree, disagree = [], []
    confusion = Counter()
    rules = Counter()

    for task in tasks:
        decision = route_of(task["goal"], task.get("allow_planner", True))
        got = decision["route"]
        want = task["expected_route"]
        confusion[(want, got)] += 1
        for reason in decision["reasons"]:
            rules[reason["rule"]] += 1
        record = {
            "id": task["id"],
            "kind": task["kind"],
            "want": want,
            "got": got,
            "why_expected": task["why"],
            "rules_fired": [r["rule"] for r in decision["reasons"]],
            "reasons": [r["detail"] for r in decision["reasons"]],
        }
        (agree if got == want else disagree).append(record)

    total = len(tasks)
    print(f"routing fixtures: {total}")
    print(f"  agree     {len(agree)}")
    print(f"  disagree  {len(disagree)}")
    print()

    if disagree:
        print("Disagreements — each is a finding, not a failure to hide:")
        for record in disagree:
            print(f"  {record['id']} ({record['kind']}): wanted {record['want']}, got {record['got']}")
            print(f"      expected because: {record['why_expected']}")
            print(f"      rules fired: {', '.join(record['rules_fired'])}")
        print()

    print("Rules fired across the suite:")
    for rule, count in rules.most_common():
        print(f"  {count:>3}  {rule}")
    print()

    # Over-routing is the expensive direction: the pilot measured the lead's
    # overhead at ~30,000 tokens per run, so sending single-piece work to the
    # planner costs more than sending planner work to one strong call.
    over = sum(
        count
        for (want, got), count in confusion.items()
        if want != got and got == "planner"
    )
    under = sum(
        count
        for (want, got), count in confusion.items()
        if want == "planner" and got != "planner"
    )
    print(f"over-routed to the planner: {over}   (the expensive direction)")
    print(f"under-routed from the planner: {under}")

    json.dump(
        {
            "total": total,
            "agree": len(agree),
            "disagree": disagree,
            "over_routed": over,
            "under_routed": under,
            "rules": dict(rules),
        },
        open("/dev/stdout" if len(sys.argv) > 2 and sys.argv[2] == "-" else "/dev/null", "w"),
        indent=1,
    )
    # Disagreements are information, not an error: this script reports, and a
    # person decides whether the rules or the expectation should move.
    return 0


if __name__ == "__main__":
    sys.exit(main())
