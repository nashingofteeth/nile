# Analysis sampling + sidecar contract

Read before adding frame/audio sampling, an analyzer sidecar, or a CV-backed
expectation. The **sampler is live** in `crates/nle-ffmpeg/src/sampling.rs`
(RGB24/PCM plus a content-addressed cache); the sidecar/analyzer, frame
times/indices and the CV checks below are the pending half. This doc records
the full contract and marks the split. Precedents: `crates/nle-cli/src/proxy.rs`
(`RENDER_VERSION`, tmp+rename, `prune_at`),
`crates/nle-cli/src/filmstrip.rs`/`thumbnails.rs` (path + mtime key;
thumbnails also size) and `crates/nle-core/src/analysis.rs`
(`TranscriptStore`, `t1-` addresses).

## Purpose and where it lands

- **Sampling decodes bytes; it does not know timelines.** The request is about
  one media file: a source window on the asset's own clock plus shape/rate
  parameters. The caller maps timeline positions with `Clip::source_at` /
  `TimeMap` first, exactly as proxy segments are built.
- `nle-ffmpeg/src/sampling.rs` owns the argv builder, the packed bytes and the
  cache entry; it stays free of `.nile/` paths and project state. The cache
  root and the sidecar host live in `nle-cli` beside `proxy.rs` (pending).
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
  frame, no row padding), frames concatenated in presentation order. The
  current `<key>.json` carries `version`, `kind`, the spec params, `count` and
  the source signature; `times`/`indices` are planned for the sidecar.
- **Times and frame identity are planned, not implemented.** The sampler
  currently samples a uniform grid (`fps` filter) from `start` for `duration`;
  the sidecar will add exact per-frame `times` and an index request
  (`select='eq(n,…)'`) without silently resampling. A grid request and an
  index request must never share a cache entry.
- **Audio: PCM s16le**, interleaved, no WAV/container header. The `.json`
  carries `version`, `kind`, rate/channels, start and the sample count, where
  the count is per-channel sample frames, so the payload length is
  `sample_count × channels × 2`.
- **Validate shape before spawning ffmpeg.** The current specs reject
  negative/zero ranges, zero fps/rate/channels and zero dimensions. A hard
  cap (default 512 MiB, 20 000 frames) is planned; until then the sampler
  buffers the whole sample, so callers must pick a range/geometry that fits
  memory.

## Cache

- Root: `SampleCache::new(root)` takes a caller-provided directory. `nle-cli`
  will pass `state::cache_dir(project_path).join("analysis")`
  (`.nile/<stem>/cache/analysis/`), a `sample_cache_dir` helper mirroring
  `proxy_cache_dir` (wiring pending).
- **Key = request + source signature.** `SAMPLE_VERSION` in
  `nle-ffmpeg::sampling` (same discipline as `RENDER_VERSION`: bump when the
  *meaning* of a parameter changes), the request kind, the exact rational
  params, the canonical media path, and the source size + mtime; spelled as
  16 lowercase hex digits. The entry stores both the canonical and the lexical
  absolute source spelling so an offline pass can still match after the file
  is gone.
- The entry stores the source signature (`mtime` secs/nanos + `size`) in its
  metadata. A hit requires that signature to match the current stat **or the
  source to be missing** — a sample already extracted from bytes that no
  longer exist is still valid, and re-running ffmpeg would only fail. Offline
  lookup scans the entry metadata newest-first and falls through a corrupt
  newest generation.
- **Atomic writes, raw first.** Write the payload to
  `<key>.tmp-<pid>-<seq>`, rename to `<key>.raw`, then persist the metadata
  JSON last. A crash leaves raw without metadata, which is a miss.
- **A hit is only served when it is complete:** metadata parses, versions and
  params match, the source spelling matches, and the raw file's length equals
  the length recomputed from the metadata exactly. Anything else is a miss
  and is overwritten. A hit spawns nothing and does not touch mtime, so
  `cache prune --days` keeps ageing it.
- Misses: replaced source (mtime/size), changed params, changed kind, a
  `SAMPLE_VERSION` bump. On a miss the caller extracts, stores, and serves.
- `nile cache prune` gains an `analysis` line through `proxy::prune_at`
  (widening `is_entry_name` for `.raw`; `<key>.raw|.json` plus the `.tmp-`
  names). Pruning is explicit; a pruned sample re-extracts. **`analysis/` is
  never swept.**

## Sidecar contract

The analyzer is the child, `nile` is the host. Transport is **NDJSON on the
child's stdin/stdout**, one UTF-8 object per line, never re-wrapped as a
JSON-RPC envelope. stderr is unstructured log text and is never parsed. A
control line is capped at 1 MiB; a longer line is fatal, not truncated.
Packed bytes never travel on the wire — only paths.

- **Handshake.** Host: `{"id":1,"method":"hello","protocol":1}`; analyzer:
  `{"id":1,"ok":{"protocol":1}}`. A mismatch refuses to run.
