//! The application session: a document, its yank register, and its files.
//!
//! A session is either **direct** — a `Document` plus the sidecars — or
//! **remote** — a protocol client with a cached project when `nile serve`
//! owns the project socket. The TUI uses [`Session::open`] to pick; the CLI
//! and tests use [`Session::load`] for the file-backed path.
//!
//! The media-frame playhead policy lives in [`playhead`] and is shared by the
//! TUI and `nile serve`.

pub mod playhead;

use std::io::BufReader;
use std::ops::Deref;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use nle_core::edit::{Actor, EditError, Histories, History, UndoOutcome};
use nle_core::jobs::{JobExport, JobRecord};
use nle_core::{
    Action, AudioSettings, Batch, BatchOutcome, Document, Outcome as ActionOutcome, PreviewOutcome,
    Project, ProjectError, Register, VideoSettings, frame_of, grid_time, load_register,
    register_path, save_register, secs_f64,
};
use nle_protocol::{Client, ClientError, Outcome as WireOutcome, Preview as WirePreview};

/// A session file or remote failure. Project-loader errors pass through
/// unchanged; sidecar/register problems are I/O; protocol failures are
/// `Remote`.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Project(#[from] ProjectError),
    #[error(transparent)]
    Edit(#[from] EditError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Remote(String),
}

impl From<ClientError> for SessionError {
    fn from(error: ClientError) -> Self {
        Self::Remote(error.to_string())
    }
}

enum Backend {
    Direct {
        document: Document,
        register: Option<Register>,
    },
    Remote(RemoteSession),
}

type RpcClient = Client<BufReader<UnixStream>, UnixStream>;

struct RemoteSession {
    client: RpcClient,
    project: Project,
    revision: u64,
    depth: u64,
    last_poll: Instant,
    /// The connection is gone; stop polling and refuse remote work.
    failed: Option<String>,
    /// A playhead save failed but the connection may still be alive (a
    /// poisoned server, for example); surface it on the next save.
    save_error: Option<String>,
}

/// A session file's identity as last read or written. Atomic saves rename a
/// fresh temp into place, so the inode changes even when the filesystem's
/// mtime granularity is coarser than the write. Mirrors the TUI's `Seen` so a
/// file writer cannot be mistaken for our own save.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FileStamp {
    mtime: Option<SystemTime>,
    len: u64,
    ino: u64,
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let ino = {
        use std::os::unix::fs::MetadataExt;
        meta.ino()
    };
    #[cfg(not(unix))]
    let ino = 0;
    Some(FileStamp {
        mtime: meta.modified().ok(),
        len: meta.len(),
        ino,
    })
}

/// A loaded project plus the state that travels with it across commands.
pub struct Session {
    backend: Backend,
    path: PathBuf,
    /// The register sidecar as we last read/wrote it; a different stamp is a
    /// file writer's yank and must not be clobbered by our next save.
    register_stamp: Option<FileStamp>,
}

impl Session {
    /// Load the file-backed project, its history sidecar and register.
    pub fn load(path: &Path) -> Result<Self, SessionError> {
        let project = Project::load(path)?;
        let histories = Histories::load_project(path).unwrap_or_default();
        Ok(Self::from_histories(project, histories, path.to_path_buf()))
    }

    /// Open the live server when its socket accepts; otherwise load the file.
    /// A non-connection error (permissions, broken protocol) is reported
    /// rather than silently falling back to a second writer.
    #[allow(dead_code)] // TUI startup
    pub fn open(path: &Path) -> Result<Self, SessionError> {
        match RemoteSession::connect(path) {
            Ok(Some(remote)) => Ok(Self {
                backend: Backend::Remote(remote),
                path: path.to_path_buf(),
                register_stamp: None,
            }),
            Ok(None) => Self::load(path),
            Err(e) => Err(SessionError::Io(e)),
        }
    }

    /// Adopt a project and its per-sequence histories (the TUI), and read the
    /// shared register beside `path`.
    pub fn from_histories(project: Project, histories: Histories, path: PathBuf) -> Self {
        let register = load_register(&path);
        // Read the stamp *after* `load_register`, which may copy a legacy
        // sidecar into the state directory: that migration is ours, not a
        // foreign write.
        let register_stamp = file_stamp(&register_path(&path));
        Self {
            backend: Backend::Direct {
                document: Document::with_histories(project, histories),
                register,
            },
            path,
            register_stamp,
        }
    }

