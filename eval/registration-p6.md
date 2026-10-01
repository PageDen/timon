# Pipeline measurement registration — the gate after P6

**Registered 2026-10-01 at commit `fc7a29d`, before any pipeline run was
executed for it.** Written while the gates could still be argued with and before
a single model call was made on their behalf.

The plan's gating reads *"P3 → P4 (with the write-sandbox gate at P4.3) → P5 →
P6 → **measure**."* P0–P6 are built and merged. This is that measurement.

**It is not P8.** P8 compares Timon against single calls across a broad suite to
decide whether the project is worth it. This gate asks a narrower question that
has to be settled first: **does the planner path do what it claims, and does it
stay inside the bounds it promises?** A pipeline that cannot be trusted to report
its own results honestly cannot be evaluated against anything.

## Why this document is stricter than P2's

Five instruments in this project were wrong on first contact with real data. The
sixth fault was mine and was not an instrument at all: the P2 cost gate was
reported INCONCLUSIVE against the median of per-pair *ratios*, which was not the
statistic P2's registration named, and which I chose after seeing the numbers.
Applied to run 1 the same substitution would have concealed the ordering
confound — the largest real effect that gate found.

So every gate below names **the statistic, the estimator and the threshold**, not
just the threshold. A figure that looks more informative once the data arrives is
reported beside the registered one, never instead of it.

## Unit

**One run.** A run is one `timon run --execute` invocation: triage, and whatever
route triage chose, including the planner call, every worker session, integration
and verification.

This is deliberately not P2's unit. P2 measured one worker session because the
fast path *is* one session. The planner path's cost is the whole pipeline or it
is nothing, and a per-session figure would hide the planner and judge calls that
are exactly what the route adds. An arm reporting "one call" meaning anything
else is not comparable.

## The gates

Four, in the order I would stop the project over them. Each can fail alone.

| # | Gate | Statistic | Passes when |
|---|---|---|---|
| 1 | **Verdict honesty** | Count of runs reporting `done` whose claimed criteria do not all hold on the result branch, hand-checked | **Exactly zero** |
| 2 | **Deliverables** | Median **fraction** of criteria established per task (established ÷ total), pipeline against a single strong call | Pipeline **≥** strong |
| 3 | **Budget** | **Maximum** wall-clock overrun past `--budget-secs`, over every run | **Zero overruns** |
| 4 | **Cost** | Median tokens per **established criterion**, pipeline against a single strong call | Ratio **≤ 2.0** |

### Gate 1 is the one that matters, and why its threshold is zero

Every other number in this project is downstream of the pipeline reporting its
own results truthfully. A false `done` is not a quality problem, it is a
corrupted instrument wearing the project's name — and this has already happened
once: writing tasks ran read-only *and* were reported `done` with an empty
branch. That was found by hand, not by any check.

A rate threshold would be wrong here. One false `done` in twenty is not 95%
success, it is a verifier that cannot be believed, because nobody knows which
one. **Threshold zero, and a single occurrence stops the gate.**

### Gate 2: why "criteria established" and not "tasks accepted"

The planner path's whole claim is work a single call cannot do *well* — several
interdependent deliverables. Scoring that pass/fail per task throws away the
information: a strong call that writes two of three files correctly and a
pipeline that writes three both score zero on an all-or-nothing measure, and the
difference between them is the entire point of P3 and P4.

So the statistic is a **count per task**, and the comparison is of **medians of
each arm** — not of per-task ratios. That is the P2 lesson applied directly: a
ratio of two small noisy counts is dominated by its denominator, and a task where
the strong call establishes one criterion would swamp every other task in the
suite.

**The direction is registered as ≥, not >.** The pipeline costs more and takes
longer; merely matching a single strong call on deliverables would be a real
finding against the route, and I would rather record that honestly than discover
I had registered a threshold the route could only clear by being lucky.

**Amended 2026-10-01, before any run and with no data in existence**, from
"median criteria established per task" to the median *fraction*. Writing the
reporter exposed that the first version mixed incomparable units: the suite's
tasks have 6, 7, 7, 5 and 2 criteria, so a count of 4 means four sevenths on p2
and is impossible on p5, and a *perfect* score on p5 (2 of 2) would sit below a
*failing* score on p2 (4 of 7) in the same median.

The amendment is recorded rather than quietly applied because the distinction
that matters is *when*. Changing a statistic before the data exists is fixing a
mistake; changing it afterwards is what happened on the P2 cost gate and is the
reason this document names estimators at all. Nothing had been run, so nothing
about this choice could have been motivated by a result.

