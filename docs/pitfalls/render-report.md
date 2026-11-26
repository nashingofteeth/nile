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
- **The map runs to `timeline.end()`, across every track.** A trailing audio
  bed or a longer muted video track is padded with picture black; seed the
  final cut or the map stops at the last visible picture and those frames
  resolve to `None`.
- **Chunked exports write the same sidecar.** `windows` lists the scheduler's
  half-open frame ranges; the frame map is still the whole output.
- **A report is read, not just printed.** `nile annotation check` (and the
  protocol's `annotation.check`) consumes it to evaluate a sequence's
  expectations over the frame map.
- **Product checks go output → source → sample grid.** A `subject-in-frame`
  check calls `locate(frame)` for `(clip, asset, source)`, then
  `SampleGrid::index_at(source)` on the `a1-…` product, and applies that
  clip's crop at `timeline_offset_of(source)`. Never sample the composited
  render and never index the product by output frame: the detector sampled
  the source asset on its own clock.
- **A report write failure must not lose the video.** Log it and keep the
  export; the summary says `report failed`.
