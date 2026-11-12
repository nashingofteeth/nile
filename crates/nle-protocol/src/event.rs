//! Server-pushed events.
//!
//! Events are JSON-RPC notifications (`{"jsonrpc":"2.0","method":…,
//! "params":…}`), but they carry no request id and expect no response. Only
//! events with a defined domain type live here; a future job/annotation
//! event arrives with its type rather than as an untyped placeholder.
//!
//! The enum, its wire names, and [`EVENTS`] come from the same
//! `wire_catalog!` list. `notification()` serializes the enum and extracts
//! `params`, so a new field cannot drift from a hand-written params builder.

use nle_core::Seconds;
use serde_json::{Value, json};

use crate::rpc::{Notification, RpcError};

wire_catalog! {
    /// A server-pushed event, written as a JSON-RPC notification.
    pub enum Event, methods: EVENTS {
        /// The model advanced (an edit, undo/redo, or another client's write).
        ProjectChanged { revision: u64 } = "project.changed";
        /// The shared playhead moved. A rational string keeps the exact
        /// position; floats only appear at the ffmpeg boundary.
        Playhead {
            #[serde(with = "nle_core::time::rational_ser")]
            at: Seconds,
        } = "playhead";
    }
}

impl Event {
    /// The wire notification for this event.
    ///
    /// Serialization is fallible in principle, but every current field is a
    /// finite integer or a rational string; the `Result` keeps the library
    /// path panic-free.
    pub fn notification(&self) -> Result<Notification, serde_json::Error> {
        let encoded = serde_json::to_value(self)?;
        let params = encoded.get("params").cloned().unwrap_or(Value::Null);
        Ok(Notification::new(self.method(), params))
    }

    /// Decode a notification from a peer. An unknown event name is
    /// method-not-found; a known one with bad params is invalid-params.
    pub fn from_notification(notification: Notification) -> Result<Self, RpcError> {
        if !EVENTS.contains(&notification.method.as_str()) {
            return Err(RpcError::method_not_found(&notification.method));
        }
        let Notification { method, params, .. } = notification;
        let empty = params.is_null() || params.as_object().is_some_and(|o| o.is_empty());
        let encoded = if params.is_null() {
            json!({ "method": method.as_str() })
        } else {
            json!({ "method": method.as_str(), "params": params })
        };
        match serde_json::from_value(encoded) {
            Ok(event) => Ok(event),
            Err(first) => {
                if empty
                    && let Ok(event) = serde_json::from_value(json!({ "method": method.as_str() }))
                {
                    return Ok(event);
                }
                Err(RpcError::invalid_params(first.to_string()))
            }
        }
    }
}
