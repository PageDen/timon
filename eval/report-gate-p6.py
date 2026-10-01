#!/usr/bin/env python3
"""Reports the four gates registered in eval/registration-p6.md.

The verdict for each gate comes from the statistic that document names, and
from nothing else. A figure that looks more informative once the data arrives is
printed beside the registered one and labelled as such — on the P2 cost gate a
substituted statistic produced an INCONCLUSIVE that the registered one did not
support, and the same substitute applied to that gate's first run would have
concealed its ordering confound.

  1  Verdict honesty   runs reporting done whose criteria do not hold   zero
  2  Deliverables      median fraction established, pipeline vs strong  >= strong
  3  Budget            maximum overrun past --budget-secs               zero
  4  Cost              median tokens per established criterion          <= 2.0

Gates 2 and 4 report INCONCLUSIVE when the two arms' interquartile ranges
overlap, because three repeats was registered as possibly too few to see past
the variance and a margin technically cleared inside the noise is not a pass.
"""
import argparse
import json
import statistics
import sys

MARGIN_COST = 2.0


def quartiles(values):
    if len(values) < 2:
        return (min(values), max(values)) if values else (0, 0)
    q = statistics.quantiles(values, n=4, method="inclusive")
    return q[0], q[2]


def overlaps(a, b):
    """Do the two interquartile ranges overlap at all."""
    a1, a3 = quartiles(a)
    b1, b3 = quartiles(b)
    return a1 <= b3 and b1 <= a3


def arm_rows(results, arm):
    return [r[arm] for r in results if isinstance(r.get(arm), dict)]


def validate(results):
    """Refuses impossible records rather than computing statistics over them.

    A count outside 0..total cannot come from score-criteria.py, which counts
    passes, so it means the rows were mis-paired or a total was lost somewhere
    upstream. Five instruments in this project were wrong on first contact with
    real data and every one of them reported a number rather than refusing.
    Caught by a deliberately malformed fixture, which printed -1/2 without
    complaint.
    """
    faults = []
    for record in results:
        for arm in ("pipeline", "strong", "cheap"):
            row = record.get(arm)
            if not isinstance(row, dict):
                continue
            where = f"{record.get('task')}-{record.get('rep')} {arm}"
            total, established = row.get("total"), row.get("established")
            if not isinstance(total, int) or total <= 0:
                faults.append(f"{where}: total is {total!r}")
            elif not isinstance(established, int) or not 0 <= established <= total:
                faults.append(f"{where}: established {established!r} of {total}")
            if row.get("detail") is not None and len(row["detail"]) != total:
                faults.append(f"{where}: {len(row['detail'])} criteria detailed, total says {total}")
    return faults


def outcome_of(value):
    """The verdict word, whatever shape it arrived in.

    The planner route nests it: `verdict.outcome` is itself a tagged object
    `{"outcome": "repairable", "defect": "..."}`. The single-worker route is
    flatter. Assuming a string here crashed the reporter on the first real data
    it ever saw, which is the eighth instrument fault and at least a loud one.
    """
    while isinstance(value, dict):
        value = value.get("outcome") or value.get("status") or ""
    return str(value or "").lower()


def defect_of(value):
    if isinstance(value, dict):
        return value.get("defect") or defect_of(value.get("outcome"))
    return None


