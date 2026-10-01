# P2 measurement registration

**Registered 2026-09-28, before any route was executed.** Written while triage
could still be changed and before a single model call was made on its behalf.

Four instruments in this project were wrong on first contact with real data, and
a fifth claim — that the provider holds conversation state — was wrong on first
contact with a real test. Defining success after seeing results is how that
happens. So this is what the gates are, fixed in advance.

## Unit

**One worker session**, which may take several model and tool turns inside it.
Not one provider request. Every figure below is in this unit, and an arm that
reports "one call" meaning something else is not comparable.

## The two gates

They are separate and either can fail on its own.

| Gate | Passes when | Does not establish |
|---|---|---|
| **Cost** | Median fast-path tokens are **no more than 25% above** a direct cheap call on the same task, with triage overhead reported rather than assumed zero | Anything about whether the answers are good |
| **Quality** | Task acceptance rate on the fast path is not worse than the direct cheap call, on the same fixtures | Anything about cost |

Low overhead on a bad answer is not success. That is why quality is a gate and
not a footnote.

**Why 25%.** The fast path adds a broker hop and a grant round trip to a call
that would otherwise go straight to the provider; none of that is model tokens,
so on token count it should be close to free. The margin is there for the
difference in how the two arms are prompted, not as room for orchestration
overhead — the pilot's 1.365x is what this route exists to avoid, so a margin
anywhere near that would make the gate meaningless. Fixed at 25% on 2026-09-28,
before the gate was run.

**Ground truth for the gate suite** was read from the repository at commit
`c6f182e` and recorded in `eval/gate-p2-suite.json`, so a later disagreement is
settled against the files rather than against anyone's memory.

**The checker is self-tested before each run.** `eval/score-answer.py
--self-test` covers thirteen cases, and the runner refuses to proceed if any
fail. It caught its own first version treating "the value is 300." as wrong,
because a sentence-ending full stop looked like part of a number.

## Measured

- Task acceptance rate, against criteria written before execution.
- Wall-clock latency.
- Tokens by model, **including failed attempts and repairs**. Excluding them
  would flatter every arm that retries.
- Account quota observations, with the attribution limit stated (below).
- Routing decisions and their reasons, so misrouting is measurable at all.
- Human review and correction effort. The cost this project exists to reduce,
  and the one most easily left out of a comparison.

## Arms

Matched on task input, repository state, tools and permissions.

1. Timon, routed by triage.
2. A single strong call.
3. A single cheap call.

Enough repeats to show variance rather than one number.

## Stopping rules, fixed now

- Spend approved per account before any run.
- An arm that passes its per-account allowance **stops for a decision**. It does
  not continue and it does not quietly switch accounts.
- An instrument that disagrees with a hand-checked sample on any task stops the
  run until the instrument is fixed. The instrument is validated on saved output
  first, never on the run it is scoring.

## The attribution limit, stated rather than discovered

Quota moves whenever anyone uses an account. A window that drops during a run is
not that run's spend unless the account was otherwise idle. Either an account is
reserved and quiet for the duration, or the numbers are reported as an upper
bound with the contamination named.

## Routing, measured already

Routing is deterministic and costs nothing, so it is measured now rather than at
the gate: `eval/score-routing.py` against `eval/triage-suite.json`.

The suite deliberately contains cases the rules were expected to get wrong. A
perfect score would mean the suite had been written to flatter them.

**As registered: 9 of 10 agree, 0 over-routed to the planner.**

One standing disagreement, recorded rather than reconciled:

- **t9, long but mechanical.** A 380-character extraction task that a cheap call
  would handle perfectly is sent to a strong call, because length disqualifies it
  from the fast path. Length is weak evidence in both directions and the rules
  only use it in the safe one. The cost is a strong call where a cheap one would
  do; the alternative is guessing at difficulty, which is what sends the wrong
  work to the wrong place. Left as a known limit.

One rule changed in response to this measurement, which is what the suite is for:

- **t7, coupled code change.** "Rename the type; then update every call site"
  was routed to the planner. Each writing worker gets its own worktree, so two
  workers editing the same symbol produce a guaranteed conflict — a lead call and
  two workers to arrive back where one worker started. Multiple deliverables that
  are one code change now go to a single strong worker.

