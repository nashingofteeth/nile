# Sequences

`Project { media, sequences, active }` holds one or more editable timelines.
The media pool is shared; `fps`, `video`, `timeline`, `playhead` and `audio`
belong to a `Sequence`. Project files are v19 (v11 added sequences, v12 media
groups, v13 annotations, v14 clip-relative timeline anchors, v15 crop windows,
v16 crop keyframes, v17 render expectations, v18 audio post, v19 subject-box
checks); older files are rejected, never migrated.

## One active sequence, one undo stack

- `Project` derefs to the active sequence, so `project.timeline` and
  `project.fps` stay the sequence-agnostic spelling every edit path already
  uses. Read the active sequence explicitly (`active_sequence()`,
  `active_id()`) only when the id itself matters.
- `Document` owns `Histories` keyed by sequence id and routes every edit,
  asset change, undo and redo through `project.active_id()`. Switching
  sequences switches undo; never share one stack across timelines.
- History sidecars are v25 (`stacks` keyed by id, plus grouped changes,
  media groups, annotations, timeline anchors, crop keyframes, sequence
  audio settings and subject-box checks).
  Pool edits are recorded in the sequence active at the time; undoing one
  still validates against every sequence, so an asset used elsewhere refuses
  loudly.
- Media-group changes are project-wide but recorded on the active sequence's
  stack; changing a group from another sequence can strand older entries.
  Annotations are sequence-scoped for this reason.
- `Project::select_sequence` is model-only. Use `Document::select_sequence` in
  a front-end so the history switches too.
- `active` is persisted session state, like the playhead: `Document::save`
  writes it, `Document::select_sequence` does not bump `revision`.

## Save and validation span sequences

- `Project::validate` walks every sequence and checks unique ids, a valid
  `active`, a positive `fps`, a non-zero output canvas, and that every
  `Derived` origin names an existing source. It also refuses any sequence,
  group, annotation, asset, clip or transition id at `u32::MAX`.
  `u32::MAX` is reserved: allocators hand out at most `u32::MAX - 1`, then
  return `ProjectError::IdExhausted`; the CLI import/detect counters and
  `add_asset` follow the same rule.
  `parse_fps_arg` and `Document::add_sequence` reject `--fps 0` before it can
  reach `grid_time` (which divides by the rate);
  `Document::add_sequence`/`derive_sequence` and the TUI also refuse a zero
  width/height before saving.
  Loading rejects a project with no sequences.
- Segment ids are unique **within a sequence, across every track**: clips and
  transitions share the space (`next_clip_id` spans tracks) and gaps have
  none. `Project::validate` refuses a duplicate as
  `ProjectError::SegmentDuplicate(sequence, segment)`, so a hand-edited
  `nile.json` cannot make `--clip` ambiguous; `Project::load`,
  `Document::save` and `History::apply` all run that check.
- `History::apply` clamps the edited sequence. `Document::save` uses
  `clamp_project_all`, so a sequence that is not active is still clamped
  before the file is written: its clips to their media and its stored
  playhead to `[0, that sequence's own last_frame]` (a negative cursor would
  serialize as an unreadable `"-5/1"`). A save validates a copy
  first, so a refused save leaves the in-memory document untouched.
- `asset_usage` spans every sequence: removing a pool asset must stay refused
  while any timeline uses it.

## Front-ends

- TUI: `:sequence ls`, `:sequence N`/`:sequence use N`, `:sequence add NAME
  [--width W --height H --fps RATE]`, `:sequence derive --from N --name NAME
  [--size WxH] [--fps RATE]`. Switching resets the active track (tracks
  are per sequence), adopts the target's stored playhead, saves the project,
  and rebuilds the monitor.
- CLI: `nile sequence ls|add|use|derive|audio`; `--sequence N` on `show`,
  `edl` and `export` reads/renders that timeline without changing the stored
  active one.
  `show --json` always prints the whole project, so it refuses `--sequence`
  rather than rewriting `active` in the machine-readable output.
- `sequence audio [--normalize [BOOL]] [--fade-in S] [--fade-out S]` changes
  the active sequence's `AudioSettings` and records a `Change::Audio` (undoable
  like an edit). No flags prints the current setting. A settings change is a
  manual edit (`manual_edits` bumps) and a no-op records nothing. Fades are
  parsed with the ordinary time grammar (`0.5`, `1:02.5`), so a negative value
  is refused before the model sees it. The TUI spells it `:sequence audio …`
  through `Session::set_audio` (direct: Document/History + save; remote: the
  typed `sequence.audio` call); the protocol mirrors it as a CAS model write
  and every `SequenceSummary` carries `audio`.
- `AudioSettings` is a typed struct with `#[serde(default)]`: a missing `audio`
  object reads as normalize off / fades zero. `Project::validate` refuses
  negative fades (`SequenceAudio`), but a fade longer than the timeline is
  allowed and clamped at export — clips may arrive after the setting.
- The status row names the active sequence only when there is more than one,
  so single-sequence output is unchanged.

## Derived sequences

`Sequence.origin` is `Root` or `Derived { from, spec }`, and every sequence
carries a monotonic `manual_edits` counter (undo does not decrement it).

`nile sequence derive --from N --name NAME [--size WxH] [--fps RATE]` (the
TUI spells it `:sequence derive …`) copies the source sequence's **snapshot**:
tracks/segments, effects, audio post, and settings. This is deliberately not
a live derivation:

- The source and copy share the media pool, but not their timelines or undo
  stacks. Editing either leaves the other untouched.
- `--size`/`--fps` override the copy's canvas/grid; without them the source
  settings are copied. No crop/scale policy is applied — a 9x16 preset belongs
  to the toolkit, not `nle-core`.
- The copy is not made active and starts at playhead 0 with `manual_edits = 0`.
- **Expectations are copied**, with fresh annotation ids and the copy's
  sequence id. Their clip anchors stay valid because the timeline is identical;
  this prevents a derived render from checking green-on-nothing. Notes and
  markers stay on the source (a separate note-copy policy is not decided).
  Copied expectations keep their review status; the check still runs.
- The new sequence is a whole-project model write (revision bump), not an
  undoable edit; its history starts empty. `spec` is
  `{"kind":"copy"}` plus `fps`/`video` overrides when supplied, so a later
  regeneration can distinguish an inherited canvas from a chosen one.
- `sequence.derive` exposes the same operation over the protocol. Additive, so
  `PROTOCOL_VERSION` stays 5.

Regeneration from a changed source is not implemented: the origin records the
snapshot, but no code reapplies it yet.
