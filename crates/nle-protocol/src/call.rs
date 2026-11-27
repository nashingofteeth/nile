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

use nle_core::analysis::Word;
use nle_core::jobs::{JobExport, JobRecord};
use nle_core::{
    Action, Annotation, AudioSettings, CheckReport, Project, ProjectDiff, Seconds, Sequence,
    VideoSettings,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::rpc::RpcError;

/// The wire schema version, independent of the `nile.json` project version
/// and the crate version. Bump it for a breaking method, field, or unit
/// change. Additive methods and events stay on the current version;
/// `hello.methods` is the fine-grained capability list.
pub const PROTOCOL_VERSION: u32 = 5;

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
        /// Run actions against a copy and report what would change. They
        /// form one undo step but preview writes nothing.
        ActionPreview {
            base_revision: u64,
            actions: Vec<Action>,
            /// The undo anchor for the hypothetical edit; absent means the
            /// document's shared playhead.
            #[serde(
                default,
                with = "nle_core::time::rational_ser::opt",
                skip_serializing_if = "Option::is_none"
            )]
            from: Option<Seconds>,
        } = "action.preview";
        /// Run catalog actions as one undo step. A single-element list keeps
        /// the normal action path and `Action::Undo`/`Redo` aliases; a
        /// multi-action batch rejects undo/redo.
        ActionApply {
            base_revision: u64,
            actions: Vec<Action>,
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
        /// Copy a sequence snapshot into a new sequence (not active). The
        /// source and copy share the media pool but not timelines/histories.
        SequenceDerive {
            base_revision: u64,
            from: u32,
            name: String,
            #[serde(
                default,
                with = "nle_core::time::rational_ser::opt",
                skip_serializing_if = "Option::is_none"
            )]
            fps: Option<Seconds>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            video: Option<VideoSettings>,
        } = "sequence.derive";
        /// Set the active sequence's audio-post settings; a model write
        /// guarded by CAS.
        SequenceAudio {
            base_revision: u64,
            audio: AudioSettings,
        } = "sequence.audio";
        /// The media pool, summarized for a client.
        MediaList = "media.list";
        /// Probe and add files to the shared media pool as one undo step.
        MediaAdd {
            base_revision: u64,
            paths: Vec<PathBuf>,
        } = "media.add";
        /// Move the shared playhead (session state, like sequence.use).
        PlayheadSet {
            #[serde(with = "nle_core::time::rational_ser")]
            at: Seconds,
        } = "playhead.set";
        /// Notes and markers for one sequence (default: the active one).
        AnnotationList {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            sequence: Option<u32>,
        } = "annotation.list";
        /// Check a render report against the expectations on its sequence.
        /// A read: it evaluates the sidecar and changes nothing.
        AnnotationCheck {
            /// The exported video or its `.report.json` sidecar.
            report: PathBuf,
            #[serde(default)]
            sequence: Option<u32>,
            /// Annotation ids to check; empty means every expectation.
            #[serde(default)]
            id: Vec<u32>,
        } = "annotation.check";
        /// Store a transcript for an asset in the project's analysis state.
        /// Not a project write: the revision and history are untouched.
        AnalysisAdd {
            asset: u32,
            words: Vec<Word>,
        } = "analysis.add";
        /// Queue a durable export. The server snapshots the project at
        /// `base_revision` and renders that snapshot; the live document may
        /// move on. `output` must be absolute (the server's cwd is its own).
        JobSubmit {
            base_revision: u64,
            output: PathBuf,
            /// The sequence the snapshot renders.
            sequence: u32,
            #[serde(default)]
            export: JobExport,
        } = "job.submit";
        /// Every durable job, oldest first, plus the live project revision.
        JobList = "job.list";
        /// One job by id.
        JobGet { id: u64 } = "job.get";
        /// Cancel a queued job, or kill the running export. A terminal job is
        /// invalid params.
        JobCancel { id: u64 } = "job.cancel";
        /// Delete a **terminal** job's record and snapshot. Queued/running
        /// jobs are refused; the removed record is returned.
        JobRemove { id: u64 } = "job.remove";
        /// Retention: delete terminal jobs beyond the newest `keep` and, when
        /// `older_than_days` is set, older than that window. Queued/running
        /// jobs are never candidates. Returns the removed ids, oldest first.
        JobPrune {
            /// How many of the newest terminal jobs to retain.
            keep: u64,
            /// Also require a job's finish/creation time to be older than
            /// this many 86 400-second days.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            older_than_days: Option<u64>,
        } = "job.prune";
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
    /// The active sequence's undo depth.
    pub depth: u64,
    /// The active sequence id.
    pub active: u32,
    /// Every method this server serves, from the generated [`METHODS`].
    pub methods: Vec<String>,
}

