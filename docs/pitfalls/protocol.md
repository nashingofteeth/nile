# Protocol wire contract

`nle-protocol` is JSON-RPC 2.0 over NDJSON: one object per line, a blocking
client/server, no async runtime. `nile serve` implements `Handler` on a
`Document`; the crate owns no project I/O, process, or socket.

## Catalog

`hello`, `project.show`, `timeline.edl`, `action.preview`, `action.apply`,
`undo`, `redo`, `sequence.list`, `sequence.use`, `sequence.add`, `media.list`,
`media.add`, `playhead.set`. The internal `wire_catalog!` macro
(`crates/nle-protocol/src/catalog.rs`) is the single source: it generates the
enum, `method()`, and `METHODS`/`EVENTS`. Add a method to the invocation, then
a sample to `all_calls()`; the `METHODS.len()` assertion fails until the sample
exists. `hello` reports `PROTOCOL_VERSION` (bump only for a breaking method,
field or unit change; additive methods stay on the current version and are
listed in `hello.methods`), the `nile.json` version, the model revision, the
active sequence's undo depth and id, and the served methods. `Outcome` and
the flattened `Preview` also carry `depth`.

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
  assets_added: [asset id], assets_removed: [asset id], assets_changed: [asset id]
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
- `target`: `{"at":…}`, `{"clip":…}`, `{"segment":…}`.
- `edge`: `"in"` | `"out"`; `GapLen`/`TransitionLen`: `{"frames":…}`,
  `{"seconds":…}`, `{"percent":…}`.

That is the no-parallel-DTO tradeoff. Normalizing it would change persisted
`nile.json`/history types and needs a project/history version bump.

## Events

`project.changed`, `playhead` and `sequence.changed` are notifications: no
id, no response. `sequence.changed` carries the active id and the current
revision (unchanged by a pure switch, bumped by `sequence.add`). The server
writes a successful response before its events; the client buffers
events seen before a response and `next_event()` reads those after it.
Unknown or malformed notifications are skipped and counted
(`Client::skipped()`); a malformed **response** is fatal. Current events are
request-triggered; an async event source needs a channel/thread.

Events belong to a successful call: an error reply discards any events the
handler queued.

## Transport

- Requests need an id. Notifications (no id or null) are ignored, not
  answered. JSON-RPC batches are not supported.
- `MAX_LINE` is 64 MiB of payload (excluding the newline); an over-cap line
  ends the connection with `LineTooLong`.
- Timeouts belong to the socket owner (`set_read_timeout`), not this crate.
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
A killed owner leaves both files behind; the next start checks the lock PID
and clears a dead owner's state. One-shot CLI edits are still file-based; do
not run them alongside the server; `nile edit` attaches to the socket when it
is live. Foreign-change adoption is not implemented for file writers. The
socket path is `sun_path`-limited (about 108 bytes on
Linux), so very deep project trees fail at bind. Action mutations go through one `apply`, which owns CAS, the
edit, playhead conversion, saving, and events. `sequence.use`/`sequence.add`
and `media.add` are separate model paths, but they use the same writeability,
revision and save/poison helpers. A failed save poisons every later write; reads reflect
the in-memory model.

The server shares the TUI's media-frame playhead anchor on the base video
track: a media edit keeps the same source frame, falls back to the seam when
the clip disappears, and clamps into the new timeline. `Outcome.playhead` and
`playhead` events report the resulting cursor when it moves; undo/redo use
their restored position, and track/pool changes leave it clamped. Recorded
history uses `Actor::Agent("nile-serve")`.

A save failure poisons mutations (the response is `internal`, and later writes
say to restart); reads continue. `timeline.edl` returns the `edl://` URL that
`nile edl` prints, and its `track` is 0-based while the CLI flag is 1-based.
`stdout` is protocol-only: no `say!`/`println!`; logs go to stderr or
`NLE_LOG`.

## Tests

`crates/nle-protocol/tests/wire.rs` covers the envelopes, every call/event,
ordering, error codes, notifications, omitted params, and failed-call events.
The line cap is unit-tested in `transport.rs`
(`the_line_cap_accepts_the_limit_and_rejects_one_more`). `wire.rs` also pins
the golden `sequence.*` and `media.add` requests and the `sequence.changed`
event.
`crates/nle-core/src/action/tests/wire.rs` covers every action slot.
