# Running the Timon usage recorder

For whoever operates the host. It assumes the recorder is installed by
`deploy/install.sh`.

## What it is, and what it is not

It records what each account's model attempts **reported** spending. That is
useful for visibility and useless as a bill: an account can under-report,
fabricate its payload, delete its spool, or run a model client directly and be
recorded not at all. Any figure taken from here should be described as reported
usage, never as measured spend, and never as a per-account invoice — the provider
quota is shared.

## Install and update

```
sudo ./deploy/install.sh /path/to/timon [admin-user]
```

Idempotent. It creates what is missing, leaves what exists, never touches the
database, and restarts the service only when the binary actually changed. Slot
files are never replaced, because replacing one while a launcher holds its lock
would hand that slot to another process and double the host limit.

Adding a user who should be recorded:

```
sudo usermod -aG adaptive-users <user>      # takes effect at their next login
```

That last part matters and has bitten us: `usermod` does not change the
credentials of sessions already running, so a logged-in user will keep getting
permission denied on the socket until they log in again.

## Checks

```
systemctl is-active timon-usage.service
systemctl list-timers timon-usage-backup.timer
sudo -u <user> /usr/local/lib/timon/timon usage report --socket /run/timon-usage/usage.sock
```

Expected permissions — if any of these differ, stop and find out why before
changing them:

| Path | Owner | Mode |
|---|---|---|
| `/run/timon-usage` | `adaptive-usage:adaptive-users` | `0750` |
| `/run/timon-usage/usage.sock` | `adaptive-usage:adaptive-users` | `0660` |
| `/var/lib/timon-usage` | `adaptive-usage:adaptive-usage` | `0700` |
| `/var/lib/timon-usage/usage.db` | `adaptive-usage` | `0600` |
| `/var/lib/timon-slots` | `adaptive-usage:adaptive-users` | `0750` |
| `/var/lib/timon-slots/slot-*.lock` | `adaptive-usage:adaptive-users` | `0660` |

## Symptoms

**A user gets "permission denied" on the socket.** Almost always stale session
credentials: `id -nG` in their shell will not list `adaptive-users` even though
`/etc/group` does. They need a fresh login.

**Runs succeed but nothing is recorded.** Recording is deliberately unable to
fail a run. Check the attempt's JSON for a `recording` field: `spooled` means the
recorder was unreachable and the event is waiting, `dropped` means it is gone.
`timon usage replay --socket … ` drains a spool.

**A report total looks too low.** Look at `events_with_unknown_usage` beside it.
Attempts whose usage was never reported are excluded from the sum by design,
because unknown is not zero. A low total with a high unknown count is the system
working, not a fault.

**`no free slot`, exit 75.** Every slot on the host is held. This is a host-wide
limit across all accounts, not per-user. Genuine exhaustion looks like four
holders in `timon` processes; if nothing is running and slots still refuse,
capture the state and report it — a spurious refusal was a real defect once
(fixed in `90cde93`) and could recur in another form.

**Backup did not appear.** A copy that fails verification is deleted rather than
published, so a failed run leaves the previous backup as the newest. Check
`journalctl -u timon-usage-backup.service`. An interrupted run leaves a
`.partial-*` file, which is deliberately not a backup.

## Restore

```
sudo -u adaptive-usage /usr/local/lib/timon/timon usage restore \
     --from /var/lib/timon-usage/backups/usage-<stamp>.db \
     --to /var/lib/timon-usage/usage.db --dry-run
```

Always dry-run first: it reports the recovery point and **how many live rows the
backup does not have**. A restore is a recovery-point rollback, not a repair —
every event acknowledged after that point is gone, and restoring is not evidence
that later events survived. Stop the service first, and the database it replaces
is renamed aside rather than deleted.

## Retention and uid reuse

Backups are pruned to the newest `--keep` (14 by default) after a successful
publish. Set `TIMON_KEEP_BACKUPS` when installing to change it.

**A uid must not be recycled while records for it are retained.** The number is
the identity; a username change does not create a new one. If an account is
deleted and its uid later reissued, the new person inherits the old usage
history. The code does not and cannot enforce this — it is an operating rule.

## Removal

```
sudo ./deploy/uninstall.sh            # keeps the usage history
sudo ./deploy/uninstall.sh --purge    # removes it too
```

Group membership for real users is not reverted.