impl Hello {
    pub fn new(revision: u64, depth: u64, active: u32) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            project_version: nle_core::project::PROJECT_VERSION,
            revision,
            depth,
            active,
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
    Annotations(AnnotationList),
    Checks(CheckReport),
    Analysis(AnalysisHash),
    Preview(Preview),
    Playhead(PlayheadReply),
    Job(JobRecord),
    Jobs(JobList),
    Pruned(JobPruneReply),
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
            Reply::Annotations(_) => "annotations",
            Reply::Checks(_) => "checks",
            Reply::Analysis(_) => "analysis",
            Reply::Preview(_) => "preview",
            Reply::Playhead(_) => "playhead",
            Reply::Job(_) => "job",
            Reply::Jobs(_) => "jobs",
            Reply::Pruned(_) => "pruned",
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
            (
                Reply::Sequence(sequence),
                Call::SequenceUse { .. }
                | Call::SequenceAdd { .. }
                | Call::SequenceDerive { .. }
                | Call::SequenceAudio { .. },
            ) => serde_json::to_value(sequence).map_err(|e| {
                RpcError::internal(format!("sequence reply is not serializable: {e}"))
            }),
            (Reply::Media(media), Call::MediaList | Call::MediaAdd { .. }) => {
                serde_json::to_value(media).map_err(|e| {
                    RpcError::internal(format!("media reply is not serializable: {e}"))
                })
            }
            (Reply::Annotations(list), Call::AnnotationList { .. }) => serde_json::to_value(list)
                .map_err(|e| {
                    RpcError::internal(format!("annotation reply is not serializable: {e}"))
                }),
            (Reply::Checks(checks), Call::AnnotationCheck { .. }) => serde_json::to_value(checks)
                .map_err(|e| RpcError::internal(format!("check report is not serializable: {e}"))),
            (Reply::Analysis(hash), Call::AnalysisAdd { .. }) => serde_json::to_value(hash)
                .map_err(|e| RpcError::internal(format!("analysis hash is not serializable: {e}"))),
            (Reply::Playhead(at), Call::PlayheadSet { .. }) => {
                serde_json::to_value(at).map_err(|e| {
                    RpcError::internal(format!("playhead reply is not serializable: {e}"))
                })
            }
            (Reply::Preview(preview), Call::ActionPreview { .. }) => serde_json::to_value(preview)
                .map_err(|e| RpcError::internal(format!("preview is not serializable: {e}"))),
            (
                Reply::Job(job),
                Call::JobSubmit { .. }
                | Call::JobGet { .. }
                | Call::JobCancel { .. }
                | Call::JobRemove { .. },
            ) => serde_json::to_value(job)
                .map_err(|e| RpcError::internal(format!("job is not serializable: {e}"))),
            (Reply::Jobs(jobs), Call::JobList) => serde_json::to_value(jobs)
                .map_err(|e| RpcError::internal(format!("job list is not serializable: {e}"))),
            (Reply::Pruned(pruned), Call::JobPrune { .. }) => serde_json::to_value(pruned)
                .map_err(|e| RpcError::internal(format!("prune reply is not serializable: {e}"))),
            (
                Reply::Outcome(outcome),
                Call::ActionApply { .. } | Call::Undo { .. } | Call::Redo { .. },
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
            Call::SequenceUse { .. }
            | Call::SequenceAdd { .. }
            | Call::SequenceDerive { .. }
            | Call::SequenceAudio { .. } => serde_json::from_value(result).map(Reply::Sequence),
            Call::MediaList | Call::MediaAdd { .. } => {
                serde_json::from_value(result).map(Reply::Media)
            }
            Call::AnnotationList { .. } => serde_json::from_value(result).map(Reply::Annotations),
            Call::AnnotationCheck { .. } => serde_json::from_value(result).map(Reply::Checks),
            Call::AnalysisAdd { .. } => serde_json::from_value(result).map(Reply::Analysis),
            Call::PlayheadSet { .. } => serde_json::from_value(result).map(Reply::Playhead),
            Call::ActionPreview { .. } => serde_json::from_value(result).map(Reply::Preview),
            Call::JobSubmit { .. }
            | Call::JobGet { .. }
            | Call::JobCancel { .. }
            | Call::JobRemove { .. } => serde_json::from_value(result).map(Reply::Job),
            Call::JobList => serde_json::from_value(result).map(Reply::Jobs),
            Call::JobPrune { .. } => serde_json::from_value(result).map(Reply::Pruned),
            Call::ActionApply { .. } | Call::Undo { .. } | Call::Redo { .. } => {
                serde_json::from_value(result).map(Reply::Outcome)
            }
        }
    }
}

