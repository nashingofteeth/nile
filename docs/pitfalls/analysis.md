# Analysis sampling + sidecar contract

Read before adding frame/audio sampling, an analyzer sidecar, or a CV-backed
expectation. The **sampler** lives in `crates/nle-ffmpeg/src/sampling.rs`
(`FrameSpec`/`AudioSpec`, `SampleCache`, RGB24/PCM); the **sidecar host and
bundled analyzers** live in `crates/nle-cli/src/analysis.rs`, with products
in `crates/nle-core/src/analysis.rs` (`ProductStore`, `a1-` addresses, the
`subject_boxes` result). Frame times/indices, subject boxes and audio levels
have landed. Precedents: `crates/nle-cli/src/proxy.rs`
(`RENDER_VERSION`, tmp+rename, `prune_at`),
`crates/nle-cli/src/filmstrip.rs`/`thumbnails.rs` (`asset.path` + size + mtime
key; filmstrip also the source span and dimensions) and the transcript store
(`t1-` addresses).

## Purpose and where it lands

- **Sampling decodes bytes; it does not know timelines.** The request is about
  one media file: a source window (a grid) or exact source indices, on the
  asset's own clock, plus shape/rate parameters. The caller maps timeline
  positions with `Clip::source_at` / `TimeMap` first, exactly as proxy
  segments are built.
- `crates/nle-ffmpeg/src/sampling.rs` owns the argv builder, the packed bytes
  and the cache entry; it stays free of `.nile/` paths and project state.
  `SampleCache::new(root)` takes a caller directory; `nile analysis analyze`
  passes `cache_dir(project_path).join("analysis")`, creates a staging
  directory beneath it, and spawns the hidden child.
- **ML stays out of process and out of the dependency graph.** No new runtime
  crate: an analyzer is a child that reads packed files and writes a product.
  `Cargo.toml` does not change for CV.
- Samples are **disposable cache** (`cache/analysis/`). Transcripts and
  analyzer products are **analysis state** (`analysis/`): content-addressed,
  never mtime-pruned, never in `nile.json`.

## Packed formats

One response is **one packed file**, never a directory of JPEGs: N files have
N mtimes, no atomic write, lossy pixels and a decode step in the analyzer.
Packed is one read, exact bytes.

- **Frames: RGB24**, row-major, tightly packed (`width × height × 3` per
  frame, no row padding), frames concatenated in presentation order.
- **The frames metadata records shape and exact times.** `<key>.json` is
  `version`, `params`, `source`, `source_arg`, `source_size`,
  `source_mtime_secs`, `source_mtime_nanos`, `key`, `bytes`, plus the
  flattened `kind: "frames"`, `width`, `height`, `frames`, `times` and (for an
  index request) `indices`. `params` is a 16-hex fingerprint, not the spec;
  the window is carried by the request only. **`meta` is authoritative for
  shape** (`frames`, `width`, `height`); the request is authoritative for
  time, and a recorded time list that disagrees with it is a miss.
- **Identity is mode-defined.** A **grid** runs `fps=<rate>` from `start` for
  `duration` and scales to the geometry, so frame `i` is `start + i/rate` by
  construction; a range past the media end packs fewer frames
  (`frames <= floor(duration × fps)`), and `frames == 0` is an error, never an
  empty success. An **index** request names exact source frames at the source
  `rate`: frame `i` is source frame `indices[i]`, at `indices[i] / rate`, and
  `indices` is strictly increasing so payload order is request order. The
  sampler seeks half a frame before the first index and uses
  `select=eq(n,…)` **before** the scale, so the picture is the source's own
  frame, not a resample. An index request names at most `MAX_INDEX_FRAMES`
  (96) frames: `select=eq(n,…)+…` is one left-deep expression and ffmpeg's
  parser stops at about 100 terms, so a larger request is refused before
  ffmpeg spawns — sample a grid for a longer range. If the media ends before
  the last requested index, the sampler refuses the short sample
  (`ShortIndex`) and writes no entry; it never caches a prefix.
  `FrameSample::times()`/`time(i)`/`indices()` expose both.
