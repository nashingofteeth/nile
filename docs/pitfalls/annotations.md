# Annotations

Read before touching `annotation.rs`, the `Change::Annotations` path, or
`nile annotation`.

- **A source anchor is a media time, not a timeline position.** Trims,
  splits, ripples and paste cannot move it; timeline anchors (a later slice)
  will be clip-relative so edits can re-anchor them. Do not "helpfully"
  rewrite a source time when a clip moves.
- **Ids are project-wide, the list is scoped by `sequence`.** `nile
  annotation ls` defaults to the active sequence; `--sequence N` reads
  another. Adding always writes to the active sequence's history.
- **Validation is on every save.** A note needs non-empty text; a source
  time is half-open `[0, duration)` and a group time must resolve through
  `MediaGroup::group_time`. Removing an anchored asset or group is refused
  (`AssetInAnnotation` / `GroupInAnnotation`), never left dangling.
- **The whole list is the undo unit** (`Change::Annotations`), verified by
  exact equality like media groups. Resolving an already-resolved annotation
  records nothing, so undo does not step through dead entries.
