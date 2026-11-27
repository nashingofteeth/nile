# Protocol wire contract

`nle-protocol` is JSON-RPC 2.0 over NDJSON: one object per line, a blocking
client/server, no async runtime. `nile serve` implements `Handler` on a
`Document`; the crate owns no project I/O, process, or socket.

## Catalog

`hello`, `project.show`, `timeline.edl`, `action.preview`, `action.apply`,
`undo`, `redo`, `sequence.list`, `sequence.use`, `sequence.add`,
`sequence.derive`, `sequence.audio`, `media.list`, `media.add`, `playhead.set`,
`annotation.list`, `annotation.check`, `analysis.add`, `job.submit`,
`job.list`, `job.get`, `job.cancel`, `job.remove`, `job.prune`. The internal
`wire_catalog!` macro
(`crates/nle-protocol/src/catalog.rs`) is the single source: it generates the
enum, `method()`, and `METHODS`/`EVENTS`. Add a
method to the invocation, then a sample to `all_calls()`; the `METHODS.len()`
assertion fails until the sample exists. `hello` reports `PROTOCOL_VERSION`
(bump only for a breaking method, field or unit change; additive methods stay
on the current version and are listed in `hello.methods`), the `nile.json`
version, the model revision, the active sequence's undo depth and id, and the
served methods. `Outcome` and the flattened `Preview` also carry `depth`.

## Actions

`action.preview`/`apply` take an `actions` list. A single-element list keeps
the normal path: timeline-mutating actions produce one undo entry,
`Yank`/`YankRange` only change the register, and `Action::Undo`/`Redo` are the
undo operations themselves. A multi-action batch is atomic and one undo
entry: a failure restores the pre-batch snapshot of the project, histories
and register, and undo/redo inside a batch is refused. A preview returns
`diff` and `warnings`; see below. The batch
plays back on undo in reverse and redo in order (`Change::Group`). Its final
playhead is the last action's explicit position, else the cursor is clamped;
the single-action media reanchor does not apply to a batch. A batch's `desc`
is `batch: <child> · <child>…`, so clients can identify it without parsing
each child.

`action.preview` returns the outcome fields plus `diff` and `warnings`.
`settings_changed` covers the sequence's name, fps, video and audio post.
The diff shape is:

```text
diff = {
  sequences: [{
    id, settings_changed,
    tracks_added: [index], tracks_removed: [index],
    tracks: [{ index, before_segments, after_segments,
               segments_changed, kind_changed, muted_changed }]
  }],
  sequences_added: [id], sequences_removed: [id],
  assets_added: [asset id], assets_removed: [asset id], assets_changed: [asset id],
  annotations_added: [annotation id], annotations_removed: [annotation id],
  annotations_changed: [annotation id]
}
```

`warnings` is omitted from the JSON when empty and otherwise carries the
clamp notes the real apply would produce (for example, an out-point pulled
back to the media end). `action.apply` keeps the plain outcome.

`base_revision` is compare-and-swap: a stale value returns `-32001` with
`{expected, actual}`. Protocol v4 changed `action` to the `actions` list; v5
gave `action.preview` its `diff`/`warnings` reply.

`from` is the optional rational undo anchor; absent means the shared
`Project.playhead`. It is an **input**: the returned `Outcome.playhead` is the
edit's own result, converted from `nle_core::Outcome` frames with
`frames_to_seconds` (a blend midpoint, the moved edge), never an echo of
`from`.

`playhead.set { at }` moves the shared cursor without a model revision change
and emits `playhead`; it returns `{revision, depth, at}`.

The action is the catalog's serde form (`{"action":"split",...}`), not a
parallel DTO. `crates/nle-core/src/action/tests/wire.rs` pins every slot.

`undo`/`redo` are aliases for `action.apply` with `Action::Undo`/`Redo`.
Both go through one handler function so the revision check and event
emission cannot drift. `from` is ignored for undo/redo: the history restores
its own playhead.

## Sequences

`sequence.list` returns every sequence summary plus the active id.
`sequence.use` switches the edit scope: it saves `active` and the project file
but does not bump the revision (session state, like the playhead), then emits
`sequence.changed` and, when the stored cursor differs, `playhead`.
`sequence.add` is a model write: it takes `base_revision`, bumps the revision,
persists, and emits `project.changed` then `sequence.changed`. It does not
switch.
`sequence.derive {base_revision, from, name, fps?, video?}` is the snapshot
copy: it shares the media pool but not the timeline/history, copies the source
timeline (annotations are not copied), optionally overrides fps/video, does
not switch, and returns the new `SequenceSummary`. It is the same model-write
path as `sequence.add` (CAS, revision bump, both events). Additive, so
`PROTOCOL_VERSION` stays 5.
`sequence.audio {base_revision, audio}` sets the active sequence's
loudness/fade settings. `audio` is the model's `AudioSettings` serde form
(`{"normalize":bool,"fade_in":"1/2","fade_out":"1/1"}`; missing fields default
to off/zero). It is a model write recorded on the active sequence's undo
stack: CAS, revision bump, save, then `project.changed` + `sequence.changed`.
Every `SequenceSummary` carries the current `audio`, so `sequence.list` and
the reply show the setting without a `project.show`. Additive, so
`PROTOCOL_VERSION` stays at 5.

