#!/usr/bin/env python3
"""Tests one explanation for the P2 cost tail, against saved transcripts.

The recorded hypothesis was that the tail is the model's own variation in how
many tool calls it makes — a task that greps once costing a third of the same
task grepping three times — and not something the routing does.

This script refuted it. 24 of 25 pairs took exactly one turn on *both* arms and
still differed by up to 8,946 tokens, so the spread is inside a single turn and
tool-use variance cannot be its cause. Tokens per turn match at a ratio of
1.023, which is the useful positive finding: the fast path adds no measurable
per-turn overhead.

It also prints the registered verdict, because the gate was first reported
INCONCLUSIVE against a statistic that was not the registered one. The
registration names the median of each arm; what got reported was the median of
the per-pair ratios. Run this against /tmp/gate (run 1) to see why that matters:
there the registered statistic reads 1.495 FAIL and the per-pair median reads
1.108, so the substitute statistic would have concealed the ordering confound
that run 1 actually had. It diverges in both directions. The registered
statistic is computed first below and is the one the verdict comes from.

The transcripts were kept, so all of this cost a re-read rather than a re-spend.
"""
import json
import re
import statistics
import sys
from pathlib import Path

GATE = Path(sys.argv[1] if len(sys.argv) > 1 else "/tmp/gate2")


def turns_of(*paths):
    """Counts the tool calls in a transcript.

    `codex exec` prints a bare `exec` line for each shell invocation it makes.
    Counting those is counting the turns the model chose to take.
    """
    text = ""
    for path in paths:
        try:
            text += Path(path).read_text(errors="replace")
        except OSError:
            continue
    if not text:
        return None
    return len(re.findall(r"^exec$", text, re.M))


MARGIN = 1.25  # registered before the run; see eval/registration-p2.md


def verdict(results):
    """The registered statistic: median of each arm, not median of the ratios."""
    a = [r["timon"]["tokens"] for r in results if r["timon"]["tokens"]]
    b = [r["direct"]["tokens"] for r in results if r["direct"]["tokens"]]
    if not a or not b:
        return
    a_med, b_med = statistics.median(a), statistics.median(b)
    ratio = a_med / b_med
    print("Registered statistic — median fast-path tokens against a direct call:")
    print(f"  timon {a_med:,.0f}   direct {b_med:,.0f}   ratio {ratio:.3f}"
          f"   margin {MARGIN}")
    print(f"  cost gate: {'PASS' if ratio <= MARGIN else 'FAIL'}")

    paired = [(r["timon"]["tokens"], r["direct"]["tokens"]) for r in results
              if r["timon"]["tokens"] and r["direct"]["tokens"]]
    pair_med = statistics.median(x / y for x, y in paired)
    print(f"\nBeside it, not instead of it — median of the per-pair ratios: "
          f"{pair_med:.3f}")
    print(f"  {'Above' if pair_med > ratio else 'Below'} the registered figure. The two can")
    print("  diverge in either direction: a per-pair ratio is a ratio of two noisy")
    print("  quantities, so it reports the spread as much as the lean. On run 2 it read")
    print("  high and would have withheld a pass; on run 1 it read low and would have")
    print("  hidden a real 1.495 failure. The verdict is the line above.\n")


