#!/usr/bin/env bash
# Host-wide worker slots across real accounts.
#
# The limit is enforced by OS file locks on operator-owned files, so this needs
# separate UIDs and a provisioned directory. A single-process test would only
# show that the code calls flock, not that one limit actually covers the host.
set -uo pipefail

TIMON=${TIMON:-/usr/local/lib/timon/timon}
SLOTS_DIR=${SLOTS_DIR:-/var/lib/timon-slots}
A=timontest1
B=timontest2
OUTSIDER=timontest3
pass=0; fail=0
ok()  { printf '  PASS  %s\n' "$1"; pass=$((pass+1)); }
bad() { printf '  FAIL  %s\n     -> %s\n' "$1" "${2:-}"; fail=$((fail+1)); }

# Runs a worker as $1 that holds its slot for $2 seconds.
hold() {
  local user=$1 secs=$2 tag=$3
  sudo -u "$user" env HOME="/home/$user" sh -c "
    rm -rf /home/$user/slot-$tag && mkdir -p /home/$user/slot-$tag && chmod 700 /home/$user/slot-$tag
    echo x | $TIMON worker run \
      --output-dir /home/$user/slot-$tag/att --deadline-secs 30 \
      --slot-dir $SLOTS_DIR --slots 4 --provisioned-slots \
      -- /bin/sleep $secs" >/dev/null 2>&1
  echo $? > "/tmp/slotrc-$tag"
}

slot_count=$(sudo sh -c "ls -1 $SLOTS_DIR/slot-*.lock | wc -l")
echo "=== host-wide worker slots ==="
echo "directory: $SLOTS_DIR ($slot_count slot files)"
echo

# 1 -- a member of the group can take a slot
sudo -u $A env HOME=/home/$A sh -c "
  rm -rf /home/$A/s1 && mkdir -p /home/$A/s1 && chmod 700 /home/$A/s1
  echo x | $TIMON worker run --output-dir /home/$A/s1/att --deadline-secs 15 \
    --slot-dir $SLOTS_DIR --slots 4 --provisioned-slots -- /bin/true" >/dev/null 2>&1
[ $? -eq 0 ] && ok "a group member can take a slot" || bad "member can take a slot"

# 2 -- a non-member cannot
out=$(sudo -u $OUTSIDER env HOME=/home/$OUTSIDER sh -c "
  rm -rf /home/$OUTSIDER/s1 && mkdir -p /home/$OUTSIDER/s1 && chmod 700 /home/$OUTSIDER/s1
  echo x | $TIMON worker run --output-dir /home/$OUTSIDER/s1/att --deadline-secs 15 \
    --slot-dir $SLOTS_DIR --slots 4 --provisioned-slots -- /bin/true" 2>&1)
if [ $? -ne 0 ] && grep -qiE 'permission denied|denied' <<<"$out"; then
  ok "a non-member cannot reach the slot files"
else bad "non-member blocked" "$out"; fi

# 3 -- one limit spans accounts: fill it from two, then a third request fails
for i in 1 2; do hold $A 8 "a$i" & done
for i in 1 2; do hold $B 8 "b$i" & done
sleep 3
extra=$(sudo -u $A env HOME=/home/$A sh -c "
  rm -rf /home/$A/s9 && mkdir -p /home/$A/s9 && chmod 700 /home/$A/s9
  echo x | $TIMON worker run --output-dir /home/$A/s9/att --deadline-secs 5 \
    --slot-dir $SLOTS_DIR --slots 4 --provisioned-slots -- /bin/true" 2>&1; echo "rc=$?")
if grep -q 'rc=75' <<<"$extra"; then
  ok "the limit spans accounts: a 5th worker is refused with exit 75"
else bad "host-wide limit" "$extra"; fi
wait
if [ "$(cat /tmp/slotrc-a1 /tmp/slotrc-b1 2>/dev/null | sort -u)" = "0" ]; then
  ok "the four holders all completed and released"
else bad "holders released" "$(cat /tmp/slotrc-* 2>/dev/null | tr '\n' ' ')"; fi

# 4 -- the slot files are not the users' to change
if sudo -u $A rm -f "$SLOTS_DIR/slot-000.lock" 2>/dev/null; then
  bad "a member could unlink a slot file"
elif sudo test -e "$SLOTS_DIR/slot-000.lock"; then
  ok "a member cannot unlink a slot file"
else bad "a member could unlink a slot file" "it is gone"; fi

if sudo -u $A sh -c "touch $SLOTS_DIR/slot-099.lock" 2>/dev/null; then
  bad "a member could add a slot, raising the host limit"
else ok "a member cannot add a slot"; fi

# 5 -- the inode must not move: replacing a file would double the limit
before=$(sudo sh -c "stat -c %i $SLOTS_DIR/slot-*.lock" | tr '\n' ' ')
for i in 1 2 3; do
  sudo -u $A env HOME=/home/$A sh -c "
    rm -rf /home/$A/c$i && mkdir -p /home/$A/c$i && chmod 700 /home/$A/c$i
    echo x | $TIMON worker run --output-dir /home/$A/c$i/att --deadline-secs 10 \
      --slot-dir $SLOTS_DIR --slots 4 --provisioned-slots -- /bin/true" >/dev/null 2>&1
done
after=$(sudo sh -c "stat -c %i $SLOTS_DIR/slot-*.lock" | tr '\n' ' ')
[ "$before" = "$after" ] && ok "slot inodes are stable across many acquire/release cycles" \
  || bad "inode stability" "before=$before after=$after"

# 6 -- slots must not depend on the recorder being up
was_active=$(systemctl is-active timon-usage 2>/dev/null)
sudo systemctl stop timon-usage 2>/dev/null
sudo -u $A env HOME=/home/$A sh -c "
  rm -rf /home/$A/s2 && mkdir -p /home/$A/s2 && chmod 700 /home/$A/s2
  echo x | $TIMON worker run --output-dir /home/$A/s2/att --deadline-secs 15 \
    --slot-dir $SLOTS_DIR --slots 4 --provisioned-slots -- /bin/true" >/dev/null 2>&1
rc=$?
[ "$was_active" = "active" ] && sudo systemctl start timon-usage 2>/dev/null && sleep 2
[ $rc -eq 0 ] && ok "slots still work while the usage recorder is down" \
  || bad "slots independent of the recorder" "exit $rc"

rm -f /tmp/slotrc-*
echo
echo "=== $pass passed, $fail failed ==="
[ "$fail" -eq 0 ]
