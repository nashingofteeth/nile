# nile audit queue

The slices an audit run works through, one per subagent. `/review` freezes this
file into the run's state directory on the first turn, so editing it never
changes a run already in progress — start a fresh `REVIEW_HOME` after a
milestone to pick up the new wording.

Each slice is an **area to check**, not a list of known bugs. Keep the "Done
when" line: that is how a subagent knows it has finished. The report contract
lives in `.opencode/agents/nile-review.md`.

`Before touching` names the `docs/pitfalls/` note the slice must read first;
`AGENTS.md` holds the invariants every slice checks against.

### S00 baseline

Record `git rev-parse --short HEAD`, run the fast loop (`cargo test -p
nle-core`) and the full gate (`scripts/check.sh`) once, and note the wall time,
any failure, and the per-battery test counts. A red baseline means later
findings are measured against a known-bad tree.
Before touching: —
Done when: `state.md` has the hash, the gate verdict, and the counts.

### S01 invariant sweep

For each bullet in `AGENTS.md` ("Model invariants", "Editing and TUI
conventions"), grep for the **bypass** rather than the implementation:
`video_track()` outside the action wrappers, `edit::` calls from `nle-cli`, raw
`serde_json` project writes, `speed` arithmetic outside `source_span`/
`source_at`/`timeline_offset_of`, `Change::Tracks` outside `History`,
`hints_for` strings elsewhere, `.clamp(` on frames instead of `last_frame`.
Done when: every bullet has a clean line or a finding.

### S02 project/document persistence

`project.rs`, `document.rs`, `state.rs`: version bump and refusal path,
save-time clamp over every sequence, playhead clamp to `last_frame`,
`.nile/<stem>/` layout, legacy sidecar copy-in, TUI foreign-change refusal.
Look for a rejected file that is still half-read (mutation before the error).
Done when: each write path is traced caller → bytes.

### S03 trim, gaps, GapHead

`edit/trim.rs`, `edit/clip/`, `edit/tests/trim.rs`: `e`/`E`/`i`/`I`/`o`/`O`/
`[`/`]` agreement on a leading gap, `trim_consume_target_on` edge semantics
(delete the segment the edge ends/starts at a shared cut), extend-mode
neighbour growth, append-at-end as one gap, ripple vs overwrite direction.
Build the adversarial timeline (leading gap + adjacent gap + shared cut)
instead of only reading.
Before touching: `docs/pitfalls/playhead.md`
Done when: the two-neighbour shared-cut case is exercised and matches the doc.

### S04 history and undo

`edit/history.rs`, `Document`'s history pairing: re-baselining only in
`init`/`import-*`/`detect-edits`, both stacks persisted, `Change::Tracks`
exact-equality verify, clamp after every batch, undo of a whole-track change,
undo across a sequence switch. Hunt for an action that mutates without
recording (apply vs run split) and a no-op that records anyway.
Done when: every `Change` variant has an inverse test or a finding.

### S05 transitions and blends

`edit/transitions.rs`, blend tests: trimming beside or through a blend moves the
blend, never a gap; a retime scales each side's span, not its rate; blend sides
stay contiguous at the neighbour's rate; deleting a side restores the
survivor's borrowed handle; a length-changing master edit skips a boundary with
a notice.
Before touching: `docs/pitfalls/transitions.md`
Done when: the delete-a-side matrix (left/right/both, with and without ripple)
is enumerated.

### S06 speed and the source boundary

`edit/speed.rs`, `time.rs`, and every `/ speed` or `* speed` outside
`source_time`/`source_span`/`source_at`/`timeline_offset_of`. Check percentage
parsing (100 = normal), always-ripple, proxy for non-1×, EDL export refusal.
Done when: every speed read/write crosses a named helper.

### S07 action catalog integrity

`action/mod.rs`, `action/parse.rs`, the action tests, protocol wire tests: the
new-variant checklist (`slot`, both parity tests, wire sample, `SLOTS` guard),
the `:cmd` grammar identical to CLI flags, `--at`/`--track`/`--clip` defaults,
range-register refusal for a plain `p`.
Done when: a hypothetical new action is walked through every gate that would
fail.

### S08 ffmpeg graph and process safety

`nle-ffmpeg/src/*`: no shell, `--` before positional paths, escaping for odd
paths (`:`, `'`, `[`, `,`, `\`), padding of every track to the timeline length,
overlay order bottom → top with `black@0.0`/`yuva420p` upper gaps, `amix` and
muted drop-out, and the byte-identical single-track concat path.
Before touching: `docs/pitfalls/mpv-ffmpeg.md`
Done when: the escaping audit has a concrete adversarial filename and a real
render.

### S09 protocol and serve

`nle-protocol/src/*`, `serve.rs`, `tests/serve.rs`: JSON-RPC error codes,
client timeouts, socket isolation and permissions, the one-session invariant,
catalog/schema drift, no project I/O or process spawning in the protocol crate,
and `Call`/`Reply` pairing exhaustiveness.
Before touching: `docs/pitfalls/protocol.md`
Done when: a malformed or partial frame cannot wedge the server.

### S10 monitor, composite and proxy cache

`preview.rs`, `proxy.rs`, `mpv.rs`, `composite_segments`: the active track does
not reload the monitor; black only where every track is a gap; audible-mix
spans use the export graph; the proxy cache key covers every input that changes
the output (effects, retime, crop window, picture size); a stale cache after an
edit.
Before touching: `docs/pitfalls/mpv-ffmpeg.md`
Done when: the cache key is diffed against the full input set.

### S11 TUI render, keys, hints

`tui/panels/timeline.rs`, `panel_heights`, `marker_col` as the only quantizer,
`hints_for` as the only hint source, `FakeMpv` routing, digits as counts (`3l`),
`edit --dump`-backed render tests. Check one narrow (60×18) and one wide
terminal, plus the count-vs-track-key conflict.
Done when: both geometries and the key conflict have a verdict.

### S12 playhead audit

`docs/pitfalls/playhead.md` rule by rule: no-op must not move it, media-frame
following, extend lands on the edge, undo/redo restore the pre-edit position,
deletes keep the offset or land on the seam, clamp to `last_frame` never `end`,
track/pool changes leave it put.
Before touching: `docs/pitfalls/playhead.md`
Done when: every rule names the test that guards it, or a finding.

### S13 test-coverage gaps

`tests/hostile.rs`, `tests/behaviour.rs`, `tests/fixtures.rs`, `tests/common`:
propose missing garbage classes (non-UTF8 argv, 0-byte media, absurd fps, a
duration under one frame, id collisions across tracks, negative or overflowing
frame fields, symlinked project dirs) plus a same-flag → `Action` parity sweep
across the CLI parser and the catalog parser. Watch for fixtures where id ==
index hiding an id-vs-index bug.
Done when: a ranked list of at most six concrete cases, each with argv.

### S14 docs drift

Claim by claim: `README.md`, `docs/pitfalls/*`, then `PLAN.md` status rows and
version numbers against the code. Report only. Prefer deleting a stale rule to
rewording it. `PLAN.md` and `AGENTS.md` changes need the user's approval.
Done when: each doc has a verdict (holds / stale / needs-guard-test).

### S15 panic and unwrap audit

Non-test `.rs` only: `unwrap`/`expect`/`panic!`/`as usize` on user input,
`chars[cur..o]` slices, index arithmetic on frame counts, rational division by
zero (`fps == 0`, `speed == 0`). Rank by reachability from argv.
Done when: the reachable set is listed with a repro or "guard exists at <site>".