- **Audio: PCM s16le**, interleaved, no WAV/container header. The `.json` is
  the same envelope with `kind: "audio"`, `sample_rate`, `channels` and
  `samples` (per-channel frames actually packed, at most
  `floor(duration × sample_rate)`); there is no `start`. Payload length is
  `samples × channels × 2`. Audio levels are computed from this payload by the
  child, so introducing them did not change the packed format: `SAMPLE_VERSION`
  stays 2.
- **The request must agree with the payload.** Recompute the length from
  `meta` and require `bytes == raw.len()`; require `width`/`height` (or
  `sample_rate`/`channels`) to equal the request, `frames`/`samples` not to
  exceed the request's count, and `times`/`indices` to be exactly the
  request's. An index entry that packs fewer frames than its `indices` names
  is a miss: it is re-sampled, and the sampler then refuses the short sample.
  A disagreeing pair is corrupt input, not a shorter sample.
- **Validation is positivity plus the sampler's caps.** `FrameSpec` /
  `AudioSpec` reject negative/zero ranges, zero fps/rate/channels, zero
  dimensions, empty/out-of-order indices, an index request over
  `MAX_INDEX_FRAMES` (96), and any request over `MAX_SAMPLE_FRAMES`
  (100 000) or `MAX_SAMPLE_BYTES` (256 MiB) — **before** the source is
  stat'ed or ffmpeg spawns. `nile analysis analyze` keeps its own budget check
  for a CLI-level message; it uses the same byte limit.

## Cache

- Root: `SampleCache::new(root)` takes a caller directory. `nle-cli` passes
  `analysis::sample_cache_dir(project_path)` — `cache_dir(...).join(
  "analysis")`, `.nile/<stem>/cache/analysis/` — a helper mirroring
  `proxy_cache_dir`; the sampler's own comment records the same split
  (samples are disposable, `analysis/` is state).
- **Key = request + source signature.** `SAMPLE_VERSION` in
  `nle-ffmpeg::sampling` (same discipline as `RENDER_VERSION`: bump when the
  *meaning* of a parameter changes; v2 introduced per-frame times and exact
  indices), the kind, the mode tag and its exact rational window/rate and
  indices, and the geometry — hashed to the 16-hex `params` — plus the
  canonical source path and the source `size` + `mtime` secs/nanos. The full
  key is the 16-hex `key` that names the entry. Both are opaque fingerprints,
  not a readable spec. A grid and an index request never share an entry: the
  mode tag is part of `params`.
- Metadata keeps both source spellings: `source` (canonical when the file
  existed) and `source_arg` (lexical absolute), so a symlinked — and a
  removed — path still resolves offline.
- A hit first recomputes `entry_key` from the current stat, then validates the
  entry. When the source is gone the key cannot be recomputed: the offline
  lookup scans `*.json` for the newest generation whose `params` and
  `source`/`source_arg` match, newest `source_mtime` first, and falls through
  a corrupt newest entry. That is what makes an offline analysis pass work.
- **Atomic writes, raw first.** `<key>.raw` is written through
  `<key>.tmp-<pid>-<seq>.raw` in the same directory and renamed into place;
  `<key>.json` is written last, so it is the commit marker. A crash leaves a
  temp or a raw without metadata, which is a miss.
- **A hit is only served when it is complete:** metadata parses, `version`,
  `params` and `key` match, the source spelling matches, `meta.bytes` equals
  the raw length, and `frames`/`samples`, geometry and rate match the spec
  with a recomputed length equal to the raw length; the recorded `times` and
  `indices` must equal what the spec derives. Anything else is a miss and an
  atomic overwrite. A hit spawns nothing and does not touch mtime, so
  `cache prune --days` keeps ageing it.
- Misses: replaced source (mtime/size), changed params/kind/mode, a
  `SAMPLE_VERSION` bump. On a miss the caller samples, stores, and serves.
