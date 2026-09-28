#!/usr/bin/env python3
"""Re-scores a gate run from its saved logs.

The evidence is on disk, so an instrument fault costs a re-read rather than a
re-spend. That is the point of keeping the transcripts.
"""
import json
import re
import subprocess
import sys
from pathlib import Path

REPO = Path("/home/workbench/work/timon")


def lines_of(*paths):
    out = []
    for path in paths:
        try:
            out += Path(path).read_text(errors="replace").splitlines()
        except OSError:
            continue
    return out


def tokens(lines):
    for i, line in enumerate(lines):
        if line.strip() == "tokens used" and i + 1 < len(lines):
            raw = lines[i + 1].replace(",", "").strip()
            if raw.isdigit():
                return int(raw)
    return None


def accepted(answer, criterion):
    payload = json.dumps({"answer": answer, "accept": criterion})
    got = subprocess.run(
        ["python3", str(REPO / "eval/score-answer.py")],
        input=payload, capture_output=True, text=True,
    )
    return json.loads(got.stdout)["accepted"]


def main():
    out = Path(sys.argv[1])
    suite = json.load(open(REPO / "eval/gate-p2-suite.json"))
    criteria = {t["id"]: t["accept"] for t in suite["tasks"]}
    old = json.load(open(out / "results.json"))

    rescored = []
    for record in old:
        task, rep = record["task"], record["rep"]
        run_dir = out / "timon" / f"{task}-{rep}"
        # The run id is the single directory under the per-run output root.
        inner = next((p for p in run_dir.iterdir() if p.is_dir()), None) if run_dir.is_dir() else None
        a_lines = lines_of(inner / "stdout.log", inner / "stderr.log") if inner else []
        a_answer = record["timon"]["answer"]
        b_log = out / "direct" / f"{task}-{rep}.log"
        b_lines = lines_of(b_log)

        rescored.append({
            "task": task, "rep": rep,
            "timon": {**record["timon"], "tokens": tokens(a_lines),
                      "accepted": accepted(a_answer, criteria[task])},
            "direct": {**record["direct"], "tokens": tokens(b_lines),
                       "accepted": accepted(record["direct"]["answer"], criteria[task])},
        })

    json.dump(rescored, open(out / "results.json", "w"), indent=1)
    print(f"rescored {len(rescored)} paired runs from saved logs")


if __name__ == "__main__":
    main()
