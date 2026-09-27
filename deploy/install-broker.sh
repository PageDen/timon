#!/usr/bin/env bash
# Provisions the pooled credential store for quota rotation (amendment A4).
#
# Deliberately separate from install.sh and deliberately not called by it. The
# broker is the most concentrated trust in this project: the account that owns
# this store holds every pooled credential, and once the proxy exists it will see
# every prompt and response in plaintext. A host running the usage recorder should
# not acquire that by default, so installing it is a decision someone makes on
# purpose.
#
# This slice creates the store and nothing else. There is no proxy yet, so
# provisioning this has no effect on how anybody's Codex runs.
#
# Run as a user with sudo. Adding accounts is a separate, manual step, printed at
# the end: it needs an interactive login per account and cannot be scripted here.
set -euo pipefail

STORE=${TIMON_BROKER_STORE:-/var/lib/timon-broker/accounts}
SERVICE_USER=${TIMON_BROKER_USER:-adaptive-broker}
BINARY="${1:-/usr/local/lib/timon/timon}"

[ -x "$BINARY" ] || { echo "install-broker.sh: $BINARY is not executable" >&2; exit 2; }

say() { printf '  %s\n' "$*"; }

say "service account, separate from the recorder's on purpose"
# A compromise of the usage recorder must not yield model credentials, and a
# compromise of the broker must not yield the usage database. Two accounts, two
# blast radiuses.
getent group "$SERVICE_USER" >/dev/null || sudo groupadd --system "$SERVICE_USER"
if ! getent passwd "$SERVICE_USER" >/dev/null; then
  sudo useradd --system --gid "$SERVICE_USER" --no-create-home \
       --home-dir "$(dirname "$STORE")" --shell /usr/sbin/nologin "$SERVICE_USER"
fi

say "store, readable only by the service account"
sudo install -d -o "$SERVICE_USER" -g "$SERVICE_USER" -m 0700 "$(dirname "$STORE")"
sudo install -d -o "$SERVICE_USER" -g "$SERVICE_USER" -m 0700 "$STORE"

say "inventory"
# Exits non-zero while any account is faulty, which an empty store is not.
sudo -u "$SERVICE_USER" "$BINARY" broker accounts --store "$STORE" || true

cat <<NEXT

  Adding an account needs an interactive login and cannot be scripted here.
  For each pooled account, as the service account:

      sudo install -d -o $SERVICE_USER -g $SERVICE_USER -m 0700 $STORE/<name>
      sudo -u $SERVICE_USER env CODEX_HOME=$STORE/<name> codex login

  The login flow wants a browser. On a headless host, forward its port first:

      ssh -L 1455:localhost:1455 <you>@<this-host>

  Then check what landed, which never prints a credential:

      sudo -u $SERVICE_USER $BINARY broker accounts --store $STORE

  Rotation needs more than one usable account. With one, it does nothing.

  Two things this does not yet do, because the proxy is not built: nothing routes
  through these credentials, and no user's Codex is affected. Provisioning this is
  safe to do early and reversible by removing $STORE and the $SERVICE_USER account.
NEXT