### Gate 3: why the maximum and not the median

`--budget-secs` is documented as *"the worst case **is** the budget rather than
something larger that nobody worked out."* That is a claim about a bound. A
median overrun of zero is consistent with a run taking four times its budget, so
a median would not test the claim that was made. **The statistic is the maximum
over all runs**, and the threshold is zero overruns.

The run that exposed this originally took 14 minutes against a stated 4. Fixed in
`src/budget.rs`; never measured end-to-end under load with concurrency.

### Gate 4: why 2.0 and not 25%

P2's margin was 25% because the fast path adds a broker hop and a grant round
trip, none of which is model tokens, so it should be close to free. **None of
that reasoning transfers.** The planner path spends a planner call, N worker
sessions and a judge call where a single strong call spends one session. It
*must* cost multiples. A 25%-style margin here would not be a strict gate, it
would be an incoherent one.

The question worth asking is therefore not cost per run but **cost per unit of
work delivered**, which is why the denominator is established criteria. At 2.0
the route may spend twice as much per delivered criterion as a single strong
call and still pass — paid for by gate 2, where it has to deliver more.

Fixed at 2.0 on 2026-10-01, before any run. It is a judgement and I will not
pretend it is derived. What makes it honest is that it is written down now, with
its reasoning, where a later argument can be had against it.

## The verifier cannot score its own gate

P5's verifier decides `done` / `repairable` / `blocked` using a model call
against criteria another model call wrote. Scoring this gate with it would be
circular, and gate 1 exists precisely to test it.

So ground truth is established two ways, neither of them the verifier:

1. **Mechanically.** `eval/score-criteria.py` checks each registered criterion
   against the result branch by string and path, the same four shapes
   `src/acceptance.rs` supports: a file exists, contains text, is absent, or no
   longer contains text. No model judges anything.
2. **By hand, for every `done`.** Gate 1's threshold is zero, so every `done`
   verdict is read against its branch by a person before the gate reports. A rate
   hides which case broke, and twice on the P2 gate the case that broke was the
   instrument.

Criteria for the suite are **written into the suite file in advance**, not taken
from whatever the planner produces at runtime. A planner that writes itself an
easy criterion would otherwise score full marks — and the planner has already
done this once, producing `TESTING.md contains "test"`, which is true of any file
with that name.

## Arms

Matched on goal text, repository state, tools and permissions.

1. **Pipeline** — `timon run --execute --allow-planner --route planner`.
2. **Single strong call** — one `codex exec` session, strong model, same goal.
3. **Single cheap call** — same, cheap model. Included to keep the comparison
   honest in the other direction: if a cheap call establishes as many criteria as
   the pipeline on these tasks, the suite is too easy and the gate says so.

**Arm order is randomised per task.** On the P2 gate, Timon always ran first and
the direct call second, and the result was a median ratio of 1.495 that fell to
1.129 once order was randomised. That artefact would have sent someone hunting a
50% overhead that did not exist. It is the largest false effect this project has
measured and it came from arm ordering alone.

**Repeats: 3.** P2 learned that 2 could not see past the noise and went to 5, but
a pipeline run costs roughly five worker sessions rather than one, so 5 repeats
across 3 arms is not affordable against two accounts. **3 is registered as
possibly too few**: if the interquartile ranges of the two arms overlap on gates
2 or 4, the gate reports **INCONCLUSIVE** rather than printing a number. It does
not get upgraded to a pass by the margin being technically cleared.

## Stopping rules, fixed now

- **Spend approved by Chris, per account, before any run.** Nothing below is
  executed without it.
- An arm that passes its per-account allowance **stops for a decision**. It does
  not continue and it does not quietly switch accounts.
- **One false `done` stops the gate** (gate 1), before the other three are
  reported. There is no point measuring deliverables against a verdict that is
  not trustworthy.
- An instrument that disagrees with a hand-checked sample on any task stops the
  run until the instrument is fixed. **Instruments are validated on saved output
  first, never on the run they are scoring.** Five have been wrong on first
  contact.
- Every transcript and every result branch is kept. Three faults on the P2 gate
  cost nothing to fix because the transcripts had survived; re-scoring is free
  and re-spending is not.

## What this will cost, as an estimate rather than a figure

A pipeline run on these tasks is one planner call, roughly three worker sessions
and one judge call. P2's median session was ~3,200 tokens, but those were
single-turn read-only questions and these are multi-file writing tasks, so that
number is a floor and not a prediction.

