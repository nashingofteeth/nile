//! nile's wire protocol: typed JSON-RPC 2.0 calls over NDJSON.
//!
//! This crate is deliberately thin. It owns envelopes, the typed call
//! catalog, event types, and a blocking client/server — not a project, a
//! process, or a socket. `nile serve` (outside this crate) implements
//! [`Handler`] on a [`Document`] and chooses the transport; the TUI stays
//! on direct method calls for now.
//!
//! ```text
//! {"jsonrpc":"2.0","id":1,"method":"action.apply","params":{"base_revision":7,"actions":[{"action":"split","track":0,"at":1.5}]}}
//! {"jsonrpc":"2.0","id":1,"result":{"revision":8,"desc":"split clip #1 at f45","changed":true}}
//! ```
//!
//! Two boundaries carry the type safety:
//!
//! - [`Call::decode`] turns a method + `Value` into the action catalog's
//!   typed enum, so a malformed field cannot reach the editor.
//! - [`Response`] is either a result or an error; the decoder rejects a
//!   line with both (or neither), so the transport never inspects a
//!   half-valid envelope.
//!
//! [`Document`]: nle_core::Document

pub mod call;
pub mod error;
pub mod event;
pub mod rpc;
pub mod transport;

pub use call::{Call, METHODS, Outcome, Reply};
pub use error::{ClientError, ServerError};
pub use event::{EVENTS, Event};
pub use rpc::{
    INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, Id, METHOD_NOT_FOUND, Notification,
    PARSE_ERROR, REVISION_CONFLICT, Request, Response, RevisionConflict, RpcError, Version,
};
pub use transport::{Client, Handler, Server};
