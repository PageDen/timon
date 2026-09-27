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

## Retention

Two different things are retained on two different schedules.

**Backups** are pruned to the newest `--keep` (14 by default) after a successful
publish. Set `TIMON_KEEP_BACKUPS` when installing to change it.

**Usage detail** is kept for 180 days, then rolled up into per-account,
per-month totals by `timon-usage-retain.timer`, weekly on Sunday at 04:00. The
monthly totals are kept indefinitely: a total answers what an account cost,
while a year of event rows also records each individual thing that account ran,
which is a more sensitive thing to hold on a shared machine.

Nothing is deleted. `usage_events` is append-only and the triggers would refuse a
delete; retention builds a new database holding the detail still inside the
window plus totals for everything older, checks that not one token went missing
across the boundary, and swaps the file. Each published database is append-only
for its whole life.

To change the window, edit `--keep-days` in
`/etc/systemd/system/timon-usage-retain.service`.

### Running it by hand

Always look before you cut:

```
sudo -u adaptive-usage /usr/local/lib/timon/timon usage retain \
    --database /var/lib/timon-usage/usage.db \
    --socket /run/timon-usage/usage.sock \
    --keep-days 180 --dry-run
```

A real run needs the recorder stopped — it is the only writer, and swapping the
file underneath it would leave it writing where nothing reads. `--socket` is
checked first and the run refused with **exit 75** if the daemon answers, so the
ordinary mistake is caught rather than tolerated. The timer's unit handles the
stop and the restart itself.

Each run leaves the pre-retention database beside the live one as
`usage.db.pre-retain-<unix>.db`. **That file is the only remaining copy of the
detail just rolled up.** It is deliberately not removed, so a mistaken
`--keep-days` is recoverable; delete it once you are satisfied.

### Reading a total after its detail is gone

`timon usage report` says so rather than returning a small number that reads like
a quiet month: any window reaching back before the cutoff is marked, in text and
in the CSV header. The totals themselves are still readable, by their owner as
well as by an administrator:

```
timon usage monthly --socket /run/timon-usage/usage.sock
timon usage monthly --socket /run/timon-usage/usage.sock --from-month 2026-01
```

## Desktop and IDE sessions

Desktop and the IDE extensions speak the app-server protocol and do not go
through anything Timon supervises, so by default their usage is not recorded. To
record it, have each person start their client through the bridge:

```
timon bridge --socket /run/timon-usage/usage.sock -- codex app-server
```

Point the client's "codex executable" setting at a wrapper containing that line,
per account. **One app-server per account**: a shared daemon serves several people
from one process, so every session would be attributed to whoever started it. The
bridge refuses a `daemon`, `proxy` or `--code-mode-host` invocation for that
reason rather than recording something misleading.

Diagnostics: `--report` prints a JSON summary to stderr on exit, with bytes
proxied each way, turns observed, events recorded and spooled, and two fields
worth watching. `malformed_usage_notifications` above zero means the protocol has
changed shape — the sessions still work, but turns are going unrecorded, so
report it. `disagreed_with_thread_total` above zero means the per-turn figures no
longer sum to the server's own thread total, which a thread compaction can cause
legitimately.

The bridge never alters the protocol. If a client misbehaves while using it, the
first check is whether it misbehaves without it: `timon bridge` with no `--socket`
proxies and records nothing, which isolates the bridge from the app-server.

Pinned version tested: `codex-cli 0.153.2`. The pass-through is version-agnostic
by construction, but the usage notification's shape is not, so re-check
`malformed_usage_notifications` after a client upgrade.

## uid reuse

**The uid is the identity.** `SO_PEERCRED` reports a number and nothing else, a
username change does not create a new one, and Linux reissues a number after its
account is removed.

**So when you remove an account, record it:**

```
sudo -u adaptive-usage /usr/local/lib/timon/timon usage retire-uid \
    --database /var/lib/timon-usage/usage.db \
    --uid 1001 --username alice --note "left the team"
```

That closes the uid's current generation. Events recorded afterwards belong to
the next one, and the two are never totalled together: a new holder of uid 1001
sees its own usage and not the previous holder's, and cannot correct the previous
holder's rows. An administrator can still see both, labelled and listed
separately. Existing rows are not modified.

The generation is also part of the deduplication key, which matters more than it
looks: `client_event_id` is derived from the role, run id and attempt id rather
than generated, so two people who share a recycled uid can easily produce the
same id. Without a boundary the second one's event is silently dropped as a
duplicate, or refused as a conflict.

**If you forget**, the two histories merge and there is no way to separate them
afterwards — the database cannot tell that a number changed hands. The one signal
is a report showing several names within one generation, which is usually just a
rename. Recording the retirement is not optional if the figures are meant to
mean anything.

This is deliberately not automatic. Nothing watches `/etc/passwd`, because a
missing entry is not proof an account was removed — it is also what a directory
service outage looks like, and guessing wrong would split one person's history in
two.

Better still, do not recycle uids at all. Recording the boundary makes reuse
safe; not reusing the number in the first place makes the question moot.

## Removal

```
sudo ./deploy/uninstall.sh            # keeps the usage history
sudo ./deploy/uninstall.sh --purge    # removes it too
```

Group membership for real users is not reverted.
