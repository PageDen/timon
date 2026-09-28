#!/usr/bin/env python3
"""Runs the P2 gate: Timon's fast path against a direct cheap call.

Arms are matched on task input, repository state, tools and permissions. The
only intended difference is who decides the route and who holds the grant.

Two things the first run of this gate got wrong, fixed here:

**Arm order was confounded with arm.** Timon always ran first and the direct
call second, and both arms showed the first repeat costing more than the
second — consistent with the provider's prompt cache warming across the
sequence. Order is randomised per pair now.

**Two repeats could not see past the noise.** Paired deltas ranged from -3,862
to +6,826 tokens, and one task cost the same arm 4,267 tokens once and 16,343
another time. A median ratio over that is not a measurement. The report now
shows the spread and a sign test, so a result that cannot be distinguished from
noise says so instead of reading as a verdict.
"""
import argparse
import json
import os
import random
import subprocess
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


def lines_of(*paths):
    out = []
    for path in paths:
        try:
            out += Path(path).read_text(errors="replace").splitlines()
        except OSError:
            continue
    return out


def tokens_of(lines):
    """The token report, which codex writes to stderr when streams are captured
    separately and to stdout when they are merged. Looking in one place is how
    the first run of this gate measured nothing at all."""
    for i, line in enumerate(lines):
        if line.strip() == "tokens used" and i + 1 < len(lines):
            raw = lines[i + 1].replace(",", "").strip()
            if raw.isdigit():
                return int(raw)
    return None


def answer_of(lines):
    """The model's final reply, whole.

    Taking the last line was wrong and scored a correct three-line answer as a
    failure: the worker said CheapWorker / StrongWorker / Planner and the
    instrument recorded "Planner". An answer is a block, not a line.
    """
    # The transcript marks the final reply with a bare `codex` line and ends it
    # at the token report.
    start = None
    for i, line in enumerate(lines):
        if line.strip() == "codex":
            start = i + 1
    if start is None:
        return "\n".join(l for l in lines if l.strip()).strip() or None
    end = len(lines)
    for i in range(start, len(lines)):
        if lines[i].strip() == "tokens used":
            end = i
            break
    return "\n".join(lines[start:end]).strip() or None


def accepted(answer, criterion):
    payload = json.dumps({"answer": answer, "accept": criterion})
    got = subprocess.run(
        ["python3", str(REPO / "eval/score-answer.py")],
        input=payload, capture_output=True, text=True,
    )
    try:
        return json.loads(got.stdout)["accepted"]
    except Exception:
        return False


def run_timon(task, rep, args):
    run_out = f"{args.out}/timon/{task['id']}-{rep}"
    started = time.time()
    subprocess.run(
        [args.timon, "run", task["goal"],
         "--store", f"{args.out}/runs.sqlite", "--execute",
         "--account", args.account, "--broker", args.broker,
         "--cheap-model", args.model, "--strong-model", args.model,
         "--workspace", str(REPO),
         "--output-root", run_out, "--worker-deadline-secs", "180",
         "--format", "json"],
        capture_output=True, text=True, timeout=400,
    )
    secs = time.time() - started
    inner = next((p for p in Path(run_out).iterdir() if p.is_dir()), None) if Path(run_out).is_dir() else None
    lines = lines_of(inner / "stdout.log", inner / "stderr.log") if inner else []
    # The worker's clean stdout is the answer, whole. Not its last line.
    answer = None
    if inner:
        answer = "\n".join(lines_of(inner / "stdout.log")).strip() or None
    return {"tokens": tokens_of(lines), "secs": round(secs, 1), "answer": answer}


def run_direct(task, rep, args):
    log = Path(args.out) / "direct" / f"{task['id']}-{rep}.log"
    log.parent.mkdir(parents=True, exist_ok=True)
    started = time.time()
    with open(log, "w") as handle:
        subprocess.run(
            ["codex", "exec", "-m", args.model, "--skip-git-repo-check", "-C", str(REPO), "-"],
            input=task["goal"], stdout=handle, stderr=subprocess.STDOUT,
            text=True, timeout=400,
        )
    secs = time.time() - started
    lines = lines_of(log)
    return {"tokens": tokens_of(lines), "secs": round(secs, 1), "answer": answer_of(lines)}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True)
    parser.add_argument("--suite", default=str(REPO / "eval/gate-p2-suite.json"))
    parser.add_argument("--broker", default="127.0.0.1:1456")
    parser.add_argument("--account", default="acct3")
    parser.add_argument("--model", default="gpt-5.6-luna")
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--timon", default="/usr/local/bin/timon")
    parser.add_argument("--seed", type=int, default=20260928)
    args = parser.parse_args()

    # The checker does not get to score a run it has not passed its own test on.
    check = subprocess.run(
        ["python3", str(REPO / "eval/score-answer.py"), "--self-test"],
        capture_output=True, text=True,
    )
    print(check.stdout.strip())
    if check.returncode != 0:
        raise SystemExit("the checker failed its own test; the gate does not run")

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    out.chmod(0o700)

    # One Codex home for both arms, so provider configuration is not a
    # difference between them.
    home = out / "codex-home"
    home.mkdir(exist_ok=True)
    home.chmod(0o700)
    (home / "config.toml").write_text(
        'model_provider = "timon"\n\n'
        '[model_providers.timon]\n'
        'name = "Timon broker"\n'
        f'base_url = "http://{args.broker}"\n'
        'wire_api = "responses"\n'
        'requires_openai_auth = true\n'
        'env_http_headers = { "x-timon-grant" = "TIMON_GRANT" }\n'
    )
    source = Path.home() / ".timon-broker/accounts" / args.account / "auth.json"
    (home / "auth.json").write_text(source.read_text())
    (home / "auth.json").chmod(0o600)
    os.environ["CODEX_HOME"] = str(home)

    suite = json.load(open(args.suite))
    random.seed(args.seed)
    results = []

    for task in suite["tasks"]:
        for rep in range(args.repeats):
            timon_first = random.random() < 0.5
            if timon_first:
                a, b = run_timon(task, rep, args), run_direct(task, rep, args)
            else:
                b, a = run_direct(task, rep, args), run_timon(task, rep, args)
            a["accepted"] = accepted(a["answer"], task["accept"])
            b["accepted"] = accepted(b["answer"], task["accept"])
            results.append({
                "task": task["id"], "rep": rep, "timon_first": timon_first,
                "timon": a, "direct": b,
            })
            print(f"  {task['id']} rep{rep} "
                  f"({'timon' if timon_first else 'direct'} first): "
                  f"timon {a['tokens']}/{a['accepted']}  "
                  f"direct {b['tokens']}/{b['accepted']}", flush=True)
            json.dump(results, open(out / "results.json", "w"), indent=1)

    print(f"\n{len(results)} paired runs written to {out}/results.json")


if __name__ == "__main__":
    main()
