//! Server-pushed events.
//!
//! Events are JSON-RPC notifications (`{"jsonrpc":"2.0","method":…,
//! "params":…}`), but they carry no request id and expect no response. Only
//! events with a defined domain type live here; a future job/annotation
//! event arrives with its type rather than as an untyped placeholder.

use nle_core::Seconds;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::rpc::{Notification, RpcError};

/// Event method names. Keep in sync with the variants below.
pub const EVENTS: [&str; 2] = ["project.changed", "playhead"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Event {
    /// The model advanced (an edit, undo/redo, or another client's write).
    #[serde(rename = "project.changed")]
    ProjectChanged { revision: u64 },
    /// The shared playhead moved. A rational string keeps the exact frame
    /// position; floats only appear at the ffmpeg boundary.
    #[serde(rename = "playhead")]
    Playhead {
        #[serde(with = "nle_core::time::rational_ser")]
        at: Seconds,
    },
}

impl Event {
    pub fn method(&self) -> &'static str {
        match self {
            Event::ProjectChanged { .. } => "project.changed",
            Event::Playhead { .. } => "playhead",
        }
    }

    /// The wire notification. Written by hand so the two known events never
    /// depend on a fallible serialization step.
    pub fn notification(&self) -> Notification {
        let params = match self {
            Event::ProjectChanged { revision } => json!({ "revision": revision }),
            Event::Playhead { at } => json!({ "at": format!("{}/{}", at.numer(), at.denom()) }),
        };
        Notification::new(self.method(), params)
    }

    /// Decode a notification from a peer. An unknown event name is
    /// method-not-found; a known one with bad params is invalid-params.
    pub fn from_notification(notification: Notification) -> Result<Self, RpcError> {
        if !EVENTS.contains(&notification.method.as_str()) {
            return Err(RpcError::method_not_found(&notification.method));
        }
        let encoded = if notification.params.is_null() {
            json!({ "method": notification.method })
        } else {
            json!({ "method": notification.method, "params": notification.params })
        };
        serde_json::from_value::<Event>(encoded)
            .map_err(|e| RpcError::invalid_params(e.to_string()))
    }
}