- **Prune:** `nile cache prune` / `:cache prune` adds an `analysis` line —
  `proxy::prune_at` over `cache/analysis` (`<key>.raw`, `<key>.json` and a
  crashed write's `<key>.tmp-<pid>-<seq>.<ext>` are all entry names) plus
  `analysis::prune_staging` over `cache/analysis/staging/<pid>-<seq>/`
  directories, which only the killed-host leftover needs. **`analysis/` is
  never swept**: products and transcripts are state, and pruning a sample
  re-extracts it.

## Sidecar contract

The analyzer is the child, `nile` the host. Transport is **NDJSON on the
child's stdin/stdout**, one UTF-8 object per line, never re-wrapped as a
JSON-RPC envelope. stderr is unstructured log text and is never parsed. A
control line is capped at 1 MiB; a longer line is fatal, not truncated.
Packed bytes never travel on the wire — only paths.

- **Handshake.** Host: `{"id":1,"method":"hello","protocol":3}`; analyzer:
  `{"id":1,"ok":{"protocol":3,"analyzer":{"name","version"},
  "tasks":["mean_luma","subject_boxes","audio_levels"]}}`. A protocol mismatch
  refuses to run, and so does asking for a task the analyzer did not
  advertise. The host samples before the handshake, so an unsupported detector
  still pays for the packed window (the sample is cached); the child never
  reads or decodes it. Protocol 3 added `audio_levels` and an audio `request`
  tag; the envelope shape is otherwise unchanged.
- **Request.** `analyze` carries `input` (`kind`, `sample` — the cache key —
  plus the absolute `data` and `meta` paths), `request` (tagged by `mode`: a
  grid's `start`/`duration`/`fps`, an index request's `rate`/`indices`, or an
  audio window's `start`/`duration`/`sample_rate`/`channels`; grid/index also
  carry `width`/`height`), `task` (tagged by `kind`: `mean_luma`,
  `subject_boxes` with `labels` and `floor_milli`, or `audio_levels` with
  `window_samples`), `stage` (a host-made temp directory under
  `cache/analysis/`), and `limits`. A mode may not carry another mode's
  fields, and `input.kind` must agree with it. `meta` is authoritative for
  shape, the request is the window, and the analyzer cross-checks the two —
  including `times` and `indices`. Ids are positive, unique and increasing per
  child; every path is absolute (the child's cwd is not the client's) and the
  analyzer refuses a relative one.
- **Response.** Exactly one line per request, same `id`, `ok` or `error`
  (codes `bad_request`, `unsupported`, `io`, `corrupt_input`, `internal`).
  `ok` reports the product `hash` and the file name inside `stage`; the host
  renames it into `analysis/products/`. A second line for one id, an unknown
  code or a response naming more than one product file is a protocol error.

```text
{"id":2,"method":"analyze","input":{"kind":"frames","sample":"7a3c…",
 "data":"/p/.nile/nile/cache/analysis/7a3c….raw",
 "meta":"/p/.nile/nile/cache/analysis/7a3c….json"},
 "request":{"mode":"grid","start":"0/1","duration":"1/2","fps":"2/1",
 "width":32,"height":24},
 "task":{"kind":"mean_luma"},
 "stage":"/p/.nile/nile/cache/analysis/staging/1234-7"}
{"id":3,"method":"analyze", … "request":{"mode":"grid","start":"0/1",
 "duration":"2/1","fps":"2/1","width":32,"height":24},
 "task":{"kind":"subject_boxes","labels":["face"],"floor_milli":200}, …}
{"id":4,"method":"analyze","input":{"kind":"audio","sample":"9b1d…",
 "data":"/p/.nile/nile/cache/analysis/9b1d….raw",
 "meta":"/p/.nile/nile/cache/analysis/9b1d….json"},
 "request":{"mode":"audio","start":"0/1","duration":"1/1",
 "sample_rate":48000,"channels":1},
 "task":{"kind":"audio_levels","window_samples":4800},
 "stage":"/p/.nile/nile/cache/analysis/staging/1234-8"}
{"id":2,"ok":{"hash":"a1-…","stage":"product.json"}}
{"id":2,"error":{"code":"bad_request","message":"relative path"}}
```

- **Spawn without a shell.** `Command::new(absolute_analyzer_path)` with a
  fixed argv and piped stdio; configuration comes over the wire, never from
  `nile.json` into argv. ffmpeg argv puts `--` before the positional output
  (`-`) and passes inputs as `-i` values; no shell string.
- **One staging mechanism.** The host creates
  `cache/analysis/staging/<pid>-<seq>/` for the run, passes it as `stage`,
  and removes it after the rename. The analyzer may only write a plain file
  directly in that directory: a name with a path separator, an absolute
  name, or a symlink target is `bad_request`, and the host rejects a
  `stage`/file that resolves outside it. Nothing the analyzer writes lands in
  `analysis/` until the host renames a validated product.
- **Bounded result.** The host stats the staged file, caps it
  (`max_product_bytes`, default 256 MiB), verifies the `a1-` address against
  its content, then renames it to `analysis/products/a1-<16 hex>`; identical
  bytes at an existing address are idempotent, different bytes are a refusal.
- **Landed product shape.** `{"version":1,"analyzer":{"name","version"},
  "sample":{"version","key","params","source","source_arg","source_size",
  "source_mtime_secs","source_mtime_nanos"},"result":{"kind":"mean_luma",
  "luma_milli":[…]}}`; `luma_milli` is per-frame Rec. 709 luma ×1000 in
  integer units. The analyzer writes it canonically (plus one newline); the
  host accepts only those bytes (or the same bytes without the newline).
  Pretty-printed or reordered output is a refusal, not silently rewritten.
  Adoption also requires one luma entry per packed frame (`meta.frames`), so
  a truncated result is refused rather than read as a shorter sample.
  `AnalysisProduct::freshness` compares the recorded signature with the live
  file and answers `Fresh`/`Stale`/`Offline`; `is_compatible` owns the
  product/sampler version check.
- **Subject product shape.** The same envelope with
  `"result":{"kind":"subject_boxes","grid":{"start","fps","width","height"},
  "labels":["face"],"floor_milli":200,"frames":[{"index":0,"boxes":[
  {"subject":{"label":"face","id":"spk-2"},
  "rect":{"x":312500,"y":333333,"w":125000,"h":166667},
  "confidence_milli":940}]}]}`. Coordinates are integers in millionths of
  the **sampled** picture (`1_000_000` = one side), so they survive the
  sampler's `scale`; `grid` says which source times the frames describe.
  Frames are in presentation order with `frames[i].index == i`; boxes are
  sorted by `(label, id, confidence desc, x, y, w, h)`, and `validate`
  refuses unsorted bytes, a label not in `labels`, a zero-side/out-of-picture
  box, a confidence below `floor_milli`, or a frame index that is not its
  position. Adoption additionally cross-checks `grid` against the request and
  `frames.len()` against `meta.frames`.
- **Audio product shape.** The same envelope with
  `"result":{"kind":"audio_levels","start":"0/1","sample_rate":48000,
  "channels":1,"window_samples":4800,"windows":[
  {"start":0,"samples":4800,"rms_millionths":182000,
  "peak_millionths":421000}]}`. `start` is the source time of sample 0 of the
  packed payload; each window's `start` is a per-channel sample offset from
  it. Amplitudes are millionths of full scale (`1_000_000` = full s16), never
  floats. Both RMS and peak **aggregate every channel** in the window into one
  scalar: there is no per-channel breakdown. Windows tile the payload: the
  first starts at 0, each next starts
  where the previous ended, every window but the last is exactly
  `window_samples` long, and only the final one may be shorter. `validate`
  refuses a hole, an overlong or empty window, RMS above peak, a peak over
  `1_000_000`, and a zero rate/channels/window. Adoption also cross-checks
  `start`/rate/channels/`window_samples` against the request and the window
  total against `meta.samples`, so a truncated result is refused rather than
  read as a shorter sample. `cmd_analyze_audio` refuses a window shape over
  `MAX_AUDIO_WINDOWS` (250 000) **before** sampling, naming the count and
  suggesting a longer `--window` or shorter `--duration`.
- **`shutdown` is the third method.** Ids are 1/2/3 for hello/analyze/
  shutdown; the analyzer answers `{"id":3,"ok":{"shutdown":true}}` and exits.
  A bad line after the handshake gets its own error line and the loop
  continues; an oversized line or an unknown response shape is fatal.

## Out-of-process analyzer boundary

- The analyzer reads `meta` first, requires the request's `sample` to equal
  the entry it was handed, recomputes the payload length from `meta`, and
  cross-checks geometry/time against the request. It never decodes media and
  never calls ffmpeg: sampling stays in one place and the analyzer stays
  deterministic.
- **Product = content-addressed analysis state, written by the host.** The
  analyzer writes one file into `stage` and reports its hash; the host
  re-reads it, checks the size and the `a1-` spelling/content hash, renames
  it into place and removes `stage`. `a1-` reuses the `t1-` discipline:
  canonical bytes, FNV-1a 64, strict address spelling, atomic import, no
  silent substitution. No analyzer temp ever lands in `analysis/`.
- **A product records its sample, not a live key.** Its canonical JSON
  carries `version`, the analyzer name/version, the sample `key` and
  `params`, the source signature (`source`, `source_arg`, `source_size`,
  `source_mtime_secs`, `source_mtime_nanos`) and the result. Staleness is
  judged against that **recorded** signature: a live source whose size or
  mtime differs makes it stale; when the source is missing or the sample was
  pruned the key cannot be recomputed, so the product stays readable (the
  offline rule) and a re-sample compares its new signature to the recorded
  one. A `SAMPLE_VERSION` or product-schema bump makes it incompatible, not
  stale-by-mtime. Consumers refuse stale/incompatible products as
  unverifiable; `evaluate` stays pure and never runs an analyzer.
- **The contract is proved before any ML:** a hidden `nile analysis-analyzer`
  subcommand in the same binary (`#[command(hide = true)]`, not a second
  installed bin) computes per-frame mean luma or deterministic colour-region
  boxes from the packed RGB24 and writes a product into `stage`. The colour
  detector only understands `red`/`green`/`blue`/`white`/`black`; it is a test
  double for the process boundary, not a face detector. `behaviour.rs` drives
  `nile analysis analyze`/`detect` through `env!("CARGO_BIN_EXE_nile")`, so
  the process boundary — not a function call — is what the test pins.
  `nile analysis detect --analyzer /abs/path` (or `NILE_ANALYZER`) runs a real
  detector; there is no PATH search and no project-file detector setting. ML
  replaces the child outside the workspace; `nile` links nothing new.
- **A detector is an executable, not a link.** The external path is explicit
  and absolute; the host never resolves one from `nile.json`, the media
  folder or `PATH`. The product records the detector identity it wrote, so a
  detector upgrade needs a new run and a new `a1-…` address rather than
  silently changing an old check.

## Subject-in-frame evaluation

`Expectation::SubjectInFrame` names a product address, the pooled `asset` and
a detector `label` (optionally one `subject_id`). It is a source product
check, never a sample of the composited render:

1. The anchor resolves to output frames `[f0, f1)` exactly like every other
   check; a truncated export fails.
2. For each frame, `RenderReport::locate` gives `(clip, asset, source)`.
   A different asset on screen and a black frame **fail**; a blend frame is
   **unverifiable** (there is no single source picture).
3. `clip.timeline_offset_of(source)` maps source → clip-relative timeline.
   A static chain calls `Effect::geometry` (the export's own `Picture`
   tracker) for the visible box and the source→canvas projection; a keyed
   crop uses `timeline::crop_passes`/`CropPass::window_at` (the same planner
   the exporter formats, head ramp included). Static `scale`/`offset` are
   projected, not skipped; a keyed crop mixed with another effect has no
   render path and is unverifiable.
   `work-in-frame` uses the same tracker; its margin rule applies only to a
   clip carrying a geometry effect, so a plain full-bleed clip passes.
4. `SampleGrid::index_at(source)` floors exactly in `i128`; a source time
   before the grid or past its packed frames is **unverifiable**, never a
   guessed frame.
5. Every box of the label (and `subject_id`, when set) with
   `confidence_milli >= round(min_confidence × 1000)` must have its whole
   rect inside the visible window after `margin_pct`. A label with no box at a
   covered frame **fails** — this check asserts presence, unlike
   `words_complete`'s vacuous empty-span pass. If the product's retention
   floor is above the check's threshold the answer is **unverifiable** (the
   product cannot say "absent" below its own floor). `min_confidence` defaults
   to 0, so every box the product retained counts; the detector's retention
   floor belongs to `analysis detect`. If boxes exist but all are below the
   check's threshold, the detail names the highest one instead of claiming
   the subject is absent.

