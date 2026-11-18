# AGENTS.md

`nile`'s operating manual: the rules an agent needs before acting. Usage is
`README.md`, the roadmap is `PLAN.md`, and per-subsystem lore is in `docs/`.
The user is **learning Rust** through this project (from JS/PHP): teach as you
build — see "Learning Rust along the way".

## Layout and where to read first

| Crate | Job |
|---|---|
| `crates/nle-core` | Pure timeline logic — time math, media metadata, EDL model, edit ops + undo history. No processes. |
| `crates/nle-ffmpeg` | ffprobe/ffmpeg wrappers, filter-graph compiler. |
| `crates/nle-protocol` | JSON-RPC 2.0 wire types + blocking client/server. Envelopes and typed calls only; no project I/O, process, or socket. |
| `crates/nle-cli` | The `nile` binary (plus an `nle` symlink alias) + TUI + mpv IPC client. |

| Before touching | Read |
|---|---|
| `edl.rs`, `import-edl` | `docs/pitfalls/edl.md` |
| `otio.rs`, `import-otio` | `docs/pitfalls/otio.md` |
| `detect.rs`, `detect-edits` | `docs/pitfalls/detect-edits.md` |
| `graph.rs`, `preview.rs`, `proxy.rs`, mpv | `docs/pitfalls/mpv-ffmpeg.md` |
| actions, trims, playhead | `docs/pitfalls/playhead.md` |
| transitions, blends | `docs/pitfalls/transitions.md` |
| `nle-protocol`, `serve`, wire methods | `docs/pitfalls/protocol.md` |
| sequences, selection, `nile sequence` | `docs/pitfalls/sequences.md` |
| annotations, `nile annotation` | `docs/pitfalls/annotations.md` |
| render reports, `nile report`, export | `docs/pitfalls/render-report.md` |
| crop keyframes, keyed crop | `docs/pitfalls/crop-keyframes.md` |
| the borrow checker or clippy | `docs/rust-notes.md` |
| long audits, `/audit` | `docs/review-loop.md` |

Low-dependency stance: **the runtime set is frozen.** No new runtime
dependencies without explicit justification (a hand-rolled ~50 lines beats a
transitive tree); zero network code. **`tui` is a default Cargo feature** —
keep `cargo build/test --no-default-features` green. ffmpeg/ffprobe/mpv are
the attack surface: no shell, `--` before positional paths, isolated mpv
config and IPC socket. `Cargo.toml`, `Cargo.lock` and `.cargo/config.toml`
own the dependency list, release profile and lld flags.

## Keeping the docs current

`README.md` = how to **use** it; `PLAN.md` = where it is **going**;
`AGENTS.md` = the always-on rules; `docs/` = per-area notes read on demand.

**Propose exact wording for `PLAN.md`/`AGENTS.md` changes; apply only after
approval.** `README.md` and `docs/` may move with the code.

**Default is don't add.** A note earns a place only if it changes an agent's
default action and is not obvious from the code, a test, `README.md`, or a
commit body. Otherwise it stays a commit message.

Keep AGENTS rules ≤3 lines, imperative, with a file/symbol/test guard; fold
new findings into the existing rule and move subsystem detail to
`docs/pitfalls/<area>.md` (plus an index line), war stories to the commit
body. Prune at every milestone: delete what the code enforces and move detail
out before squeezing wording.

A milestone landing also means a `PLAN.md` pass: move its row and status
detail forward, note deferrals, keep the "next" pointer accurate — but don't
rewrite the roadmap unilaterally.

## Learning Rust along the way

**Every implementation step should teach, not just ship.** In plans,
explainers, and commit-adjacent notes:

- Name the Rust concept the change exercises and why (ownership/borrow,
  `Result`/`?`, enums + exhaustive `match`, `let-else`, modes-as-enums,
  closures/captures, lifetimes, iterator combinators, `Option`/`then`).
- Prefer the idiomatic construction and say what it replaces — illegal states
  unrepresentable vs. runtime checks; a pure `match` over stringly dispatch;
  splitting decide/apply with types.
- Call out the **gotcha** when the compiler or clippy pushes back, and what
  the error means (struct-literal parens, closure inference, `chars[cur..o]`
  panics). The running list is `docs/rust-notes.md`.
