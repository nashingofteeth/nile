# Protocol wire contract

`nle-protocol` is JSON-RPC 2.0 over NDJSON: one object per line, a blocking
client/server, no async runtime. `nile serve` implements `Handler` on a
`Document`; the crate owns no project I/O, process, or socket.

## Catalog

`hello`, `project.show`, `timeline.edl`, `action.preview`, `action.apply`,
`undo`, `redo`, `sequence.list`, `sequence.use`, `sequence.add`. The internal
`wire_catalog!` macro
(`crates/nle-protocol/src/catalog.rs`) is the single source: it generates the
enum, `method()`, and `METHODS`/`EVENTS`. Add a method to the invocation, then
a sample to `all_calls()`; the `METHODS.len()` assertion fails until the sample
exists. `hello` reports `PROTOCOL_VERSION` (bump on any breaking wire change),
the `nile.json` version, the model revision, and the served methods.

## Actions

`action.preview`/`apply` take one catalog `Action`. Timeline-mutating actions
produce one undo entry; `Yank`/`YankRange` only change the register, and
`Action::Undo`/`Redo` are the undo operations themselves. `base_revision` is
compare-and-swap: a stale value returns `-32001` with `{expected, actual}`.

`from` is the optional rational undo anchor; absent means the shared
`Project.playhead`. It is an **input**: the returned `Outcome.playhead` is the
edit's own result, converted from `nle_core::Outcome` frames with
`frames_to_seconds` (a blend midpoint, the moved edge), never an echo of
`from`.

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

`project.changed` and `playhead` are notifications: no id, no response. The
server writes a successful response before its events; the client buffers
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

`nile serve --stdio` loads the project, its history sidecar, and the yank
register, then serves one connection. It is a **single-writer** interface:
close the TUI and other edits while it runs; foreign-change adoption is not
implemented yet. Mutations go through one `apply`, which owns CAS, the edit,
playhead conversion, saving, and events.

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
the golden `sequence.*` request and `sequence.changed` event.
`crates/nle-core/src/action/tests/wire.rs` covers every action slot.
