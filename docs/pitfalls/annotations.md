# Annotations

Read before touching `annotation.rs`, the `Change::Annotations` path, or
`nile annotation`.

- **A source anchor is a media time, not a timeline position.** Trims,
  splits, ripples and paste cannot move it. Do not "helpfully" rewrite a
  source time when a clip moves.
- **A timeline anchor is `{ clip, source }`, not a timeline offset.** Trims,
  speed changes, ripples and paste keep the same footage; a split re-anchors
  to the piece that owns the source; a deleted endpoint drops the annotation
  with a warning. The drop is recorded in the track change, so undo restores
  it. Adding or growing a blend also consumes the source from its neighbour,
  and a blend side has no anchor identity, so a note held in those frames
  drops with the same warning. A timeline endpoint is closed at the clip's
  tail (`[asset_in, asset_out]`), so a range can end on a cut: the start
  belongs to the following clip, the end to the preceding clip's tail.
  Creation supports points and ranges (`--to`); `annotation ls` resolves the
  span.
- **Ids are project-wide, the list is scoped by `sequence`.** `nile
  annotation ls` defaults to the active sequence; `--sequence N` reads
  another. Adding always writes to the active sequence's history, and
  removing/resolving an annotation from another sequence is refused
  (`EditError::AnnotationSequence`) rather than recorded on the wrong stack.
- **`:note` follows the edit scope: the active track's clip.** The monitor
  shows the composite, so an overlay can hide the clip a note anchors to;
  use `:annotation add --asset/--group` for an explicit source.
- **Validation is on every save.** A note needs non-empty text; a source
  time is half-open `[0, duration)` and a group time must resolve through
  `MediaGroup::group_time`. Removing an anchored asset or group is refused
  (`AssetInAnnotation` / `GroupInAnnotation`), never left dangling.
- **One sequence's list is the undo unit.** `AnnotationsChange` carries
  `sequence` and only that sequence's before/after annotations, verified by
  exact equality; another sequence's edits cannot invalidate the stack.
  Resolving an already-resolved annotation records nothing, so undo does not
  step through dead entries.
