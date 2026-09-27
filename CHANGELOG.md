# Changelog

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
- A lease whose process is `SIGKILL`ed never runs its destructor, so slot release
  falls back to descriptor close.
- Process-group id reuse in a narrow window remains theoretically possible
  (round-0 finding C4), unconfirmed.
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
