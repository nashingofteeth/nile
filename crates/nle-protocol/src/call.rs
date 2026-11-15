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

use std::path::PathBuf;

use nle_core::{Action, Project, Seconds, Sequence, VideoSettings};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::rpc::RpcError;

/// The wire schema version, independent of the `nile.json` project version
/// and the crate version. Bump it for a breaking method, field, or unit change
/// or a new capability surface clients may gate on; `hello` reports it, and
/// `hello.methods` remains the fine-grained list.
pub const PROTOCOL_VERSION: u32 = 2;

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
        /// Run one catalog action as one undo step. `Action::Undo`/`Redo` are
        /// accepted here as aliases for the `undo`/`redo` methods; `from` is
        /// ignored for those two (the history restores its own playhead).
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
        /// Every sequence in the project, with the active id.
        SequenceList = "sequence.list";
        /// Switch the active sequence (session state, like the playhead).
        SequenceUse { id: u32 } = "sequence.use";
        /// Add an empty sequence; a model write guarded by CAS.
        SequenceAdd {
            base_revision: u64,
            name: String,
            #[serde(
                default,
                with = "nle_core::time::rational_ser::opt",
                skip_serializing_if = "Option::is_none"
            )]
            fps: Option<Seconds>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            video: Option<VideoSettings>,
        } = "sequence.add";
        /// The media pool, summarized for a client.
        MediaList = "media.list";
        /// Probe and add files to the shared media pool as one undo step.
        MediaAdd {
            base_revision: u64,
            paths: Vec<PathBuf>,
        } = "media.add";
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
        // Struct variants need a content field even when every field is
        // optional, so null/omitted params becomes `{}` for the first try.
        // Unit variants need the content omitted instead, so retry without it.
        let empty = params.is_null() || params.as_object().is_some_and(|o| o.is_empty());
        let encoded = if params.is_null() {
            serde_json::json!({ "method": method, "params": {} })
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
    Sequences(SequenceList),
    Sequence(SequenceReply),
    Media(MediaList),
}

impl Reply {
    pub fn kind(&self) -> &'static str {
        match self {
            Reply::Project(_) => "project",
            Reply::Hello(_) => "hello",
            Reply::Edl(_) => "edl",
            Reply::Outcome(_) => "outcome",
            Reply::Sequences(_) => "sequences",
            Reply::Sequence(_) => "sequence",
            Reply::Media(_) => "media",
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
            (Reply::Sequences(list), Call::SequenceList) => serde_json::to_value(list)
                .map_err(|e| RpcError::internal(format!("sequence list is not serializable: {e}"))),
            (Reply::Sequence(sequence), Call::SequenceUse { .. } | Call::SequenceAdd { .. }) => {
                serde_json::to_value(sequence).map_err(|e| {
                    RpcError::internal(format!("sequence reply is not serializable: {e}"))
                })
            }
            (Reply::Media(media), Call::MediaList | Call::MediaAdd { .. }) => {
                serde_json::to_value(media).map_err(|e| {
                    RpcError::internal(format!("media reply is not serializable: {e}"))
                })
            }
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
            Call::SequenceList => serde_json::from_value(result).map(Reply::Sequences),
            Call::SequenceUse { .. } | Call::SequenceAdd { .. } => {
                serde_json::from_value(result).map(Reply::Sequence)
            }
            Call::MediaList | Call::MediaAdd { .. } => {
                serde_json::from_value(result).map(Reply::Media)
            }
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

/// One sequence as the protocol reports it. `active` lives on the enclosing
/// list/reply: exactly one sequence is the session's edit scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SequenceSummary {
    pub id: u32,
    pub name: String,
    #[serde(
        default,
        with = "nle_core::time::rational_ser::opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub fps: Option<Seconds>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoSettings>,
    #[serde(with = "nle_core::time::rational_ser")]
    pub playhead: Seconds,
    /// Clips across every track, not gaps or transitions.
    pub clips: usize,
}

impl SequenceSummary {
    pub fn of(sequence: &Sequence) -> Self {
        let clips = sequence
            .timeline
            .tracks
            .iter()
            .flat_map(|track| track.segments.iter())
            .filter(|segment| matches!(segment, nle_core::timeline::Segment::Clip(_)))
            .count();
        Self {
            id: sequence.id,
            name: sequence.name.clone(),
            fps: sequence.fps,
            video: sequence.video.clone(),
            playhead: sequence.playhead,
            clips,
        }
    }
}

/// `sequence.list` result: every sequence plus the active id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SequenceList {
    pub revision: u64,
    pub active: u32,
    pub sequences: Vec<SequenceSummary>,
}

impl SequenceList {
    pub fn of(project: &Project) -> Self {
        Self {
            revision: project.revision,
            active: project.active_id(),
            sequences: project.sequences.iter().map(SequenceSummary::of).collect(),
        }
    }
}

/// `sequence.use` / `sequence.add` result: the affected sequence and the
/// active id after the call (an add does not switch; a use does).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SequenceReply {
    pub revision: u64,
    pub active: u32,
    pub sequence: SequenceSummary,
}

/// One pooled asset as the protocol reports it. `used_by` spans every
/// sequence, matching `Project::asset_usage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaAsset {
    pub id: u32,
    pub path: PathBuf,
    #[serde(
        default,
        with = "nle_core::time::rational_ser::opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub duration: Option<Seconds>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(
        default,
        with = "nle_core::time::rational_ser::opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub fps: Option<Seconds>,
    pub audio: bool,
    pub used_by: Vec<u32>,
}

/// The media pool after `media.list` or `media.add`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaList {
    pub revision: u64,
    pub assets: Vec<MediaAsset>,
}