Actions apply to the active sequence. A client that wants another timeline
calls `sequence.use` first; there is deliberately no per-call sequence field,
so one request has one unambiguous scope. Undo is the active sequence's stack.

## Media

`media.list` returns the shared pool summarized per asset: path, duration,
picture size/fps, audio, and scoped usage pairs `{sequence, segment}` across
every sequence. A segment id is a clip or a transition, and is only unique
within its sequence.
`media.add` takes `{base_revision, paths}` and probes every file before
mutating, so one bad path leaves the pool untouched. New files are one undo
entry (`action.apply`/`undo` sees it); already-pooled paths are returned
without a change and do not bump the revision. Removal is the catalog action
`AssetRm`, not a separate `media.*` method. Directories are not expanded here;
the CLI/picker owns that.

The `used_by` shape changed in protocol v3: it is a list of `{sequence,
segment}` pairs, because clip ids repeat across sequences.

## Annotations

`annotation.list {sequence?}` returns `{sequence, annotations}` for that
sequence (default: the active one); an unknown sequence is invalid params.
Mutations are the catalog actions `annotation_add`, `annotation_add_at`,
`annotation_remove` and `annotation_resolve`; each is one undo entry on the
active sequence's stack. A source anchor is a media time (`asset` or group
clock), so timeline edits do not move it; a timeline anchor is
`{clip, source}` and follows the clip. This method is additive, so
`PROTOCOL_VERSION` stays at 5.

`annotation.check {report, sequence?, id?}` evaluates a render report against
the expectations on its sequence and returns `{report, checks, summary}`. The
report is the **render artifact** (`out.mp4.report.json`, or the video path
that maps to it), so a verdict cannot outlive the render it describes. It is a
**read**: no save, revision bump, history entry or event. An explicit
`sequence` must agree with the report's; a missing/corrupt report or a
sequence the project does not have is invalid params (`-32602`), never an
empty pass, while an unknown `id` filters the set to nothing. `report` must be
an absolute path (after `~` expansion): the server's cwd is its own, so a
relative path would name a different file than the client meant. Additive, so
`PROTOCOL_VERSION` stays at 5.

## Analysis

`analysis.add {asset, words}` stores a transcript in the project's analysis
state (`.nile/<stem>/analysis/transcripts/t1-…`) and returns `{hash}`. Words
are source seconds; the server checks the asset and its probed duration. It
writes analysis state only: no project save, revision bump, history entry or
event. Malformed words are `-32602`; a read-only, corrupt or otherwise failing
store is `-32603` (internal). Additive, so `PROTOCOL_VERSION` stays at 5.

## Jobs

Jobs are **server-owned** and durable: `nile serve` is the only writer. A
submit takes `{base_revision, output, sequence, export}` and first writes a
project snapshot (`.nile/<stem>/jobs/<id>/project.json`) and then the record
(`job.json`), both atomically. The record keeps the snapshot's `revision` and
the chosen `sequence`; the renderer reads only that snapshot, so an edit after
submit cannot change the deliverable or its report. `output` must be absolute
(the server's cwd is its own); a relative path is invalid params, like
`annotation.check`.

The record is the wire DTO (`nle_core::jobs::JobRecord`):

```text
{version, id, state, revision, sequence, output, export,
 created_at, started_at?, finished_at?, progress: {done, total},
 report?, error?, snapshot}
```

`state` is `queued | running | done | failed | cancelled | interrupted`;
`export` is `{preset: "preview"|"balanced"|"max", crf?, encoder_preset?}` —
deliberately narrow. Engine, bitrate, chunk workers and the chunk cache are
deferred: a job renders one cancellable graph, so a parallel knob would be a
promise the queue cannot keep.

One export runs at a time in submission (id) order. `queued` survives a
restart; a `running` job is marked `interrupted` at startup and never silently
resumed. `job.cancel` drops a queued job, or sets the flag the running
export's ffmpeg poll loop checks (the child is killed and the hidden temp
output removed). It returns the record as of the call: a running job may still
read `running` until its child is reaped and the terminal event fires. An
unknown id or a terminal job is invalid params (`-32602`), never a silent
no-op. The render writes to a hidden temp beside `output` and renames only on
success; a cancelled job leaves no half-written deliverable.

