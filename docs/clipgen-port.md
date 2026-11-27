# Clipgen port handoff

Status: **tabled for now**. The engine is ready; this note is for the agent
working on the other device (`muta`) when the `clip-generator` port resumes.
It records the surface to build against, the gaps, and the decisions that
should be made before writing a second engine.

Baseline at the time of writing: `main` at `3d3cd03` (2026-11-27). Check the
current tree and `git log` before relying on a hash.

## 1. Ground rules

- The port lives **outside** the nile crates: a toolkit/client over the
  protocol, not a second pipeline inside `nle-core`.
- No ML in the Rust dependency graph. A detector is an executable child (see
  `docs/pitfalls/analysis.md`).
- Compatibility is **none by design**. Project/history formats reject old
  files; rebuild them with `./demo.sh`, never migrate in place.
- Do not edit `nile.json` or `.nile/<stem>/*` while a server/TUI is running.
  Use the protocol, or stop the session and use the CLI. The server is the
  writer; a direct file write can be adopted but cannot be coordinated with
  an in-flight edit.
- One slice, one branch. Work in `.worktrees/<branch>`, run
  `CHECK_SKIP_INSTALL=1 ./scripts/check.sh` there, and merge only after an
  explicit request. Full `scripts/check.sh` on `main` after each merge.
  Start with `git worktree add .worktrees/<slice> -b <slice> main`.
- Read `AGENTS.md` and the relevant `docs/pitfalls/*.md` before touching an
  area. `PLAN.md` changes need approval; README/docs can move with code.

## 2. Current formats and versions

| Contract | Version |
|---|---|
| `nile.json` / project | **19** |
| Undo sidecar | **25** |
| JSON-RPC protocol (`nle-protocol`) | **5** |
| Analysis sidecar (NDJSON, host↔analyzer) | **3** |
| Packed sample format | **2** |
| Analyzer product (`a1-…`) | **1** |
| Render report | **1** |

The project file is `nile.json` (or the directory default in
`.nile/config.json`); state and caches live under `.nile/<stem>/`:
`history.json`, `clipboard.json`, `analysis/{transcripts,products}`,
`jobs/<id>/`, `cache/`. The playhead is `Project.playhead`; it is session
state, not an edit.

## 3. Starting and speaking to a server

```sh
nile serve                 # per-project Unix socket
nile serve --stdio         # line-oriented pipes (tests, embedded clients)
nile show --json           # a read without the server
```

The socket is `.nile/<stem>/session.sock`; `serve.lock` is the single-writer
lock. A TUI started with `nile edit` attaches to a live server instead of
opening a second writer. One project can have one server.

The wire is JSON-RPC 2.0, one line per message, no shell. `hello` returns the
`PROTOCOL_VERSION`, project version, live revision, undo depth, active
sequence and the served method names. Use the typed `nle_protocol::Client`
instead of hand-written JSON; it also enforces the response/event contract.

Rough client setup:

```rust
let stream = std::os::unix::net::UnixStream::connect(socket)?;
let read = std::io::BufReader::new(stream.try_clone()?);
let write = std::io::BufWriter::new(stream);
let mut client = nle_protocol::Client::new(read, write);

let hello = client.hello()?;
let project = client.show()?;
let mut revision = hello.revision;
```

Mutations take `base_revision` (CAS). Refresh the revision from
`hello`/`project.show` or from a `project.changed` event after every write. A
stale revision is a typed conflict (`-32001`); do not retry by guessing.
Reads do not need CAS.

### Method map

| Area | Methods |
|---|---|
| Project/read | `hello`, `project.show`, `timeline.edl`, `playhead.set` |
| Edit | `action.preview`, `action.apply`, `undo`, `redo` |
| Sequences | `sequence.list`, `sequence.use`, `sequence.add`, `sequence.audio` |
| Media | `media.list`, `media.add` (removal is the `AssetRm` action) |
| Annotations/checks | `annotation.list`, `annotation.check` |
| Transcripts | `analysis.add` |
| Jobs | `job.submit|list|get|cancel|remove|prune` |

`action.apply` takes a list of catalog actions and is one undo step. The
`Action` enum (`crates/nle-core/src/action/mod.rs`) is the edit API and the
wire DTO. Do not add a parallel action request type. `sample_actions()` covers
every slot; `nle-protocol/tests/wire.rs` checks the wire forms.

