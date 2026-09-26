# Timon

Timon steers work across a strong lead model and cheaper, bounded workers on a
shared Linux server. It uses an installed, version-pinned
[Prodex](https://github.com/christiandoxa/prodex) as its model engine.

> *Timón* (Filipino/Spanish): the helm or rudder.

## Status

Early development. Attempt supervision, typed results and usage accounting are
in place; sandbox and route qualification against the pinned executables,
durable usage recording and research come next.

Nothing here has been run against a real Prodex or Codex invocation. The event
shape Timon parses is taken from documentation and public issue reports, not
from a qualified run.

## Attempts

An attempt is one supervised child process. `timon lead run` and
`timon worker run` are the same machinery with different roles: the lead is the
strong model that plans, integrates and verifies, a worker is a cheaper model
running one bounded task. Both are supervised the same way and accounted for the
same way, because usage tracking that covered only workers would leave out the
largest consumer.

### Supervision

- The task is read from stdin and delivered to the child on its stdin. It is
  never placed on the command line, so it does not appear in the process list.
- The whole attempt, including task delivery, runs under a wall-clock deadline.
- The child runs in its own process group. On timeout or cancellation (Ctrl-C or
  SIGTERM) the whole group is killed and reaped. After a normal exit, leftover
  background processes in the group are killed as well.
- stdout and stderr are captured to `stdout.log` and `stderr.log` in a private
  output directory (mode 0700, files 0600). Each stream is capped; output past
  the cap is read and discarded so the child never blocks.
- Optional concurrency slots limit how many attempts run at once across all
  users. Slot files are never deleted, so a limit cannot be exceeded by
  recreating them.

### Typed result

With `--result-file`, Timon reads the file the model was told to write and
reports whether it is there and usable. With `--result-schema` it also validates
it against the schema the model was given.

Timon does not pass these flags to the child; the caller does, and tells Timon
which paths to read. That keeps Timon out of guessing a harness flag spelling
until the route is qualified.

Schema validation covers a documented subset of JSON Schema: `type`, `required`,
`properties`, `items`, `enum` and `additionalProperties`. Anything else fails
closed as `schema_not_applied` rather than being skipped, because skipping a
keyword would report a result as validated when the constraint that mattered was
never applied.

The point is to separate three outcomes an exit code cannot: the model produced
a usable result, the model exited 0 and produced nothing usable, or the process
itself failed.

### Usage

With `--usage-source`, Timon parses the attempt's event stream into a normalized
usage report. Two rules hold throughout:

- A count the harness did not report is `null`, never `0`, and a total built
  from a partial stream is labelled `partial`.
- Nothing is counted twice. `cached_input` is part of `input` and
  `reasoning_output` is part of `output`, so neither is added into the total;
  `total` is `input + output`. Per-turn events and an attempt-level total are
  separate authorities, and the report names which one it used.

`--usage-accounting` states whether each event reports its own turn (`delta`,
the default) or a running total (`cumulative`). Timon does not infer it from the
numbers, but it does report when the numbers contradict the declaration.

Known gaps, carried as notes rather than smoothed over: reasoning tokens are not
in the Codex stream ([openai/codex#19022]), startup prewarm usage is reported
outside `turn.completed` ([openai/codex#46975]), and `--json` can be ignored
when tools or MCP servers are active ([openai/codex#15451]) — which shows up as
`unknown` usage, not as zero.

Usage is what the harness reported. It is not a measurement, not a bill, and not
a spend limit.

### Identity

`--run-id`, `--attempt-id` and the role derive a stable `client_event_id`. It is
derived rather than generated so that a retry or a replay after a restart
produces the same id, which is what lets durable usage recording deduplicate
instead of double counting.

## Example

```sh
echo "Summarise the release notes" | timon worker run \
  --output-dir /tmp/timon/run-42/attempt-1 \
  --deadline-secs 600 \
  --run-id run-42 --attempt-id 1 \
  --result-file /tmp/timon/run-42/attempt-1/result.json \
  --result-schema /etc/timon/research-evidence.json \
  --usage-source stdout \
  --slot-dir /var/lib/timon/slots --slots 4 --provisioned-slots \
  -- prodex run exec -s read-only --json \
       -o /tmp/timon/run-42/attempt-1/result.json \
       --output-schema /etc/timon/research-evidence.json -
```

The report is printed as JSON.

Exit codes:

| Code | Meaning |
|------|---------|
| 0    | The attempt succeeded and its result, if requested, is usable |
| 1    | The child exited unsuccessfully |
| 2    | Invalid input or supervisor error |
| 65   | The child exited 0 but its result is missing or invalid |
| 75   | No free slot; retry later |
| 124  | Deadline expired; child killed |
| 130  | Cancelled; child killed |

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

Every child in the tests is a small `/bin/sh` script and every event stream is a
fixture, so no model provider is needed. They establish local supervision and
accounting behaviour only; they prove nothing about a real harness's sandbox,
flags or usage completeness.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

[openai/codex#15451]: https://github.com/openai/codex/issues/15451
[openai/codex#19022]: https://github.com/openai/codex/issues/19022
[openai/codex#46975]: https://github.com/openai/codex/issues/46975
