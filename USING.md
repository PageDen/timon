# Using Timon

A hand-off takes a goal, does the work on a branch, and tells you what it could
and could not establish. Your working tree is never touched.

## Setup, once

```sh
timon config init          # writes ~/.config/timon/config.toml, all commented out
timon qualify status       # may writing workers run on this host?
timon status               # broker, accounts, recent runs — can it take work now?
```

Edit the config to set at least the two models and, if you want the planner
path, `allow_planner`. `timon config show` prints what is in effect and whether
each value came from your file or a built-in default.

Nothing in the config can authorise spending. `timon run` still needs
`--execute` before it will pay for anything.

## Handing off work

```sh
cd ~/my-project
timon run "Write docs/overview.md describing this project"
```

That records the run and routes it, and **spends nothing**. It prints the route
it chose and why. When you are happy with the route:

```sh
timon run "Write docs/overview.md describing this project" --execute
```

The result is a branch. `timon runs show <id>` repeats its name and the three
things you can do with it:

```sh
timon runs diff <id>            # what it changed, as a summary
timon runs diff <id> --full     # the whole diff
timon runs accept <id>          # merge it into the current branch
timon runs discard <id>         # delete its branches
```

`accept` refuses when your working tree has uncommitted changes, rather than
mixing them into the run's work, and it never pushes. `discard` removes the
run's own worktrees first and deletes only branches belonging to that run.

The diff is taken against the commit the run **started from**, not your current
HEAD — otherwise work you committed since the hand-off began would be
attributed to the run.

Plain git works too, if you prefer:

```sh
git branch --list 'timon/*'
git diff HEAD..timon/run-.../result
git merge timon/run-.../result
```

Either way, your own files are untouched until you accept.

## Reading the verdict

Every run reports five dimensions, and says *not established* rather than
guessing:

| | means |
|---|---|
| execution | did the tasks run to completion |
| mechanical validation | did the worker branches merge |
| automated checks | your project's own build and tests, if it declares any |
| task acceptance | criteria written *before* the work, checked against the files |
| evidence | whether a claim follows from what it cites — unimplemented, always *not established* |

**`passes the 3 checks that were run` is not `everything passed`.** If task
acceptance says *not established*, nobody checked that the work did what you
asked. The report says so on purpose.

A run gives you **one pass**. Whether to run again is yours to decide — Timon
will not repair work against its own criteria, because a host that does that can
make correct work worse.

## Declaring your project's checks

Automated checks say *not applicable* until the repository says what to run:

```sh
git config timon.checks "cargo test && cargo clippy -- -D warnings"
```

Guessing a build command from a filename would mean executing arbitrary commands
in your checkout on the strength of a convention, so an undeclared project gets
*not applicable* rather than a silent pass.

## The budget

`budget_secs` is the whole run, and it is divided between the planner and each
level of the task graph. A small total leaves each worker very little:

| total | per worker |
|---|---|
| 300 | 60s — not enough to write a document |
| 900 | 180s — the default |

If a task reports *the worker passed its deadline*, raise `--budget-secs`. The
worst case for a run genuinely is the number you asked for, rather than
something larger nobody worked out.

## When something is wrong

```sh
timon status                 # the first thing to check
timon runs list              # what has been handed off
timon runs show <id>         # one run in detail
timon runs cancel <id>       # stops the run and its worker, from any terminal
```

A run that says `recorded` was never executed — add `--execute`.

Worker transcripts are kept under `~/.local/share/timon/runs/<run-id>/`, which
is the only record of what a worker actually did. They are deliberately not in
`/tmp`: that directory was cleared under a measurement run on this project and
took every transcript with it.

## What it will not do

- **Untracked files are not given to a worker.** A run against a tree with new
  files starts from the last commit, and the report names what was left out.
- **Acceptance criteria only describe files** — exists, contains, absent, no
  longer contains. "The endpoint returns 400" needs the endpoint run, and
  running a model-chosen command is the model choosing its own permissions.
- **No automatic repair.** One pass, then your decision.
- **Model policy cannot refuse on capability.** A request for something the
  assigned model cannot do is substituted, not refused.