| | runs | estimate |
|---|---|---|
| Pipeline | 5 tasks × 3 repeats = 15 | ~50–75k tokens each → **0.75–1.1M** |
| Strong | 15 | ~15k each → **~225k** |
| Cheap | 15 | ~10k each → **~150k** |

**Order 1.0–1.5M tokens.** Stated as a range because the per-session figure it
rests on was measured on much smaller work. The P2 gate spent 113,355 tokens and
moved a 7-day quota window by nothing visible; this is roughly ten times that, so
it may be the first run where quota percentage is a usable unit at all.

**Account state at registration:** two active Pro accounts. `acct2` at 22% (resets
4 Oct), `acct3` on `prolite` at ~2% (resets 5 Oct). Whether `prolite` serves the
strong model is **unverified** and must be settled before the strong arm runs,
because the models endpoint is known to lie about exactly this.

## The attribution limit, stated rather than discovered

Quota moves whenever anyone uses an account. A window that drops during a run is
not that run's spend unless the account was otherwise idle. Either an account is
reserved and quiet for the duration, or the numbers are reported as an upper
bound with the contamination named.

## What this gate will not establish

- **Whether Timon is worth it.** That is P8, with a broader suite and human
  review effort included. This gate only asks whether the route does what it
  says inside the bounds it promises.
- **Anything about work whose requirements are not file-shaped.** Criteria are
  mechanically checkable by construction, which is `src/acceptance.rs`'s standing
  limit, not a property of this suite.
- **Anything about repair.** P6 is now one pass and a developer decision, so
  there is no repair loop left to measure. A `repairable` verdict is an output
  here, not a trigger.
- **Cost causes.** The P2 gate found per-pair token figures varying by a factor
  of eight at identical turn counts, in bands both arms occupied equally, with no
  mechanism established. That variance is still unexplained and will be present
  here.

## The instrument, validated before any spend

`eval/score-criteria.py` was written and validated with this document, on
synthetic trees, at zero spend. Five instruments in this project were wrong on
first contact with real data; none of them had been pointed at a case whose
answer was known in advance.

**15/15 self-tests pass** (`--self-test`), covering all four criterion shapes in
both directions, case folding, and `exact`.

One of them is the trap this checker exists to avoid: **`absent_text` on a
missing file fails rather than vacuously passing.** Read the other way,
"accounts-public.md does not contain acct2" is trivially true of a file nobody
wrote, and p4's two redaction criteria would have scored as passes for a worker
that skipped the deliverable entirely. That is the shape of every instrument
fault here so far — a check satisfied by the absence of the work.

Then the three cases whose scores were known before running them:

| Tree | Expected | Scored |
|---|---|---|
| This repository, no deliverable written | 0 | **0/27** |
| Hand-built correct answers | 27 | **27/27** |
| Hand-built plausible-but-flawed | partial | **20/27** |

The third is the one that shows the criteria discriminate, which is the P5 lesson
applied: a criterion that passes anything measures nothing. It was built as the
output a single call plausibly produces, and the checker isolated exactly the
four failures the suite was designed to separate — the index that never named
the file it was told to link, the summary that said "12 hours" without ever
computing 43200, one of three independent files dropped, and one of two account
names left unredacted.

**0, 20 and 27 on known inputs is the range this gate needs.** An instrument that
only ever reports full marks or zero cannot report a difference between arms.

## The runner and the reporter, also validated before any spend

Both exist. Neither has made a model call.

**`eval/run-gate-p6.py`** refuses to start unless `score-criteria.py
--self-test` passes, and refuses to start without an explicit
`--ceiling-tokens`: a default ceiling would be the script approving its own
spend. `--account` is required and pinned, because an unpinned run lets the
broker pick, and the broker picks the healthiest account — possibly the one a
developer is mid-session on.