    /// Adopt a single stack for the active sequence (tests, scratch dumps).
    #[allow(dead_code)] // TUI tests/dump
    pub fn from_history(project: Project, history: History, path: PathBuf) -> Self {
        let active = project.active_id();
        Self::from_histories(project, Histories::from_history(active, history), path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[allow(dead_code)] // TUI attach
    pub fn is_remote(&self) -> bool {
        matches!(self.backend, Backend::Remote(_))
    }

    pub fn project(&self) -> &Project {
        match &self.backend {
            Backend::Direct { document, .. } => document.project(),
            Backend::Remote(remote) => &remote.project,
        }
    }

    pub fn revision(&self) -> u64 {
        match &self.backend {
            Backend::Direct { document, .. } => document.revision(),
            Backend::Remote(remote) => remote.revision,
        }
    }

    pub fn depth(&self) -> usize {
        match &self.backend {
            Backend::Direct { document, .. } => document.depth(),
            Backend::Remote(remote) => remote.depth as usize,
        }
    }

    pub fn set_actor(&mut self, actor: Actor) {
        if let Backend::Direct { document, .. } = &mut self.backend {
            document.set_actor(actor);
        }
    }

    /// The direct document, for process-bound commands (`:add` probes a file
    /// and records the pool insertion). Remote sessions do not expose one.
    #[allow(dead_code)] // TUI :add
    pub fn document_mut(&mut self) -> Option<&mut Document> {
        match &mut self.backend {
            Backend::Direct { document, .. } => Some(document),
            Backend::Remote(_) => None,
        }
    }

    pub fn set_playhead(&mut self, at: f64) -> f64 {
        match &mut self.backend {
            Backend::Direct { document, .. } => document.set_playhead(at),
            Backend::Remote(remote) => {
                let snapped = grid_time(remote.project.fps.as_ref(), at);
                match remote.call(|client| client.set_playhead(snapped)) {
                    Ok(reply) => {
                        remote.project.playhead = reply.at;
                        remote.revision = reply.revision;
                        remote.depth = reply.depth;
                        let _ = remote.client.drain_buffered_events();
                        secs_f64(&reply.at)
                    }
                    Err(error) => {
                        remote.save_error = Some(error.to_string());
                        secs_f64(&remote.project.playhead)
                    }
                }
            }
        }
    }

    #[allow(dead_code)] // the TUI tests read the shared register
    pub fn register(&self) -> Option<&Register> {
        match &self.backend {
            Backend::Direct { register, .. } => register.as_ref(),
            Backend::Remote(_) => None,
        }
    }

    #[allow(dead_code)] // TUI dump
    pub fn set_register(&mut self, register: Option<Register>) {
        if let Backend::Direct {
            register: current, ..
        } = &mut self.backend
        {
            *current = register;
        }
    }

    /// Run one catalog action, recording it in the session's history and
    /// register. The caller decides whether to save.
    pub fn apply_action(&mut self, action: &Action, from: f64) -> Result<ActionOutcome, EditError> {
        match &mut self.backend {
            Backend::Direct { document, register } => document.edit(action, from, register),
            Backend::Remote(remote) => {
                let at = grid_time(remote.project.fps.as_ref(), from);
                let revision = remote.revision;
                let outcome = remote
                    .call(|client| client.apply(revision, vec![action.clone()], Some(at)))
                    .map_err(remote_edit_error)?;
                remote
                    .refresh_after(&outcome)
                    .map_err(|e| EditError::Remote(e.to_string()))?;
                remote_action_outcome(&outcome, remote.project.fps.as_ref())
            }
        }
    }

    /// Run several actions as one undo step. Atomic: a failure restores the
    /// project, histories and register.
    pub fn apply_actions(
        &mut self,
        actions: &[Action],
        from: f64,
    ) -> Result<BatchOutcome, EditError> {
        match &mut self.backend {
            Backend::Direct { document, register } => {
                document.edit_actions(actions, from, register)
            }
            Backend::Remote(remote) => {
                let at = grid_time(remote.project.fps.as_ref(), from);
                let revision = remote.revision;
                let outcome = remote
                    .call(|client| client.apply(revision, actions.to_vec(), Some(at)))
                    .map_err(remote_edit_error)?;
                remote
                    .refresh_after(&outcome)
                    .map_err(|e| EditError::Remote(e.to_string()))?;
                Ok(BatchOutcome {
                    outcome: remote_action_outcome(&outcome, remote.project.fps.as_ref())?,
                    warnings: Vec::new(),
                })
            }
        }
    }

    /// Direct-only prepared batch (an extend commit builds one before
    /// committing). The protocol sends catalog actions, not model ops.
    #[allow(dead_code)] // test fixtures and direct-only paths
    pub fn apply(&mut self, batch: Batch, playhead: i64) -> Result<(), EditError> {
        match &mut self.backend {
            Backend::Direct { document, .. } => document.apply(batch, playhead),
            Backend::Remote(_) => Err(EditError::Remote(
                "a direct Batch cannot be sent to nile serve".into(),
            )),
        }
    }

    /// Run one action against a copy. The clone includes the history, so an
    /// undo/redo preview is real, and the cloned register keeps a preview yank
    /// away from the live clipboard.
    pub fn preview(&mut self, action: &Action, from: f64) -> Result<PreviewOutcome, EditError> {
        self.preview_actions(std::slice::from_ref(action), from)
    }

    /// Preview a batch against a copy; like [`Session::preview`], nothing is
    /// saved and the register is cloned.
    pub fn preview_actions(
        &mut self,
        actions: &[Action],
        from: f64,
    ) -> Result<PreviewOutcome, EditError> {
        match &mut self.backend {
            Backend::Direct { document, register } => {
                let before = document.project().clone();
                let mut document =
                    Document::with_histories(before.clone(), document.histories().clone());
                let mut register = register.clone();
                let batch = document.edit_actions(actions, from, &mut register)?;
                Ok(PreviewOutcome {
                    outcome: batch.outcome,
                    diff: before.diff(document.project()),
                    warnings: batch.warnings,
                })
            }
            Backend::Remote(remote) => {
                let at = grid_time(remote.project.fps.as_ref(), from);
                let revision = remote.revision;
                let preview: WirePreview = remote
                    .call(|client| client.preview(revision, actions.to_vec(), Some(at)))
                    .map_err(remote_edit_error)?;
                let outcome = WireOutcome {
                    revision: preview.outcome.revision,
                    depth: preview.outcome.depth,
                    desc: preview.outcome.desc,
                    changed: preview.outcome.changed,
                    playhead: preview.outcome.playhead,
                };
                Ok(PreviewOutcome {
                    outcome: remote_action_outcome(&outcome, remote.project.fps.as_ref())?,
                    diff: preview.diff,
                    warnings: preview.warnings,
                })
            }
        }
    }

    #[allow(dead_code)] // test fixtures and direct-only paths
    pub fn undo(&mut self) -> Result<UndoOutcome, EditError> {
        match &mut self.backend {
            Backend::Direct { document, .. } => document.undo(),
            Backend::Remote(remote) => {
                Err(EditError::Remote(remote.failed.clone().unwrap_or_else(
                    || "undo/redo is not wired for a remote session".into(),
                )))
            }
        }
    }

    #[allow(dead_code)] // test fixtures and direct-only paths
    pub fn redo(&mut self) -> Result<UndoOutcome, EditError> {
        match &mut self.backend {
            Backend::Direct { document, .. } => document.redo(),
            Backend::Remote(remote) => {
                Err(EditError::Remote(remote.failed.clone().unwrap_or_else(
                    || "undo/redo is not wired for a remote session".into(),
                )))
            }
        }
    }

    #[allow(dead_code)] // test fixtures and direct-only paths
    pub fn can_undo(&self) -> bool {
        match &self.backend {
            Backend::Direct { document, .. } => document.can_undo(),
            Backend::Remote(remote) => remote.depth > 0,
        }
    }

    #[allow(dead_code)] // test fixtures and direct-only paths
    pub fn can_redo(&self) -> bool {
        match &self.backend {
            Backend::Direct { document, .. } => document.can_redo(),
            Backend::Remote(_) => false,
        }
    }

    /// Direct-only whole-track edit (test fixtures and track actions).
    #[allow(dead_code)] // test fixtures and direct-only paths
    pub fn edit_tracks<F>(&mut self, playhead: i64, edit: F) -> Result<String, EditError>
    where
        F: FnOnce(&mut Project) -> Result<String, EditError>,
    {
        match &mut self.backend {
            Backend::Direct { document, .. } => document.edit_tracks(playhead, edit),
            Backend::Remote(_) => Err(EditError::Remote(
                "track-list edits need the direct session".into(),
            )),
        }
    }

    /// Add a batch of files as one undo step.
    pub fn add_assets(
        &mut self,
        assets: Vec<(PathBuf, nle_core::media::RawProbe)>,
        playhead: i64,
    ) -> Result<Vec<u32>, SessionError> {
        match &mut self.backend {
            Backend::Direct { document, .. } => Ok(document.add_assets(assets, playhead)),
            Backend::Remote(remote) => {
                let paths: Vec<PathBuf> = assets.iter().map(|(path, _)| path.clone()).collect();
                let _ = playhead;
                let revision = remote.revision;
                let media = remote
                    .call(|client| client.add_media(revision, paths))
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                remote.revision = media.revision;
                remote
                    .refresh()
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                Ok(media.assets.iter().map(|asset| asset.id).collect())
            }
        }
    }

    pub fn add_sequence(
        &mut self,
        name: impl Into<String>,
        fps: Option<nle_core::Seconds>,
        video: Option<VideoSettings>,
    ) -> Result<u32, SessionError> {
        match &mut self.backend {
            Backend::Direct { document, .. } => {
                document.add_sequence(name, fps, video).map_err(Into::into)
            }
            Backend::Remote(remote) => {
                let revision = remote.revision;
                let reply = remote
                    .call(|client| client.add_sequence(revision, name, fps, video))
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                remote
                    .refresh()
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                Ok(reply.sequence.id)
            }
        }
    }

    /// Copy a sequence snapshot into a new sequence (not active).
    pub fn derive_sequence(
        &mut self,
        from: u32,
        name: impl Into<String>,
        fps: Option<nle_core::Seconds>,
        video: Option<VideoSettings>,
    ) -> Result<u32, SessionError> {
        match &mut self.backend {
            Backend::Direct { document, .. } => document
                .derive_sequence(from, name, fps, video)
                .map_err(Into::into),
            Backend::Remote(remote) => {
                let revision = remote.revision;
                let reply = remote
                    .call(|client| client.derive_sequence(revision, from, name, fps, video))
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                remote
                    .refresh()
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                Ok(reply.sequence.id)
            }
        }
    }

    /// Set the active sequence's audio-post settings. Direct sessions record
    /// the change in the document/history and save both sidecars; remote
    /// sessions send the typed call and re-fetch the server's state. The
    /// shared playhead is the undo anchor, so undo/redo land where they were.
    pub fn set_audio(&mut self, audio: AudioSettings) -> Result<String, SessionError> {
        match &mut self.backend {
            Backend::Direct { document, .. } => {
                let playhead = frame_of(document.fps.as_ref(), secs_f64(&document.playhead));
                let desc = document.set_audio(audio, playhead)?;
                document.save(&self.path)?;
                document.save_history(&Histories::sidecar_path(&self.path))?;
                Ok(desc)
            }
            Backend::Remote(remote) => {
                let revision = remote.revision;
                let reply = remote
                    .call(|client| client.set_audio(revision, audio))
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                remote
                    .refresh()
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                Ok(format!("sequence audio {}", reply.sequence.audio.summary()))
            }
        }
    }

    /// The durable job queue is server state: a direct (file-backed) session
    /// has no worker and must say so instead of pretending it queued anything.
    fn jobs_need_server() -> SessionError {
        SessionError::Remote("jobs need a running `nile serve` for this project".into())
    }

    #[allow(dead_code)] // TUI job panel
    pub fn jobs(&mut self) -> Result<Vec<JobRecord>, SessionError> {
        match &mut self.backend {
            Backend::Direct { .. } => Err(Self::jobs_need_server()),
            Backend::Remote(remote) => {
                let list = remote
                    .call(|client| client.jobs())
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                Ok(list.jobs)
            }
        }
    }

    #[allow(dead_code)] // TUI job panel (`o` refetches the live record)
    pub fn job(&mut self, id: u64) -> Result<JobRecord, SessionError> {
        match &mut self.backend {
            Backend::Direct { .. } => Err(Self::jobs_need_server()),
            Backend::Remote(remote) => remote
                .call(|client| client.job(id))
                .map_err(|e| SessionError::Remote(e.to_string())),
        }
    }

    #[allow(dead_code)] // TUI job panel
    pub fn cancel_job(&mut self, id: u64) -> Result<JobRecord, SessionError> {
        match &mut self.backend {
            Backend::Direct { .. } => Err(Self::jobs_need_server()),
            Backend::Remote(remote) => remote
                .call(|client| client.cancel_job(id))
                .map_err(|e| SessionError::Remote(e.to_string())),
        }
    }

    /// Snapshot and queue the server's current project. The client sends its
    /// own revision; a stale TUI gets the typed conflict rather than a
    /// snapshot of a project the user has not seen.
    #[allow(dead_code)] // CLI submits through the client; kept for a TUI `:job submit`
    pub fn submit_job(
        &mut self,
        output: PathBuf,
        sequence: u32,
        export: JobExport,
    ) -> Result<JobRecord, SessionError> {
        match &mut self.backend {
            Backend::Direct { .. } => Err(Self::jobs_need_server()),
            Backend::Remote(remote) => {
                let revision = remote.revision;
                remote
                    .call(|client| client.submit_job(revision, output, sequence, export))
                    .map_err(|e| SessionError::Remote(e.to_string()))
            }
        }
    }

    pub fn select_sequence(&mut self, id: u32) -> Result<(), SessionError> {
        match &mut self.backend {
            Backend::Direct { document, .. } => document.select_sequence(id).map_err(Into::into),
            Backend::Remote(remote) => {
                remote
                    .call(|client| client.use_sequence(id))
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                remote
                    .refresh()
                    .map_err(|e| SessionError::Remote(e.to_string()))
            }
        }
    }

    /// Re-fetch a remote session (a manual reload or a foreign edit).
    #[allow(dead_code)] // TUI reload
    pub fn refresh(&mut self) -> Result<bool, SessionError> {
        match &mut self.backend {
            Backend::Direct { .. } => Ok(false),
            Backend::Remote(remote) => {
                let before = remote.revision;
                remote.refresh()?;
                Ok(remote.revision != before)
            }
        }
    }

    /// Cheap remote revision poll for the TUI tick.
    #[allow(dead_code)] // TUI poll
    pub fn poll_remote(&mut self) -> Result<bool, SessionError> {
        match &mut self.backend {
            Backend::Direct { .. } => Ok(false),
            Backend::Remote(remote) => {
                if remote.failed.is_some() {
                    return Ok(false);
                }
                if remote.last_poll.elapsed() < Duration::from_millis(200) {
                    return Ok(false);
                }
                remote.last_poll = Instant::now();
                let hello = remote
                    .call(|client| client.hello())
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                if hello.revision == remote.revision && hello.active == remote.project.active_id() {
                    remote.depth = hello.depth;
                    let _ = remote.client.drain_buffered_events();
                    return Ok(false);
                }
                remote.project = remote
                    .call(|client| client.show())
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                remote.revision = hello.revision;
                remote.depth = hello.depth;
                let _ = remote.client.drain_buffered_events();
                Ok(true)
            }
        }
    }

    /// Save project, history, and register. This is the CLI's one-shot path.
    pub fn persist(&mut self) -> Result<(), SessionError> {
        self.save_if(true, true)
    }

    pub fn save_register(&mut self) -> std::io::Result<()> {
        match &mut self.backend {
            Backend::Direct { register, .. } => {
                save_register(&self.path, register.as_ref())?;
                self.register_stamp = file_stamp(&register_path(&self.path));
                Ok(())
            }
            Backend::Remote(remote) => match remote.take_save_error() {
                Some(reason) => Err(std::io::Error::other(reason)),
                None => Ok(()),
            },
        }
    }

    /// Save only the project (a playhead move, a TUI reload).
    pub fn save_project(&mut self) -> Result<(), ProjectError> {
        match &mut self.backend {
            Backend::Direct { document, .. } => document.save(&self.path),
            Backend::Remote(remote) => match remote.take_save_error() {
                Some(reason) => Err(ProjectError::Io(std::io::Error::other(reason))),
                None => Ok(()),
            },
        }
    }

    /// Save only the undo stack beside the project.
    pub fn save_history(&mut self) -> std::io::Result<()> {
        match &mut self.backend {
            Backend::Direct { document, .. } => {
                document.save_history(&Histories::sidecar_path(&self.path))
            }
            Backend::Remote(remote) => match remote.take_save_error() {
                Some(reason) => Err(std::io::Error::other(reason)),
                None => Ok(()),
            },
        }
    }

    /// Drop the undo stack and its sidecar (a rebuild).
    #[allow(dead_code)] // used by the TUI rebuild tests
    pub fn reset_history(&mut self) {
        if let Backend::Direct { document, .. } = &mut self.backend {
            document.reset_history(&self.path);
        }
    }

    /// Replace the sidecar stacks (the TUI dump path).
    #[allow(dead_code)] // TUI dump
    pub fn set_histories(&mut self, histories: Histories) {
        if let Backend::Direct { document, .. } = &mut self.backend {
            document.set_histories(histories);
        }
    }

    /// Save the parts a caller actually changed. A yank only needs the
    /// register; a playhead-only move needs the project but not the stack.
    pub fn save_if(&mut self, changed: bool, playhead_moved: bool) -> Result<(), SessionError> {
        if let Backend::Remote(remote) = &mut self.backend {
            return match remote.take_save_error() {
                Some(reason) => Err(SessionError::Remote(reason)),
                None => Ok(()),
            };
        }
        if changed || playhead_moved {
            self.save_project()?;
        }
        if changed {
            self.save_history()?;
        }
        if self.register_changed() {
            // A file writer got there first (`nile yank`): adopt its register
            // instead of overwriting it. The action that triggered this save
            // loses its register change, which is the no-clobber guarantee.
            self.adopt_register();
        } else {
            self.save_register()?;
        }
        Ok(())
    }

    /// Whether a file writer changed the register since we loaded or last
    /// saved it. A remote session leaves the file to the server.
    fn register_changed(&self) -> bool {
        !self.is_remote() && file_stamp(&register_path(&self.path)) != self.register_stamp
    }

    /// Take the register now on disk and re-baseline the stamp, so a later
    /// save writes from the file writer's state rather than its old snapshot.
    fn adopt_register(&mut self) {
        if self.is_remote() {
            return;
        }
        self.set_register(load_register(&self.path));
        self.register_stamp = file_stamp(&register_path(&self.path));
    }
}

impl Deref for Session {
    type Target = Project;

    fn deref(&self) -> &Project {
        self.project()
    }
}

impl RemoteSession {
    fn call<T>(
        &mut self,
        call: impl FnOnce(&mut RpcClient) -> Result<T, ClientError>,
    ) -> Result<T, ClientError> {
        if let Some(reason) = &self.failed {
            return Err(ClientError::Io(std::io::Error::other(reason.clone())));
        }
        match call(&mut self.client) {
            Ok(value) => Ok(value),
            Err(error) => {
                if fatal(&error) {
                    self.failed = Some(error.to_string());
                }
                Err(error)
            }
        }
    }

    fn take_save_error(&mut self) -> Option<String> {
        self.failed.clone().or_else(|| self.save_error.take())
    }

    #[allow(dead_code)] // TUI attach
    fn connect(path: &Path) -> std::io::Result<Option<Self>> {
        let socket = nle_core::state::session_socket_path(path);
        let stream = match UnixStream::connect(&socket) {
            Ok(stream) => stream,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let reader = stream.try_clone()?;
        let mut client = Client::new(BufReader::new(reader), stream);
        let hello = client.hello().map_err(client_io)?;
        let project = client.show().map_err(client_io)?;
        Ok(Some(Self {
            client,
            project,
            revision: hello.revision,
            depth: hello.depth,
            last_poll: Instant::now(),
            failed: None,
            save_error: None,
        }))
    }

    fn refresh(&mut self) -> std::io::Result<()> {
        let hello = self.call(|client| client.hello()).map_err(client_io)?;
        self.project = self.call(|client| client.show()).map_err(client_io)?;
        self.revision = hello.revision;
        self.depth = hello.depth;
        self.last_poll = Instant::now();
        let _ = self.client.drain_buffered_events();
        Ok(())
    }

    fn refresh_after(&mut self, outcome: &WireOutcome) -> std::io::Result<()> {
        self.project = self.call(|client| client.show()).map_err(client_io)?;
        self.revision = outcome.revision;
        self.depth = outcome.depth;
        self.last_poll = Instant::now();
        let _ = self.client.drain_buffered_events();
        Ok(())
    }
}

fn fatal(error: &ClientError) -> bool {
    !matches!(
        error,
        ClientError::Server(_) | ClientError::Encode(_) | ClientError::UnexpectedReply(_)
    )
}

fn client_io(error: ClientError) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

fn remote_edit_error(error: ClientError) -> EditError {
    EditError::Remote(error.to_string())
}

fn remote_action_outcome(
    outcome: &WireOutcome,
    fps: Option<&nle_core::Seconds>,
) -> Result<ActionOutcome, EditError> {
    Ok(ActionOutcome {
        desc: outcome.desc.clone(),
        changed: outcome.changed,
        playhead: outcome.playhead.map(|at| frame_of(fps, secs_f64(&at))),
    })
}

#[cfg(test)]
mod remote_tests {
    use super::*;
    use nle_core::Seconds;
    use nle_core::timeline::{Clip, Gap, Segment};
    use nle_protocol::Server;
    use std::io::BufReader;
    use std::os::unix::net::UnixListener;

    fn seeded() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.json");
        let mut project = Project::new();
        project.fps = Some(nle_core::Seconds::from_integer(30));
        project.timeline.video_track_mut().unwrap().segments = vec![Segment::Gap(Gap {
            duration: nle_core::Seconds::from_integer(2),
        })];
        project.save(&path).expect("seed project");
        (dir, path)
    }

    fn project_with_clip() -> Project {
        let mut project = Project::new();
        project.fps = Some(nle_core::Seconds::from_integer(30));
        let (asset, _) = project.add_asset(
            "/media/red.mp4".into(),
            nle_core::media::RawProbe::default(),
        );
        project.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip::new(
            asset,
            asset,
            nle_core::Seconds::from_integer(0),
            nle_core::Seconds::from_integer(2),
        ))];
        project
    }