A missing/corrupt product, a product for another source, a non-`subject_boxes`
result, a label the product did not detect, an incompatible
`SAMPLE_VERSION`/`PRODUCT_VERSION`, and a stale source are all unverifiable
with a specific detail. An offline source still evaluates: the grid and source
signature are recorded in the product. `evaluate`/`evaluate_with` take an
`AnalysisStores` bundle (transcripts + products); the report-only `evaluate`
uses empty stores, so it stays pure.

## Security and failure rules

- Malformed JSON, an unknown method, a missing/wrong-typed field, a relative
  path, an escaping `stage` name or an over-cap request is refused with a
  typed error before any process spawns; the id is echoed when it was
  parseable.
- **The sampler bounds every request.** `MAX_SAMPLE_BYTES` (256 MiB) and
  `MAX_SAMPLE_FRAMES` (100 000) are checked in `FrameSpec::validate` /
  `AudioSpec::validate` before the source is stat'ed or ffmpeg spawns; the
  refusal (`SampleError::TooLarge` / `TooManyFrames`) names the count and the
  limit. The sidecar host keeps its own budget line for a CLI message, using
  the same byte constant.
- Missing source: `SampleError::MissingFile` (or the offline cache lookup) —
  ffmpeg is not spawned to discover it.
- ffmpeg failure: `SampleError::Ffmpeg { status, message }` with the tail from
  `trim::error_lines`, plus `Ragged`/`Empty` for a process that returned the
  wrong shape; no cache entry is committed.