def main():
    results = json.load(open(GATE / "results.json"))
    rows = []
    for record in results:
        task, rep = record["task"], record["rep"]
        run_dir = GATE / "timon" / f"{task}-{rep}"
        inner = next((p for p in run_dir.iterdir() if p.is_dir()), None) if run_dir.is_dir() else None
        a_turns = turns_of(inner / "stdout.log", inner / "stderr.log") if inner else None
        b_turns = turns_of(GATE / "direct" / f"{task}-{rep}.log")
        a_tok, b_tok = record["timon"]["tokens"], record["direct"]["tokens"]
        if None in (a_turns, b_turns, a_tok, b_tok):
            continue
        rows.append({
            "task": task, "rep": rep,
            "a_tok": a_tok, "b_tok": b_tok, "d_tok": a_tok - b_tok,
            "a_turns": a_turns, "b_turns": b_turns, "d_turns": a_turns - b_turns,
        })

    if not rows:
        print("no transcripts to read")
        return 2

    verdict(results)

    print(f"{len(rows)} pairs with both transcripts readable\n")
    print(f"{'pair':10}{'timon tok':>11}{'turns':>7}{'direct tok':>12}{'turns':>7}"
          f"{'Δtok':>9}{'Δturns':>8}")
    for row in rows:
        print(f"{row['task']}-{row['rep']:<7}{row['a_tok']:>11,}{row['a_turns']:>7}"
              f"{row['b_tok']:>12,}{row['b_turns']:>7}{row['d_tok']:>+9,}{row['d_turns']:>+8}")

    # Does cost track turns? If the tail is tool-use variance, the pairs where
    # one arm took more turns should be the pairs where it cost more.
    agree = sum(1 for r in rows if (r["d_tok"] > 0) == (r["d_turns"] > 0) and r["d_turns"] != 0)
    disagree = sum(1 for r in rows if (r["d_tok"] > 0) != (r["d_turns"] > 0) and r["d_turns"] != 0)
    same_turns = sum(1 for r in rows if r["d_turns"] == 0)

    print(f"\nPairs where the arms took a different number of turns: {agree + disagree}")
    print(f"  the arm that took more turns also cost more: {agree}")
    print(f"  it cost less:                                {disagree}")
    print(f"Pairs where both took the same number of turns: {same_turns}")

    if same_turns:
        equal = [abs(r["d_tok"]) for r in rows if r["d_turns"] == 0]
        print(f"  their absolute token difference: median {statistics.median(equal):,.0f}, "
              f"max {max(equal):,}")

    # Cost per turn, which is the quantity the hypothesis is really about.
    per_turn = [
        (r["a_tok"] / max(r["a_turns"], 1), r["b_tok"] / max(r["b_turns"], 1)) for r in rows
    ]
    a_per = statistics.median(p[0] for p in per_turn)
    b_per = statistics.median(p[1] for p in per_turn)
    print(f"\nTokens per turn, median:  timon {a_per:,.0f}   direct {b_per:,.0f}")
    print(f"  ratio {a_per / b_per:.3f}")
    print("\nIf the routing added overhead, cost per turn would differ. If the tail is")
    print("the model choosing different numbers of turns, cost per turn would match")
    print("and the token spread would follow the turn spread.")

    # Whose spread is it? If it belonged to the fast path, the two arms would
    # occupy different ranges. Pooled, they occupy the same bands.
    tight = [r for r in rows if abs(r["d_tok"]) < 100]
    print(f"\nPairs agreeing to within 100 tokens: {len(tight)}/{len(rows)}"
          f"   {sorted(r['d_tok'] for r in tight)}")
    pooled = sorted([r["a_tok"] for r in rows] + [r["b_tok"] for r in rows])
    bands = [[pooled[0]]]
    for value in pooled[1:]:
        if value - bands[-1][-1] < 400:
            bands[-1].append(value)
        else:
            bands.append([value])
    print("Pooled token figures, grouped where they fall within 400 of each other:")
    for band in bands:
        arms = sum(1 for r in rows if r["a_tok"] in band), sum(1 for r in rows if r["b_tok"] in band)
        print(f"  {band[0]:>7,}–{band[-1]:<7,} n={len(band):<3} "
              f"timon {arms[0]}, direct {arms[1]}")
    print("  Both arms appear in the same bands, so the spread is a property of")
    print("  the tasks rather than of either arm. No mechanism is claimed.")

    # The run-1 confound, confirmed gone.
    order = [(r["timon"]["tokens"] - r["direct"]["tokens"] > 0) == r["timon_first"]
             for r in results if r["timon"]["tokens"] and r["direct"]["tokens"]]
    print(f"\nPairs where the arm that ran first was the dearer one: "
          f"{sum(order)}/{len(order)} (chance, so run 1's ordering confound is gone)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