    #[test]
    fn load_save_round_trip() {
        let (_dir, path) = seeded();
        let mut session = Session::load(&path).expect("load");
        assert_eq!(
            session.project().fps,
            Some(nle_core::Seconds::from_integer(30)),
            "the project loads"
        );
        session.set_playhead(1.0);
        session.persist().expect("save");
        let again = Session::load(&path).expect("reload");
        assert_eq!(again.project().playhead, nle_core::Seconds::from_integer(1));
    }

    // The server is the single writer, but a one-shot CLI command is still
    // file-based: a `nile yank` between load and save must survive `save_if`.
    #[test]
    fn save_if_does_not_clobber_a_foreign_register() {
        let (_dir, path) = seeded();
        let project = Project::load(&path).expect("load");
        let ours = Register::of(
            &project,
            Segment::Gap(Gap {
                duration: nle_core::Seconds::from_integer(1),
            }),
        );
        let foreign = Register::of(
            &project,
            Segment::Gap(Gap {
                duration: nle_core::Seconds::from_integer(2),
            }),
        );
        nle_core::save_register(&path, Some(&ours)).expect("seed register");
        let mut session = Session::load(&path).expect("load");
        assert_eq!(session.register(), Some(&ours));

        // A file writer replaces the register after we loaded.
        nle_core::save_register(&path, Some(&foreign)).expect("foreign yank");
        session.set_register(Some(ours.clone()));
        session.save_if(true, true).expect("save");

        assert_eq!(
            nle_core::load_register(&path),
            Some(foreign.clone()),
            "the shell's register survived"
        );
        assert_eq!(session.register(), Some(&foreign), "and was adopted");

        // The stamp is re-baselined, so the next save writes again.
        session.set_register(Some(ours.clone()));
        session.save_if(true, true).expect("save");
        assert_eq!(nle_core::load_register(&path), Some(ours));
    }

