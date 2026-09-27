#!/usr/bin/env bash
# Installs or updates the Timon usage recorder on a host.
#
# Idempotent by design: it creates what is missing and leaves what exists alone.
# It never touches the database, and it never removes a slot file -- a slot file
# replaced while a launcher holds its lock would hand that slot to someone else
# and quietly double the host limit.
#
# Run as a user with sudo. Pass the binary to install as the first argument.
set -euo pipefail

BINARY="${1:?usage: install.sh /path/to/timon [admin-user]}"
ADMIN_USER="${2:-$(id -un)}"
SLOTS="${TIMON_SLOTS:-4}"
KEEP_BACKUPS="${TIMON_KEEP_BACKUPS:-14}"

STATE=/var/lib/timon-usage
SLOTDIR=/var/lib/timon-slots
LIBDIR=/usr/local/lib/timon
UNITS=/etc/systemd/system

[ -x "$BINARY" ] || { echo "install.sh: $BINARY is not executable" >&2; exit 2; }
id -u "$ADMIN_USER" >/dev/null 2>&1 || { echo "install.sh: no such user $ADMIN_USER" >&2; exit 2; }
command -v systemctl >/dev/null || { echo "install.sh: systemd is required" >&2; exit 2; }

say() { printf '  %s\n' "$*"; }

say "client group"
getent group adaptive-users >/dev/null || sudo groupadd --system adaptive-users
say "service account, deliberately outside the client group so group membership grants it nothing"
getent group adaptive-usage >/dev/null || sudo groupadd --system adaptive-usage
if ! getent passwd adaptive-usage >/dev/null; then
  sudo useradd --system --gid adaptive-usage --no-create-home \
       --home-dir "$STATE" --shell /usr/sbin/nologin adaptive-usage
fi

say "state directory, private to the service account"
sudo install -d -o adaptive-usage -g adaptive-usage -m 0700 "$STATE"
sudo install -d -o adaptive-usage -g adaptive-usage -m 0700 "$STATE/backups"

say "binary"
sudo install -d -o root -g root -m 0755 "$LIBDIR"
CHANGED=no
if ! sudo cmp -s "$BINARY" "$LIBDIR/timon" 2>/dev/null; then
  sudo install -o root -g root -m 0755 "$BINARY" "$LIBDIR/timon"
  CHANGED=yes
fi
say "version: $("$LIBDIR/timon" --version)"

say "host-wide worker slots"
sudo "$LIBDIR/timon" slots provision --dir "$SLOTDIR" --slots "$SLOTS" \
     --group adaptive-users >/dev/null

say "systemd units"
HERE="$(cd "$(dirname "$0")" && pwd)"
for unit in timon-usage.service timon-usage-backup.service timon-usage-backup.timer; do
  [ -f "$HERE/$unit" ] || { echo "install.sh: $HERE/$unit missing" >&2; exit 2; }
  tmp=$(mktemp)
  # The reference unit ships admin-uid 0; point it at this host's administrator.
  sed "s|--admin-uid 0|--admin-uid $(id -u "$ADMIN_USER")|; s|--keep 14|--keep $KEEP_BACKUPS|" \
      "$HERE/$unit" > "$tmp"
  sudo install -m 0644 "$tmp" "$UNITS/$unit"
  rm -f "$tmp"
done
sudo systemctl daemon-reload
sudo systemctl enable --now timon-usage.service >/dev/null
sudo systemctl enable --now timon-usage-backup.timer >/dev/null
# Only bounce the service when the binary actually changed, so re-running this
# does not interrupt recording for no reason.
if [ "$CHANGED" = yes ]; then
  say "binary changed; restarting the recorder"
  sudo systemctl restart timon-usage.service
fi

sleep 2
say "recorder: $(systemctl is-active timon-usage.service)"
say "backup timer: $(systemctl is-active timon-usage-backup.timer)"
echo
say "Add each user who should be recorded to the adaptive-users group:"
say "    sudo usermod -aG adaptive-users <user>     # takes effect at their next login"
say "Usage recording is visibility, not billing: an account can under-report or"
say "run a model client directly, so totals are reported figures, not measurements."
