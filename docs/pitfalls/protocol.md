# Protocol wire contract

`nle-protocol` is JSON-RPC 2.0 over NDJSON: one object per line, a blocking
client/server, no async runtime. `nile serve` implements `Handler` on a
`Document`; the crate owns no project I/O, process, or socket.

## Catalog

`hello`, `project.show`, `timeline.edl`, `action.preview`, `action.apply`,
`undo`, `redo`. The internal `wire_catalog!` macro
(`crates/nle-protocol/src/catalog.rs`) is the single source: it generates the
enum, `method()`, and `METHODS`/`EVENTS`. Add a method to the invocation, then
a sample to `all_calls()`; the `METHODS.len()` assertion fails until the sample
exists. `hello` reports `PROTOCOL_VERSION` (bump on any breaking wire change),
the `nile.json` version, the model revision, and the served methods.

## Actions

`action.preview`/`apply` take one catalog `Action`; one apply is one
`Document::edit` and one undo entry. `base_revision` is compare-and-swap: a
stale value returns `-32001` with `{expected, actual}`. `from` is the optional
rational undo anchor; absent means the shared `Project.playhead`.

The action is the catalog's serde form (`{"action":"split",...}`), not a
parallel DTO. `crates/nle-core/src/action/tests/wire.rs` pins every slot.

## Time

`from`, `Outcome.playhead`, and event `at` are rational strings (`"1/2"`).
Action targets (`at`, `to`) stay the catalog's f64 fields and snap to the
frame grid; outcomes and events are exact.

## Events

`project.changed` and `playhead` are notifications: no id, no response. The
server writes a successful response before its events; the client buffers
events seen before a response and `next_event()` reads those after it.
Current events are request-triggered; an async event source needs a
channel/thread.

## Transport

- Requests need an id. Notifications (no id or null) are ignored, not
  answered. JSON-RPC batches are not supported.
- `MAX_LINE` is 64 MiB; an over-cap line ends the connection with
  `LineTooLong`.
- Timeouts belong to the socket owner (`set_read_timeout`), not this crate.
- `deny_unknown_fields` makes a params typo invalid-params, not a silent
  default.

## Tests

`crates/nle-protocol/tests/wire.rs` covers the envelopes, every call/event,
ordering, error codes, notifications, and the cap.
`crates/nle-core/src/action/tests/wire.rs` covers every action slot.
