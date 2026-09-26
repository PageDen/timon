#!/usr/bin/env bash
# A1 cross-account checks against a running recorder.
#
# These need real separate accounts: the isolation they check is enforced by the
# kernel (SO_PEERCRED and socket permissions), so a single-process test with
# mocked identities would prove nothing. Run as a user with sudo.
set -uo pipefail

TIMON=${TIMON:-/usr/local/lib/timon/timon}
SOCK=${SOCK:-/run/timon-usage/usage.sock}
A=timontest1
B=timontest2
OUTSIDER=timontest3
ADMIN=${ADMIN:-workbench}
pass=0; fail=0
# Every run uses fresh event ids, so the suite can be run repeatedly against a
# live database without colliding with its own earlier rows. `occurred_at` is
# fixed within a run so that a replay really is byte-identical content.
RUN_TAG=${RUN_TAG:-$(date +%s)-$$}
OCCURRED=1790000000

ok()   { printf '  PASS  %s\n' "$1"; pass=$((pass+1)); }
bad()  { printf '  FAIL  %s\n     -> %s\n' "$1" "${2:-}"; fail=$((fail+1)); }

# Run the client as another account. `sudo -u` builds fresh credentials, so the
# supplementary group membership actually applies.
as() { local u=$1; shift; sudo -u "$u" -- "$TIMON" "$@" 2>&1; }
append_as() { local u=$1 json=$2; printf '%s' "$json" | sudo -u "$u" -- "$TIMON" usage append --socket "$SOCK" 2>&1; }

event() { # id, run, output-tokens
  printf '{"version":1,"client_event_id":"%s-%s","run_id":"%s","attempt_id":"1","role":"worker","provider":"openai","model":"gpt-5.5","usage":{"input":100,"cached_input":40,"output":%s,"reasoning_output":2},"usage_status":"complete","duration_ms":10,"occurred_at":%s}' \
    "$RUN_TAG" "$1" "$2" "$3" "$OCCURRED"
}

uid_of() { id -u "$1"; }
echo "=== A1 cross-account checks ==="
echo "socket : $SOCK"
echo "A=$A($(uid_of $A))  B=$B($(uid_of $B))  outsider=$OUTSIDER($(uid_of $OUTSIDER))  admin=$ADMIN($(uid_of $ADMIN))"
echo

# 1 -- a member can record, and the row is attributed to the connecting account
out=$(append_as $A "$(event shared-id run-a 10)")
if grep -q '"type": "receipt"' <<<"$out" && grep -q '"duplicate": false' <<<"$out"; then
  ok "member records an event"
else bad "member records an event" "$out"; fi

# 2 -- a spoofed identity in the payload is ignored, not honoured
spoof=$(printf '{"version":1,"client_event_id":"%s-spoof","run_id":"run-a","attempt_id":"1","role":"worker","usage":{"input":1,"output":1},"usage_status":"complete","occurred_at":%s,"uid":0,"user":"root","admin":true}' "$RUN_TAG" "$OCCURRED")
out=$(append_as $A "$spoof")
if grep -q '"identity_claim_ignored": true' <<<"$out"; then
  ok "payload uid/user/admin claims are reported as ignored"
else bad "payload identity claims ignored" "$out"; fi

rows=$(as $A usage query --socket "$SOCK")
if ! grep -q "\"peer_uid\": 0" <<<"$rows" && grep -q "\"peer_uid\": $(uid_of $A)" <<<"$rows"; then
  ok "the spoofed row is owned by the connecting uid, not the claimed one"
else bad "spoofed row ownership" "$rows"; fi

# 3 -- the same event id from another account is a different event
out=$(append_as $B "$(event shared-id run-b 20)")
if grep -q '"duplicate": false' <<<"$out"; then
  ok "one account cannot suppress another's record by reusing its event id"
else bad "uid-scoped deduplication" "$out"; fi