- Corrupt cache: a bad metadata parse, an unknown version, or a length
  mismatch is a miss and an atomic overwrite, never a panic or an indexing
  read of untrusted lengths.
- Cache poisoning is local and single-user, so it is not a security boundary
  — but every byte-affecting input is in the key, and a raw file that does
  not describe itself is rejected. Prune never follows a symlink
  (`symlink_metadata`), and products must resolve under `analysis/products/`.
- Analyzer failure (missing binary, nonzero exit, malformed or oversized
  line, id mismatch, bad `stage` name) leaves the sample cache and the
  project untouched; `stage` lives under `cache/analysis/`, so the next prune
  removes it.

## Test plan

Live in `sampling.rs`:
- **Frame bytes:** a generated testsrc sampled at a grid → payload is exactly
  `frames × width × height × 3`, packed rows, expected RGB and dimensions.
- **Times at 29.97:** grid times are `start + i/(30000/1001)` exactly; index
  times are `index/(30000/1001)`.
- **Exact indices:** a moving test pattern's index sample compares byte-for-byte
  with the same source frames from a 30 fps grid; a case whose first index is
  non-zero (30 and 29.97 fps) guards the half-frame-early seek.
- **Index caps:** an index request over `MAX_INDEX_FRAMES` is refused before
  any spawn; a request whose last index is past the media end returns
  `ShortIndex`, writes no entry, and leaves a later valid request working.
