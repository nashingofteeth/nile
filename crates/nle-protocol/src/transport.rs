//! Blocking NDJSON transport.
//!
//! There is no async runtime and no socket code here: a [`Client`] owns a
//! reader and a writer and does one request at a time; a [`Server`] reads
//! one line, dispatches it, writes the response, then writes any events the
//! handler produced. That is enough for a stdio or Unix-socket front-end to
//! share a [`Document`] without a second scheduler.
//!
//! Timeouts are not imposed here. The owner of a socket sets
//! `set_read_timeout` before wrapping it in a `BufReader`; a stdio peer waits
//! until it closes.
//!
//! [`Document`]: nle_core::Document

use std::collections::VecDeque;
use std::io::{BufRead, Write};

use nle_core::{Action, Project, Seconds};
use serde_json::Value;

use crate::call::{Call, Hello, Outcome, Reply};
use crate::error::{ClientError, ServerError};
use crate::event::Event;
use crate::rpc::{Id, Notification, Request, Response, RpcError};

/// The largest accepted NDJSON payload, excluding the trailing newline.
///
/// `project.show` is the largest message; 64 MiB leaves room for a big media
/// pool while still bounding a hostile peer's allocation.
pub const MAX_LINE: usize = 64 * 1024 * 1024;

/// The document operations a server exposes. Implementations live outside
/// this crate (`nile serve`), so the wire crate stays free of project I/O.
pub trait Handler {
    fn handle(&mut self, call: &Call) -> Result<Reply, RpcError>;

    /// Events produced while handling the last **successful** call. The
    /// server writes them after the response; an error reply discards them.
    /// The default is none.
    fn take_events(&mut self) -> Vec<Event> {
        Vec::new()
    }
}

/// Borrowed handlers compose: a caller can keep the real handler and hand
/// `&mut` to a server (and get it back when the server is dropped).
impl<T: Handler + ?Sized> Handler for &mut T {
    fn handle(&mut self, call: &Call) -> Result<Reply, RpcError> {
        (**self).handle(call)
    }

    fn take_events(&mut self) -> Vec<Event> {
        (**self).take_events()
    }
}

/// A blocking request/response client. Request ids start at 1 and increase.
///
/// Events are buffered when they arrive before a response; call
/// [`Client::next_event`] to consume them, including events the server writes
/// after the response.
pub struct Client<R, W> {
    reader: R,
    writer: W,
    next_id: u64,
    events: VecDeque<Event>,
    skipped: usize,
}

impl<R, W> Client<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            next_id: 1,
            events: VecDeque::new(),
            skipped: 0,
        }
    }
}