Tracks are **1-based** in the CLI/TUI spellings (`--track 1`, `<N>t`), but
`Action` tracks and `timeline.edl --track` are **0-based** indices. Frames are
0-based everywhere; times are exact rational strings (`"30000/1001"`), never
floats on the wire.

## 4. Events and jobs

The server queues `project.changed`, `playhead`, `sequence.changed`,
`job.progress` and `job.changed` notifications. They are **request-drained**:
there is no async push. A client polls `job.get`/`job.list` or drains events
with `Client::next_event`/`drain_buffered_events` during its own calls. The TUI
does the same. Do not block waiting for an unsolicited notification.

A job submit takes `{base_revision, output, sequence, export}`, snapshots the
project, and returns a durable record. Output must be absolute. Rendering is
one job at a time, FIFO; a queued job can be cancelled, a running one kills
its child, terminal jobs can be removed/pruned. `JobExport` is deliberately
narrow (`preset`, optional `crf`, optional encoder preset); engine choice,
chunk workers and chunk cache are not in the job DTO. The report path is
returned on the terminal event / record.

## 5. Port map

| Clipgen concern | nile surface today | Status / gap |
|---|---|---|
| Live-show ingest | CLI `nile import`, `import-edl`, `import-otio`; protocol `media.add` | CLI-only for EDL/OTIO; protocol only pools already-known files. Append and audio-only EDL are landed. |
| Multi-cam/media groups | Model `MediaGroup`, import `--group`, time maps | Ready; group parts are logical recordings, not duplicate assets. |
| Transcript mining | Protocol `analysis.add {asset, words}`; `t1-…` store | Ready. Words are exact source seconds; server validates asset/duration. |
| Subject/CV mining | CLI `nile analysis detect --asset N --label … [--analyzer]`; `a1-…` product | **No protocol method.** Run the CLI locally, then name the product in an expectation. External detectors are absolute executables; no PATH/project-file detector. |
| Audio mining | CLI `nile analysis analyze-audio`; `audio_levels` product | **No protocol method and no consumer check yet.** CLI-only. |
| Angle/crop planning | Sequences, `AddEffect` crop/scale/offset, keyed crop, undo | No derived-sequence operation; see gaps. |
| Cases/expectations | Catalog actions `AnnotationAddAt` with an `AnnotationKind::Expectation`; `annotation.check` | Ready. `subject-in-frame` is source-product based; `words-complete` uses a transcript. |
| Verification | `annotation.check` returns `CheckReport` (pass/fail/unverifiable); `nile report` | Ready. Check does not write. |
| Review/provenance | History actors/labels, annotation status/resolve | History is available in the model; a review UI is not built. |
| Batch rendering | Loop `job.submit` over sequences/presets | The queue is one-at-a-time; no batch DTO/command and no async push. |

## 6. Gaps to decide before porting

1. **Derived sequences are absent.** There is no `sequence derive` and no
   "copy/replace project" protocol method. Angle/crop planning that produces
   variants must either be implemented in `nile-core` (a derived-sequence
   operation with undo) or the toolkit must create a sequence and apply the
   edit actions itself. Decide this first; it is the main engine change the
   port is likely to need.
2. **Analysis is CLI-only.** `analysis.add` stores transcripts but there is no
   protocol `analysis.detect`/`analyze-audio`. The earlier contract deliberately
   keeps a remote client from naming executables. Options: run the CLI on the
   server host and exchange `a1-…` hashes, or add a server-level detector
   allowlist (never a path from `nile.json`). Do not add an arbitrary
   executable field to the protocol.
3. **Batch jobs and async events.** The queue can accept N submissions, but
   each is a separate snapshot and the client must poll. A batch helper belongs
   above the protocol; async push remains deferred.
4. **Scale/offset geometry.** `subject-in-frame` marks blends and
   `Scale`/`Offset` frames unverifiable; the visible-window model is a model
   slice, not a toolkit workaround. `work-in-frame` still checks crop only.
5. **Chunked cancellation.** Jobs deliberately exclude the parallel knob; a
   job renders one cancellable graph. Do not promise chunked cancellation
   through the job API without a scheduler redesign.