- **Grid vs index keys:** the two modes produce different `params`/`key` and
  two entries, never a shared one.
- **Caps:** an over-byte or over-frame request to a missing source is refused
  before any spawn (`TooLarge`/`TooManyFrames`, no cache directory).
- **Audio bytes:** a generated tone at 48 kHz → the requested sample count,
  `len == samples × channels × 2`, and a non-zero peak; an over-cap audio
  request is refused.
- **Cache hit after source removal:** sample, delete the media, sample again
  → identical bytes/times/indices, no ffmpeg spawn; the offline lookup picks
  the newest generation when two exist.
- **Invalidation:** a spec/source change produces a new key; a truncated raw
  turns a hit into a miss that re-extracts.
- **argv guard:** no shell, `--` before the positional output, the
  `rawvideo`/`s16le` flags, and `select=eq(n\,…)` + a half-frame-early seek for
  index requests.

Landed with the sidecar/analyzer:
- The hidden analyzer → `a1-…` product via `stage`, with address
  re-hash and sample/source/times cross-checks; `behaviour.rs` also removes the
  source and re-runs to prove the offline sample hit.
- A malformed line, oversized line, relative path and escaping stage name are
  typed errors; a grid request carrying `rate`/`indices` (or the reverse) is a
  `bad_request`; `hostile.rs` drives the child with garbage and asserts it
  never panics.