# 4 -- a replay of the same content returns the stored row
out=$(append_as $A "$(event replay-1 run-a 10)")
first=$(grep -o '"id": [0-9]*' <<<"$out" | head -1)
out2=$(append_as $A "$(event replay-1 run-a 10)")
if grep -q '"duplicate": true' <<<"$out2" && grep -q "$first" <<<"$out2"; then
  ok "a replayed delivery returns the committed row"
else bad "replay is idempotent" "$out2"; fi

# 5 -- the same id with different content is refused, never overwritten
out=$(append_as $A "$(event replay-1 run-a 999)")
if grep -q '"code": "conflict"' <<<"$out"; then
  ok "the same id with different content is refused"
else bad "conflicting duplicate refused" "$out"; fi

# 6 -- one account cannot read another's rows
rows=$(as $A usage query --socket "$SOCK" --limit 500)
if ! grep -q "\"peer_uid\": $(uid_of $B)" <<<"$rows"; then
  ok "own-scope query returns no other account's rows"
else bad "cross-account read leak" "$rows"; fi

# 7 -- asking for another account is refused outright
out=$(as $A usage query --socket "$SOCK" --only-uid "$(uid_of $B)")
if grep -q '"code": "forbidden"' <<<"$out"; then
  ok "requesting another account is refused, not quietly narrowed"
else bad "only-uid refusal" "$out"; fi

# 8 -- a non-member cannot reach the socket at all
# A valid event, so a refusal can only come from the socket, not from validation.
out=$(append_as $OUTSIDER "$(event outsider-1 run-x 10)")
if grep -qiE 'permission denied|no such file|connecting to' <<<"$out"; then
  ok "a non-member of the client group cannot connect"
else bad "non-member blocked" "$out"; fi

# 9 -- the configured administrator sees every account
rows=$(as $ADMIN usage query --socket "$SOCK")
if grep -q "\"peer_uid\": $(uid_of $A)" <<<"$rows" && grep -q "\"peer_uid\": $(uid_of $B)" <<<"$rows"; then
  ok "the configured administrator reads every account"
else bad "administrator scope" "$rows"; fi

# 10 -- an account cannot correct another account's row
target=$(as $ADMIN usage query --socket "$SOCK" --only-uid "$(uid_of $B)" | grep -o '"id": [0-9]*' | head -1 | grep -o '[0-9]*')
corr=$(printf '{"version":1,"client_event_id":"%s-steal","run_id":"run-a","attempt_id":"1","role":"worker","usage":{"input":1,"output":1},"usage_status":"complete","occurred_at":%s,"corrects":%s}' "$RUN_TAG" "$OCCURRED" "$target")
out=$(append_as $A "$corr")
if grep -q '"code": "forbidden"' <<<"$out"; then
  ok "correcting another account's row is refused"
else bad "cross-account correction" "$out"; fi

# 11 -- no account can touch the database or replace the socket
if ! sudo -u $A test -r /var/lib/timon-usage/usage.db 2>/dev/null; then
  ok "the database is unreadable to ordinary accounts"
else bad "database readable by $A"; fi
# Stat via sudo: this script's own session may predate its group membership.
if sudo -u $A rm -f "$SOCK" 2>/dev/null; then
  bad "socket unlinkable by $A"
elif sudo test -S "$SOCK"; then
  ok "an account cannot unlink the socket"
else bad "socket unlinkable by $A" "socket missing after the attempt"; fi

# 12 -- an oversized request is refused and the service keeps serving others
big=$(head -c 200000 /dev/zero | tr '\0' 'x')
out=$(append_as $A "{\"version\":1,\"client_event_id\":\"$big\"}")
if grep -qiE 'too.?large|exceeds' <<<"$out"; then
  ok "an oversized request is refused"
else bad "oversized request refused" "$(head -c 200 <<<"$out")"; fi
out=$(append_as $B "$(event after-flood run-b 3)")
if grep -q '"type": "receipt"' <<<"$out"; then
  ok "the service still serves other accounts after a bad request"
else bad "service survived bad request" "$out"; fi

echo
echo "=== $pass passed, $fail failed ==="
[ "$fail" -eq 0 ]
