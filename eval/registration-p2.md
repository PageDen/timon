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
