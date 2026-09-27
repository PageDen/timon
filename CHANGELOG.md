# Changelog

## Unreleased

### The success rubric was validated against the pilot's saved answers, and was wrong

The rubric was repaired after a harness fault voided the pilot's quality
comparison, but it had only ever been run on hand-written fixtures. Running it
against the 30 real answers the pilot saved found four defects. `eval/score.py`
now has tests — `eval/test_score.py`, 32 cases, offline — and
`eval/validate-rubric.py` re-runs the whole check.

**The hallucination check false-failed correct refusals.** It tested for one of
ten hardcoded substrings anywhere in the payload. All four c1 answers in the
pilot refused correctly, and three of them were recorded as *"invented a release
that does not exist"* because their wording missed the list: "No official release
of Rust 9.9.9 exists" does not contain the substring "no release". The one that
passed did so because its *evidence* sentence happened to say "contains no
release for version 9.9.9" — luck, not judgement.

**The same check passed real fabrications.** Because any hedge word anywhere in
the answer suppressed the invention test, an answer asserting a fabricated date
passed if it contained a hedge — even one in an unrelated field. The test is now
on the side that defines the failure: does a clause name the version, give a
date, and not deny the version exists? Existence denial and hedging are now
distinguished, because "this cannot be confirmed" is not a statement that
something does not exist.

**`dimensions_backed()` was dead code.** The function added to make citing
nothing score zero was never called; `score()` used a bare substring test
instead, so that repair was not in effect. It is now wired in, and takes an
already-computed verifier report rather than fetching every page twice.

**A cited hostname counted as addressing a subject.** Dimensions were matched
against the whole JSON payload, so an answer whose only statement was "the sky is
blue" satisfied both the `rust` and `go` dimensions of task r1 by citing
blog.rust-lang.org and go.dev. Matching is now against the answer's own words.

Two smaller things: a passing row's recorded reason led with "addressed every
required dimension" and never mentioned whether a citation had been checked; and
`run-pilot.sh` interpolated task JSON into a shell string to read its id, which
the shell unescaped, so tasks whose text contained a quote wrote their output to
`t<i>` instead of their id and could not be mapped back to their task afterwards.

**Wiring in the dimension requirement is a stricter definition of success than
the pilot used.** It is a change to the endpoint, so any further paid evaluation
has to re-register it rather than inherit it.


### Usage detail is retained for 180 days, then rolled up

Usage detail no longer accumulates forever. Individual events are kept for 180
days; older ones become per-account, per-month totals, which are kept
indefinitely. A monthly total answers what an account cost, while a year of event
rows also records each individual thing that account ran, and on a shared machine
those are different things to hold.

Nothing is deleted to achieve this. `usage_events` is append-only and its
triggers would refuse a delete, so retention builds a new database — detail still
inside the window, plus totals for everything older — verifies that the live
event count and token sums are identical across the boundary, and swaps the file.
Each published database stays append-only for its whole life; history is reshaped
only at a visible, verified, operator-initiated boundary.

- `timon usage retain --keep-days 180 [--dry-run]` does the work, and
  `timon-usage-retain.timer` runs it weekly. It refuses with **exit 75** while the
  recorder is listening, because the daemon is the only writer and swapping the
  file under it would leave it writing where nothing reads. A *stale* socket left
  by a killed daemon does not block it.
- The pre-retention database is kept beside the live one, not removed: it is the
  only remaining copy of the detail just rolled up, so a mistaken `--keep-days` is
  recoverable.
- A correction chain spanning the cutoff is kept whole, so a retained correction
  can never point at a row that was rolled up. A row a correction already
  replaced is rolled up but not counted, exactly as a report never counted it.
- `timon usage report` marks any window reaching back before the cutoff, in text
  and in the CSV header. The failure this prevents is the quiet one: a total that
  reads like a quiet month when it is really a month whose detail is gone.
- `timon usage monthly` reads the totals, over the socket, reachable by an
  ordinary account for its own figures — after retention it is the only way their
  owner can still see them, and a usage figure its subject cannot see is not
  visibility.
- Unreported usage stays unreported: an event whose producer gave no figures is
  rolled up as unknown rather than folded in as zero.

### A reused uid no longer inherits the previous holder's history

The uid is the identity — `SO_PEERCRED` reports a number and nothing else — and
Linux reissues that number once its account is gone. Previously the runbook
carried this as an operating rule the code could not enforce. Now
`timon usage retire-uid` records a boundary, closing the uid's generation:

- Events recorded after a boundary belong to the next generation, and the two are
  never totalled together. A new holder of uid 1001 sees its own usage, not the
  previous holder's, and cannot correct the previous holder's rows.
- An administrator still sees every generation, labelled and listed separately
  rather than summed.
- The generation is part of the deduplication key, which matters more than it
  first appears: `client_event_id` is derived from role, run id and attempt id
  rather than generated, so two people sharing a recycled uid easily produce the
  same id. Without a boundary the second event was silently dropped as a
  duplicate, or refused as a conflict. **This was a real defect, not a
  theoretical one.**
- Schema v2. An existing v1 database is migrated on open by rebuilding
  `usage_events` with the wider key, carrying ids over so corrections still
  resolve, and aborting the whole transaction if the row count or token sum
  changes. Pre-existing rows are generation 0: there was no boundary to place
  them after.

Recording the retirement is still the operator's job, and deliberately so.
Nothing watches `/etc/passwd`, because a missing entry is not proof an account was
removed — it is also what a directory service outage looks like, and guessing
wrong would split one person's history in two. What has changed is that the rule
is now enforceable at all, and that forgetting it is visible rather than silent.

## v0.2.0 — 2026-09-27

