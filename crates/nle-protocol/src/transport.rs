//! Blocking NDJSON transport.
//!
//! There is no async runtime and no socket code here: a [`Client`] owns a
//! reader and a writer and does one request at a time; a [`Server`] reads
//! one line, dispatches it, writes one line, flushes, and repeats. That is
//! enough for a stdio or Unix-socket front-end to share a [`Document`]
//! without a second scheduler.
//!
//! [`Document`]: nle_core::Document

use std::io::{BufRead, Write};

use nle_core::{Action, Project};
use serde_json::Value;

use crate::call::{Call, Outcome, Reply};
use crate::error::{ClientError, ServerError};
use crate::rpc::{Id, Request, Response, RpcError};

/// The document operations a server exposes. Implementations live outside
/// this crate (`nile serve`), so the wire crate stays free of project I/O.
pub trait Handler {
    fn handle(&mut self, call: &Call) -> Result<Reply, RpcError>;
}

/// Borrowed handlers compose: a caller can keep the real handler and hand
/// `&mut` to a server (and get it back when the server is dropped).
impl<T: Handler + ?Sized> Handler for &mut T {
    fn handle(&mut self, call: &Call) -> Result<Reply, RpcError> {
        (**self).handle(call)
    }
}

/// A blocking request/response client. Request ids start at 1 and increase.
pub struct Client<R, W> {
    reader: R,
    writer: W,
    next_id: u64,
}

impl<R, W> Client<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            next_id: 1,
        }
    }
}

impl<R: BufRead, W: Write> Client<R, W> {
    /// Write one request, flush, read one response, and decode the result
    /// for `call`. A server error is returned as [`ClientError::Server`]
    /// with its code intact.
    pub fn call(&mut self, call: Call) -> Result<Reply, ClientError> {
        let params = call.params()?;
        let id = Id::from(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        let request = Request::new(id.clone(), call.method(), params);

        let line = serde_json::to_string(&request)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;

        let mut response_line = String::new();
        if self.reader.read_line(&mut response_line)? == 0 {
            return Err(ClientError::Eof);
        }
        let response: Response =
            serde_json::from_str(&response_line).map_err(|e| ClientError::Decode(e.to_string()))?;
        if response.id() != Some(&id) {
            let got = response
                .id()
                .map(ToString::to_string)
                .unwrap_or_else(|| "null".to_string());
            return Err(ClientError::IdMismatch {
                expected: id.to_string(),
                got,
            });
        }
        match response {
            Response::Result { result, .. } => {
                Reply::decode(&call, result).map_err(|e| ClientError::Decode(e.to_string()))
            }
            Response::Error { error, .. } => Err(ClientError::Server(error)),
        }
    }

    pub fn show(&mut self) -> Result<Project, ClientError> {
        match self.call(Call::ProjectShow)? {
            Reply::Project(project) => Ok(project),
            other => Err(ClientError::UnexpectedReply(other.kind())),
        }
    }

    pub fn edl(&mut self, track: Option<usize>) -> Result<String, ClientError> {
        match self.call(Call::TimelineEdl { track })? {
            Reply::Edl(text) => Ok(text),
            other => Err(ClientError::UnexpectedReply(other.kind())),
        }
    }

    pub fn preview(
        &mut self,
        base_revision: u64,
        actions: Vec<Action>,
    ) -> Result<Outcome, ClientError> {
        outcome(self.call(Call::ActionPreview {
            base_revision,
            actions,
        }))
    }

    pub fn apply(
        &mut self,
        base_revision: u64,
        actions: Vec<Action>,
    ) -> Result<Outcome, ClientError> {
        outcome(self.call(Call::ActionApply {
            base_revision,
            actions,
        }))
    }

    pub fn undo(&mut self, base_revision: u64) -> Result<Outcome, ClientError> {
        outcome(self.call(Call::Undo { base_revision }))
    }

    pub fn redo(&mut self, base_revision: u64) -> Result<Outcome, ClientError> {
        outcome(self.call(Call::Redo { base_revision }))
    }
}

fn outcome(reply: Result<Reply, ClientError>) -> Result<Outcome, ClientError> {
    match reply? {
        Reply::Outcome(outcome) => Ok(outcome),
        other => Err(ClientError::UnexpectedReply(other.kind())),
    }
}

/// A single-threaded serve loop. One request is handled at a time, in the
/// order it arrives; responses are flushed before the next read so a
/// request/response client never deadlocks.
pub struct Server<R, W, H> {
    reader: R,
    writer: W,
    handler: H,
}

impl<R, W, H> Server<R, W, H> {
    pub fn new(reader: R, writer: W, handler: H) -> Self {
        Self {
            reader,
            writer,
            handler,
        }
    }

    pub fn into_inner(self) -> (R, W, H) {
        (self.reader, self.writer, self.handler)
    }
}

impl<R: BufRead, W: Write, H: Handler> Server<R, W, H> {
    /// Serve until EOF. A malformed line or a handler error becomes an error
    /// *response*; only a broken read or write ends the loop.
    pub fn serve(&mut self) -> Result<(), ServerError> {
        let mut line = String::new();
        loop {
            line.clear();
            if self.reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            if line.trim().is_empty() {
                continue;
            }
            let response = self.handle_line(&line);
            let text = serde_json::to_string(&response).map_err(ServerError::Encode)?;
            self.writer.write_all(text.as_bytes())?;
            self.writer.write_all(b"\n")?;
            self.writer.flush()?;
        }
    }

    /// Handle one wire line without any I/O. Public for embedding (a future
    /// multiplexer) and for deterministic tests.
    pub fn handle_line(&mut self, line: &str) -> Response {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(e) => {
                return Response::Error {
                    id: None,
                    error: RpcError::parse_error(e.to_string()),
                };
            }
        };
        // The id is echoed even when the rest of the request is invalid, so
        // the client can match the failure to its call.
        let id = value
            .get("id")
            .and_then(|value| serde_json::from_value::<Id>(value.clone()).ok());
        let request: Request = match serde_json::from_value(value) {
            Ok(request) => request,
            Err(e) => {
                return Response::Error {
                    id,
                    error: RpcError::invalid_request(e.to_string()),
                };
            }
        };
        let call = match Call::decode(&request.method, request.params) {
            Ok(call) => call,
            Err(error) => {
                return Response::Error {
                    id: Some(request.id),
                    error,
                };
            }
        };
        match self.handler.handle(&call) {
            Ok(reply) => match reply.encode(&call) {
                Ok(result) => Response::Result {
                    id: request.id,
                    result,
                },
                Err(error) => Response::Error {
                    id: Some(request.id),
                    error,
                },
            },
            Err(error) => Response::Error {
                id: Some(request.id),
                error,
            },
        }
    }
}
