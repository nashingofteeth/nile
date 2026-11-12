//! Transport and server failures.
//!
//! A server-side *protocol* error is not a `ServerError`: it is a normal
//! JSON-RPC error response, so the loop keeps serving. These enums cover
//! only the failures that end or break the connection.

use thiserror::Error;

use crate::rpc::RpcError;

/// What a client call can fail with. A server-sent error keeps its typed
/// [`RpcError`] (and thus its code/data) instead of flattening to a string.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("the connection closed before a response arrived")]
    Eof,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not encode a request: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("malformed response: {0}")]
    Decode(String),
    #[error("response id {got} does not match request id {expected}")]
    IdMismatch { expected: String, got: String },
    #[error("the server returned an unexpected {0} reply")]
    UnexpectedReply(&'static str),
    #[error("server error: {0}")]
    Server(#[from] RpcError),
}

/// Failures that end the serve loop. Malformed requests never land here.
#[derive(Debug, Error)]
pub enum ServerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not encode a response: {0}")]
    Encode(#[source] serde_json::Error),
}