The expectations were not moved to match the rules. Only the rules moved.


## Run 1 — 2026-09-28 — inconclusive, and why

20 paired calls, 113,355 tokens, on `acct3`. **The cost gate did not decide**,
and reporting the number it produced would have been a claim the data does not
support.

| | |
|---|---|
| Median ratio | 1.495 (+49.5%) |
| Paired delta | median +1,022 tokens, **range −3,862 to +6,826** |
| Won by | timon 4 pairs, direct 6 |
| Quality | 9/10 against 8/10 — **PASS** |

The spread is ten times the effect and the arms traded wins. One task cost the
*same arm* 4,267 tokens on one repeat and 16,343 on another. A median ratio over
that is arithmetic, not a measurement.

**Two faults in the harness, both fixed before rerunning.**

*Arm order was confounded with arm.* Timon always ran first and the direct call
second. Both arms showed the first repeat costing more than the second, which is
what a warming prompt cache looks like. Order is randomised per pair now.

*Two repeats could not see past the noise.* Raised to five, and the report now
shows the interquartile range, the paired delta with its full range, and how many
pairs each arm won. A result that cannot be distinguished from noise now says
INCONCLUSIVE rather than printing a verdict.

A third fault cost nothing to fix because the transcripts were kept: tokens were
read only from stdout, and `codex exec` writes its token report to stderr when
the streams are captured separately. The first report said "tokens median nan"
and was re-scored from the saved logs rather than re-run. **This is the fifth
instrument in this project to be wrong on first contact with real data, and the
first where the fault was in the experimental design rather than the scoring.**

## What 113,355 tokens did to the quota window

Nothing visible. `acct3` read 0% before and 0% after. At this scale the provider's
percentage is too coarse to attribute anything, so **token counts are the only
usable cost measure for P2-sized work**, and the quota figure is for capacity
planning rather than comparison. Recorded here because the plan's budget language
is written in windows, and windows cannot see work this small.


## Run 2 — 2026-09-28 — quality passes, cost is not settled

25 paired calls with randomised arm order and five repeats per task.

| | timon | direct |
|---|---|---|
| Tokens, median | 3,218 | 3,145 |
| Tokens, IQR | 1,312 – 4,802 | 1,744 – 4,844 |
| Accepted | **25/25** | **25/25** |
| Latency, median | 8.5s | 10.0s |

**Quality: PASS.** Both arms answered every task correctly.

**Cost: PASS on the registered statistic — 1.023 against a margin of 1.25.**
Arm medians are 3,218 against 3,145.

*This paragraph originally read INCONCLUSIVE, on the ground that the median
per-pair ratio was 1.129 and 10 of 25 pairs sat outside the margin. That was not
the registered statistic and it was chosen after the data arrived. Corrected in
"Run 2, re-analysed" below, which also withdraws the tool-call hypothesis for the
tail. The per-pair spread is real and is recorded there; it is a property of the
tasks, not of the fast path.*

There is no evidence the fast path is systematically expensive: it was cheaper in
9 pairs and dearer in 16, and the spread swamps the lean. **No cause for the lean
is offered, because none was established.**

### What run 1's number was worth

Run 1 reported a median ratio of **1.495**. Run 2, with the ordering confound
removed, reports **1.129** on the same tasks and the same model. The first number
was mostly an artefact of always running Timon first into a cold prompt cache.
Had it been reported as a finding, it would have sent someone looking for a 50%
overhead that does not exist.

### A third instrument fault, and what it cost

The quality gate first reported **FAIL, 21/25 against 22/25**. Every failure was
the same task, and the recorded answer was `'Planner'`. The worker had answered
correctly — *CheapWorker / StrongWorker / Planner*, on three lines — and the
instrument kept only the last one. An answer is a block, not a line.

Re-scored from saved transcripts: **25/25 on both arms.** No calls were repeated,
because the transcripts were kept. That is now three instrument faults on this
gate, two of them found only by looking at what the numbers were made of.

**Standing rule from this:** a gate result is not reported until the failing
cases have been read individually. A rate hides which case broke, and twice here
the case that broke was the instrument.

## Run 2, re-analysed — 2026-10-01 — cost: PASS, and a fourth fault that was mine