- **Request.** One `{"id":N,"method":"analyze",…}` object whose `input`
  carries `kind`, `sample` (the cache key) and the absolute `data`/`meta`
  paths of the packed file; `request` carries the params and `limits` the
  product cap. `{"id":N,"method":"shutdown"}` ends the session. Ids are
  positive, unique and increasing per child. Every path is absolute; `nile`
  canonicalises before writing the request and the analyzer refuses a
  relative one (the child's cwd is not the client's).
- **Response.** Exactly one line per request, same `id`, `ok` or `error`
  (codes `bad_request`, `unsupported`, `io`, `corrupt_input`, `internal`).
  Codes are closed; an unknown code or a second line for one id is a protocol
  error. No response names more than one product file.

```text
{"id":2,"method":"analyze","input":{"kind":"frames","sample":"7a3c…",
 "data":"/p/.nile/nile/cache/analysis/7a3c….raw",
 "meta":"/p/.nile/nile/cache/analysis/7a3c….json"},
 "request":{"mode":"grid","start":"0/1","rate":"2/1"}}
{"id":2,"ok":{"hash":"a1-…","path":"/p/.nile/nile/analysis/products/a1-…"}}
{"id":2,"error":{"code":"bad_request","message":"relative path"}}
```

- **Spawn without a shell.** `Command::new(absolute_analyzer_path)` with a
  fixed argv and piped stdio; configuration comes over the wire, never from
  `nile.json` into argv. ffmpeg argv puts `--` before the positional output
  (`-`) and passes inputs as `-i` values; no shell string.
- **Bounded result.** The host verifies the product path is under
  `analysis/products/`, its size is within `max_product_bytes` (default
  256 MiB), and its address matches its content before recording it.

## Out-of-process analyzer boundary

- The analyzer reads `meta` first, checks `kind` and the recomputed payload
  length, then indexes the packed payload by the metadata's dimensions. It
  never decodes media itself and never calls ffmpeg: sampling stays in one
  place and the analyzer stays deterministic.
- **Product = content-addressed analysis state.** The analyzer writes
  `analysis/products/a1-<16 hex>` itself (temp + rename) and reports the
  hash; the bytes are canonical JSON containing the sample key, the analyzer
  name/version and the result. `a1-` reuses the `t1-` discipline: canonical
  bytes, FNV-1a 64, strict address spelling, atomic import, no silent
  substitution. `nile` re-reads and re-hashes it (`TranscriptStore::load`
  pattern) before it is referenced from an annotation.
- A product is stale when its sample key no longer matches the current sample
  (or when `SAMPLE_VERSION` moves). Consumers refuse it as unverifiable; they
  never re-run the analyzer from `evaluate`, which stays a pure function.
- **Prove the contract before any ML:** one trivial analyzer computing
  per-frame mean luma from the packed RGB24 and writing a product. Ship it as
  a test-only workspace binary (std only, `publish = false`) and spawn it in
  `behaviour.rs`, so the process boundary — not a function call — is what the
  test pins. ML then replaces it outside the workspace; `nile` links nothing
  new.

## Security and failure rules

- Malformed JSON, an unknown method, a missing/wrong-typed field, a relative
  path or an over-cap request is refused with a typed error before any
  process spawns; the id is echoed when it was parseable.
- Missing source: `NotFound` (or a signature-validated cache hit, above) —
  ffmpeg is not spawned to discover it.
- ffmpeg failure: `{ status, stderr tail }`, no cache entry is written.
- Corrupt cache: a bad metadata parse, an unknown version, or a length
  mismatch is a miss and an atomic overwrite, never a panic or an indexing
  read of untrusted lengths.
- Cache poisoning is local and single-user, so it is not a security boundary
  — but every byte-affecting input is in the key, and a raw file that does
  not describe itself is rejected. Prune never follows a symlink
  (`symlink_metadata`), and products must resolve under `analysis/products/`.
- Analyzer failure (missing binary, nonzero exit, malformed or oversized
  line, id mismatch) leaves the sample cache and the project untouched; its
  temp files live in the cache directory so the next prune removes them.

## Test plan

Live in `sampling.rs` (15 tests):
- **Frame bytes:** a generated testsrc sampled at a grid → payload is exactly
  `count × width × height × 3`, packed rows, expected RGB/dimensions.
- **Audio bytes:** a generated tone at 48 kHz → the requested sample count,
  `len == samples × channels × 2`, and a non-zero peak.
- **Cache hit after source removal:** sample, delete the media, sample again
  → identical bytes, no ffmpeg spawn.
- **Invalidation:** a spec/source change produces a new key; a truncated raw
  turns a hit into a miss that re-extracts.
- **argv guard:** no shell, `--` before the positional output, and the
  `rawvideo`/`s16le` flags.

Pending with the sidecar/analyzer:
- `times`/`indices` and exact source-frame selection agree with the source
  grid.
- A trivial mean-luma analyzer → `a1-…` product, address re-hash verified; a
  malformed request, relative path, nonzero exit and oversized line each
  produce a typed error and no project change.
- `cache prune` reports the analysis line, leaves `analysis/` alone, and a
  pruned sample re-extracts.
