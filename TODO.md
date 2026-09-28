# Outstanding

Things known to be missing or unsettled, with why they were left. Kept here so
they are not rediscovered as surprises.

## From measurement

**The P2 cost tail is not explained.** The gate's per-pair ratio has a median of
1.129, inside the registered 25% margin, but 10 of 25 pairs sit outside it and
paired deltas range from −8,946 to +8,910 tokens. The working hypothesis is that
it is the model's own variation in how many tool calls it makes, not anything the
routing does — a task that greps once costs a third of the same task grepping
three times. **Untested.** Settling it needs the per-pair transcripts compared
turn by turn, and probably more traffic than a five-task suite produces.
Deferred, not dismissed: the cost gate stays inconclusive until it is done.

**Quota percentage cannot see work this small.** 113,355 tokens moved a 7-day
window by nothing visible. The plan's budget language is written in windows, and
windows are the wrong unit below roughly a day of real use. Token counts are the
only usable comparison for now; the window figure is for capacity, not
attribution.

## Known limits, accepted for now

**Triage sends long mechanical work to a strong call.** A 380-character
extraction task a cheap call would handle is disqualified from the fast path by
length alone. Length is weak evidence in both directions and the rules use it
only in the safe one; fixing it means guessing at difficulty, which is what sends
the wrong work to the wrong place. Recorded in `eval/registration-p2.md` as a
standing disagreement rather than reconciled.

**Cancellation does not reach a running worker.** `timon runs cancel` moves the
record to `cancelling` and revokes the grant, so no *new* request is authorised —
but the worker's current turn runs to its deadline. Closing this needs a cancel
signal the executor watches, which is P4's scheduler work.

**Model policy cannot refuse on capability.** P1.3 substitutes the model and says
so, but a request asking for something the assigned model cannot do is not
refused, because that needs a capability model per model. Guessing would be worse
than the gap.

## Operational

**The broker runs as `workbench`, and so does development.** The store is owned
by that login, so the developing account can read pooled credentials. The systemd
unit bounds the blast radius — its own `User=`, the store the only writable path —
but it does not change who can read. Needs its own service account before a
second person uses the host. Open question 2 in the plan.

**No per-user fairness limits.** `TasksMax`, `LimitNOFILE` and `MemoryMax` cap the
process as a whole, so one runaway script can still occupy every slot for
everyone. Codex's review asked for per-user limits and they are not built.

**`acct3`'s plan is still settling.** It reported `go`, then `plus` with a 5-hour
window, then `prolite` with a 7-day one, over about twenty minutes on
2026-09-28. Each change invalidated its session; the broker now recovers from
that automatically. Whether `prolite` serves `gpt-5.5` is **unverified** — that
was the original capability defect on the `go` plan, and the models endpoint is
known to lie about it.

**The superseded handoff document still reads `canonical`.** Correcting the field
means rewriting 311,274 characters to change one line, and Pageden's only status
tool promotes rather than demotes. A banner at the top says so instead. It
resolves when the plan is promoted.