- **Product staleness:** `ProductStore` tests cover `Fresh`/`Stale`/`Offline`
  and an incompatible schema/sampler bump; a pruned sample leaves the product
  readable because the source signature is recorded, not recomputed.
- **Prune:** `analysis::prune` removes old `.raw`/`.json`/tmp files and stale
  `staging/<pid>-<seq>/` directories (dry run included), leaves a young entry,
  and never touches `analysis/products/`; `cache prune` prints the `analysis`
  line. A pruned sample re-extracts.

Landed with subject detection:
- **Schema:** subject boxes validate their grid, frame order, sorted labels
  and boxes, non-zero in-picture rects, the confidence floor, the exact
  `SampleGrid::index_at` floor at 29.97, and `sort_detected_boxes`.
- **Process:** the bundled `subject_boxes` task turns a known colour run into
  the exact millionth rect; typed `task` wire forms; an unsupported label is
  `unsupported`, not a silent empty result. `require_result_shape` refuses a
  grid that disagrees with the request or a frame count that disagrees with
  `meta.frames`.
- **Evaluation:** synthetic `SubjectInFrame` products cover inside/outside
  margin, a crop window, static crop+scale+offset projection, a keyed crop at
  each key, no box, another asset, black, blend, grid coverage,
  incompatible/stale/missing products, label/instance and confidence
  filtering.
- **Geometry:** `EffectGeometry` is the export's own tracker, so
  `Effect::chain`'s final `pad` and `visible`/`project` agree
  (`effect_geometry_agrees_with_the_chain_it_renders`); a no-op scale stays
  identity, a zoom/offset clips the box, and `work-in-frame` passes a plain
  clip but fails a zoom that fills the frame.
- **End to end:** `behaviour.rs` runs `nile analysis detect --label red`, then
  `annotation add --expect subject-in-frame` and `annotation check` on a real
  export; a crop that excludes the box flips the check to fail.

Landed with audio levels:
- **Compute:** a known half-scale square wave is 500 000 RMS and peak, full
  scale is a 1 000 000 peak, silence is 0/0; windows tile 5 samples as 2+2+1,
  and stereo counts frames, not interleaved values.
- **Schema:** `AudioLevels::validate` refuses a hole, a short non-last window,
  zero rate/channels/window, empty windows, RMS above peak and a peak over
  full scale; the canonical bytes stay integer.
- **Process:** the analyzer derives the same windows from packed s16le, and
  `require_audio_result` refuses a product whose rate/channels/window or
  covered sample count disagrees with the metadata.
- **End to end:** `behaviour.rs` runs `nile analysis analyze-audio`, checks the
  `audio_levels` product shape, proves a frame sample and an audio sample get
  different cache entries, refuses an audio-less asset, and re-runs with the
  source removed to prove the offline sample hit.
