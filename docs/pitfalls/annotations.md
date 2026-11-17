# Annotations

Read before touching `annotation.rs`, the `Change::Annotations` path, or
`nile annotation`.

- **A source anchor is a media time, not a timeline position.** Trims,
  splits, ripples and paste cannot move it; timeline anchors (a later slice)
  will be clip-relative so edits can re-anchor them. Do not "helpfully"
  rewrite a source time when a clip moves.
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
