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
