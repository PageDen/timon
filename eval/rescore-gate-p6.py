#!/usr/bin/env python3
"""Re-scores the pipeline arm from its result branches.

The runner read the result branch from `report["run"]["branch"]`, which is where
the *single-worker* route puts it. The planner route emits a flat object with
`result_branch` at the top level. So the branch was always None, nothing was
checked out, and every pipeline run was recorded as establishing 0 of 6 — a
confident wrong number rather than a refusal, which is the seventh instrument
fault in this project and the worst-shaped one.

Re-scoring costs a git checkout. The result branches are still in the
repository, exactly as the P2 gate's three faults cost nothing because its
transcripts had been kept.
"""
import argparse
import json
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
import importlib.machinery

_sc = importlib.machinery.SourceFileLoader(
    "score_criteria", str(HERE / "score-criteria.py")
).load_module()


def git(*args, check=True):
    return subprocess.run(["git", *args], cwd=REPO, check=check,
                          capture_output=True, text=True).stdout.strip()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--suite", default=str(HERE / "gate-p6-suite.json"))
    parser.add_argument("--task", default="p1")
    args = parser.parse_args()
    suite = json.load(open(args.suite))

    branches = [b.strip().lstrip("+ ").strip() for b in
                git("branch", "--list", "timon/*/result").splitlines() if b.strip()]
    if not branches:
        print("no result branches left to score", file=sys.stderr)
        return 2

    print(f"{len(branches)} result branch(es)\n")
    rows = []
    for branch in branches:
        tree = Path("/tmp") / f"rescore-{branch.replace('/', '-')}"
        subprocess.run(["git", "worktree", "remove", "--force", str(tree)],
                       cwd=REPO, capture_output=True, text=True)
        git("worktree", "add", "--detach", str(tree), branch)
        scored = _sc.score(suite, tree, {args.task})[0]
        rows.append((branch, scored))
        print(f"{branch}")
        print(f"  established {scored['established']}/{scored['total']}")
        for item in scored["detail"]:
            mark = "ok  " if item["passed"] else "MISS"
            c = item["criterion"]
            value = f" {c.get('value')!r}" if "value" in c else ""
            print(f"    {mark} {c['kind']} {c['path']}{value} — {item['note']}")
        # What the worker actually wrote, which is the thing a 0/6 hid.
        docs = sorted(p.relative_to(tree) for p in (tree / "docs").glob("*")) \
            if (tree / "docs").is_dir() else []
        print(f"    files in docs/: {[str(d) for d in docs] or 'none'}")
        subprocess.run(["git", "worktree", "remove", "--force", str(tree)],
                       cwd=REPO, capture_output=True, text=True)
        print()

    best = [s["established"] for _, s in rows]
    print(f"pipeline established, per run: {best}")
    print(f"out of {rows[0][1]['total']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