- Keep it short and tied to the code at hand — a concrete explainer beats a
  tutorial dump. Tie lessons to a real file/line.

## Working loop

One slice, one branch. Each step is the default; skipping one needs the user
to say so.

1. **Plan first.** Discuss scope, model/API, tests, docs and the genuinely
   open choices before editing; offer options instead of inventing a second
   roadmap. In Plan mode, do not modify files — the user switches agents when
   ready.
2. **Branch from `main`.** `feat/<slice>` or `fix/<slice>`; never work or
   commit on `main`.
3. **Commit in reviewable slices.** Core/model first, then front-ends and
   protocol, then tests/docs when they do not belong with the code. One
   logical change per commit (`feat:`/`fix:`/`refactor:`/`docs:`), subject then
   a short why; name the Rust concept the change teaches and the test that
   guards it.
4. **Verify before the final commit.** Code changes: focused tests while
   iterating; `scripts/check.sh` (fmt, shellcheck, clippy, docs, tests,
   release build, install, demo) before pushing. Markdown-only changes
   (`README.md`, `PLAN.md`, `AGENTS.md`, `docs/**`) skip that gate: run
   `git diff --check`, check touched paths/links, and smoke-run any command
   example you changed. Doc comments are code and still need
   `cargo doc -D warnings`. Push with a clean tree.
5. **Review fixes stay here.** A review is not merge permission. Commit fixes
   onto the feature branch and push; do not open a separate
   `fix/...-review` branch. The branch reviewed is the branch that merges.
6. **Merge only on an explicit request.** `git merge --no-ff`, rerun
   `scripts/check.sh` on `main`, push, then delete the merged branch locally
   and remotely. "review" or "proceed" is not "merge".
7. **Plan next after merge.** Propose the next slice before starting it; update
   `PLAN.md` status and the next pointer only with approval.

After pushing `main`, track CI with `gh run list` / `gh run watch`; report the
result and call queue/runner-capacity failures infrastructure, not code.

**Report each slice** with its branch and commit range, exact test counts,
what was not done, and the next decision. Report flaky tests or rerun gates
plainly; never claim a pass you did not observe.

## Model invariants

- **Compatibility: none, by design.** Single-user project; `nile.json` and
  `.nile/<stem>/` are local state, not a distributed format. They may change
  shape any time; old files are **rejected with a clear error** (never silently
  misread) and the fix is "rebuild it" (`./demo.sh`). Bump the version; no
  migration code. Times are exact rationals (`"30000/1001"`); floats only at
  the ffmpeg/IPC boundary.
- **Frames are 0-based everywhere** (`f0` = first frame). A timeline of `dur`
  spans `[0, dur)`; the last representable position is `dur - 1/fps`.
- **Every write path clamps clips to their media.** `Document::save` clamps
  before writing, `History::apply` after every batch; new saves go through
  `Document`, or the model reads past EOF and the export silently truncates.
- **Sequences.** Switch via `Document::select_sequence` (undo too);
  `Document::save` clamps every sequence. Details:
  `docs/pitfalls/sequences.md`.
- **Track-scoped edits.** Every track-resolving edit is
  `*_on(project, track, …)`; the plain name wraps the first video track. New
  ops take the index — don't add fresh `video_track()` calls. A stale index
  returns `TrackNotFound` (`track_at`), never panics.
- **A track is a sequence.** A trim that shortens a clip without a gap op
  ripples the followers; gap trims always ripple; an inward gap move ripples
  and outward growth honours the flag (`e` overwrite, `E` insert). An append
  at/past the end is one gap (`append_at_end`) — never two adjacent gaps.
  `e`/`E`/`[`/`]` must agree on a leading gap's `GapHead`; retire redundant
  variants so paths can't drift.
- **The base video track is the master sequence.** A length-changing edit
  there shifts/removes the same span on every other track (`apply_synced`);
  overlay/audio edits stay local. A transition boundary skips with a notice.
  Seed the sync's split ids from the base batch (`next_id_after`): a shared
  counter gives two tracks the same id and `--clip` edits the wrong clip.
- **Floors that shift a segment are slivers.** `insert_from_pool` floors an
  asset duration to whole frames (6.006 → 6.0) so later splits can't strand a
  sub-frame stump; the renderer skips zero-width segments.
