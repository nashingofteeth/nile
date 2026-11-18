# The audit loop (`/audit`)

A long, resumable audit of the codebase, run through this agent rather than as a
shell script. One slice at a time, each in a fresh read-only subagent, with the
state on disk so a lost context or an overnight pause costs nothing.

## Run it

| Command | What it does |
|---|---|
| `/audit` | run every pending slice from the default queue |
| `/audit 3` | run at most three slices, then stop |
| `/audit all scripts/review/queue.md` | run all of an explicit queue |

A first run freezes the queue and the baseline commit:

```
~/.local/state/nile/review/
  queue.md      the queue as it was when the run started
  state.md      coverage table, findings ledger, verified-clean list
  log/          one report per slice (00-baseline.md, …)
  pin           the commit the review tree is detached at

.review/worktree/   a git worktree of the repo, detached at the pin
                    (gitignored; recreated from the pin if removed)
```

`state.md` starts from `scripts/review/state.template.md`. The state is written
through the shell, so it can sit outside the tree while the worktree stays
inside it — that keeps the review paths inside the project, where the read tools
can reach them without an approval prompt. Export `REVIEW_HOME` before starting
the session to put the state elsewhere.

`/audit` switches the session to `build`, because only a primary agent can
spawn the reviewers. Do not name a project command after a built-in (`init`,
`review`, `goal`): a project command silently replaces it.

Resume by running `/audit` again: `state.md` is the memory, not the model's
context. Inspect progress with `cat ~/.local/state/nile/review/state.md`; stop
the run by interrupting the session (state stays accurate for the next run).
Reports are append-only; the coverage table is the index.

## What it guards

- **Pinned baseline.** Subagents review a worktree detached at the commit the
  run started from, never your working tree. Every report cites that commit.
- **Read-only.** The reviewer agent cannot edit files or spawn agents; the
  parent only writes under `REVIEW_HOME`. A turn that leaves the worktree dirty
  stops the loop.
- **Sequential.** One slice at a time, so two children cannot race `state.md`.
- **Findings only.** The loop never fixes, commits, merges or cleans. Fixes are
  a separate, explicit pass on their own branch, as `AGENTS.md` requires.

## Models

The files name no model. The reviewer is a subagent with no configured model, so
it inherits the parent session's model; `/audit` runs in the current session so
"the parent" is whatever the session is already using. Pick the model in the
session, not in these files.

## The queue

`scripts/review/queue.md` is the default: sixteen audit areas from the gate and
model invariants through the boundary layers, TUI, coverage, docs and panics.
Each slice says which `docs/pitfalls/` note to read and when it is done.

Refresh the queue after a milestone — the areas move as the code moves — and
start a new `REVIEW_HOME` so the frozen copy is the new one. The report contract
lives in `.opencode/agents/nile-review.md`; the orchestration in
`.opencode/commands/audit.md`.