**The single-call arms use the pipeline's own write-worker command**, `codex
exec -m MODEL -s workspace-write --skip-git-repo-check -` with the task on
stdin, started in a throwaway worktree off HEAD. Not an approximation of it. A
single call run read-only, or denied the write sandbox, would lose every writing
criterion before the comparison began, and the gate would be measuring its own
harness rather than the arms.

### Arm order is counterbalanced, not merely randomised

Independent shuffling per pair does not balance at this sample size. The first
implementation, on the registered seed, put the pipeline arm **last in 8 of 15
pairs and second in 2**. Whoever runs later inherits a warm prompt cache, so a
lopsided draw reintroduces a weaker form of the exact confound that made run 1
of the P2 gate report 1.495 against a true 1.129.

The design is therefore counterbalanced first and randomised second: every arm
occupies every position **exactly five times** across the fifteen pairs, and the
order of the blocks is shuffled so position is still not predictable from where a
task sits in the schedule. Verified at every suite size, with the imbalance never
exceeding one where the count is not a multiple of three.

### The reporter, on five fixtures whose answers were known first

| Fixture | Expected | Reported |
|---|---|---|
| Clean separation | all gates pass | **all pass** |
| A `done` verdict with unmet criteria | gate 1 fails and stops | **FAIL, exit 1, later gates not computed** |
| High variance, overlapping IQRs | gates 2 and 4 inconclusive | **INCONCLUSIVE on both** |
| Pipeline over its budget | gate 3 fails on the maximum | **FAIL, worst +130.0s** |
| p5 sent to the planner | control fails | **FAIL** |

It also **refuses malformed records** rather than computing over them — a count
outside 0..total cannot come from the checker, so it means rows were mis-paired
or a total was lost upstream. That guard exists because a deliberately broken
fixture printed `-1/2` and the reporter scored it without complaint. Five
instruments here have been wrong on first contact with real data and every one
of them reported a number rather than refusing.

## What is left before this gate runs

**Only your approval.** The suite, the checker, the runner and the reporter are
built and validated at zero spend. What is needed from Chris:

- Which account the gate may spend, and whether the strong arm pays from a
  different one.
- A ceiling per account. The stop rule is already enforced in the runner:
  reaching it stops for a decision, does not continue, and does not switch
  accounts.
- Whether the account is reserved and quiet for the duration. This is
  measurement rather than budget — concurrent use makes the quota figures an
  upper bound with the contamination named.

### Settled 2026-10-01: both candidate models serve on `acct3`

> **Withdrawn the same day.** These probes, and gate stage one below, never went
> through the broker. `timon run` gave workers a grant but no provider pointing
> at the broker, so every call went straight to OpenAI on the `workbench` user's
> own login. The probes show both models answer on *that* login; whether they
> serve on acct3 is unverified. The stage-one spend was on that login too, not
> on acct3, so the account ceiling and attribution sections of this document
> describe the wrong account. The gate's measurements of the pipeline itself are
> unaffected, since every arm used the same login. See `TODO.md`.

The models endpoint lies about capability, so this was answered by calling them.
One trivial request each — *"Reply with the single word: ok"* — pinned to
`acct3` through the running broker:

| Model | Answered | Tokens |
|---|---|---|
| `gpt-5.6-luna` | `ok` | 4,598 |
| `gpt-5.5` | `ok` | 8,020 |

Both serve. The capability worry that produced this item — the original defect on
the `go` plan — does not apply to either model on this account today. Cost: 12,618
tokens, one call per model.

The plan name itself could not be confirmed from this login, because the account
store is `0700` owned by `timon-broker` and no longer any developer's to read,
which is the service-account fix working as intended. What is confirmed is the
operationally useful fact: both models answer on `acct3` now, whatever the plan
is called.

**Which of the two is the strong model is not settled, and it is not mine to
assume.** `gpt-5.6-luna` carries the higher version number and is what the P2
gate used; `gpt-5.5` is the local Codex default. Getting the assignment backwards
would invert gate 2, since the pipeline's planner would run on the weaker model
while the "single strong call" arm ran on the better one. The runner therefore
requires both names explicitly and **refuses to start if they are equal** — see
below.

### A fault in the runner, found before it spent anything

Both model flags defaulted to `gpt-5.6-luna`, copied from the P2 runner. For P2
that was correct: it compared the fast path against a direct call on the *same*
model, so routing overhead was the only difference between the arms. Here it
would have **collapsed the strong and cheap arms into one**, making gate 2 a
comparison of the pipeline against itself and leaving the cheap arm unable to do
the one job it has — detecting a suite so easy that a cheap call matches the
pipeline. Both flags are now required and must differ.

### The spend estimate was wrong, and is revised upward

The registered estimate — 1.0–1.5M tokens, from "P2's 3,200-token median session
× five sessions per run" — rested on P2's tasks, which were one-turn read-only
questions against a small fixture repository. The probe above is the first
measurement of a session in *this* repository, and a **one-word answer cost 4,598
and 8,020 tokens**. That is the floor for any session here, before any work is
done, because the session loads the repository's own context first.

| | runs | revised |
|---|---|---|
| Pipeline | 15 | ~75–150k each → **1.1–2.3M** |
| Strong | 15 | ~15–25k each → **225–375k** |
| Cheap | 15 | ~10–20k each → **150–300k** |

**Order 1.5–3.0M tokens, roughly double what was registered.** Recorded as a
correction rather than quietly updated: the first figure was an estimate built on
the wrong reference work, and the measurement that fixed it cost 12,618 tokens.

**Which is why the recommendation is to run it in two stages.** One task, three
repeats, nine sessions — around 150k tokens — then read the actual per-run cost
and decide whether to commit to the remaining four tasks. An estimate that has
already proved wrong by a factor of two once should not be the basis for
authorising its own full value. `--task p1` runs the first stage.

**Staging does not cost the counterbalance**, which was the thing worth checking
before recommending it. Three pairs give each arm each position once and twelve
give four each, so the union is five each — identical to a single run of fifteen.
Verified rather than assumed, because a split that quietly unbalanced arm order
would reintroduce the confound that made run 1 of the P2 gate read 1.495 against
a true 1.129.

`--dry-run` prints the full schedule and arm order and spends nothing.

## Stage one — 2026-10-01 — the suite cannot answer the question

Ran `--task p1`, three repeats, nominally on `acct3` (in fact on the `workbench` login; see the withdrawal above), `gpt-5.6-luna` as strong and
`gpt-5.5` as cheap, binary `b820dc9` built `--release`. **It stopped itself at
the ceiling after 8 of 9 sessions: 469,341 tokens against a ceiling of 400,000.**

The stop rule worked exactly as registered — it halted for a decision and did
not switch accounts. But the estimate it was sized against was wrong again:
**~150k predicted, 469k spent, off by a factor of three.** That is the second
time this gate's spend estimate has been wrong in the same direction, and the
first correction was itself a 2× revision made the same day.

### An instrument fault, and the worst-shaped one yet

Every pipeline run was recorded as **0 of 6**. All three result branches on disk
in fact held **6 of 6**.

The two routes print different JSON. The single-worker route wraps its record as
`{"run": {… "branch": …}}`; the planner route emits a flat object with
`result_branch` at the top level. The runner read only the first, found no
branch, checked out nothing, and scored the default zero.

It cost nothing to correct, because the branches and transcripts were kept —
the same reason three faults on the P2 gate were free to fix. But this one is
worse in shape than those: it did not produce a *missing* number, it produced a
**confident wrong one that pointed at the conclusion I was most primed to
believe** — that the planner path does not work. A harness that cannot find the
work and a pipeline that produced none are opposite findings from an identical
`0/6`. The runner now refuses rather than scoring zero when no branch is in the
report, and reads both shapes.

A second fault alongside it: all three repeats shared one output directory, so
two of the three reports were overwritten and only the last could be re-read.

A third, in the reporter: `verdict.outcome` is a tagged object on the planner
route — `{"outcome": "repairable", "defect": "…"}` — not a string, and the
reporter raised `AttributeError` on the first real data it ever saw. Faults 7, 8
and 9 in this project. All three are fixed; none changes what was spent.

### Every session, so a rate never hides a case

| pair | arm | established | tokens | secs | over |
|---|---|---|---|---|---|
| p1-0 | pipeline | 6/6 | 45,985 | 74.5 | 0 |
| p1-0 | strong | 6/6 | 33,382 | 39.2 | 0 |
| p1-0 | cheap | 6/6 | 30,425 | 49.2 | 0 |
| p1-1 | pipeline | 6/6 | 109,076 | 109.5 | 0 |
| p1-1 | strong | 6/6 | 35,008 | 38.9 | 0 |
| p1-1 | cheap | 6/6 | 42,151 | 63.0 | 0 |
| p1-2 | pipeline | 6/6 | 149,904 | 87.6 | 0 |
| p1-2 | cheap | 6/6 | 23,410 | 45.4 | 0 |

`p1-2`'s strong arm is the session the ceiling refused.

### The pipeline under-claimed twice, which is the error to prefer

Gate 1 asks only about claiming *more* than was done, and nothing did. But two of
the three runs reported `repairable` while all six registered criteria held.

The reason is worth keeping: the planner wrote itself **seven** criteria where the
registration gives six, including the exact phrase *"Continuity, then Capability,
then Headroom"*, and then honestly reported a defect when its own stricter
criterion was unmet. The verifier was telling the truth about a harder test than
the one it was being scored against.

A verifier that cries defect on good work still costs a developer a review pass,
so it is recorded rather than waved through. But of the two directions an
inaccurate verdict can take, this is the one to want.

### The gates, on rescored data

| Gate | Result |
|---|---|
| 1 Verdict honesty | **PASS** — no run claimed `done` without its criteria holding |
| 3 Budget | **PASS** — 74.5s, 109.5s, 87.6s against a 300s budget |
| 2 Deliverables | **INCONCLUSIVE** — pipeline 1.000, strong 1.000, cheap 1.000 |
| 4 Cost | **FAIL** — 18,179 tokens per criterion against 5,699; ratio 3.19, margin 2.0 |

### What gate 2 actually revealed, which is about the suite

**The cheap arm scored 6 of 6.** That arm exists for exactly one purpose — to
detect a suite so easy that a single cheap call matches the pipeline — and it
fired on the first task. All three arms were perfect, so the comparison cannot
distinguish them and gate 4's failure means only that the pipeline charged three
times as much for work a cheap call already did.

The plan's own requirement for P8 reads: *"Suite must include work a single call
cannot do well, since that is the planner path's claimed territory."* **p1 is not
that, and the registration should have been refused on those grounds before
anything ran.** I wrote both documents and did not check one against the other.

### The dependencies were never dependencies

The planner emitted `depends_on: []` for both of p1's tasks, with a note that
they were independent — and it was **right**. p1's goal names `docs/selection.md`
in the text, so the index task never needed the first deliverable's output.

The same holds across the suite. p2's summary needs the three *values*, which
both tasks can read from source independently. p4's redaction can be written
without reading the unredacted file, because the goal states both account names.
**All five criteria marked `dependency: true` are satisfiable by two independent
workers reading the same source**, so the suite does not exercise P3's dependency
machinery at all, despite the suite file asserting that it does.

There is a structural reason, worth stating because it constrains any fix: a
criterion that is *fixed in advance* and *mechanically checkable* cannot depend
on a free choice the first worker makes at runtime. Those two requirements pull
against dependency testing. The way through is a **cross-file consistency**
criterion — every `##` heading in A also appears in B — which is checkable, fixed
as a rule rather than a string, and genuinely requires B to read A.

