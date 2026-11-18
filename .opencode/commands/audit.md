---
description: Run or resume the nile audit loop (one read-only subagent per slice)
agent: build
subagent: false
---

Run the nile audit loop: one read-only review slice per subagent, resumable
across sessions and context loss.

Arguments: `$ARGUMENTS`

Parse them as: an optional slice count (`all` when empty), then an optional
queue path (default `scripts/review/queue.md`). Anything else is a usage error —
say so and stop without touching state.

Paths:

- repo: the current project root — **never edit it**
- `REVIEW_HOME="${REVIEW_HOME:-$HOME/.local/state/nile/review}"`
- review tree: `<repo>/.review/worktree`, a git worktree detached at the pin
- pin: the commit saved in `$REVIEW_HOME/pin`

Reach `$REVIEW_HOME` and the review tree's own state with **shell only**
(`cat`, `mkdir -p`, `cat > file`). Do not use the read/write/edit tools outside
the repo: those paths are outside the project and will prompt or be refused.

## 1. Preflight

- Resolve repo, `REVIEW_HOME`, the queue argument, and the pin.
- First run (no `$REVIEW_HOME/state.md`): `mkdir -p "$REVIEW_HOME/log"`, copy
  the queue to `$REVIEW_HOME/queue.md` (frozen for this run — later edits to
  the repo's queue do not affect a run in progress), save
  `git -C <repo> rev-parse HEAD` into `$REVIEW_HOME/pin`, copy
  `scripts/review/state.template.md` to `$REVIEW_HOME/state.md`, fill in the pin
  and the date, and replace the sample coverage row with one row per `### S…`
  heading in the frozen queue, all `pending`.
- Worktree: if `<repo>/.review/worktree` exists, verify its
  `git rev-parse HEAD` equals the pin; otherwise create it with
  `git -C <repo> worktree add --detach .review/worktree "$(cat "$REVIEW_HOME/pin")"`.
- Stop immediately if `git -C <repo>/.review/worktree status --porcelain` is
  non-empty: the review tree must be pristine.

## 2. Loop

Repeat up to the slice limit (`all` means until the queue is done):

1. Pick the lowest-numbered slice marked `pending` or `partial` in `state.md`.
   If none is left, go to step 3.
2. Spawn exactly one subagent with `agent: nile-review` and **no model** — it
   inherits this session's model. Its prompt carries the slice text verbatim,
   the review tree path, the pin, and "return the report as text".
   When it returns: write `$REVIEW_HOME/log/<NN>-<slug>.md` with its report
   verbatim, append its findings to the ledger in `state.md`, append its
   `Checked clean` lines to the clean list, and flip the slice to `done` (or
   `partial`, keeping the blocker).
   Then verify the review tree is still clean and still at the pin. If either
   changed, stop and say which.
   If the subagent failed, record `partial` and stop.
3. Finish with a table of slice, status and finding count, then the next
   decision. Mention anything left `partial`.

## Rules

- This command produces **findings only**: never fix, edit, commit, merge,
  reset or clean the repo or the review tree.
- Keep your own context small: read `state.md` and the queue, not the code the
  subagents review. Do not re-derive their findings.
- One subagent per slice, one slice at a time — no parallel slices, or two
  children will race the same state file.
- If the user asks to stop, stop after the current subagent returns and leave
  `state.md` accurate.
