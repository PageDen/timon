#!/usr/bin/env bash
# Runs the P2 gate: Timon's fast path against a direct cheap call.
#
# Arms are matched on task input, repository state, tools and permissions. The
# only difference is who decides the route and who holds the credential.
#
# The checker is self-tested before anything runs. Four instruments in this
# project were wrong on first contact with real data, so an instrument that has
# not been checked does not get to score a run.
set -uo pipefail

REPO="${REPO:-/home/workbench/work/timon}"
SUITE="${SUITE:-$REPO/eval/gate-p2-suite.json}"
OUT="${OUT:?set OUT to a results directory}"
BROKER="${BROKER:-127.0.0.1:1456}"
ACCOUNT="${ACCOUNT:-acct3}"
MODEL="${MODEL:-gpt-5.6-luna}"
REPEATS="${REPEATS:-2}"
TIMON="${TIMON:-/usr/local/bin/timon}"

python3 "$REPO/eval/score-answer.py" --self-test || {
  echo "the checker failed its own test; the gate does not run" >&2
  exit 1
}

mkdir -p "$OUT"; chmod 700 "$OUT"

# One Codex home for both arms, so provider configuration is not a difference
# between them. Both talk to the broker; only the grant differs.
HOME_DIR="$OUT/codex-home"
mkdir -p "$HOME_DIR"; chmod 700 "$HOME_DIR"
cat > "$HOME_DIR/config.toml" <<TOML
model_provider = "timon"

[model_providers.timon]
name = "Timon broker"
base_url = "http://$BROKER"
wire_api = "responses"
requires_openai_auth = true
env_http_headers = { "x-timon-grant" = "TIMON_GRANT" }
TOML
cp "$HOME/.timon-broker/accounts/$ACCOUNT/auth.json" "$HOME_DIR/auth.json"
chmod 600 "$HOME_DIR/auth.json"
export CODEX_HOME="$HOME_DIR"

tokens_from() {  # reads "tokens used\n N,NNN" from a codex transcript
  grep -A1 '^tokens used$' "$1" 2>/dev/null | tail -1 | tr -d ', ' | grep -E '^[0-9]+$' || echo ""
}

echo "[]" > "$OUT/results.json"
python3 - "$SUITE" "$OUT" "$REPEATS" "$BROKER" "$ACCOUNT" "$MODEL" "$TIMON" <<'PY'
import json, os, subprocess, sys, time

suite_path, out, repeats, broker, account, model, timon = sys.argv[1:8]
suite = json.load(open(suite_path))
repeats = int(repeats)
repo = os.environ.get("REPO", "/home/workbench/work/timon")
results = []

def tokens_of(path):
    try:
        lines = open(path, errors="replace").read().splitlines()
    except OSError:
        return None
    for i, line in enumerate(lines):
        if line.strip() == "tokens used" and i + 1 < len(lines):
            raw = lines[i + 1].replace(",", "").strip()
            if raw.isdigit():
                return int(raw)
    return None

def answer_of(path):
    """The model's reply: the last non-empty line before the token report."""
    try:
        lines = open(path, errors="replace").read().splitlines()
    except OSError:
        return None
    for i, line in enumerate(lines):
        if line.strip() == "tokens used":
            for back in range(i - 1, -1, -1):
                if lines[back].strip():
                    return lines[back].strip()
    return lines[-1].strip() if lines else None

for task in suite["tasks"]:
    for rep in range(repeats):
        # --- arm A: Timon, routed by triage ---
        run_out = f"{out}/timon/{task['id']}-{rep}"
        started = time.time()
        done = subprocess.run(
            [timon, "run", task["goal"],
             "--store", f"{out}/runs.sqlite", "--execute",
             "--account", account, "--broker", broker,
             "--cheap-model", model, "--strong-model", model,
             "--workspace", repo,
             "--output-root", run_out, "--worker-deadline-secs", "180",
             "--format", "json"],
            capture_output=True, text=True, timeout=300,
        )
        a_secs = time.time() - started
        a_id = None
        try:
            a_id = json.loads(done.stdout)["run_id"]
        except Exception:
            pass
        a_log = f"{run_out}/{a_id}/stdout.log" if a_id else ""
        a_tokens, a_answer = tokens_of(a_log), answer_of(a_log)

        # --- arm B: a direct cheap call, same task, same repository ---
        b_log = f"{out}/direct/{task['id']}-{rep}.log"
        os.makedirs(os.path.dirname(b_log), exist_ok=True)
        started = time.time()
        with open(b_log, "w") as handle:
            subprocess.run(
                ["codex", "exec", "-m", model, "--skip-git-repo-check", "-C", repo, "-"],
                input=task["goal"], stdout=handle, stderr=subprocess.STDOUT,
                text=True, timeout=300,
            )
        b_secs = time.time() - started
        b_tokens, b_answer = tokens_of(b_log), answer_of(b_log)

        def check(ans):
            payload = json.dumps({"answer": ans, "accept": task["accept"]})
            got = subprocess.run(
                ["python3", f"{repo}/eval/score-answer.py"],
                input=payload, capture_output=True, text=True,
            )
            try:
                return json.loads(got.stdout)["accepted"]
            except Exception:
                return False

        record = {
            "task": task["id"], "rep": rep,
            "timon": {"tokens": a_tokens, "secs": round(a_secs, 1),
                      "answer": a_answer, "accepted": check(a_answer)},
            "direct": {"tokens": b_tokens, "secs": round(b_secs, 1),
                       "answer": b_answer, "accepted": check(b_answer)},
        }
        results.append(record)
        print(f"  {task['id']} rep{rep}: timon {a_tokens} tok/{record['timon']['accepted']}"
              f"   direct {b_tokens} tok/{record['direct']['accepted']}", flush=True)

json.dump(results, open(f"{out}/results.json", "w"), indent=1)
PY
echo
python3 "$REPO/eval/report-gate-p2.py" "$OUT/results.json"
