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

## Not built yet

**Untracked files are never given to a worker.** `git stash create` captures
modifications to tracked files and nothing else, and adding untracked files to a
snapshot would mean touching the developer's checkout to do it. So a run against
a tree with new files starts from the commit, and the report names the files
that were left out rather than calling the tree clean. Closing it properly needs
a way to build a commit from the index plus untracked paths without disturbing
the working tree — `git stash create` will not do it, and nothing else obvious
will either.

**Acceptance criteria only describe files.** A criterion says a file exists,
contains text, is absent, or no longer contains text — because a criterion is
checked mechanically and those are the claims a machine can settle. Plenty of
real requirements are not of that shape: "the endpoint returns 400 on bad input"
needs the endpoint run, and running a model-chosen command is the model choosing
its own permissions. A run whose requirements do not fit reports *not
established*, which is honest and is also a real limit on how much the verifier
can ever say.

**A project declares its checks or gets none.** `git config timon.checks` is how
a repository says what to run. Guessing a build command from a filename would
mean executing arbitrary commands in somebody's checkout on the strength of a
convention, so an undeclared project gets *not applicable* rather than a
silent pass.

## Known limits, accepted for now

**Triage misses a comma-separated list of deliverables.** "Write A, B, and C"
reads as one deliverable, because the separators it looks for are `;`, ` then `,
` and also ` and list markers — a comma and a bare "and" appear constantly in
ordinary prose, and counting them would send single-piece work to the planner,
which is the expensive mistake. Observed on 2026-10-01: a three-file goal went
to one worker, which did all three in 46s. Not obviously wrong, but it was not a
decision anybody made.

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

## Qualified, with two things to keep in view

**The write-sandbox gate passes as of 2026-09-28**, all nine checks probed. See
`eval/results/write-sandbox-2026-09-28.json`, which keeps every run including the
two that did not pass.

Two results are narrower than their names, and both are recorded:

*Hooks.* What was shown is that a `pre-commit` hook could not be triggered,
because committing is refused. Not that every path to a hook is closed.

*Processes.* The sandbox does **not** stop a worker spawning something that
outlives it — under bare `codex exec` the background process survived. What makes
the check hold is Timon's own worker supervision reaping the process group. The
confinement comes from two places and only one of them is Codex's, so a change to
worker supervision could break this without touching anything that looks like
sandboxing.

## Operational

**Fixed 2026-09-28: the broker has its own account and the store is not
readable by workers.** It ran as `workbench`, the same login as the workers, and
a probe confirmed a worker could read `auth.json` and return its bytes. The
broker now runs as the system account `timon-broker` with the store at
`/var/lib/timon-broker/accounts` at 0700, and the same probe is refused. The
duplicate under `~/.timon-broker` was deleted — a second copy of a credential
whose refresh token is single-use is not a backup, it is a way to kill the live
one.

**What it does not cover:** a worker runs as the developer, so it can still read
that developer's own logins, `~/.codex/auth.json` among them. That is inherent to
running as them and no sandbox undoes it. What changed is that the *pooled* store
is no longer any developer's to read.

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
