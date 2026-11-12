//! The typed call catalog and its results.
//!
//! A [`Call`] is the `method` + `params` half of a request; the transport
//! adds `jsonrpc` and `id`. The enum, its wire names, and [`METHODS`] are
//! generated from one list by the internal `wire_catalog!` macro, so a new
//! method cannot leave a stale name table behind.
//!
//! Decoding is explicit so an unknown method is a *method-not-found* error
//! and a known method with a bad field is *invalid-params* — the two
//! failures a client needs to tell apart.

use nle_core::{Action, Project, Seconds};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::rpc::RpcError;

/// The wire schema version, independent of the `nile.json` project version
/// and the crate version. Bump it for any breaking method, field, or unit
/// change; `hello` lets a client fail clearly instead of guessing.
pub const PROTOCOL_VERSION: u32 = 1;

wire_catalog! {
    /// A typed request. `base_revision` is the compare-and-swap token: the
    /// handler refuses a mutation if the live document has moved on.
    pub enum Call, methods: METHODS {
        /// The whole project document (the same JSON `nile show --json` prints).
        ProjectShow = "project.show";
        /// Protocol and project metadata, plus the served method names.
        Hello = "hello";
        /// The composite monitor EDL, or one track's (0-based) when `track` is set.
        TimelineEdl {
            #[serde(default)]
            track: Option<usize>,
        } = "timeline.edl";
        /// Run one action against a copy and report what would change.
        ActionPreview {
            base_revision: u64,
            action: Action,
            /// The undo anchor for the hypothetical edit; absent means the
            /// document's shared playhead.
            #[serde(
                default,
                with = "nle_core::time::rational_ser::opt",
                skip_serializing_if = "Option::is_none"
            )]
            from: Option<Seconds>,
        } = "action.preview";
        /// Run one catalog action as one undo step.
        ActionApply {
            base_revision: u64,
            action: Action,
            /// The undo anchor; absent means the document's shared playhead.
            #[serde(
                default,
                with = "nle_core::time::rational_ser::opt",
                skip_serializing_if = "Option::is_none"
            )]
            from: Option<Seconds>,
        } = "action.apply";
        Undo { base_revision: u64 } = "undo";
        Redo { base_revision: u64 } = "redo";
    }
}

impl Call {
    /// The `params` half of the request. A null result means a unit call
    /// (`project.show`, `hello`), whose params are omitted on the wire.
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

    /// Decode a wire method + params into the typed call. [`METHODS`] is
    /// generated from the same list as the enum, so this is the one place
    /// that knows the catalog's string names.
    pub fn decode(method: &str, params: Value) -> Result<Self, RpcError> {
        if !METHODS.contains(&method) {
            return Err(RpcError::method_not_found(method));
        }
        // Unit calls accept an omitted, null, or empty params object. Try the
        // full decode first so a future all-optional struct call is not
        // mistaken for a unit variant; only fall back to the unit spelling
        // for null/empty params.
        let empty = params.is_null() || params.as_object().is_some_and(|o| o.is_empty());
        let encoded = if params.is_null() {
            serde_json::json!({ "method": method })
        } else {
            serde_json::json!({ "method": method, "params": params })
        };
        match serde_json::from_value(encoded) {
            Ok(call) => Ok(call),
            Err(first) => {
                if empty
                    && let Ok(call) =
                        serde_json::from_value(serde_json::json!({ "method": method }))
                {
                    return Ok(call);
                }
                Err(RpcError::invalid_params(first.to_string()))
            }
        }
    }
}

/// Protocol and build metadata returned by `hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    /// [`PROTOCOL_VERSION`] of the server.
    pub protocol: u32,
    /// The `nile.json` version this build reads.
    pub project_version: u32,
    /// The document's current model revision.
    pub revision: u64,
    /// Every method this server serves, from the generated [`METHODS`].
    pub methods: Vec<String>,
}

impl Hello {
    pub fn new(revision: u64) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            project_version: nle_core::project::PROJECT_VERSION,
            revision,
            methods: METHODS.iter().map(|m| (*m).to_string()).collect(),
        }
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
    Hello(Hello),
    Edl(String),
    Outcome(Outcome),
}

impl Reply {
    pub fn kind(&self) -> &'static str {
        match self {
            Reply::Project(_) => "project",
            Reply::Hello(_) => "hello",
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
            (Reply::Hello(hello), Call::Hello) => serde_json::to_value(hello)
                .map_err(|e| RpcError::internal(format!("hello is not serializable: {e}"))),
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
            Call::Hello => serde_json::from_value(result).map(Reply::Hello),
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
    /// Exact timeline position the playhead should land on, when the action
    /// specified one. Rational seconds, like `playhead` events — one unit on
    /// the wire.
    #[serde(
        default,
        with = "nle_core::time::rational_ser::opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub playhead: Option<Seconds>,
}