/// The common result of a mutating (or previewing) call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    /// The document revision after the call. A preview reports the current
    /// revision: it did not change anything.
    pub revision: u64,
    /// The active sequence's undo depth after the call.
    pub depth: u64,
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

/// Notes and markers for one sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnnotationList {
    pub sequence: u32,
    pub annotations: Vec<Annotation>,
}

/// The content address returned by `analysis.add`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalysisHash {
    pub hash: String,
}

impl AnnotationList {
    pub fn of(project: &Project, sequence: u32) -> Self {
        Self {
            sequence,
            annotations: project
                .annotations
                .iter()
                .filter(|annotation| annotation.sequence == sequence)
                .cloned()
                .collect(),
        }
    }
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
    /// Loudness/fade treatment of the mixed programme.
    #[serde(default)]
    pub audio: AudioSettings,
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
            audio: sequence.audio,
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

/// One place an asset is used. Segment ids are only unique within a sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaUsage {
    pub sequence: u32,
    /// A clip or transition id, as stored in that sequence.
    pub segment: u32,
}

/// One pooled asset as the protocol reports it. `used_by` spans every
/// sequence and includes both clips and transition sources.
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
    pub used_by: Vec<MediaUsage>,
}

/// The pool after `media.list`, or the affected assets after `media.add`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaList {
    pub revision: u64,
    pub assets: Vec<MediaAsset>,
}

/// `action.preview` result: the would-be outcome, a structural diff and the
/// clamp warnings a real apply would have to repair. The outcome fields are
/// flattened, so the JSON stays the same flat shape as before.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    #[serde(flatten)]
    pub outcome: Outcome,
    pub diff: ProjectDiff,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// `playhead.set` result: where the cursor landed and the session metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayheadReply {
    pub revision: u64,
    pub depth: u64,
    #[serde(with = "nle_core::time::rational_ser")]
    pub at: Seconds,
}

/// `job.list` result. Jobs are server state, not model state: `revision` is
/// the live project revision purely so a client can correlate the two.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobList {
    pub revision: u64,
    pub jobs: Vec<JobRecord>,
}

impl JobList {
    pub fn new(revision: u64, jobs: Vec<JobRecord>) -> Self {
        Self { revision, jobs }
    }
}

/// `job.prune` result: the terminal job ids the server deleted, oldest first,
/// plus any it could not delete (the pass continues past a failure).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobPruneReply {
    pub removed: Vec<u64>,
    /// Terminal jobs whose directory could not be deleted, with the reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<JobPruneFailure>,
}

/// One terminal job a prune pass could not delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobPruneFailure {
    pub id: u64,
    pub error: String,
}
