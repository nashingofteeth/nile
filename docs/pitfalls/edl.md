# EDL import (CMX3600) pitfalls

Read before touching `crates/nle-core/src/edl.rs` or the `import-edl` path.

- **Cuts only, but never fatal.** A non-cut event (`D`, `W001`, `K`, …)
  imports as a cut and is reported on `EdlDoc::warnings`, like the OTIO
  path's skipped transitions; a zero-length event is skipped; a black reel
  (`BL`/`BLK`/`BLACK`) becomes a gap. One dissolve must not sink a conform
  list. `build` never reads the transition field — the mapping is the
  parser's job.
- **The reel fallback skips the fuzzy match for a black reel**, or `BL`
  latches onto `blue.mp4` (the edge rule matches `"blue".starts_with("bl")`).
  Only an explicit `--map` or a `SOURCE FILE`/`FROM CLIP NAME` hint resolves
  one. `B` is not in the black list — it is a plausible tape name.
- **A channel code combines with `/`** (`AA/V` = both audio channels *plus*
  video): match any part, or `AA/V` reads as audio-only and loses the picture.
  A video clip carries its embedded audio, so a combined event's sound is
  linked, not doubled.
- **An audio-only EDL is representable.** The model requires a video track but
  not video *material*: `build` emits an empty base video track plus one audio
  track per channel (`A`, `A2`, `AA`/`B`), and the timeline length comes from
  the audio. No black or placeholder clip is invented. CMX `B` means both
  audio channels, so it groups with `AA`. Audio-only media carries no frame
  rate, so pass `--fps` (or keep a video file in `--media` to seed it).
- **Every event's channel must name picture or sound.** An unknown part
  (`X`, `N`, `V/X`) is `BuildError::UnknownChannel`; dropping it would silently
  shift every later event off record time. Zero-length and non-cut events keep
  their existing warn-and-continue path.
- **`--append` adds after the current sequence end.** Each imported track keeps
  its kind (first video → `--track N` or the first video track; audio → the
  next audio track, created if needed) and is padded with a gap to the common
  end, so the imported tracks stay aligned. `--append` uses the project's frame
  rate — a conflicting `--fps` is refused — and conflicts with `--force`
  (`--force` means replace). Like other imports it re-baselines history.
- **Record time starts at `01:00:00:00`** — the timeline origin is the
  *earliest* event, not 0. Treating 0 as the origin turns the first hour
  into one giant gap.
- **`FCM: NON-DROP FRAME` contains the substring "DROP"** — test
  `!contains("NON")`, or every non-drop EDL is misread as drop-frame.
- **Source timecodes are file-relative by default**; `--src-offset` maps
  hour-based media. Audio events whose reel + source match a video event are
  **linked** and skipped, so embedded audio isn't doubled.
- **A speed change hides in the src/rec span mismatch.** EDLs carry no rate
  field; `speed = src_span / rec_span`. Deriving it per event keeps the built
  timeline aligned with record time; assuming 1× shifts every later event by
  the accumulated mismatch.
- **`--group NAME=FILE,FILE` is explicit and ordered.** `~` expands; parts
  resolve as given, then in `--media`, then by a unique pooled name, and a
  miss is fatal (unlike a stray media-dir file, which only warns). Explicit
  parts are pooled media, so they can satisfy an otherwise empty `--media`.
  Commas separate parts, so a filename containing one cannot be a part.
- **Fuzzy reel matching must be edge-aligned and unique.** A substring that
  matches anywhere latches `AX` onto anything containing it (`taxi.mp4`),
  and first-match-in-pool-order is a decision, not evidence: match a stem
  edge, require exactly one candidate, else report `Unresolved`.
