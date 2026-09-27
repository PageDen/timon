#!/usr/bin/env bash
# Runs the registered pilot: every task through every arm, paired.
#
# Arms are fixed here rather than discovered at run time, because the
# registration says model choice is fixed before the first confirmatory run.
set -uo pipefail

Q=/home/workbench/work/timon-qual
export CODEX_HOME="${CODEX_HOME:-$Q/codex-home}"
CX="${CX:-$Q/tools/node_modules/.bin/codex}"
T="${T:-/home/workbench/work/timon/target/release/timon}"
SUITE="${SUITE:-/home/workbench/work/timon/eval/suite.json}"
OUT="${OUT:?set OUT to a results directory}"
STRONG="${STRONG:-gpt-6-astra}"
CHEAP="${CHEAP:-gpt-5.5}"

mkdir -p "$OUT"; chmod 700 "$OUT"
# Each schema the suite declares, written out once. A single shared schema was a
# bug: it forced every task into {claims, unsupported}, which an extraction task
# cannot satisfy.
python3 - "$SUITE" "$OUT" <<'PY'
import json, sys
suite, out = json.load(open(sys.argv[1])), sys.argv[2]
for name, schema in suite["schemas"].items():
    json.dump(schema, open(f"{out}/schema-{name}.json", "w"))
PY

echo "arm,task,ok,seconds,lead_tokens,worker_tokens,total_tokens,reason" > "$OUT/results.csv"

n=$(python3 -c "import json;print(len(json.load(open('$SUITE'))['tasks']))")
for i in $(seq 0 $((n-1))); do
  task=$(python3 -c "import json;print(json.dumps(json.load(open('$SUITE'))['tasks'][$i]))")
  # Read from the suite by index rather than interpolating the task JSON into a
  # shell string. The old form did the latter, and the shell unescapes a
  # backslash-quote inside double quotes, so any task whose own text contained a
  # quote arrived at Python as broken JSON and fell back to "t$i". Nothing was
  # mis-scored -- the score comes from the task passed on argv below -- but the
  # output directory then disagreed with the task id in results.csv, so a saved
  # answer could not be mapped back to its task by name afterwards.
  id=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['tasks'][int(sys.argv[2])]['id'])" "$SUITE" "$i")
  goal=$(python3 -c "import json,sys;print(json.loads(sys.stdin.read())['goal'])" <<<"$task")
  schema="$OUT/schema-$(python3 -c "import json,sys;print(json.loads(sys.stdin.read())['schema'])" <<<"$task").json"

  # ---- arm: orchestrated (strong lead plans, cheap workers, strong lead integrates)
  d="$OUT/$id/orchestrated"; mkdir -p "$d"; chmod 700 "$d"
  start=$(date +%s.%N)
  timeout 1200 "$T" orchestrate --run-id "eval-$id" --goal "$goal" \
    --output-root "$d/run" --max-tasks 3 --deliverable-schema "$schema" \
    --lead-deadline-secs 300 --worker-deadline-secs 300 --usage-source stdout \
    --run-ledger "$d/led" --run-token-ceiling 900000 --attempt-reserve 60000 \
    --lead-command "[\"$CX\",\"exec\",\"-m\",\"$STRONG\",\"-s\",\"read-only\",\"--skip-git-repo-check\",\"--json\",\"-o\",\"{result}\",\"--output-schema\",\"{schema}\",\"-\"]" \
    --worker-command "[\"$CX\",\"--search\",\"exec\",\"-m\",\"$CHEAP\",\"-s\",\"read-only\",\"--skip-git-repo-check\",\"--json\",\"-o\",\"{result}\",\"--output-schema\",\"{schema}\",\"-\"]" \
    > "$d/outcome.json" 2>"$d/err.txt"
  secs=$(python3 -c "print(f'{$(date +%s.%N)-$start:.1f}')")
  python3 - "$task" "$d" "$secs" "$OUT/results.csv" <<'PY'
import json, subprocess, sys
task, d, secs, csv = json.loads(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
sys.path.insert(0, '/home/workbench/work/timon/eval')
from score import score
try:
    o = json.load(open(f"{d}/outcome.json"))
except Exception:
    o = {}
answer = None
if o.get("answer"):
    try: answer = json.loads(o["answer"])
    except Exception: answer = None
passed, reasons = score(task, answer)
lead, work = o.get("lead_tokens") or 0, o.get("worker_tokens") or 0
with open(csv, "a") as f:
    f.write(f'orchestrated,{task["id"]},{int(passed)},{secs},{lead},{work},{lead+work},"{reasons[0][:90]}"\n')
print(f'  orchestrated  {task["id"]:4} ok={int(passed)}  {secs}s  lead={lead} worker={work}  {reasons[0][:70]}')
PY

  # ---- arms: one model, one call
  for arm in strong cheap; do
    model=$STRONG; [ "$arm" = "cheap" ] && model=$CHEAP
    d="$OUT/$id/$arm"; mkdir -p "$d"; chmod 700 "$d"
    start=$(date +%s.%N)
    printf '%s\n\nReply with JSON only, matching the schema you were given.' "$goal" | \
      timeout 600 "$CX" --search exec -m "$model" -s read-only --skip-git-repo-check --json \
        -o "$d/result.json" --output-schema "$schema" - > "$d/stream.jsonl" 2>"$d/err.txt"
    secs=$(python3 -c "print(f'{$(date +%s.%N)-$start:.1f}')")
    python3 - "$task" "$d" "$secs" "$OUT/results.csv" "$arm" <<'PY'
import json, sys
task, d, secs, csv, arm = json.loads(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
sys.path.insert(0, '/home/workbench/work/timon/eval')
from score import score
try: answer = json.load(open(f"{d}/result.json"))
except Exception: answer = None
total = 0
for line in open(f"{d}/stream.jsonl", errors="replace"):
    line = line.strip()
    if not line.startswith("{"): continue
    try: e = json.loads(line)
    except Exception: continue
    if e.get("type") == "turn.completed":
        u = e.get("usage") or {}
        total += (u.get("input_tokens") or 0) + (u.get("output_tokens") or 0)
passed, reasons = score(task, answer)
with open(csv, "a") as f:
    f.write(f'{arm},{task["id"]},{int(passed)},{secs},0,0,{total},"{reasons[0][:90]}"\n')
print(f'  {arm:12}  {task["id"]:4} ok={int(passed)}  {secs}s  total={total}  {reasons[0][:70]}')
PY
  done
done
echo
echo "results: $OUT/results.csv"
