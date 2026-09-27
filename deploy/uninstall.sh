#!/usr/bin/env bash
# Removes the Timon usage recorder from a host.
#
# The database is kept unless --purge is given, because removing a service is a
# different decision from destroying the usage history it recorded.
set -euo pipefail
PURGE=no
[ "${1:-}" = "--purge" ] && PURGE=yes

say() { printf '  %s\n' "$*"; }

say "stopping and disabling"
sudo systemctl disable --now timon-usage-backup.timer 2>/dev/null || true
sudo systemctl disable --now timon-usage-backup.service 2>/dev/null || true
sudo systemctl disable --now timon-usage.service 2>/dev/null || true
sudo rm -f /etc/systemd/system/timon-usage.service \
           /etc/systemd/system/timon-usage-backup.service \
           /etc/systemd/system/timon-usage-backup.timer
sudo systemctl daemon-reload
sudo rm -rf /run/timon-usage

say "binary and slots"
sudo rm -rf /usr/local/lib/timon /var/lib/timon-slots

if [ "$PURGE" = yes ]; then
  say "purging the database and its backups, as asked"
  sudo rm -rf /var/lib/timon-usage
else
  say "keeping /var/lib/timon-usage; pass --purge to remove the usage history too"
fi

say "accounts and groups"
sudo userdel adaptive-usage 2>/dev/null || true
sudo groupdel adaptive-usage 2>/dev/null || true
sudo groupdel adaptive-users 2>/dev/null || true

echo
for p in /etc/systemd/system/timon-usage.service /usr/local/lib/timon /var/lib/timon-slots; do
  [ -e "$p" ] && say "STILL PRESENT: $p" || say "gone: $p"
done
getent passwd adaptive-usage >/dev/null && say "STILL PRESENT: user adaptive-usage" || say "gone: user adaptive-usage"
[ -d /var/lib/timon-usage ] && say "kept: /var/lib/timon-usage (usage history)" || say "gone: /var/lib/timon-usage"
say "Group membership changes for real users are not reverted; remove them by hand if wanted."