impl<R: BufRead, W: Write> Client<R, W> {
    /// Write one request, flush, read until its response, and decode the
    /// result for `call`. Server events seen on the way are buffered.
    ///
    /// A server error is returned as [`ClientError::Server`] with its code
    /// intact.
    pub fn call(&mut self, call: Call) -> Result<Reply, ClientError> {
        let params = call.params()?;
        let id = Id::from(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        let request = Request::new(id.clone(), call.method(), params);

        let line = serde_json::to_string(&request)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;

        loop {
            let Some(value) = self.read_line()? else {
                return Err(ClientError::Eof);
            };
            match decode_event(&value) {
                Ok(Some(event)) => {
                    self.events.push_back(event);
                    continue;
                }
                // A newer server may send an event this build does not know.
                // Skip it: abandoning the pending response would desync every
                // later call by one reply.
                Err(_) => {
                    self.skipped += 1;
                    continue;
                }
                Ok(None) => {}
            }
            let response: Response =
                serde_json::from_value(value).map_err(|e| ClientError::Decode(e.to_string()))?;
            let response = match response {
                // A null-id error is the server's own parse/invalid-request
                // path; there is no response to match, so surface the error.
                Response::Error { id: None, error } => return Err(ClientError::Server(error)),
                response => response,
            };
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
            return match response {
                Response::Result { result, .. } => {
                    Reply::decode(&call, result).map_err(|e| ClientError::Decode(e.to_string()))
                }
                Response::Error { error, .. } => Err(ClientError::Server(error)),
            };
        }
    }

    /// How many notifications this client skipped because they did not
    /// decode. A non-zero count after `hello` usually means version skew;
    /// skipping keeps the pending response (and the connection) in sync.
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// The next buffered or incoming event. Blocks until an event arrives or
    /// the peer closes (`Ok(None)`); call it from an event loop, not between
    /// unrelated requests on a live connection.
    ///
    /// Undecodable notifications are skipped and counted, like in
    /// [`Client::call`]. A response with no pending call is an error.
    /// The server writes a response before its events, so after a successful
    /// [`Client::call`] this reads the event lines that followed it.
    pub fn next_event(&mut self) -> Result<Option<Event>, ClientError> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
            let Some(value) = self.read_line()? else {
                return Ok(None);
            };
            match decode_event(&value) {
                Ok(Some(event)) => return Ok(Some(event)),
                Err(_) => {
                    self.skipped += 1;
                    continue;
                }
                Ok(None) => {
                    return Err(ClientError::Decode(
                        "response without a pending call".to_string(),
                    ));
                }
            }
        }
    }

    fn read_line(&mut self) -> Result<Option<Value>, ClientError> {
        loop {
            let mut line = String::new();
            match read_capped(&mut self.reader, &mut line, MAX_LINE) {
                Ok(0) => return Ok(None),
                Ok(_) => {}
                Err(LineError::TooLong) => return Err(ClientError::LineTooLong),
                Err(LineError::InvalidUtf8) => {
                    return Err(ClientError::Decode("line is not valid UTF-8".to_string()));
                }
                Err(LineError::Io(e)) => return Err(ClientError::Io(e)),
            }
            if line.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(&line)
                .map(Some)
                .map_err(|e| ClientError::Decode(e.to_string()));
        }
    }

    pub fn show(&mut self) -> Result<Project, ClientError> {
        match self.call(Call::ProjectShow)? {
            Reply::Project(project) => Ok(project),
            other => Err(ClientError::UnexpectedReply(other.kind())),
        }
    }

    pub fn hello(&mut self) -> Result<Hello, ClientError> {
        match self.call(Call::Hello)? {
            Reply::Hello(hello) => Ok(hello),
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
        action: Action,
        from: Option<Seconds>,
    ) -> Result<Outcome, ClientError> {
        outcome(self.call(Call::ActionPreview {
            base_revision,
            action,
            from,
        }))
    }

    pub fn apply(
        &mut self,
        base_revision: u64,
        action: Action,
        from: Option<Seconds>,
    ) -> Result<Outcome, ClientError> {
        outcome(self.call(Call::ActionApply {
            base_revision,
            action,
            from,
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

fn is_notification(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.contains_key("method") && matches!(object.get("id"), None | Some(Value::Null))
}

/// Decode a notification, or return `None` when the value is not one.
fn decode_event(value: &Value) -> Result<Option<Event>, ClientError> {
    if !is_notification(value) {
        return Ok(None);
    }
    let notification: Notification = serde_json::from_value(value.clone())
        .map_err(|e| ClientError::Decode(format!("bad event envelope: {e}")))?;
    let event = Event::from_notification(notification)
        .map_err(|e| ClientError::Decode(format!("bad event: {e}")))?;
    Ok(Some(event))
}

/// A single-threaded serve loop. One request is handled at a time, in the
/// order it arrives; the response is flushed before its events so a
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
    /// Serve until EOF.
    ///
    /// A malformed line or a handler error becomes an error *response*; only
    /// a broken read/write or an over-cap line ends the loop. A successful
    /// reply is written before its events; a failed call writes only its
    /// error. A JSON-RPC notification (no id) is ignored, as the spec
    /// requires; batches are not supported.
    pub fn serve(&mut self) -> Result<(), ServerError> {
        let mut line = String::new();
        loop {
            match read_capped(&mut self.reader, &mut line, MAX_LINE) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(LineError::TooLong) => return Err(ServerError::LineTooLong),
                Err(LineError::InvalidUtf8) => {
                    let response = Response::Error {
                        id: None,
                        error: RpcError::parse_error("line is not valid UTF-8"),
                    };
                    self.write_value(&response)?;
                    continue;
                }
                Err(LineError::Io(e)) => return Err(ServerError::Io(e)),
            }
            if line.trim().is_empty() {
                continue;
            }
            let response = self.handle_line(&line);
            let success = matches!(&response, Some(Response::Result { .. }));
            // Drain on every path so stale events cannot leak into a later
            // reply. Events belong to a successful call: an error response
            // (or an ignored notification) drops them.
            let events = self.handler.take_events();
            if let Some(response) = response {
                self.write_value(&response)?;
                if success {
                    for event in events {
                        let notification = event.notification().map_err(ServerError::Encode)?;
                        self.write_value(&notification)?;
                    }
                }
            }
        }
    }

    /// Handle one wire line without any I/O, returning `None` for a
    /// notification (which must not be answered). Public for embedding (a
    /// future multiplexer) and for deterministic tests. Events are drained by
    /// [`Server::serve`], not here.
    pub fn handle_line(&mut self, line: &str) -> Option<Response> {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(e) => {
                return Some(Response::Error {
                    id: None,
                    error: RpcError::parse_error(e.to_string()),
                });
            }
        };
        if is_notification(&value) {
            return None;
        }
        // The id is echoed even when the rest of the request is invalid, so
        // the client can match the failure to its call.
        let id = value
            .get("id")
            .and_then(|value| serde_json::from_value::<Id>(value.clone()).ok());
        let request: Request = match serde_json::from_value(value) {
            Ok(request) => request,
            Err(e) => {
                return Some(Response::Error {
                    id,
                    error: RpcError::invalid_request(e.to_string()),
                });
            }
        };
        let call = match Call::decode(&request.method, request.params) {
            Ok(call) => call,
            Err(error) => {
                return Some(Response::Error {
                    id: Some(request.id),
                    error,
                });
            }
        };
        match self.handler.handle(&call) {
            Ok(reply) => match reply.encode(&call) {
                Ok(result) => Some(Response::Result {
                    id: request.id,
                    result,
                }),
                Err(error) => Some(Response::Error {
                    id: Some(request.id),
                    error,
                }),
            },
            Err(error) => Some(Response::Error {
                id: Some(request.id),
                error,
            }),
        }
    }

    fn write_value<T: serde::Serialize>(&mut self, value: &T) -> Result<(), ServerError> {
        let text = serde_json::to_string(value).map_err(ServerError::Encode)?;
        self.writer.write_all(text.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }
}

enum LineError {
    Io(std::io::Error),
    TooLong,
    InvalidUtf8,
}

/// Read one line with a bounded allocation. `cap` counts the payload, not
/// the trailing newline.
///
/// `BufRead::read_line` itself has no cap, so this fills from `fill_buf` and
/// stops at `cap + 1` bytes. The extra byte distinguishes "exactly at the
/// cap, no newline yet" from "one byte over".
fn read_capped<R: BufRead>(
    reader: &mut R,
    line: &mut String,
    cap: usize,
) -> Result<usize, LineError> {
    line.clear();
    let mut bytes = Vec::new();
    loop {
        let buf = reader.fill_buf().map_err(LineError::Io)?;
        if buf.is_empty() {
            break;
        }
        let newline = buf.iter().position(|&b| b == b'\n');
        let want = newline.map_or(buf.len(), |i| i + 1);
        let take = want.min((cap + 1).saturating_sub(bytes.len()));
        if take == 0 {
            return Err(LineError::TooLong);
        }
        let ended = newline.is_some_and(|i| take == i + 1);
        bytes.extend_from_slice(&buf[..take]);
        reader.consume(take);
        if ended {
            break;
        }
        if bytes.len() > cap {
            return Err(LineError::TooLong);
        }
    }
    if bytes.is_empty() {
        return Ok(0);
    }
    let payload = if bytes.last() == Some(&b'\n') {
        bytes.len() - 1
    } else {
        bytes.len()
    };
    if payload > cap {
        return Err(LineError::TooLong);
    }
    *line = String::from_utf8(bytes).map_err(|_| LineError::InvalidUtf8)?;
    Ok(line.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn the_line_cap_accepts_the_limit_and_rejects_one_more() {
        let mut line = String::new();
        let mut ok = Cursor::new(b"12345678\n".to_vec());
        assert!(read_capped(&mut ok, &mut line, 8).is_ok());
        assert_eq!(line, "12345678\n");

        let mut over = Cursor::new(b"123456789\n".to_vec());
        assert!(matches!(
            read_capped(&mut over, &mut line, 8),
            Err(LineError::TooLong)
        ));

        let mut exact_no_newline = Cursor::new(b"12345678".to_vec());
        assert!(read_capped(&mut exact_no_newline, &mut line, 8).is_ok());
    }

    #[test]
    fn invalid_utf8_is_a_line_error_not_an_io_error() {
        let mut line = String::new();
        let mut bad = Cursor::new(vec![0xff, b'\n']);
        assert!(matches!(
            read_capped(&mut bad, &mut line, 8),
            Err(LineError::InvalidUtf8)
        ));
    }

    #[test]
    fn a_notification_is_a_method_without_an_id() {
        assert!(is_notification(&serde_json::json!({
            "jsonrpc": "2.0", "method": "project.show"
        })));
        assert!(is_notification(&serde_json::json!({
            "jsonrpc": "2.0", "method": "project.show", "id": null
        })));
        assert!(!is_notification(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "project.show"
        })));
    }
}