`job.submit` is compare-and-swap: a stale `base_revision` is `-32001`. It is
not a model write — the project revision, history and playhead do not move —
but its snapshot is project data, so the read-only latch gates it like a
model write: after a save failure or a handler panic the submit is refused
rather than rendering a half-applied state. `job.list` returns `{revision:
<live>, jobs}` (the revision is context, not a CAS token); `job.get`/
`job.cancel` return the same record shape as submit.
`job.remove {id}` deletes a **terminal** job's `job.json` and snapshot and
returns the removed record; a queued or running job is invalid params (cancel
it first), as is an unknown id.

`job.prune {keep, older_than_days?}` is retention across jobs and returns
`{removed: [id…], failed?: [{id, error}…]}` (removed oldest first). Only
terminal jobs are candidates: the newest `keep` are always retained, and
`older_than_days` further requires the job's finish time (its creation time
if it never started) to be older than `now - days`; ties break on id, which is
submission order. Queued and running jobs are never pruned, so the server and
not the client owns the plan. A directory that cannot be deleted does not
abort the pass: it is reported in `failed` while the rest are removed.
Additive, so `PROTOCOL_VERSION` stays at 5.

Job ids are never recycled: `.nile/<stem>/jobs/.next-id` keeps the high-water
mark, because a pruned or removed highest id would otherwise let the next
submit reuse it. Removal advances the marker before the directory disappears,
which also covers a project written before the file existed; an unreadable or
non-positive marker is an error, not a directory-scan rebuild. A server start
also sweeps crash leftovers: a hidden `.name.jobN.ext` temp is deleted only
when it is exactly the `nle_core::jobs::partial_path` of a **terminal** record
(the executor renames or removes it before the terminal write); a
queued/running temp, another record's output, or an unexplained file is left
for a human. The executor and
the sweep share `partial_path`, so their naming cannot drift.

`JobRecord` ignores unknown fields so a record from a newer build still parses
far enough to hit the `version` check; startup logs and skips a corrupt or
newer record and keeps its directory. `JobQueue::start` selects the job's
`sequence` on the snapshot before computing the frame total, and finish reuses
that total for the progress record.

## Time
`from`, `Outcome.playhead`, `SetSpeed.rate`, and event `at` are rational
strings (`"1/2"`, `"30000/1001"`). Action targets (`at`, `to`) stay the
catalog's f64 fields and snap to the frame grid; outcomes and events are
exact.

## Action vocabulary

The nested values reuse the model's serde forms, not a separate wire spelling:

- `kind`: `"Dissolve"` | `"FadeBlack"`; `TrackKind`: `"Video"` | `"Audio"`.
- `effect`: `{"Crop":…}`, `{"Scale":…}`, `{"Offset":…}`, `{"Eq":…}`,
  `{"Lut":…}`.
- `segment`: `{"Clip":…}`, `{"Gap":…}`, `{"Transition":…}`.
- `annotation_add`: `kind` is `{"kind":"note","text":…}` or
  `{"kind":"marker"}`, and `anchor` is `{"on":"source",
  "source":{"of":"asset","id":1},"time":"1/2"}` (or `"of":"group"`) — the
  persisted `Annotation` forms, not a parallel spelling.
- `annotation_add_at`: `{track, at, to?, kind}`; `to` is omitted for a point.
  The core resolves the clips under `at`/`to` to
  `{"on":"timeline","start":{"clip":1,"source":"1/2"},"end":…}`, so a client
  needs no project access to build the anchor. A point is a frame reference
  (half-open); a range end resolves closed at a clip's tail, and a range start
  at a cut belongs to the following clip.
- `target`: `{"at":…}`, `{"clip":…}`, `{"segment":…}`.
- `edge`: `"in"` | `"out"`; `GapLen`/`TransitionLen`: `{"frames":…}`,
  `{"seconds":…}`, `{"percent":…}`.

That is the no-parallel-DTO tradeoff. Normalizing it would change persisted
`nile.json`/history types and needs a project/history version bump.

## Events

`project.changed`, `playhead` and `sequence.changed` are notifications: no
id, no response. `sequence.changed` carries the active id and the current
revision (unchanged by a pure switch, bumped by
`sequence.add`/`derive`). The server
writes a successful response before its events; the client buffers
events seen before a response and `next_event()` reads those after it.
Unknown or malformed notifications are skipped and counted
(`Client::skipped()`); a malformed **response** is fatal. Current events are
request-triggered; an async event source needs a channel/thread.

