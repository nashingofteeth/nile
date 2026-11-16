# OTIO import pitfalls

Read before touching `crates/nle-core/src/otio.rs` or `import-otio`.

- **`--group NAME=FILE,FILE` is explicit and ordered.** `~` expands; parts
  resolve as given, then against the `.otio` directory, then by a unique
  pooled name, and a miss is fatal. Parts pool before clips, so they can
  satisfy a moved `target_url` through the pooled-name fallback. Commas
  separate parts, so a filename containing one cannot be a part.

- **Kdenlive's OTIO export has no `LinearTimeWarp`.** It encodes a rate
  conform as `source_range` in *project* frames + `available_range` in the
  *media* rate, so derive `speed = source_rate / media_rate` and source
  seconds as `frame / media_rate`. (A real 24 fps project over 29.97 fps
  media comes out as 1001/1250, not a rounded 0.8.)
- **Dispatch on the OTIO schema *name*, not the tag:** `OTIO_SCHEMA` is
  versioned (`Clip.1`, `Clip.2`), so match the part before the `.` over
  `serde_json::Value`; a `#[serde(tag)]` enum breaks on a version bump.
- **`Clip.2` nests media under `media_references.DEFAULT_MEDIA`** (a map),
  while `Clip.1` uses a singular `media_reference` — handle both.
