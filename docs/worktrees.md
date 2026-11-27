# Worktrees

Use a worktree when two things need the repo at once: a review against a
pinned commit, a fix branch while `main` stays usable, or a long build beside
another task. One worktree per branch; `main` stays checked out in the main
checkout.

## Layout

| Tree | Job |
|---|---|
| `.worktrees/<branch>` | Feature/fix work. Slashes become dashes: `.worktrees/feat-crop` for `feat/crop`. Gitignored, own `target/`. |
| `.review/worktree` | Audit-owned, detached at the pinned commit. Read-only; never a feature branch. |
| `/tmp/opencode/…` | Disposable transcripts/artifacts only. Do not keep a full build there: `/tmp` cannot hold two targets. |

```sh
git worktree add .worktrees/feat-crop -b feat/crop main
git worktree add .worktrees/fix-x fix/x          # existing branch
git worktree add --detach .worktrees/review 48deaa6
git worktree list
git worktree remove .worktrees/feat-crop         # clean tree only
git worktree prune
```

A branch can only be checked out in one worktree; do not try to `switch` it in
`main` while the worktree holds it. Move the session there when it becomes the
working directory (`session_move`), or pass absolute paths.

## Slice flow

Plan one slice, branch it in a worktree, commit reviewable steps, run the
worktree gate, then a `/review` pass (`nile-review` subagent) before merging.
Fix findings on the same branch. Independent slices may run as parallel
subagents, one worktree per agent; merge only with explicit approval.

## Builds and gates

- Each worktree uses its own `target/` (Cargo's default). **Never share
  `CARGO_TARGET_DIR`** between concurrent worktrees: the shared lock serializes
  builds and a stale artifact has already produced a spurious test failure.
- Run `CHECK_SKIP_INSTALL=1 scripts/check.sh` in a worktree. It runs the same
  gate as CI, minus `scripts/install.sh` — that would `cargo install` the
  worktree binary into `~/.cargo/bin` and repoint the global `nile`/`nle`.
  `demo.sh` still uses the worktree's `target/release/nile`.
- The main checkout keeps running the full gate (with install) before a merge.
- One active worktree build at a time on a 65 GB disk: a full target is ~11 GB.
  Remove the tree after merge to reclaim it.

## Cleanup

After merge: `git worktree remove .worktrees/<branch>`, delete the branch
locally and remotely, then `git worktree prune`. A dirty worktree needs a
deliberate `--force`; never discard someone else's tree by accident.
