# Timon

Timon steers work across a strong lead model and cheaper, bounded workers on a
shared Linux server. It uses an installed, version-pinned
[Prodex](https://github.com/christiandoxa/prodex) as its model engine.

> *Timón* (Filipino/Spanish): the helm or rudder.

## Status

Early development. The worker supervisor is in place; model routing, typed
results, usage tracking and research come next.

## Worker supervisor

`timon worker run` starts one worker process and supervises it:

- The task is read from stdin and delivered to the worker on its stdin. It is
  never placed on the command line, so it does not appear in the process list.
- The whole attempt, including task delivery, runs under a wall-clock deadline.
- The worker runs in its own process group. On timeout or cancellation
  (Ctrl-C or SIGTERM) the whole group is killed and reaped. After a normal exit,
  leftover background processes in the group are killed as well.
- stdout and stderr are captured to `stdout.log` and `stderr.log` in a private
  output directory (mode 0700, files 0600). Each stream is capped; output past
  the cap is read and discarded so the worker never blocks.
- Optional concurrency slots limit how many workers run at once across all
  users. Slot files are never deleted, so a limit cannot be exceeded by
  recreating them.

The outcome is printed as JSON.

```sh
echo "Summarise the release notes" | timon worker run \
  --output-dir /tmp/timon/attempt-1 \
  --deadline-secs 600 \
  --slot-dir /var/lib/timon/slots --slots 4 --provisioned-slots \
  -- prodex run exec -
```

Exit codes:

| Code | Meaning |
|------|---------|
| 0    | Worker exited with code 0 |
| 1    | Worker exited unsuccessfully |
| 2    | Invalid input or supervisor error |
| 75   | No free worker slot; retry later |
| 124  | Deadline expired; worker killed |
| 130  | Cancelled; worker killed |

### Shared slot directory

On a shared server, provision the slot files once and use
`--provisioned-slots`:

```sh
sudo install -d -o root -g timon-users -m 2770 /var/lib/timon/slots
for i in 000 001 002 003; do
  sudo install -o root -g timon-users -m 0660 /dev/null /var/lib/timon/slots/slot-$i.lock
done
```

## Requirements

- Linux (Unix-like systems only)
- Rust 1.89 or newer

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The tests use small `/bin/sh` scripts as workers; no model provider is needed.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
