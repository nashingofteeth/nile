//! The typed call catalog and its results.
//!
//! A [`Call`] is the `method` + `params` half of a request; the transport
//! adds `jsonrpc` and `id`. Decoding is explicit so an unknown method is a
//! *method-not-found* error and a known method with a bad field is
//! *invalid-params* — the two failures a client needs to tell apart.

use nle_core::{Action, Project};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::rpc::RpcError;

/// Every method this build serves. Keep in sync with [`Call::method`] and
/// the variants below; `every_call_has_a_known_method` guards the list.
pub const METHODS: [&str; 6] = [
    "project.show",
    "timeline.edl",
    "action.preview",
    "action.apply",
    "undo",
    "redo",
];

/// A typed request. `base_revision` is the compare-and-swap token: the
/// handler refuses a mutation if the live document has moved on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Call {
    /// The whole project document (the same JSON `nile show --json` prints).
    #[serde(rename = "project.show")]
    ProjectShow,
    /// The composite monitor EDL, or one track's when `track` is set.
    #[serde(rename = "timeline.edl")]
    TimelineEdl {
        #[serde(default)]
        track: Option<usize>,
    },
    /// Run actions against a copy and report what would change.
    #[serde(rename = "action.preview")]
    ActionPreview {
        base_revision: u64,
        actions: Vec<Action>,
    },
    /// Run actions as one undo step.
    #[serde(rename = "action.apply")]
    ActionApply {
        base_revision: u64,
        actions: Vec<Action>,
    },
    #[serde(rename = "undo")]
    Undo { base_revision: u64 },
    #[serde(rename = "redo")]
    Redo { base_revision: u64 },
}

impl Call {
    pub fn method(&self) -> &'static str {
        match self {
            Call::ProjectShow => "project.show",
            Call::TimelineEdl { .. } => "timeline.edl",
            Call::ActionPreview { .. } => "action.preview",
            Call::ActionApply { .. } => "action.apply",
            Call::Undo { .. } => "undo",
            Call::Redo { .. } => "redo",
        }
    }

    /// The `params` half of the request. A null result means a unit call
    /// (`project.show`), whose params are omitted on the wire.
    ///
    /// Fails only if an action carries a non-finite float, which
    /// `serde_json` cannot represent — the same boundary the project file
    /// already rejects.
    pub fn params(&self) -> Result<Value, serde_json::Error> {
        let mut encoded = serde_json::to_value(self)?;
        Ok(match encoded.as_object_mut() {
            Some(object) => object.remove("params").unwrap_or(Value::Null),
            None => Value::Null,
        })
    }

    /// Decode a wire method + params into the typed call. This is the one
    /// place that knows the method catalog, so a rename is deliberate.
    pub fn decode(method: &str, params: Value) -> Result<Self, RpcError> {
        if !METHODS.contains(&method) {
            return Err(RpcError::method_not_found(method));
        }
        // A unit method tolerates an omitted, null, or empty params object;
        // anything else is passed through so serde can reject it as
        // invalid params rather than silently ignoring a typo.
        let omit = params.is_null()
            || (method == "project.show" && params.as_object().is_some_and(|o| o.is_empty()));
        let encoded = if omit {
            serde_json::json!({ "method": method })
        } else {
            serde_json::json!({ "method": method, "params": params })
        };
        serde_json::from_value(encoded).map_err(|e| RpcError::invalid_params(e.to_string()))
    }
}

/// What a successful call returns.
///
/// The variants are deliberately few: `Outcome` is shared by preview, apply,
/// undo and redo because they all report the same shape, and the server
/// checks that the kind fits the method before writing it.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Project(Project),
    Edl(String),
    Outcome(Outcome),
}

impl Reply {
    pub fn kind(&self) -> &'static str {
        match self {
            Reply::Project(_) => "project",
            Reply::Edl(_) => "edl",
            Reply::Outcome(_) => "outcome",
        }
    }

    /// Serialize just the `result` payload, refusing a handler that returned
    /// the wrong kind for `call` (a server bug, reported as internal).
    pub fn encode(self, call: &Call) -> Result<Value, RpcError> {
        match (self, call) {
            (Reply::Project(project), Call::ProjectShow) => serde_json::to_value(project)
                .map_err(|e| RpcError::internal(format!("project is not serializable: {e}"))),
            (Reply::Edl(text), Call::TimelineEdl { .. }) => Ok(Value::String(text)),
            (
                Reply::Outcome(outcome),
                Call::ActionPreview { .. }
                | Call::ActionApply { .. }
                | Call::Undo { .. }
                | Call::Redo { .. },
            ) => serde_json::to_value(outcome)
                .map_err(|e| RpcError::internal(format!("outcome is not serializable: {e}"))),
            (reply, call) => Err(RpcError::internal(format!(
                "handler returned a {} reply for {}",
                reply.kind(),
                call.method()
            ))),
        }
    }

    /// Decode a `result` payload for `call`. The method decides the shape,
    /// so a client never has to guess what a bare JSON value means.
    pub fn decode(call: &Call, result: Value) -> Result<Self, serde_json::Error> {
        match call {
            Call::ProjectShow => serde_json::from_value(result).map(Reply::Project),
            Call::TimelineEdl { .. } => serde_json::from_value(result).map(Reply::Edl),
            Call::ActionPreview { .. }
            | Call::ActionApply { .. }
            | Call::Undo { .. }
            | Call::Redo { .. } => serde_json::from_value(result).map(Reply::Outcome),
        }
    }
}

/// The common result of a mutating (or previewing) call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    /// The document revision after the call. A preview reports the current
    /// revision: it did not change anything.
    pub revision: u64,
    /// The notice the action would show in the TUI message line.
    pub desc: String,
    /// Whether the model changed. `false` for a yank or a refused no-op.
    pub changed: bool,
    /// Frame the playhead should land on, when the action specified one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playhead: Option<i64>,
}
