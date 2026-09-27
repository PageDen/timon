#!/usr/bin/env python3
# Re-scores the pilot's saved answers and checks the rubric against them.
#
# Why this exists: the rubric was repaired after a harness fault voided the
# pilot's quality comparison, and a rubric that has only ever run on hand-written
# fixtures is exactly what let six defects through in the citation verifier. The
# saved answers are real model output, so they are the only material that can
# show what the rubric does in practice.
#
# Two different things are checked:
#
#   1. Agreement. Does the current rubric reach the verdict the pilot recorded?
#      A disagreement is not automatically a fault -- pages change, and the
#      rubric was deliberately changed -- but each one has to be explained.
#   2. Defensibility. Is the verdict right about the answer in front of it? That
#      is a judgement, so this prints what each verdict rested on rather than
#      pretending to settle it.
#
# Run: python3 eval/validate-rubric.py <results-dir> [<results-dir> ...]
#
# Network: scoring a research answer fetches its cited pages, so a verdict
# depends on what those pages say today, not during the pilot.
import csv
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from score import score  # noqa: E402

SUITE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "suite.json")


def predicted_dir(task, index):
    # What the harness names this task's directory, and why it is sometimes
    # wrong.
    #
    # run-pilot.sh interpolates the task JSON into a double-quoted shell string
    # to read its id back out. The shell processes backslashes inside double
    # quotes, so the JSON escape for a quote -- which is what a double quote in a
    # task's own text becomes -- arrives at Python as a bare quote and breaks the
    # parse. The command then falls back to "t<index>".
    #
    # So the directory name disagrees with the task id in results.csv for exactly
    # those tasks whose text contains a double quote. No score is wrong because
    # of it: the score is computed from the task handed to the scorer on stdin,
    # not from the directory. What it breaks is mapping a saved answer back to
    # its task afterwards, which is how the wrong answer gets scored against the
    # wrong task later.
    mangled = '\\"' in json.dumps(task)
    return (f"t{index}" if mangled else task["id"]), mangled


def resolve_dir(results_dir, task, index):
    # Read off the filesystem rather than derived, so this keeps working once the
    # harness defect above is fixed.
    for candidate in (task["id"], f"t{index}"):
        if os.path.isdir(os.path.join(results_dir, candidate)):
            return candidate
    return None


def recorded(results_dir):
    # The verdict the pilot recorded, per (arm, task).
    path = os.path.join(results_dir, "results.csv")
    if not os.path.exists(path):
        return {}
    out = {}
    with open(path) as handle:
        for row in csv.DictReader(handle):
            out[(row["arm"], row["task"])] = (row["ok"] == "1", row["reason"])
    return out


def saved_answer(results_dir, directory, arm):
    # The answer one arm produced, or None with why not.
    if arm == "orchestrated":
        # The orchestrated arm's answer is the lead's integration, which the
        # harness reads out of outcome.json rather than a result file.
        outcome = os.path.join(results_dir, directory, arm, "outcome.json")
        if not os.path.exists(outcome):
            return None, "no outcome.json"
        try:
            blob = json.load(open(outcome)).get("answer")
        except Exception as error:
            return None, f"outcome.json unreadable: {error}"
        if not blob:
            return None, "outcome.json carried no answer"
        try:
            return json.loads(blob), None
        except Exception as error:
            return None, f"answer was not JSON: {error}"
    path = os.path.join(results_dir, directory, arm, "result.json")
    if not os.path.exists(path):
        return None, "no result.json"
    try:
        return json.load(open(path)), None
    except Exception as error:
        return None, f"result.json unreadable: {error}"


def evidence(task, answer):
    # What a verdict rests on, so a reader can check it without the model.
    if answer is None:
        return {"answer": None}
    if not isinstance(answer, dict):
        return {"answer_type": type(answer).__name__}
    out = {"keys": sorted(answer.keys())}
    claims = answer.get("claims")
    if isinstance(claims, list):
        out["claims"] = [
            {
                "kind": c.get("kind"),
                "url": c.get("url"),
                "statement": (c.get("statement") or "")[:140],
                "evidence": (c.get("evidence") or "")[:140],
            }
            for c in claims
            if isinstance(c, dict)
        ]
    if answer.get("unsupported"):
        out["unsupported"] = answer["unsupported"]
    if task.get("expect_exact"):
        out["wanted"] = task["expect_exact"]
        out["got"] = answer.get("result", answer)
    return out


def main(dirs):
    suite = json.load(open(SUITE))
    rows = []

    print("directory naming (see predicted_dir for the harness defect):")
    for index, task in enumerate(suite["tasks"]):
        name, mangled = predicted_dir(task, index)
        note = "  <- id lost to shell unescaping" if mangled else ""
        print(f"  {task['id']:3} -> {name:3}{note}")
    print()

    for results_dir in dirs:
        was = recorded(results_dir)
        for index, task in enumerate(suite["tasks"]):
            directory = resolve_dir(results_dir, task, index)
            if directory is None:
                continue
            for arm in ("orchestrated", "strong", "cheap"):
                answer, why = saved_answer(results_dir, directory, arm)
                if answer is None and why in ("no outcome.json", "no result.json"):
                    continue
                now_pass, now_reasons = score(task, answer)
                then = was.get((arm, task["id"]))
                run = os.path.basename(results_dir.rstrip("/"))
                rows.append(
                    {
                        "run": run,
                        "task": task["id"],
                        "dir": directory,
                        "arm": arm,
                        "then_pass": None if then is None else then[0],
                        "then_reason": None if then is None else then[1],
                        "now_pass": now_pass,
                        "now_reason": now_reasons[0] if now_reasons else "",
                        "agrees": None if then is None else then[0] == now_pass,
                        "load_error": why,
                        "evidence": evidence(task, answer),
                    }
                )
                mark = "?" if then is None else ("=" if then[0] == now_pass else "CHANGED")
                print(
                    f"{run:14} {task['id']:3} {arm:13} "
                    f"then={'-' if then is None else int(then[0])} now={int(now_pass)} "
                    f"{mark:8} {rows[-1]['now_reason'][:76]}",
                    flush=True,
                )

    json.dump(rows, open("/tmp/rubric-validation.json", "w"), indent=2)
    agree = [r for r in rows if r["agrees"] is True]
    differ = [r for r in rows if r["agrees"] is False]
    print(f"\n{len(rows)} answers re-scored: {len(agree)} agree, {len(differ)} changed")
    for r in differ:
        print(
            f"  CHANGED {r['run']}/{r['task']}/{r['arm']}: "
            f"then {int(r['then_pass'])} -> now {int(r['now_pass'])}\n"
            f"      then: {r['then_reason'][:96]}\n"
            f"      now:  {r['now_reason'][:96]}"
        )
    print("\nfull detail, including what each verdict rested on: /tmp/rubric-validation.json")


if __name__ == "__main__":
    main(sys.argv[1:] or ["/home/workbench/work/timon-qual/eval-pilot2"])
