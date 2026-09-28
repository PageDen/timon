#!/usr/bin/env python3
"""Re-scores a gate run from its saved transcripts.

The evidence is on disk, so an instrument fault costs a re-read rather than a
re-spend. This has now paid for itself twice.
"""
import json
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO / "eval"))
runner = __import__("run-gate-p2".replace("-", "_")) if False else None

# Imported by path because the module name has hyphens in it.
import importlib.util
spec = importlib.util.spec_from_file_location("gate", REPO / "eval/run-gate-p2.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


def main():
    out = Path(sys.argv[1])
    suite = json.load(open(REPO / "eval/gate-p2-suite.json"))
    criteria = {t["id"]: t["accept"] for t in suite["tasks"]}
    results = json.load(open(out / "results.json"))

    for record in results:
        task, rep = record["task"], record["rep"]
        run_dir = out / "timon" / f"{task}-{rep}"
        inner = next((p for p in run_dir.iterdir() if p.is_dir()), None) if run_dir.is_dir() else None
        if inner:
            a_lines = gate.lines_of(inner / "stdout.log", inner / "stderr.log")
            record["timon"]["tokens"] = gate.tokens_of(a_lines)
            record["timon"]["answer"] = "\n".join(gate.lines_of(inner / "stdout.log")).strip() or None
        b_lines = gate.lines_of(out / "direct" / f"{task}-{rep}.log")
        record["direct"]["tokens"] = gate.tokens_of(b_lines)
        record["direct"]["answer"] = gate.answer_of(b_lines)
        for arm in ("timon", "direct"):
            record[arm]["accepted"] = gate.accepted(record[arm]["answer"], criteria[task])

    json.dump(results, open(out / "results.json", "w"), indent=1)
    print(f"rescored {len(results)} paired runs from saved transcripts")


if __name__ == "__main__":
    main()
