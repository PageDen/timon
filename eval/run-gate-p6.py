#!/usr/bin/env python3
"""Runs the pipeline gate registered in eval/registration-p6.md.

**This spends quota.** It refuses to start without an explicit per-account
ceiling, because the plan's stopping rule is that spend is approved per account
before any run, and a default ceiling would be this script approving it.

Three arms, matched on goal text, repository state, tools and permissions:

  pipeline  timon run --execute --allow-planner --route planner
  strong    one `codex exec` session on the strong model
  cheap     one `codex exec` session on the cheap model

The single-call arms use *exactly* the command the pipeline gives a writing
worker — `codex exec -m MODEL -s workspace-write --skip-git-repo-check -`, task
on stdin, started in a throwaway worktree off HEAD. Not an approximation of it.
A single call denied the write sandbox, or run read-only, would lose every
writing criterion before the comparison started, and the gate would be measuring
its own harness.

Arm order is randomised per task and repeat. On the P2 gate, Timon always ran
first and the direct call second, which produced a median ratio of 1.495 that
fell to 1.129 once order was randomised. That artefact is the largest false
effect this project has measured and it came from ordering alone.

Nothing is scored by the pipeline's own verifier. See score-criteria.py.
"""
import argparse
import json
import os
import random
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
sys.path.insert(0, str(HERE))

# The P2 gate's token parser, reused rather than rewritten. It already carries
# the fix for codex writing its token report to stderr when the streams are
# captured separately, which is how the first P2 run measured nothing at all.
_p2 = __import__("importlib").machinery.SourceFileLoader(
    "gate_p2", str(HERE / "run-gate-p2.py")
).load_module()
lines_of, tokens_of = _p2.lines_of, _p2.tokens_of

import importlib.machinery
_sc = importlib.machinery.SourceFileLoader(
    "score_criteria", str(HERE / "score-criteria.py")
).load_module()
score_tree = _sc.score


ARMS = ["pipeline", "strong", "cheap"]


def balanced_orders(count, rng):
    """Arm orders in which every arm appears in every position equally often.

    Shuffling independently per pair does not give this at these sample sizes:
    seed 20261001 put the pipeline arm last in 8 of 15 pairs and second in 2.
    That matters because arm order is the largest false effect this project has
    measured — on the P2 gate it produced a median ratio of 1.495 that fell to
    1.129 once order was randomised, since whoever runs later inherits a warm
    prompt cache. Randomising removed the systematic version of that bias; it
    did not remove a lopsided draw.

    So the design is counterbalanced first and randomised second. All six
    permutations of three arms give each arm each position twice; the three
    cyclic rotations give each arm each position once. Repeating that structure
    keeps the balance exact, and the order of the resulting blocks is shuffled
    so position is still not predictable from where a task sits in the run.
    """
    import itertools

    every = [list(p) for p in itertools.permutations(ARMS)]          # 6
    cyclic = [ARMS[i:] + ARMS[:i] for i in range(len(ARMS))]         # 3
    orders = []
    while len(orders) < count:
        orders += every if len(orders) + len(every) <= count else []
        if len(orders) + len(cyclic) <= count:
            orders += cyclic
        elif len(orders) < count:
            # Only reached when count is not a multiple of 3; top up cyclically
            # so the shortfall still spreads positions evenly.
            orders += cyclic[: count - len(orders)]
    rng.shuffle(orders)
    return orders


def git(*args, cwd=REPO, check=True):
    return subprocess.run(["git", *args], cwd=cwd, check=check,
                          capture_output=True, text=True).stdout.strip()


def fresh_worktree(root, label):
    """A throwaway checkout of HEAD. Every arm starts from the same tree."""
    path = Path(root) / label
    git("worktree", "add", "--detach", str(path), "HEAD")
    return path


def drop_worktree(path):
    subprocess.run(["git", "worktree", "remove", "--force", str(path)],
                   cwd=REPO, capture_output=True, text=True)


def run_single(task, arm, model, args, out):
    """One `codex exec` session, with the pipeline's own write-worker command."""
    tree = fresh_worktree(out, f"{task['id']}-{arm}-tree")
    log = Path(out) / f"{task['id']}-{arm}.log"
    started = time.monotonic()
    with open(log, "w") as handle:
        subprocess.run(
            ["codex", "exec", "-m", model, "-s", "workspace-write",
             "--skip-git-repo-check", "-"],
            input=task["goal"], stdout=handle, stderr=subprocess.STDOUT,
            text=True, cwd=tree, timeout=args.budget_secs * 2, check=False,
        )
    secs = time.monotonic() - started
    lines = lines_of(log)
    rows = score_tree(args.suite_data, tree, {task["id"]})
    drop_worktree(tree)
    return {
        "arm": arm, "tokens": tokens_of(lines), "secs": round(secs, 1),
        "established": rows[0]["established"], "total": rows[0]["total"],
        "detail": rows[0]["detail"], "route": arm, "verdict": None,
        "budget_secs": args.budget_secs,
        "overrun_secs": round(max(0.0, secs - args.budget_secs), 1),
    }