## 7. A safe first vertical case

Use the checked-in 2-cam fixture, not a real show:

1. Start `nile serve` on a rebuilt demo/fixture project.
2. `hello` / `project.show`; note the revision.
3. `media.add` the fixture assets (or use the fixture project already pooled).
4. `analysis.add` a known transcript.
5. Run `nile analysis detect` (CLI) for a known colour label; keep the
   `a1-…` hash.
6. `action.apply` an `AnnotationAddAt` with a `subject-in-frame` (or
   `words-complete`) expectation.
7. `job.submit` an export; poll `job.get` until terminal; load the report.
8. `annotation.check` with the report; assert the summary and the detail.
9. Repeat from a fresh project in a test, not just a manual run.

This proves ingest → analysis product → expectation → render → report → check
without changing `nle-core`. Add the case as an integration test in the
toolkit repo; nile’s `crates/nle-cli/tests/behaviour.rs` already shows the
fixture/CLI patterns.

## 8. Invariants that bite a client

- **Frames are 0-based; a `dur` timeline is `[0, dur)`.** The last position is
  `dur - 1/fps`. Edit-path frame→seconds conversions use
  `checked_frames_to_seconds`; a client computes its own conversions at its
  peril.
- **Exact rationals everywhere on the model/wire.** Send `"30000/1001"`, not
  `29.97`; floats only at ffmpeg/IPC boundaries.
- **Source ↔ timeline crosses through `source_time`/`offset_of_source`.**
  A `Clip` wraps them as `source_at`/`timeline_offset_of`; edits take the
  `source_span` delta. Speed is a rational rate, and a retime ripples.
- **The base video track is the master sequence.** Length-changing edits there
  shift the same span on every other track; overlay/audio edits stay local.
- **Transitions are opaque, video-only and own their sides.** A trim beside a
  blend moves the blend; deleting a side removes it. Do not treat a blend as a
  clip for source mapping.
- **A track is a sequence of clips and gaps.** Appends past the end are one
  trailing gap; do not invent two adjacent gaps. Track numbers are 1-based in
  CLI spellings.
- **Every write path clamps clips to media.** `Document::save` and
  `History::apply` enforce it; bypassing `Document` can read past EOF.
- **Project/history versions have no migration.** A toolkit that upgrades nile
  must rebuild local state; old projects are rejected with a clear version
  error, never partially read.
- **Analysis products are content-addressed and self-describing.** A product
  records its sample grid/signature and analyzer identity; stale sources are
  unverifiable, offline sources stay readable. Cache is disposable;
  `analysis/products/` is state and is never swept.

## 9. Files and docs to read first

| Read | Why |
|---|---|
| `AGENTS.md` | Always-on project rules and invariants |
| `docs/pitfalls/protocol.md` | Wire methods, CAS, events, jobs, budgets |
| `docs/pitfalls/analysis.md` | Sampling, products, sidecar, `detect`, audio levels |
| `docs/pitfalls/annotations.md` | Expectation checks and report evaluation |
| `docs/pitfalls/render-report.md` | Output→source frame map |
| `docs/pitfalls/sequences.md` | Multiple sequences and history |
| `crates/nle-protocol/src/call.rs` | The one `wire_catalog!` method list |
| `crates/nle-protocol/src/transport.rs` | Typed `Client` helpers |
| `crates/nle-core/src/action/mod.rs` | Catalog actions and `sample_actions()` |
| `crates/nle-cli/src/serve.rs` | Socket/lock, handler, job queue |
| `crates/nle-cli/tests/behaviour.rs` | Fixture-backed end-to-end patterns |

## 10. Handoff questions for `muta`

1. Where does the clipgen checkout live, and is its detector available on the
   nile host? If not, the CV case is blocked on a local detector or a
   server-side allowlist decision.
2. Does the port need derived sequences, or can it submit separate jobs per
   planned variant? Answer this before starting the first code slice.
3. Does it need batch submission/async events, or is a polling helper enough
   for the initial cases?
4. Will the toolkit write only through `nile serve`, or also need CLI-only
   import/analysis on the same host? Keep one writer per project.

If the answer to (2) is “yes, derived sequences,” make that the first nile
slice after the audit; do not build a parallel timeline in the toolkit.