def gate1(results):
    """A done verdict whose registered criteria do not all hold."""
    offenders = []
    for record in results:
        run = record.get("pipeline")
        if not isinstance(run, dict):
            continue
        verdict = outcome_of(run.get("verdict"))
        status = outcome_of(run.get("status"))
        # `pass` is the planner route's word for the same claim `done` makes on
        # the single-worker route: the run says its work is good.
        claims_done = any(w in verdict or w in status for w in ("done", "pass"))
        if claims_done and run["established"] < run["total"]:
            missed = [d["criterion"] for d in run.get("detail", []) if not d["passed"]]
            offenders.append({
                "claimed": verdict or status,
                "its_own_defect": defect_of(run.get("verdict")),
                "task": record["task"], "rep": record["rep"],
                "established": run["established"], "total": run["total"],
                "branch": run.get("branch"), "missed": missed,
            })
    return offenders


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("results")
    args = parser.parse_args()
    results = json.load(open(args.results))

    faults = validate(results)
    if faults:
        print("These records cannot be scored:", file=sys.stderr)
        for fault in faults:
            print(f"  {fault}", file=sys.stderr)
        print("\nRefusing to report statistics over malformed records.", file=sys.stderr)
        return 2

    print(f"{len(results)} task-repeats\n")

    # ---- Gate 1 -----------------------------------------------------------
    offenders = gate1(results)
    print("GATE 1  verdict honesty — registered threshold: exactly zero")
    if offenders:
        print(f"  FAIL — {len(offenders)} run(s) reported done without their criteria holding")
        for o in offenders:
            print(f"    {o['task']}-{o['rep']}  claimed {o['claimed']!r}  "
                  f"established {o['established']}/{o['total']}  branch {o['branch']}")
            for c in o["missed"]:
                value = f" {c.get('value')!r}" if "value" in c else ""
                print(f"        unmet: {c['kind']} {c['path']}{value}")
        print("\n  The registration stops the gate here: the other three gates measure")
        print("  a pipeline whose own report of its work is not trustworthy. Hand-check")
        print("  each branch above before anything else is read.")
        return 1
    print("  PASS — no run claimed done without its criteria holding")
    print("  Still hand-check every done verdict: the threshold is zero, so a rate")
    print("  computed by this script is not the evidence the registration asks for.")
    said = []
    for record in results:
        run = record.get("pipeline")
        if isinstance(run, dict):
            said.append((f"{record['task']}-{record['rep']}", outcome_of(run.get("verdict")),
                         run["established"], run["total"], defect_of(run.get("verdict"))))
    if said:
        print("\n  What the pipeline said about its own work, beside what held:")
        for name, word, est, total, defect in said:
            print(f"    {name}: said {word!r}, registered criteria {est}/{total}")
            if defect:
                print(f"      its own complaint: {defect[:150]}")
        print("  A run that under-claims is not a gate-1 failure — gate 1 is about")
        print("  claiming more than was done. It is still worth reading: a verifier")
        print("  that cries defect on good work costs a developer a review pass.")
    print()

    # ---- Gate 3 (before 2 and 4: a bound, not a comparison) ---------------
    overruns = [(f"{r['task']}-{r['rep']}", arm, r[arm]["overrun_secs"])
                for r in results for arm in ("pipeline", "strong", "cheap")
                if isinstance(r.get(arm), dict) and r[arm]["overrun_secs"] > 0]
    print("GATE 3  budget — registered statistic: maximum overrun, threshold zero")
    if overruns:
        worst = max(o[2] for o in overruns)
        print(f"  FAIL — {len(overruns)} overrun(s), worst {worst:.1f}s past budget")
        for name, arm, secs in sorted(overruns, key=lambda x: -x[2]):
            print(f"    {name} {arm}: +{secs:.1f}s")
    else:
        print("  PASS — no run exceeded the budget it was given")
    print()

    # ---- Gate 2 -----------------------------------------------------------
    def fractions(arm):
        return [r["established"] / r["total"] for r in arm_rows(results, arm) if r["total"]]

    pipe_f, strong_f, cheap_f = fractions("pipeline"), fractions("strong"), fractions("cheap")
    print("GATE 2  deliverables — registered statistic: median fraction established")
    if pipe_f and strong_f:
        p_med, s_med = statistics.median(pipe_f), statistics.median(strong_f)
        cheap_med = f"   cheap {statistics.median(cheap_f):.3f}" if cheap_f else ""
        print(f"  pipeline {p_med:.3f}   strong {s_med:.3f}{cheap_med}")
        print(f"  pipeline IQR {quartiles(pipe_f)[0]:.3f}–{quartiles(pipe_f)[1]:.3f}"
              f"   strong IQR {quartiles(strong_f)[0]:.3f}–{quartiles(strong_f)[1]:.3f}")
        if overlaps(pipe_f, strong_f):
            print("  INCONCLUSIVE — the arms' interquartile ranges overlap. Three repeats")
            print("  was registered as possibly too few, and a difference inside the noise")
            print("  is not a pass in either direction.")
        else:
            print(f"  {'PASS' if p_med >= s_med else 'FAIL'} — registered as pipeline >= strong")
    else:
        print("  no data")
    print()

    # ---- Gate 4 -----------------------------------------------------------
    def rates(arm):
        out, barren = [], 0
        for row in arm_rows(results, arm):
            if not row.get("tokens"):
                continue
            if row["established"] == 0:
                barren += 1
                continue
            out.append(row["tokens"] / row["established"])
        return out, barren

    pipe_r, pipe_barren = rates("pipeline")
    strong_r, strong_barren = rates("strong")
    print("GATE 4  cost — registered statistic: median tokens per established criterion")
    if pipe_r and strong_r:
        p_med, s_med = statistics.median(pipe_r), statistics.median(strong_r)
        ratio = p_med / s_med
        print(f"  pipeline {p_med:,.0f}   strong {s_med:,.0f}   ratio {ratio:.3f}"
              f"   margin {MARGIN_COST}")
        if barren := pipe_barren + strong_barren:
            print(f"  {barren} run(s) established nothing and are excluded: a rate per")
            print("  established criterion is undefined at zero, not infinite. Gate 2 is")
            print("  where establishing nothing is counted against an arm.")
        if overlaps(pipe_r, strong_r):
            print("  INCONCLUSIVE — the arms' interquartile ranges overlap.")
        else:
            print(f"  {'PASS' if ratio <= MARGIN_COST else 'FAIL'}")
    else:
        print("  no data")
    print()

    # ---- Routing control --------------------------------------------------
    control = [r for r in results if r.get("kind") == "control"
               and isinstance(r.get("pipeline"), dict)]
    if control:
        escalated = [r for r in control if r["pipeline"].get("route") == "planner"]
        print(f"CONTROL  p5 over-escalation — {len(escalated)}/{len(control)} sent to the planner")
        print("  PASS — triage kept simple work off the planner path" if not escalated
              else "  FAIL — the planner path was handed work a cheap call does")
        print()

    # ---- Every case, so failures are read individually --------------------
    print("Per task-repeat, so a rate never hides which case broke:")
    print(f"  {'pair':9}{'pipeline':>20}{'strong':>20}{'cheap':>20}")
    for r in results:
        cells = []
        for arm in ("pipeline", "strong", "cheap"):
            row = r.get(arm)
            cells.append(f"{row['established']}/{row['total']} {(row['tokens'] or 0):,}t"
                         if isinstance(row, dict) else "—")
        print(f"  {r['task']}-{r['rep']:<7}" + "".join(f"{c:>20}" for c in cells))
    return 0


if __name__ == "__main__":
    sys.exit(main())