def run_pipeline(task, rep, args, out):
    """The whole route: triage, planner, workers, integration, verification."""
    # Keyed by repeat. Sharing one directory across repeats overwrote
    # stdout.json and stderr.log each time, so stage one kept only the last of
    # three reports. The token sum survived only because it was already scoped
    # by run id.
    run_out = Path(out) / f"{task['id']}-{rep}-pipeline"
    # 0700: a worker refuses an output directory that group or others can read,
    # which is right — its output can contain whatever it was working on. The
    # default umask here gives 0755 and the run fails before it starts.
    run_out.mkdir(parents=True, exist_ok=True, mode=0o700)
    run_out.chmod(0o700)  # exist_ok could have found one left at 0755
    command = [
        args.timon, "run", task["goal"], "--execute",
        "--store", str(Path(out) / "runs.sqlite"),
        "--account", args.account, "--broker", args.broker,
        "--cheap-model", args.cheap_model, "--strong-model", args.strong_model,
        "--workspace", str(REPO), "--output-root", str(run_out),
        "--budget-secs", str(args.budget_secs), "--format", "json",
    ]
    # p5 is the over-escalation control: triage decides, so the route is not
    # forced. Every other task is the planner path by construction.
    if task["kind"] != "control":
        command += ["--allow-planner", "--route", "planner"]

    started = time.monotonic()
    done = subprocess.run(command, capture_output=True, text=True,
                          timeout=args.budget_secs * 3, check=False)
    secs = time.monotonic() - started
    (run_out / "stdout.json").write_text(done.stdout)
    (run_out / "stderr.log").write_text(done.stderr)

    try:
        report = json.loads(done.stdout)
    except json.JSONDecodeError:
        report = {}
    # Two shapes, because two routes. The single-worker route wraps its record
    # as {"run": {...branch...}}; the planner route emits a flat object with
    # `result_branch` at the top level. Reading only the first gave branch=None
    # for every planner run, so nothing was checked out and stage one recorded
    # 0 of 6 three times while the branches on disk each held 6 of 6. A
    # confident wrong number, not a refusal — the seventh instrument fault here
    # and the worst-shaped one.
    run = report.get("run") if isinstance(report.get("run"), dict) else report
    branch = run.get("branch") or report.get("result_branch")
    if branch is None and report:
        print(f"    no result branch in the report for {task['id']}-{rep}; "
              f"keys were {sorted(report)}", flush=True)

    # Score the result branch, never the pipeline's own verdict about it.
    established, total, detail = 0, len(task["criteria"]), []
    if branch:
        tree = Path(out) / f"{task['id']}-{rep}-pipeline-result"
        git("worktree", "add", "--detach", str(tree), branch)
        rows = score_tree(args.suite_data, tree, {task["id"]})
        established, total, detail = rows[0]["established"], rows[0]["total"], rows[0]["detail"]
        drop_worktree(tree)

    # Tokens across every session the route spent: planner, workers, judge.
    #
    # Scoped to this run's own directory and to the two transcript names codex
    # writes, rather than everything beneath the output root. Two reasons, both
    # found by probing a real run before spending anything on the gate:
    #
    # A worker's worktree is a full checkout of the repository, so a glob for
    # `*.log` and `*.txt` under the output root walks it. Nothing tracked here
    # matches today, but a task that writes a log file — which these tasks
    # plausibly could — would have its own output counted as model tokens.
    #
    # And a glob over the output root sums every run that has ever used it. The
    # probe read 12,618 tokens across two unrelated runs that shared a
    # directory. Keying on the run id makes a reused directory harmless.
    tokens = 0
    seen = False
    run_id = run.get("run_id")
    transcripts = run_out / run_id if run_id else run_out
    for log in transcripts.rglob("*"):
        if "worktrees" in log.parts:
            continue
        if log.is_file() and log.name in ("stdout.log", "stderr.log"):
            found = tokens_of(lines_of(log))
            if found is not None:
                tokens += found
                seen = True

    return {
        "arm": "pipeline", "tokens": tokens if seen else None, "secs": round(secs, 1),
        "established": established, "total": total, "detail": detail,
        "route": run.get("route"), "status": run.get("status"),
        "graph": report.get("graph", {}).get("complete"),
        "verdict": (report.get("verdict") or {}).get("outcome")
                   or (report.get("verdict") or {}).get("status"),
        "branch": branch,
        "budget_secs": args.budget_secs,
        "overrun_secs": round(max(0.0, secs - args.budget_secs), 1),
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True)
    parser.add_argument("--suite", default=str(HERE / "gate-p6-suite.json"))
    parser.add_argument("--broker", default="127.0.0.1:1456")
    parser.add_argument("--account", required=True,
                        help="The pooled account this gate may spend. Pinned, "
                             "not left to the broker: an unpinned run spills "
                             "onto whichever account looks healthiest, which "
                             "may be the one a developer is mid-session on")
    parser.add_argument("--ceiling-tokens", type=int, required=True,
                        help="Per-account spend ceiling. No default: a default "
                             "would be this script approving its own spend")
    # Required, and required to differ. P2's runner defaulted both to one model
    # and that was right for its question: it compared the fast path against a
    # direct call on the *same* model, so routing overhead was the only
    # difference. Copying those defaults here would have collapsed the strong
    # and cheap arms into one, silently turning gate 2 into a comparison of the
    # pipeline against itself and leaving the cheap-arm control unable to detect
    # an easy suite. Caught before any run; nothing was spent on it.
    parser.add_argument("--strong-model", required=True)
    parser.add_argument("--cheap-model", required=True)
    parser.add_argument("--budget-secs", type=int, default=300)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--timon", default="/usr/local/bin/timon")
    parser.add_argument("--seed", type=int, default=20261001)
    parser.add_argument("--task", action="append",
                        help="Run only these suite task ids. For staging a gate: "
                             "one task first, read the real cost, then decide "
                             "whether to commit to the rest")
    parser.add_argument("--dry-run", action="store_true",
                        help="Print the plan and the arm order, spend nothing")
    args = parser.parse_args()

    # The instrument is validated before the run it is scoring, never on it.
    check = subprocess.run([sys.executable, str(HERE / "score-criteria.py"), "--self-test"],
                           capture_output=True, text=True)
    if check.returncode != 0:
        print(check.stdout)
        print("score-criteria.py self-test FAILED — refusing to run", file=sys.stderr)
        return 2
    print(check.stdout.strip().splitlines()[-1])

    if args.strong_model == args.cheap_model:
        print(
            f"--strong-model and --cheap-model are both {args.strong_model!r}. The "
            "registered arms are the pipeline, a single strong call and a single "
            "cheap call; with one model the last two are the same arm and gate 2 "
            "compares the pipeline against itself.",
            file=sys.stderr,
        )
        return 2

    args.suite_data = json.load(open(args.suite))
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True, mode=0o700)
    # mkdir's mode is masked by the umask, so set it explicitly as well.
    out.chmod(0o700)

    chosen = args.suite_data["tasks"]
    if args.task:
        wanted = set(args.task)
        chosen = [t for t in chosen if t["id"] in wanted]
        missing = wanted - {t["id"] for t in chosen}
        if missing:
            print(f"no such task(s) in the suite: {', '.join(sorted(missing))}",
                  file=sys.stderr)
            return 2

    rng = random.Random(args.seed)
    orders = balanced_orders(len(chosen) * args.repeats, rng)
    schedule = [
        (task, rep, orders.pop())
        for task in chosen
        for rep in range(args.repeats)
    ]

    print(f"{len(schedule)} pairs × 3 arms = {len(schedule) * 3} sessions", flush=True)
    print(f"account {args.account}, ceiling {args.ceiling_tokens:,} tokens, "
          f"budget {args.budget_secs}s, seed {args.seed}", flush=True)
    if args.dry_run:
        for task, rep, arms in schedule:
            print(f"  {task['id']}-{rep}  {' → '.join(arms)}")
        print("\ndry run: nothing was spent")
        return 0

    results, spent = [], 0
    for task, rep, arms in schedule:
        record = {"task": task["id"], "kind": task["kind"], "rep": rep,
                  "arm_order": arms}
        for arm in arms:
            if spent >= args.ceiling_tokens:
                print(f"\nCEILING REACHED at {spent:,} tokens. Stopping for a "
                      f"decision; the remaining schedule was not run and no "
                      f"other account was used.", file=sys.stderr)
                record["stopped"] = True
                results.append(record)
                json.dump(results, open(out / "results.json", "w"), indent=1)
                return 3
            if arm == "pipeline":
                got = run_pipeline(task, rep, args, out)
            else:
                model = args.strong_model if arm == "strong" else args.cheap_model
                got = run_single(task, arm, model, args, out)
            spent += got["tokens"] or 0
            record[arm] = got
            flag = "  OVERRUN" if got["overrun_secs"] > 0 else ""
            # flush: redirected to a file, Python buffers this and a run that
            # takes twenty minutes shows nothing at all until it exits. A gate
            # that cannot be watched cannot be stopped early for a good reason.
            print(f"  {task['id']}-{rep} {arm:9} {got['established']}/{got['total']} "
                  f"{(got['tokens'] or 0):>8,}tok {got['secs']:>6.1f}s{flag}", flush=True)
        results.append(record)
        json.dump(results, open(out / "results.json", "w"), indent=1)

    print(f"\n{spent:,} tokens spent on {args.account}, ceiling {args.ceiling_tokens:,}")
    print(f"results in {out / 'results.json'} — score with report-gate-p6.py")
    return 0


if __name__ == "__main__":
    sys.exit(main())