- **Speed.** `Clip.speed` is a rational rate; timeline duration = source /
  speed. Cross the boundary only via `source_span` (edits) or `Clip::source_at`
  / `timeline_offset_of` (everything else). A speed change always ripples and
  the monitor proxies any non-1× clip (an EDL entry can't retime). The CLI/TUI
  take a **percentage** (100 = normal); the model keeps the rate (`@2×`).
- **Transitions.** Trimming beside or through a blend moves the blend, never a
  gap; a retime scales each side's span, not its rate. A blend is opaque,
  video-only and keeps its sides contiguous at the neighbour's rate. Full
  model and every edge case: `docs/pitfalls/transitions.md`.
- **An edge trim consumes the segment on its side.** `i`/`I` the one ending
  at the playhead, `o`/`O` the one starting there; a gap is extended into,
  not trimmed. Details and the guarded branch:
  `docs/pitfalls/playhead.md`.
- **Undo/history.** Writes through `History` (edits, `add_asset` /
  `add_assets` / `remove_asset`, `edit_tracks`) record; only whole-project
  rebuilds (`init`, `import-*`, `detect-edits`) re-baseline it via
  `save_and_reset_history`, or the next undo refuses ("history entry does not
  apply"). Both stacks persist. Track add/mute/rm and multi-track sync ripples
  are whole-list changes (`Change::Tracks`), verified by exact equality.
- **State lives with the project.** `nile.json` plus `.nile/<stem>/`
  (`history.json`, `clipboard.json`, `cache/`); `nile default FILE` sets a
  directory default (`.nile/config.json`), `--project` overrides. Legacy
  adjacent sidecars are copied in once — don't add scattered state files. The
  playhead stays in `Project.playhead`. The TUI adopts foreign changes and
  refuses to clobber (`persist_playhead` → `Ok(false)`).
- **Gaps and the pool.** `Mode::Pool(PoolState)` keeps "pool open" and its
  selection together; tracks are 1-based (0 refused); a shorter track pads
  with a trailing gap. Source marks are keyed by asset id (`view.json`);
  `remember_pool` restores the selected asset, preview clock and timeline.
- **One quantizer per axis.** Draw from the same function the playhead/cut
  indicator uses (`marker_col`), or drawn edges drift from real cuts.

## Monitor and export

- The monitor plays the **composite**, not the active track:
  `composite_segments` walks every unmuted track's cuts and picks the topmost
  video with material; black only where all are gaps. A span where another
  track is audible is rendered as the export's mix through the proxy cache.
  Switching the active track does not reload it; `nile edl` prints what it
  plays.
- **The active track drives navigation and edits, not the monitor.** Digits
  are vim counts, so track selection is `t`/`T`/`<N>t` — bare `1..9` would
  break `3l`.
- **The monitor proxies whatever `edl://` can't express** (effects, non-1×,
  transitions, foreign sizes, audible mixes) through the export graph, cached
  under `.nile/<stem>/cache/proxy`; see `docs/pitfalls/mpv-ffmpeg.md`.
- **Export presets and the chunk scheduler are local design choices.** Auto
  stays single-worker; details and benchmarks: `PLAN.md` (M6).
- **Export stays byte-identical for the simple case:** one video track + no
  audio track keeps the classic concat path; otherwise every track is padded
  to the timeline length, video layers `overlay` bottom→top (upper gaps
  `black@0.0`/`yuva420p`), audio `amix`es, and muted tracks drop out. A video
  track plus an audio one takes the multi-track path (fast = no audio).

## Editing and TUI conventions

- **One action catalog, four front-ends.** Every timeline edit is an `Action`
  (`nle-core/src/action/mod.rs`) run by `run_action`; keys, the `:cmd` line,
  the CLI and the protocol server all use it, and the `:cmd` grammar *is* the
  CLI grammar (same flags; `--at`/`--track`/`--clip` default to the
  playhead/active track/clip-under in the TUI). A new action = an `Action`
  variant + a slot in `Action::slot` (exhaustive) + both parity tests + a wire
  sample in `action/tests/wire.rs` (the `SLOTS` guard). Session state
  (playhead, register contents) is not an action. The catalog is also the
  wire DTO (`Action` serde): `nle-protocol` sends it, so don't add a parallel
  action request type.
- **CLI, TUI, and serve share one session.** Load/edit/save go through
  `nile-cli/src/session.rs`; media anchoring through `session/playhead.rs`.
  Don't duplicate either in a front-end.
- **Visual range ops are catalog actions.** `DeleteRange`, `YankRange`, and
  `PasteRange` back visual `d`/`y`/`p`; front-ends call `run_action`, never
  `edit::delete_range_on`/`insert_segments_on` directly. `DeleteRange`
  restores a boundary transition's surviving handle and drops interior blends
  with the range. The shared register may hold a range; normal `p` refuses it
  (`RangeRegister`) and `:paste --range`/visual `p` consume it.
- **Deleting a transition side removes the blend.** `dd` leaves the target's
  visible span as a gap, `D` ripples it away, and both restore the surviving
  side's borrowed handle; a gap beside a blend follows the same ripple rule.
  Range deletes apply this at the range boundary.
- **`--clip N` finds the clip's own track** (`resolve_track`); `--track`
  addresses position-based targets only. Tracks are 1-based — 0 is refused
  (`cli_index`), never silently "track 1".
- **The message line has one writer.** `run_action` sets the notice, then
  `persist_and_refresh` runs; a failure there **appends**
  (`{notice} — preview rebuild failed: …`) instead of replacing it.
- **Case encodes ripple** (`e`/`E`, `i`/`I`, `o`/`O`) everywhere.
  **`hints_for` is the single source of mode hints**; the message panel shows
  the latest notice only — don't put hint text in the message.
- A key-capturing panel forwards valid actions; `m` toggles the pool. Opening
  it notices "media", closing "timeline". Track changes work while it is open:
  they retarget `enter` but keep the asset preview (`select_active_track`).
- **Import is yazi-first but optional.** `a` in the pool runs
  `yazi --chooser-file`; without yazi it opens `:import`. Never shell out.
  `:import` quotes paths, expands `~`/directories, and probes on a worker.
- **The timeline panel sizes to show every track** (`panel_heights`): one row
  per track, two filmstrip rows per video track when enabled, waveform rows,
  marker row and borders; keys/message drop first on a short terminal. Keep
  hint lines ≲76 columns.

## Playhead

- Normal edits follow the **media frame** (clip id + source); extend lands on
  the edge; undo/redo restore the pre-edit position; deletes keep the offset
  (or the seam for ripple/gap).
- **A no-op edit must not move it**, an anchor survives while the segment
  does, and every write clamps to `last_frame` (`dur - 1/fps`), never `end`;
  a track or pool change leaves it put.
- Details, the float-vs-frames rule, and the audit tests:
  `docs/pitfalls/playhead.md`.

## Testing and debugging

- **Three integration batteries back the unit tests.**
  `crates/nle-cli/tests/behaviour.rs` asserts *results* (pixels, durations,
  frame counts, structure); `hostile.rs` holds garbage input to *invariants*
  (never panic, exit 0/1/2 only, leave a project that loads with a finite
  non-negative length); `fixtures.rs` pins the 2-cam golden show (normalized
  project/EDL, frame count, sampled PSNR). New end-to-end behaviour →
  `behaviour.rs`; new "garbage in" → `hostile.rs` (a case is a list of argv
  sequences); a fixture change → `fixtures.rs`. All run under `cargo test`
  against the debug binary; `cargo test -p nle-core` is the sub-second loop.
  `tests/common` rebuilds templates every run — a stale template poisons every
  case.
- **Split decide from apply** so logic is testable without mpv/ffmpeg:
  `plan_for_armed`/`apply_extend_plan`, `commit_batch` vs
  `run_commit_action`, `pool_step_target` vs `pool_step`,
  `edit_anchor`/`reanchor_playhead`.
- **TUI render tests** use `ratatui::backend::TestBackend` and read the
  `Buffer` cell by cell. Count indices including the border, and when an
  assertion fails, print the grid first and write assertions from it.
- **Beware tests that pass by coincidence.** A one-item fixture where
  id == index hides an id-vs-index bug; build the distinguishing case (e.g.
  two assets so index 1 ≠ id 2).
- **The monitor is behind the `Mpv` trait**; TUI routing is tested with
  `FakeMpv` (add a `Call` variant, assert on it).