First release with an install path. The per-account usage recorder is the part
with the strongest evidence behind it and the reason for the version; the
orchestration is present but **experimental**, for the reason in the last
section.

### Per-account usage recording

A local daemon records what each account's model attempts reported spending,
durably, on a shared host. Identity comes from the kernel: every connection's
uid is read with `SO_PEERCRED` and decides both attribution and what the caller
may read, so no payload can record usage as another account or read one that is
not its own.

- At-least-once delivery with idempotent insertion on `(peer_uid,
  client_event_id)`. A replay returns the committed row; the same id with
  different content is refused rather than overwriting what is stored.
- The receipt is produced only after the transaction commits under
  `synchronous = FULL`, so a caller that never sees an acknowledgement can
  safely retry.
- Rows are never updated or deleted. A correction is a new row pointing at what
  it corrects, and only its owner may write it.
- An event is written to the account's private spool *before* it is sent, so a
  recorder that is down costs a delay rather than the record. `timon usage
  replay` drains a spool stranded by an outage.
- Losslessness is not promised, so loss is made visible: a full spool records a
  coalesced gap marker with how many events were lost and the window they fell
  in.
- Unreported usage is stored as SQL `NULL` and reads back unknown. It is never
  zero, and totals count it separately rather than absorbing it.
- Reports as text, JSON or CSV, with UTC window boundaries. Corrections are
  counted once, in place of what they correct.
- `VACUUM INTO` backups, verified before publishing: integrity, the presence of
  the deduplication key, and that every row committed before the copy began is
  in it. A restore states its recovery point and how many live rows it would
  discard.

### Host-wide worker slots

One concurrency limit across every account on a host, enforced by locks on
operator-provisioned files. `timon slots provision` creates them idempotently and
never replaces an existing file, because a slot file replaced while a launcher
holds its lock would hand that slot to someone else and quietly double the limit.

### Worker supervision

Bounded task delivery on stdin, a deadline covering input delivery as well as
execution, process-group containment, size-bounded private output capture, and
typed process outcomes.

### Typed results and usage parsing

A result is validated against the schema the model was given, over a documented
subset of JSON Schema that fails closed on unsupported keywords. Usage is
normalised with explicit delta or cumulative semantics, cached input treated as a
subset rather than added twice, and unknown never rendered as zero.

### Per-run admission

An attempt count that is enforced, and a token *admission ceiling* that is an
estimate. The ceiling decides whether to start another attempt; it cannot bound
what a running one spends, and nothing here claims otherwise. Unknown usage keeps
its full reservation rather than settling to zero.

### Citation checking

`timon research verify` fetches a cited page and looks for the passage quoted.
Address screening happens on the resolved address and on every redirect hop, and
refuses loopback, private, link-local including the metadata address, CGNAT,
benchmarking and protocol-assignment ranges, their IPv6 equivalents, and IPv4
addresses mapped into IPv6.

What a pass establishes is named precisely: `quotation_present`, meaning the
passage is on the page. **Not** that the page is correct, that the claim follows
from it, or that the quotation kept the page's qualifications and negations. A
claim contradicting its own genuine quotation passes this check.

### Orchestration — experimental, and it did not pay on what was measured

`timon orchestrate` runs a goal through plan, delegate and integrate: the lead
plans and integrates, the host runs what it asked for and returns every result
and rejection. One round; no replanning.

A six-task pilot measured it against a single strong model and a single cheap
model on the same tasks. It cost more on four of six tasks — geometric mean of
task-level ratios 1.365× against the strong model, 2.967× against the cheap one —
and was slower on all six, median 89.9s against 29.7s. The quality comparison
from that pilot is void through a harness fault. Every orchestrated run pays a
stable floor of roughly 30,000 tokens in two lead calls before delegating
anything.

So it ships opt-in and experimental, with **no** cost or speed claim.

### Known limitations

- Usage recording is visibility, not billing. An account can under-report,
  fabricate its own payload, delete its spool, or run a model client directly.
  Period reports cannot prove completeness.
- The append-only triggers are application integrity guards, not protection
  against root or the file's owner.
- No hard token cap exists. Admission is an estimate and overshoot is possible.
- **Accepted, not fixed:** a lease whose process is `SIGKILL`ed never runs its
  destructor, so slot release falls back to the descriptor closing at exit. Where
  a concurrently forked child shares that open file description, the slot stays
  locked until the child exits and appears busy while free. This fails in the
  safe direction — the host limit is never exceeded, work is refused with exit 75
  while capacity exists — and a fix would need release that does not depend on
  the holder running code, where breaking a lock held by a process that may still
  be alive risks over-admission instead.
- **Accepted, not fixed:** after a worker exits normally its process group is
  killed to remove leftover descendants, and by then the group leader has been
  reaped so its pid is free for reuse. Reissued as a new group leader inside that
  window, an unrelated group would be killed (round-0 finding C4). The window is
  microseconds, pids are allocated sequentially to a large maximum, and this has
  never been observed. Worth revisiting on a host with very high pid churn.
- A uid must not be recycled while records for it are retained; the code does not
  enforce this.
- Semantic claim support is unimplemented; see citation checking above.
- Linux and Unix only. A non-Unix build is refused at compile time.
- The engine is the Codex CLI invoked directly. Quota rotation across several
  accounts of one provider is therefore not available: that was Prodex's reason
  for being in the design, and with a single account it bought nothing while
  adding a dependency, a version pin and a forced-full-access risk. Reconsider
  only with two or more accounts, and requalify the route before relying on it.

### Verified

181 crate tests. On a provisioned host: 20 cross-account checks and 8 shared-slot
checks, including a non-member refused with `EACCES`, a copied spool attributed
to whoever replays it, and the concurrency limit holding across two accounts
while the recorder is stopped.
