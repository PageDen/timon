# Outstanding

Things known to be missing or unsettled, with why they were left. Kept here so
they are not rediscovered as surprises.

## From measurement

**The P2 cost tail is explained only negatively, and the cost gate passed.**
Settled 2026-10-01 by `eval/explain-tail.py` against the saved transcripts. The
hypothesis recorded here — that the tail was the model varying how many tool
calls it made — **was wrong**: 24 of 25 pairs took exactly one turn on *both*
arms and still differed by up to 8,946 tokens. Tokens per turn match at a ratio
of 1.023, so the routing adds no measurable overhead, and the spread is intra-turn
variation shared by both arms. The gate passes on its registered statistic
(arm medians, 1.023 against a 1.25 margin); the earlier INCONCLUSIVE came from
me substituting an unregistered statistic after seeing the data — one that,
applied to run 1, would have concealed the ordering confound instead. Both
corrections are in `eval/registration-p2.md`.

What remains open is the *cause* of the intra-turn spread. Per-pair cost on this
suite is unpredictable within a factor of about eight, token figures fall in
shared bands near 1,100 / 2,200 / 3,200 / 4,300 / 6,200, and no mechanism is
established. It does not block anything — it bounds what a five-task single-turn
suite can measure. **A future cost gate needs more pairs or multi-turn tasks.**

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

## Watch this

**Criteria are only as good as the planner's wording, and the lever is a
prompt.** Two failures on 2026-10-01, in order: an over-strict criterion that
reported correct work as repairable, then — after asking for the weakest
criterion — a useless one ("TESTING.md contains 'test'", true of any file with
that name). The prompt now asks for both properties with both failures as worked
examples, and a third run produced criteria that discriminate without being
over-strict. Three runs is not a measurement. If it recurs, the next step is a
critique pass: one model call that checks a plan's criteria against its own task
text before any work runs, at the cost of one more slice of the budget.

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

**Fixed 2026-10-01: workers ran with whatever sandbox the developer's Codex
config said.** Single-worker routes passed no `-s`, so they inherited this host's
`sandbox_mode = "danger-full-access"`: the run that wrote a file had the whole
disk and stayed in its worktree only because it started there. The planner path
passed `workspace-write` for writing tasks but nothing for itself or for reading
tasks. And `[sandbox_workspace_write] network_access = true` in the same config
gave writing workers the network while the qualification record said
`reach_git_remote: blocked`. Every worker now states its sandbox and has network
pinned off, and a single worker writes only when the host is qualified, the rule
the planner path already followed.


**Fixed 2026-10-01: a cancel from another terminal now reaches the worker.**
`timon runs cancel` wrote `cancelling` to the record and revoked the grant, so
nothing new was authorised, but the turn already running carried on in a process
that never read its own row. Ctrl-C always worked, because that flag is set in
the same process. The run now polls its own record once a second and sets the
same flag, so killing the worker's process group and declining further tasks
needed no change. Verified end to end with a worker that sleeps instead of
calling a model: the process dies within a second of the cancel.

A read failure never cancels. `look` returns *unreadable* rather than *cancel*
when the record cannot be read, because a watcher that cancelled on a failed
read would turn brief lock contention into a killed run — a worse failure than
the one it fixes.

**Fixed with it: a cancelled run is no longer recorded as `finished`.**
`Status::Cancelled` existed and nothing set it. All four paths out of execution
settled `Finished`, so `timon runs list` printed `finished` beside work nobody
received — including after Ctrl-C, which had always been reported that way. Found
by running the cancellation for real, not by a test, which is why there is a test
for it now.


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

**Fixed 2026-10-01: per-user slot limits, Codex review point 11.**
`TasksMax`, `LimitNOFILE` and `MemoryMax` cap the process as a whole and say
nothing about *whose* work is on the host, so one developer's loop could hold
every slot and everyone else saw `Full` until it finished. `--slots-per-user`
caps how many any one uid holds at once, using the same file-lock primitive in a
directory of the caller's own: the locks are the accounting, so no process reads
another's state. The caller's own slot is taken first, because taking a host slot
and then refusing on the cap would occupy a slot for the length of the refusal.

`Full` and `YoursFull` are reported differently, because "the host is busy" and
"you are" call for different responses from whoever reads them. Verified with the
real binary: with a cap of 1 and one worker running, a second is refused with
exit 75 while three host slots sit free, and the identical call without the flag
succeeds — so the cap is what binds, not something else.

**What it does not do.** The per-user directory is created on demand even on a
provisioned host, because an operator cannot provision directories for uids they
have not met. So **a user who deletes their own cap files can exceed their cap.**
They still cannot exceed the host limit, which is provisioned and not theirs to
touch. This is fairness against runaway work, not a boundary against someone
determined to take more, and nothing here should be read as the latter.

**Withdrawn 2026-10-01: "both candidate models serve on `acct3`."** That claim
was wrong, and so was every statement that a `timon run` was paid by the pooled
accounts. `timon run` gave each worker its grant in `TIMON_GRANT` but never told
Codex where the broker was; the design assumed a provider in the developer's
Codex config, and none was ever installed. So workers called OpenAI directly on
the `workbench` user's own login: the broker forwarded nothing
(`requests_forwarded: 0` after a run) and the transcript said `provider: openai`.
The two model probes, gate stage one's 469,341 tokens, and every hand-off before
this fix were spent on that login, not on acct2 or acct3. What the probes do show
is that both models answer on *that* login. Whether `prolite` on acct3 serves
them is again **unverified**. Fixed by passing the broker as the provider on
every worker's command line; the first run after the fix moved the broker's
forwarded count from 0 to 6 and its transcript said `provider: timon`. The P2
gate is not affected: its script wrote its own provider config.

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