The cost tail was left unexplained above, with a recorded hypothesis: that the
spread was the model varying how many tool calls it made. `eval/explain-tail.py`
counts turns per transcript from the saved `/tmp/gate2` logs — a re-read, not a
re-spend — and **refutes it.**

**24 of the 25 pairs took exactly one turn on both arms.** Only `g5-4` differed
(2 against 1). Identical tool use, and the absolute token difference at identical
turn counts still has a median of 2,012 and a maximum of 8,946. Whatever the
spread is, it happens *inside* a single turn, so tool-call variance cannot be the
cause. The hypothesis is withdrawn.

What the same analysis shows positively:

| | timon | direct |
|---|---|---|
| Tokens per turn, median | 3,218 | 3,145 |
| | **ratio 1.023** | |

If the routing added overhead, cost per turn would differ. It does not — 2.3%,
which is within a single pair's noise.

**The spread belongs to the task, not to the arm.** Pooling both arms' 50
figures and grouping values that fall within 400 tokens of each other gives
bands that *both arms occupy about equally*:

| band | n | timon | direct |
|---|---|---|---|
| 1,095–1,324 | 13 | 7 | 6 |
| 2,219–2,221 | 3 | 0 | 3 |
| 2,769–3,350 | 17 | 9 | 8 |
| 4,055–5,310 | 8 | 4 | 4 |
| 5,915–6,302 | 5 | 2 | 3 |
| 7,443 … 12,186 | 4 | 3 | 1 |

Five pairs agree to within 100 tokens (−40, −18, +11, +29, +32); the rest differ
by whole bands in either direction. The thin top tail is 3 timon against 1
direct, which at n=4 is not a finding either way.

A mechanism for the banding is **not** established. The best single-quantum fit
is 531 tokens with a ±100 residual, which is a harmonic of the band spacing
rather than evidence of a quantum, so the banding is recorded as an observation
with no cause claimed.

The ordering confound is genuinely gone: the arm that ran first was the dearer
one in 11 of 25 pairs, which is chance.

### The fourth fault: I changed the statistic after seeing the data

The registered criterion, written before the run, is at **Measured** above:
*median fast-path tokens no more than 25% above a direct cheap call on the same
task.* That is the median of each arm. It reads **3,218 against 3,145 — a ratio
of 1.023.**

What I reported instead was the median of the per-pair *ratios*, 1.129, and then
called the gate inconclusive because 10 of 25 pairs sat outside the margin. That
is a stricter test than the registered one, and I chose it after the numbers came
back. It is also the wrong estimator for this data: a per-pair ratio divides two
noisy quantities, so with both arms drawing from the same wide banded
distribution it reports the spread as much as the lean.

**It is not reliably stricter, either — it is just unmoored.** Running the same
script against run 1's transcripts:

| | registered (arm medians) | substituted (per-pair) |
|---|---|---|
| Run 1 | **1.495 — FAIL** | 1.108 |
| Run 2 | **1.023 — PASS** | 1.129 |

On run 2 the substitute withheld a pass that the registered statistic grants. On
run 1 it would have *concealed* a genuine 1.495 failure — the ordering confound,
the largest real effect this gate has found — and reported a comfortable 1.108.
Had I used it consistently from the start, run 1 would have passed and the
confound would have shipped as a finding. It diverges in both directions, and the
direction is not predictable from the data.

**Cost: PASS.** 1.023 against a registered margin of 1.25, on the registered
statistic.

This is the fourth instrument fault on this gate and the first that was a fault
of analysis rather than of code. The three before it made a number wrong. This
one made the *question* wrong, after the fact, in the stricter direction — which
is why registration specifies the statistic and not just the threshold, and why
the earlier faults were caught while this one survived two reports.

**Standing rule from this:** the verdict is computed from the statistic named in
the registration. If a different statistic looks more informative after the data
arrives, it is reported *beside* the registered one as a finding, never
substituted for it.

### What the tail still does not tell us

Five tasks at one turn each cannot distinguish a fast path that is 2% dearer from
one that is free. The claim supported is the registered one — no 25% penalty —
and nothing finer. Per-pair cost on this suite is unpredictable within a factor
of about eight for reasons that are not the routing, so **a future cost gate
needs either many more pairs or multi-turn tasks**, where the per-turn figure
that did hold steady here is the thing to measure.
