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

**Cancellation from `timon runs cancel` does not yet reach a running worker.**
Ctrl-C does: the executor watches a flag and kills the worker's process group,
and the scheduler stops starting new tasks. What is still missing is the path
from a *separate* `timon runs cancel` invocation to a worker in another process —
the record moves to `cancelling` and the grant is revoked, so no new request is
authorised, but the running turn continues. Closing it needs the run to watch its
own record, or a signal addressed to it.

**Model policy cannot refuse on capability.** P1.3 substitutes the model and says
so, but a request asking for something the assigned model cannot do is not
refused, because that needs a capability model per model. Guessing would be worse
than the gap.

## Blocking

**The write-sandbox gate has not passed**, so P4.2's writing workers are not
enabled. One check fails and four have not been probed. See
`eval/results/write-sandbox-2026-09-28.json`.

Three of the unprobed four need work to probe honestly: moving refs and running
hooks need a scratch repository with a hook installed, and leaving processes
behind needs a probe that survives its parent. The fourth, reading another
account, cannot be probed on this host at all — there is only one account, and a
check that cannot fail is not a check.

## Operational

**A worker can read the pooled credential store, and this is now measured.**
Probed on 2026-09-28 through `codex exec -s workspace-write`, the path a worker
actually takes: it read `~/.timon-broker/accounts/acct3/auth.json` and returned
its first bytes. The write sandbox restricts writes and the network; reads are
wide open, and the store is owned by the same login the worker runs as.

This was an open question and is now a **failed qualification check**, so
`timon qualify write-sandbox` does not pass and writing workers stay disabled.
The fix is the one already named — the broker gets its own service account and
the store stops being readable by the worker's uid. Nothing else in P4.3 can
compensate for it: network is blocked, so a token cannot be posted out directly,
but a worker can write it into its own worktree, which becomes a branch somebody
reviews, or simply say it in an answer.

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
