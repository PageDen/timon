#!/usr/bin/env python3
# Cross-checks the two counters that produced the pilot's cost ratio.
#
# Why this exists: the ratio "orchestration cost 1.365x the strong model" divides
# numbers produced by two different instruments. run-pilot.sh took the
# single-model arms' totals from inline Python summing `turn.completed` usage out
# of the saved stream, and the orchestrated arm's totals from Timon's own adapter
# via outcome.json. If those two count differently -- about cached input,
# reasoning tokens, or startup prewarm -- the ratio is a property of the
# instruments rather than of the models.
#
# After the citation verifier and the success rubric were both found broken the
# first time they met real output, the cost counter was the last load-bearing
# instrument in this project never checked against an independent computation on
# real data. It is checkable for nothing, because the streams are on disk.
#
# Both halves are checked:
#   * single-model arms: harness Python against Timon's adapter, replaying each
#     saved stream through `timon worker run --usage-source stdout` so the real
#     code path runs rather than a reimplementation of it;
#   * orchestrated runs: Timon's recorded lead_tokens/worker_tokens in
#     outcome.json against Python summing the children's saved stdout.
#
# Run: python3 eval/cross-check-counters.py [<qual-dir>]
import glob
import json
import os
import subprocess
import sys
import tempfile

TIMON = "/home/workbench/work/timon/target/release/timon"


def harness_total(path):
    """Exactly what run-pilot.sh computed, reproduced without touching it."""
    total = 0
    for line in open(path, errors="replace"):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except Exception:
            continue
        if event.get("type") == "turn.completed":
            usage = event.get("usage") or {}
            total += (usage.get("input_tokens") or 0) + (usage.get("output_tokens") or 0)
    return total


def turns(path):
    """How many turns the stream reports. See the note about accumulation."""
    count = 0
    for line in open(path, errors="replace"):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            if json.loads(line).get("type") == "turn.completed":
                count += 1
        except Exception:
            pass
    return count


def timon_total(path, accounting="delta"):
    """Timon's own adapter, over the same bytes.

    Replayed by handing the saved stream to `cat` as the worker, so the attempt
    supervisor and the usage adapter both run exactly as they do in a real run.
    Costs nothing and reaches no network.
    """
    with tempfile.TemporaryDirectory() as out:
        done = subprocess.run(
            [TIMON, "worker", "run", "--output-dir", os.path.join(out, "run"),
             "--deadline-secs", "120", "--usage-source", "stdout",
             "--usage-accounting", accounting, "--", "cat", path],
            input="replay", capture_output=True, text=True,
        )
    if done.returncode != 0:
        return None, f"timon exited {done.returncode}: {done.stderr.strip()[:120]}"
    try:
        usage = (json.loads(done.stdout).get("usage") or {})
    except Exception as error:
        return None, f"unparsable report: {error}"
    return usage.get("total"), None


def main(qual):
    disagreements = []
    single_turn_only = True
    checked = 0

    print("Single-model arms: harness Python vs Timon's adapter, same bytes\n")
    print(f"  {'stream':<52}{'python':>10}{'timon':>10}{'turns':>7}  agree")
    for path in sorted(glob.glob(f"{qual}/**/stream.jsonl", recursive=True)):
        if os.path.getsize(path) == 0:
            continue
        n = turns(path)
        if n == 0:
            continue
        if n > 1:
            single_turn_only = False
        py = harness_total(path)
        mine, error = timon_total(path)
        checked += 1
        agree = error is None and py == mine
        if not agree:
            disagreements.append((path, py, mine, error))
        label = path[len(qual):].lstrip("/")
        print(f"  {label:<52}{py:>10}{str(mine):>10}{n:>7}  {'yes' if agree else 'NO'}")
        if error:
            print(f"      {error}")

    print("\nOrchestrated runs: Timon's outcome.json vs Python over the children's stdout\n")
    print(f"  {'run':<30}{'lead':>18}{'workers':>20}  agree")
    for outcome in sorted(glob.glob(f"{qual}/**/orchestrated/outcome.json", recursive=True)):
        directory = os.path.dirname(outcome)
        recorded = json.load(open(outcome))
        lead, work = recorded.get("lead_tokens") or 0, recorded.get("worker_tokens") or 0
        lead_py = sum(harness_total(p) for p in glob.glob(f"{directory}/run/lead-*/stdout.log"))
        work_py = sum(harness_total(p) for p in glob.glob(f"{directory}/run/worker-*/stdout.log"))
        agree = lead == lead_py and work == work_py
        checked += 1
        if not agree:
            disagreements.append((outcome, (lead, work), (lead_py, work_py), None))
        label = outcome[len(qual):].lstrip("/").replace("/orchestrated/outcome.json", "")
        print(f"  {label:<30}{f'{lead} / {lead_py}':>18}{f'{work} / {work_py}':>20}  "
              f"{'yes' if agree else 'NO'}")

    print(f"\n{checked} comparisons, {len(disagreements)} disagreement(s)")
    for item in disagreements:
        print(f"  {item}")

    if single_turn_only:
        print(
            "\nScope of this result. Every stream checked reports exactly one\n"
            "`turn.completed`, so it cannot distinguish `delta` from `cumulative`\n"
            "accounting: with one turn, summing deltas and taking the last snapshot\n"
            "are the same arithmetic. The accumulation path is covered by synthetic\n"
            "tests in tests/usage.rs but has never been checked against real Codex\n"
            "output, because no real multi-turn stream exists here. If a workload\n"
            "ever produces one, which mode matches Codex becomes load-bearing and\n"
            "the default (delta) is an assumption, not an observation."
        )
    return 1 if disagreements else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else "/home/workbench/work/timon-qual"))
