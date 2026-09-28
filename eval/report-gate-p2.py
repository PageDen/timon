#!/usr/bin/env python3
"""Reports the P2 gate against the criteria registered before it ran.

Reports the spread, not just a ratio. The first run of this gate produced a
median ratio of 1.495 over paired deltas ranging from -3,862 to +6,826 tokens —
a number that looked like a verdict and was not one. A result that cannot be
distinguished from noise has to say so.
"""
import json
import statistics
import sys

# Registered in eval/registration-p2.md on 2026-09-28, before the gate ran.
COST_MARGIN = 0.25


def sign_test(deltas):
    """How lopsided the paired differences are.

    Not a p-value: with samples this small an exact test would be more
    ceremony than evidence. It answers one question — did one arm win most of
    the pairs, or did they trade?
    """
    wins = sum(1 for d in deltas if d < 0)   # timon cheaper
    losses = sum(1 for d in deltas if d > 0)
    return wins, losses


def main():
    results = json.load(open(sys.argv[1]))
    pairs = [(r["timon"], r["direct"]) for r in results]
    n = len(pairs)
    if not n:
        print("no results")
        return 2

    paired = [(a["tokens"], b["tokens"]) for a, b in pairs if a["tokens"] and b["tokens"]]
    a_ok = sum(1 for a, _ in pairs if a["accepted"])
    b_ok = sum(1 for _, b in pairs if b["accepted"])

    print(f"P2 gate — {n} paired runs, {len(paired)} with tokens on both arms\n")
    print(f"{'':10}{'tokens median':>15}{'tokens IQR':>22}{'accepted':>11}{'latency s':>12}")
    for name, toks, ok, secs in (
        ("timon", [a for a, _ in paired], a_ok, [a["secs"] for a, _ in pairs]),
        ("direct", [b for _, b in paired], b_ok, [b["secs"] for _, b in pairs]),
    ):
        if toks:
            q = statistics.quantiles(toks, n=4) if len(toks) >= 4 else [min(toks), statistics.median(toks), max(toks)]
            print(f"{name:10}{statistics.median(toks):>15,.0f}"
                  f"{f'{q[0]:,.0f} – {q[2]:,.0f}':>22}{ok:>8}/{n}{statistics.median(secs):>12.1f}")

    if not paired:
        print("\ntokens were not reported on both arms; the cost gate cannot be decided")
        return 2

    deltas = [a - b for a, b in paired]
    median_delta = statistics.median(deltas)
    spread = max(deltas) - min(deltas)
    wins, losses = sign_test(deltas)

    print(f"\npaired delta (timon − direct), tokens:")
    print(f"  median {median_delta:+,.0f}   range {min(deltas):+,} to {max(deltas):+,}   spread {spread:,}")
    print(f"  timon cheaper in {wins} of {len(deltas)} pairs, dearer in {losses}")

    # The decision, and the honesty about whether it is one.
    ratio = statistics.median([a for a, _ in paired]) / statistics.median([b for _, b in paired])
    print(f"\ncost   median ratio {ratio:.3f} ({ratio - 1:+.1%}), registered margin {COST_MARGIN:+.0%}")

    decisive = spread < abs(median_delta) * 2 and min(wins, losses) <= len(deltas) * 0.25
    if not decisive:
        print("       gate: INCONCLUSIVE — the spread is larger than the effect and the")
        print("             arms traded wins. More repeats are needed before this decides")
        print("             anything. Reporting a pass or fail from this would be a claim")
        print("             the data does not support.")
        cost_state = None
    else:
        cost_state = (ratio - 1) <= COST_MARGIN
        print(f"       gate: {'PASS' if cost_state else 'FAIL'}")

    quality_pass = a_ok >= b_ok
    print(f"quality {a_ok}/{n} accepted against {b_ok}/{n} direct")
    print(f"       gate: {'PASS' if quality_pass else 'FAIL'} (not worse than direct)")

    bad = [r for r in results if not r["timon"]["accepted"] or not r["direct"]["accepted"]]
    if bad:
        print("\nTasks an arm failed:")
        for r in bad:
            for arm in ("timon", "direct"):
                if not r[arm]["accepted"]:
                    print(f"  {r['task']} rep{r['rep']} [{arm}]: {r[arm]['answer']!r}")

    print("\nBoth gates must pass. Low overhead on a bad answer is not success.")
    if cost_state is None:
        return 2
    return 0 if (cost_state and quality_pass) else 1


if __name__ == "__main__":
    sys.exit(main())
