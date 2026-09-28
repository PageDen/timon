#!/usr/bin/env python3
"""Reports the P2 gate against the criteria registered before it ran."""
import json
import statistics
import sys

# Registered in eval/registration-p2.md before any arm was run.
COST_MARGIN = 0.25  # fast path may cost up to 25% more than a direct cheap call


def main():
    results = json.load(open(sys.argv[1]))
    if not results:
        print("no results")
        return 1

    pairs = [(r["timon"], r["direct"]) for r in results]
    a_tokens = [a["tokens"] for a, _ in pairs if a["tokens"]]
    b_tokens = [b["tokens"] for _, b in pairs if b["tokens"]]
    a_ok = sum(1 for a, _ in pairs if a["accepted"])
    b_ok = sum(1 for _, b in pairs if b["accepted"])
    n = len(pairs)

    print(f"P2 gate — {n} paired runs\n")
    print(f"{'':12} {'tokens (median)':>16} {'accepted':>10} {'latency s (median)':>20}")
    for name, toks, ok, secs in (
        ("timon", a_tokens, a_ok, [a["secs"] for a, _ in pairs]),
        ("direct", b_tokens, b_ok, [b["secs"] for _, b in pairs]),
    ):
        median = statistics.median(toks) if toks else float("nan")
        print(f"{name:12} {median:>16,.0f} {ok:>7}/{n} {statistics.median(secs):>20.1f}")

    if not a_tokens or not b_tokens:
        print("\ntokens were not reported for every run; the cost gate cannot be decided")
        return 2

    ratio = statistics.median(a_tokens) / statistics.median(b_tokens)
    overhead = ratio - 1
    print(f"\ncost   ratio {ratio:.3f}  ({overhead:+.1%} against a direct cheap call)")
    cost_pass = overhead <= COST_MARGIN
    print(f"       gate: {'PASS' if cost_pass else 'FAIL'} (registered margin {COST_MARGIN:+.0%})")

    quality_pass = a_ok >= b_ok
    print(f"quality {a_ok}/{n} accepted against {b_ok}/{n} direct")
    print(f"       gate: {'PASS' if quality_pass else 'FAIL'} (not worse than direct)")

    # Failures are named, because a rate hides which task broke.
    bad = [r for r in results if not r["timon"]["accepted"] or not r["direct"]["accepted"]]
    if bad:
        print("\nTasks an arm failed:")
        for r in bad:
            for arm in ("timon", "direct"):
                if not r[arm]["accepted"]:
                    print(f"  {r['task']} rep{r['rep']} [{arm}]: {r[arm]['answer']!r}")

    print()
    print("Both gates must pass. Low overhead on a bad answer is not success.")
    return 0 if (cost_pass and quality_pass) else 1


if __name__ == "__main__":
    sys.exit(main())
