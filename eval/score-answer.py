#!/usr/bin/env python3
"""Decides whether one answer met its acceptance criterion.

Mechanical on purpose. A model judging a model is another instrument to
validate before it can be trusted, and this gate is about cost and correctness
on questions whose answers this repository already states.

Run with --self-test to check the checker against fabricated answers before it
is used on real ones. That order is the project's rule: four instruments here
were wrong on first contact with real data.
"""
import json
import re
import sys


def accepted(answer, criterion):
    """True when the answer satisfies the criterion."""
    if answer is None:
        return False
    text = answer.strip()
    if not text:
        return False
    kind = criterion["kind"]
    if kind == "contains":
        return _present(criterion["value"], text)
    if kind == "all_of":
        return all(_present(v, text) for v in criterion["value"])
    raise ValueError(f"unknown criterion kind {kind!r}")


def _present(value, text):
    """True when `value` appears as a whole token in `text`.

    The dot handling is the fiddly part, and the self-test caught it wrong on
    the first attempt. A trailing full stop ends a sentence — "the value is
    300." is a correct answer — but a dot between digits is part of a number,
    so "1.280" does not contain 280. So a dot only disqualifies when a digit is
    on the other side of it.
    """
    pattern = rf"(?<!\w)(?<!\d\.){re.escape(value)}(?!\w)(?!\.\d)"
    return re.search(pattern, text) is not None


SELF_TEST = [
    # (answer, criterion, expected)
    ("300", {"kind": "contains", "value": "300"}, True),
    ("The value is 300.", {"kind": "contains", "value": "300"}, True),
    ("3000", {"kind": "contains", "value": "300"}, False),
    ("12801", {"kind": "contains", "value": "280"}, False),
    ("1.280", {"kind": "contains", "value": "280"}, False),
    ("", {"kind": "contains", "value": "300"}, False),
    (None, {"kind": "contains", "value": "300"}, False),
    ("   ", {"kind": "contains", "value": "300"}, False),
    ("src/broker/grant.rs", {"kind": "contains", "value": "src/broker/grant.rs"}, True),
    ("grant.rs", {"kind": "contains", "value": "src/broker/grant.rs"}, False),
    (
        "CheapWorker, StrongWorker, Planner",
        {"kind": "all_of", "value": ["CheapWorker", "StrongWorker", "Planner"]},
        True,
    ),
    (
        "CheapWorker and StrongWorker",
        {"kind": "all_of", "value": ["CheapWorker", "StrongWorker", "Planner"]},
        False,
    ),
    (
        "I could not determine the answer.",
        {"kind": "contains", "value": "300"},
        False,
    ),
]


def self_test():
    failures = 0
    for answer, criterion, expect in SELF_TEST:
        got = accepted(answer, criterion)
        if got != expect:
            failures += 1
            print(f"  FAIL  {answer!r} against {criterion} -> {got}, wanted {expect}")
    if failures:
        print(f"{failures} of {len(SELF_TEST)} checker cases failed. The gate must not run.")
        return 1
    print(f"checker self-test: {len(SELF_TEST)} cases, all correct")
    return 0


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        sys.exit(self_test())
    payload = json.load(sys.stdin)
    print(json.dumps({"accepted": accepted(payload["answer"], payload["accept"])}))
