# Render reports

Read before touching `nle-core/src/report.rs`, `nile report`, or the export
path.

- **A report belongs to the output file, not the project.** It is written
  beside the video (`out.mp4` → `out.mp4.report.json`) and read without a
  project. Moving or renaming the video leaves the sidecar behind; the map is
  not project state and carries its own schema version.
- **The frame map is piecewise, not per frame.** Spans come from the union of
  cuts on unmuted video tracks; the topmost track with material wins and only
  all-gaps is black. A blend reports both source sides. Do not expand this to
  one row per frame.
- **Frame boundaries use the exporter's rounding**, `round(timeline × fps)`.
  Zero-frame spans are skipped, so a sub-frame sliver cannot create a bogus
  mapping. `frame_count` is probed from the finished file; the map ends at the
  planned frame grid.
- **A report write failure must not lose the video.** Log it and keep the
  export; the summary says `report failed`.