**That change is not applied to stage one.** The planner's behaviour has now been
observed, so altering the criteria would be fitting the test to the result, which
is the fault corrected in `registration-p2.md`. Stage one stands as scored.

### Stage one's primary evidence was lost, and that is a finding too

Written under `/tmp`, so the transcripts are gone, and all thirteen result
branches have been deleted as well. **The 6/6 result can no longer be re-derived
from anything primary.** What survives is
`eval/results/gate-p6-stage1-asrun-2026-10-01.json` and its rescored pair — both
derived artifacts, trustworthy only to the degree this document is.

This breaks a commitment made above: *"Every transcript and every result branch
is kept."* It is recorded rather than quietly repaired because of what it would
have cost. Three faults on the P2 gate were free to fix precisely because its
transcripts had survived, and the fault that mattered most here — `0/6` where the
branches held `6/6` — was diagnosed by checking out those branches. Had they
vanished an hour earlier, stage one would have been filed as "the planner path
establishes nothing", which is both false and the conclusion I was most primed
to believe.

The runner now **refuses an `--out` under `/tmp`**. Retention is not a habit to
rely on when the gate's own correctness has twice depended on it.

### The pipeline's cost rose across identical repeats

45,985 → 109,076 → 149,904 tokens for the same task, in run order. Over three
points this is not a trend, and no mechanism is established. Recorded because it
is the opposite of what a warming prompt cache would do and it is the largest
unexplained thing in the data.

### Recommendation: do not run stage two

Stage two would spend roughly 1.5M more tokens measuring the same
non-distinction across four more tasks built the same way. What is needed first:

1. **A suite of work a single call demonstrably cannot do well.** Until the cheap
   arm stops scoring full marks, gate 2 cannot report anything and gate 4's
   verdict is not about the pipeline.
2. **Real dependencies**, via the cross-file criterion above.
3. **Re-registration** of both, since the suite is the measuring instrument and
   it has been shown not to measure what it claimed.

What stage one did establish, and it is not nothing: the planner path runs end to
end, plans sensibly, writes to worktrees, integrates to a result branch, and
produces work that satisfies every registered criterion — three times out of
three. Gates 1 and 3 pass on real data. The budget bound held under concurrency
for the first time.

