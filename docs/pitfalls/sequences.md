# Sequences

`Project { media, sequences, active }` holds one or more editable timelines.
The media pool is shared; `fps`, `video`, `timeline` and `playhead` belong to a
`Sequence`. Project files are v16 (v11 added sequences, v12 media groups,
v13 annotations, v14 clip-relative timeline anchors, v15 crop windows,
v16 crop keyframes); older files are rejected, never migrated.

## One active sequence, one undo stack

- `Project` derefs to the active sequence, so `project.timeline` and
  `project.fps` stay the sequence-agnostic spelling every edit path already
  uses. Read the active sequence explicitly (`active_sequence()`,
  `active_id()`) only when the id itself matters.
- `Document` owns `Histories` keyed by sequence id and routes every edit,
  asset change, undo and redo through `project.active_id()`. Switching
  sequences switches undo; never share one stack across timelines.
- History sidecars are v23 (`stacks` keyed by id, plus grouped changes,
  media groups, annotations, timeline anchors and crop keyframes).
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
  `active`, and a positive `fps`. `parse_fps_arg` and `Document::add_sequence`
  reject `--fps 0` before it can reach `grid_time` (which divides by the rate).
  Loading rejects a project with no sequences.
- `History::apply` clamps the edited sequence. `Document::save` uses
  `clamp_project_all`, so a sequence that is not active is still clamped
  before the file is written.
- `asset_usage` spans every sequence: removing a pool asset must stay refused
  while any timeline uses it.

## Front-ends

- TUI: `:sequence ls`, `:sequence N`/`:sequence use N`, `:sequence add NAME
  [--width W --height H --fps RATE]`. Switching resets the active track (tracks
  are per sequence), adopts the target's stored playhead, saves the project,
  and rebuilds the monitor.
- CLI: `nile sequence ls|add|use`; `--sequence N` on `show`, `edl` and
  `export` reads/renders that timeline without changing the stored active one.
  `show --json` always prints the whole project, so it refuses `--sequence`
  rather than rewriting `active` in the machine-readable output.
- The status row names the active sequence only when there is more than one,
  so single-sequence output is unchanged.

## Derived sequences

`Sequence.origin` is `Root` or `Derived { from, spec }`, and every sequence
carries a monotonic `manual_edits` counter (undo does not decrement it).
Nothing derives a sequence yet; the fields land now so provenance is not
retrofitted later.