    #[test]
    fn preview_does_not_touch_the_live_document_or_file() {
        let (_dir, path) = seeded();
        let mut session = Session::load(&path).expect("load");
        let before = session.project().clone();
        let preview = session
            .preview(
                &Action::TrackAdd {
                    kind: nle_core::TrackKind::Audio,
                },
                0.0,
            )
            .expect("preview");
        assert!(preview.outcome.changed, "the preview reports the change");
        assert_eq!(session.project(), &before, "the live project is untouched");
        let reloaded = Session::load(&path).expect("reload");
        assert_eq!(
            reloaded.project(),
            &before,
            "the preview did not save the project"
        );
    }

    #[test]
    fn a_remote_session_round_trips_actions_and_sequences() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        let project = project_with_clip();
        project.save(&path).unwrap();
        let socket = nle_core::state::session_socket_path(&path);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let direct = Session::from_history(project, History::default(), path.clone());
        let queue_path = path.clone();
        let serve = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let reader = stream.try_clone().unwrap();
            let handler = crate::serve::ServeHandler {
                session: direct,
                events: Vec::new(),
                poisoned: None,
                jobs: crate::jobs::JobQueue::start(&queue_path).unwrap(),
            };
            let mut server = Server::new(BufReader::new(reader), stream, handler);
            server.serve().unwrap();
        });

        let mut session = Session::open(&path).unwrap();
        assert!(session.is_remote());
        assert_eq!(session.depth(), 0);

        let outcome = session
            .apply_action(&Action::Split { track: 0, at: 1.0 }, 0.0)
            .unwrap();
        assert!(outcome.changed);
        assert_eq!(session.depth(), 1);
        assert_eq!(
            session
                .project()
                .timeline
                .video_track()
                .unwrap()
                .segments
                .len(),
            2
        );

        session.set_playhead(0.5);
        assert_eq!(session.project().playhead, nle_core::Seconds::new(1, 2));

        let second = session.add_sequence("Second", None, None).unwrap();
        session.select_sequence(second).unwrap();
        assert_eq!(session.project().active_id(), second);
        session.select_sequence(1).unwrap();
        assert_eq!(session.project().active_id(), 1);

        // Audio post goes over `sequence.audio`, then refreshes the cache so
        // the client sees the server's canonical settings and undo depth.
        let audio = AudioSettings {
            normalize: true,
            fade_in: Seconds::new(1, 2),
            fade_out: Seconds::from_integer(1),
        };
        let desc = session.set_audio(audio).unwrap();
        assert!(desc.contains("normalize on"), "{desc}");
        assert_eq!(session.project().active_sequence().audio, audio);
        assert_eq!(session.depth(), 2);

        session.apply_action(&Action::Undo, 0.0).unwrap();
        assert_eq!(
            session.project().active_sequence().audio,
            AudioSettings::default(),
            "the audio setting is one undo step"
        );
        assert_eq!(session.depth(), 1);

        session.apply_action(&Action::Undo, 0.0).unwrap();
        assert_eq!(session.depth(), 0);
        assert_eq!(
            session
                .project()
                .timeline
                .video_track()
                .unwrap()
                .segments
                .len(),
            1
        );

        // A lost/poisoned server latches: polling goes quiet and the next
        // save reports the reason instead of silently succeeding.
        if let Backend::Remote(remote) = &mut session.backend {
            remote.failed = Some("gone".into());
        }
        assert!(
            !session.poll_remote().unwrap(),
            "a dead server stops polling"
        );
        assert!(
            session.save_project().is_err(),
            "the failure reaches the save"
        );

        drop(session);
        serve.join().unwrap();
        let _ = std::fs::remove_file(&socket);
    }
}