`job.progress {id, state, progress}` fires while a render advances (throttled
and coalesced per id: a pending update is replaced, not appended). Progress
is advisory and recovered by `job.get`. `job.changed {id, state,
report?}` marks every terminal transition; startup emits it only for the
records **this** run flipped `running` → `interrupted`, never for historical
ones. `report` is the retained sidecar for a `done` job. Both are queued by the
worker and drained after the next successful call, because the
single-threaded transport writes events only with a reply: a client that wants
live progress polls `job.list`/`job.get`. An error reply drops queued events
(the transport contract), so terminal state must always be read from the
durable record.

## Transport

- Requests need an id. Notifications (no id or null) are ignored, not
  answered. JSON-RPC batches are not supported.
- `MAX_LINE` is 64 MiB of payload (excluding the newline); an over-cap line
  ends the connection with `LineTooLong`.
- Timeouts belong to the transport owner, not this crate: the socket uses
  `set_read_timeout`, `--stdio` polls a pump thread. Both keep an idle
  connection open indefinitely, and both give a frame 5 s from its first byte
  to reach a newline — bytes that keep flowing do not extend it
  (`FrameDeadline` in `crates/nle-cli/src/serve.rs`). On expiry a socket
  connection closes; `--stdio` has one connection, so it ends the process
  cleanly (exit 0).
- `deny_unknown_fields` covers call params and nested action fields; a typo is
  invalid-params, not a silent default.
- A null-id error is surfaced as `ClientError::Server`, preserving its code.
- Invalid UTF-8 gets a null-id parse-error response and the loop continues;
  only an over-cap line ends the connection. A peer can only deny itself.

## Serve

`nile serve` loads the project, its history sidecar, and the yank register,
then listens on `.nile/<stem>/session.sock` (or one stdin/stdout connection
with `--stdio`). It is a **single-writer** interface: a `serve.lock` beside
the socket serializes startup, one server owns the socket, a second server
refuses, and multiple client connections are serialized through one handler.
Socket mode binds and takes the lock **before** the handler is built, so a
refused second server cannot recover the job queue or spawn a dispatcher.
`--stdio` has no singleton gate (tests and pipes use separate projects).
A killed owner leaves both files behind; the next start checks the lock PID
and clears a dead owner's state. One-shot CLI edits are still file-based; do
not run them alongside the server; `nile edit` attaches to the socket when it
is live. Foreign-change adoption is not implemented for file writers. The
socket path is `sun_path`-limited (about 108 bytes on Linux), so very deep
project trees fail at bind. Action mutations go through one `apply`, which
owns CAS, the edit, playhead conversion, saving, and events.
`sequence.use`/`sequence.add`, `media.add` and `job.submit` are separate
paths, but they use the same writeability, revision and save/poison helpers.
A failed save, or a handler that panicked while it held the shared lock,
poisons every later project mutation (and `job.submit`); reads reflect the
in-memory model. On a poisoned mutex `ConnectionHandler::handle` recovers the
value, latches the handler read-only **before** `clear_poison`, and drops the
panicked call's queued events before dispatching the new call: the mutex is
usable again, but `ensure_writable` still refuses the write.

The server shares the TUI's media-frame playhead anchor on the base video
track: a media edit keeps the same source frame, falls back to the seam when
the clip disappears, and clamps into the new timeline. `Outcome.playhead` and
`playhead` events report the resulting cursor when it moves; undo/redo use
their restored position, and track/pool changes leave it clamped. Recorded
history uses `Actor::Agent("nile-serve")`.

A save failure poisons mutations: the failing call gets `internal`
(`save failed: …`) and every later mutation is refused with
`internal error: server is read-only after a failure: {reason}`; reads
continue. A handler panic gets no reply at all — the connection drops with the
panic — but it latches the same refusal for the next connection.
`timeline.edl` returns the `edl://` URL that `nile edl` prints, and its
`track` is 0-based while the CLI flag is 1-based. `stdout` is protocol-only:
no `say!`/`println!`; logs go to stderr or `NLE_LOG`.

## Tests

`crates/nle-protocol/tests/wire.rs` covers the envelopes, every call/event,
ordering, error codes, notifications, omitted params, and failed-call events.
The line cap is unit-tested in `transport.rs`
(`the_line_cap_accepts_the_limit_and_rejects_one_more`). `wire.rs` also pins
the golden `sequence.*` and `media.add` requests and the `sequence.changed`
event.
`crates/nle-core/src/action/tests/wire.rs` covers every action slot.
