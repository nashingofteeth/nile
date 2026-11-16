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
use std::time::{Duration, Instant};

use nle_core::edit::{Actor, EditError, Histories, History, UndoOutcome};
use nle_core::{
    Action, Batch, BatchOutcome, Document, Outcome as ActionOutcome, PreviewOutcome, Project,
    ProjectError, Register, VideoSettings, frame_of, grid_time, load_register, save_register,
    secs_f64,
};
use nle_protocol::{Client, ClientError, Outcome as WireOutcome, Preview as WirePreview};

/// A session file or remote failure. Project-loader errors pass through
/// unchanged; sidecar/register problems are I/O; protocol failures are
/// `Remote`.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Project(#[from] ProjectError),
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

struct RemoteSession {
    client: Client<BufReader<UnixStream>, UnixStream>,
    project: Project,
    revision: u64,
    depth: u64,
    last_poll: Instant,
}

/// A loaded project plus the state that travels with it across commands.
pub struct Session {
    backend: Backend,
    path: PathBuf,
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
    pub fn open(path: &Path) -> Result<Self, SessionError> {
        match RemoteSession::connect(path) {
            Ok(Some(remote)) => Ok(Self {
                backend: Backend::Remote(remote),
                path: path.to_path_buf(),
            }),
            Ok(None) => Self::load(path),
            Err(e) => Err(SessionError::Io(e)),
        }
    }

    /// Adopt a project and its per-sequence histories (the TUI), and read the
    /// shared register beside `path`.
    pub fn from_histories(project: Project, histories: Histories, path: PathBuf) -> Self {
        Self {
            backend: Backend::Direct {
                document: Document::with_histories(project, histories),
                register: load_register(&path),
            },
            path,
        }
    }

    /// Adopt a single stack for the active sequence (tests, scratch dumps).
    pub fn from_history(project: Project, history: History, path: PathBuf) -> Self {
        let active = project.active_id();
        Self::from_histories(project, Histories::from_history(active, history), path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

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
                match remote.client.set_playhead(snapped) {
                    Ok(reply) => {
                        remote.project.playhead = reply.at;
                        remote.revision = reply.revision;
                        remote.depth = reply.depth;
                        let _ = remote.client.drain_buffered_events();
                        secs_f64(&reply.at)
                    }
                    Err(_) => at,
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
                let outcome = remote
                    .client
                    .apply(remote.revision, vec![action.clone()], Some(at))
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
                let outcome = remote
                    .client
                    .apply(remote.revision, actions.to_vec(), Some(at))
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
                let preview: WirePreview = remote
                    .client
                    .preview(remote.revision, actions.to_vec(), Some(at))
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
                let outcome = remote
                    .client
                    .undo(remote.revision)
                    .map_err(remote_edit_error)?;
                remote
                    .refresh_after(&outcome)
                    .map_err(|e| EditError::Remote(e.to_string()))?;
                Ok(remote_undo_outcome(&outcome, remote.project.fps.as_ref()))
            }
        }
    }

    #[allow(dead_code)] // test fixtures and direct-only paths
    pub fn redo(&mut self) -> Result<UndoOutcome, EditError> {
        match &mut self.backend {
            Backend::Direct { document, .. } => document.redo(),
            Backend::Remote(remote) => {
                let outcome = remote
                    .client
                    .redo(remote.revision)
                    .map_err(remote_edit_error)?;
                remote
                    .refresh_after(&outcome)
                    .map_err(|e| EditError::Remote(e.to_string()))?;
                Ok(remote_undo_outcome(&outcome, remote.project.fps.as_ref()))
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

    /// Add one asset through the direct history or `media.add`.
    #[allow(dead_code)] // TUI tests use the direct form
    pub fn add_asset(
        &mut self,
        path: PathBuf,
        report: nle_core::media::RawProbe,
        playhead: i64,
    ) -> (u32, String) {
        match &mut self.backend {
            Backend::Direct { document, .. } => document.add_asset(path, report, playhead),
            Backend::Remote(_) => (0, "media.add is not available on a direct session".into()),
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
                let media = remote
                    .client
                    .add_media(remote.revision, paths)
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
                let reply = remote
                    .client
                    .add_sequence(remote.revision, name, fps, video)
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                remote
                    .refresh()
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                Ok(reply.sequence.id)
            }
        }
    }

    pub fn select_sequence(&mut self, id: u32) -> Result<(), SessionError> {
        match &mut self.backend {
            Backend::Direct { document, .. } => document.select_sequence(id).map_err(Into::into),
            Backend::Remote(remote) => {
                remote
                    .client
                    .use_sequence(id)
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                remote
                    .refresh()
                    .map_err(|e| SessionError::Remote(e.to_string()))
            }
        }
    }

    /// Re-fetch a remote session (a manual reload or a foreign edit).
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
    pub fn poll_remote(&mut self) -> Result<bool, SessionError> {
        match &mut self.backend {
            Backend::Direct { .. } => Ok(false),
            Backend::Remote(remote) => {
                if remote.last_poll.elapsed() < Duration::from_millis(200) {
                    return Ok(false);
                }
                remote.last_poll = Instant::now();
                let hello = remote
                    .client
                    .hello()
                    .map_err(|e| SessionError::Remote(e.to_string()))?;
                if hello.revision == remote.revision && hello.active == remote.project.active_id() {
                    remote.depth = hello.depth;
                    let _ = remote.client.drain_buffered_events();
                    return Ok(false);
                }
                remote.project = remote
                    .client
                    .show()
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

    pub fn save_register(&self) -> std::io::Result<()> {
        match &self.backend {
            Backend::Direct { register, .. } => save_register(&self.path, register.as_ref()),
            Backend::Remote(_) => Ok(()),
        }
    }

    /// Save only the project (a playhead move, a TUI reload).
    pub fn save_project(&mut self) -> Result<(), ProjectError> {
        match &mut self.backend {
            Backend::Direct { document, .. } => document.save(&self.path),
            Backend::Remote(_) => Ok(()),
        }
    }

    /// Save only the undo stack beside the project.
    pub fn save_history(&self) -> std::io::Result<()> {
        match &self.backend {
            Backend::Direct { document, .. } => {
                document.save_history(&Histories::sidecar_path(&self.path))
            }
            Backend::Remote(_) => Ok(()),
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
    pub fn set_histories(&mut self, histories: Histories) {
        if let Backend::Direct { document, .. } = &mut self.backend {
            document.set_histories(histories);
        }
    }

    /// Save the parts a caller actually changed. A yank only needs the
    /// register; a playhead-only move needs the project but not the stack.
    pub fn save_if(&mut self, changed: bool, playhead_moved: bool) -> Result<(), SessionError> {
        if let Backend::Remote(_) = self.backend {
            return Ok(());
        }
        if changed || playhead_moved {
            self.save_project()?;
        }
        if changed {
            self.save_history()?;
        }
        self.save_register()?;
        Ok(())
    }
}

impl Deref for Session {
    type Target = Project;

    fn deref(&self) -> &Project {
        self.project()
    }
}

impl RemoteSession {
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
        }))
    }

    fn refresh(&mut self) -> std::io::Result<()> {
        let hello = self.client.hello().map_err(client_io)?;
        self.project = self.client.show().map_err(client_io)?;
        self.revision = hello.revision;
        self.depth = hello.depth;
        self.last_poll = Instant::now();
        let _ = self.client.drain_buffered_events();
        Ok(())
    }

    fn refresh_after(&mut self, outcome: &WireOutcome) -> std::io::Result<()> {
        self.project = self.client.show().map_err(client_io)?;
        self.revision = outcome.revision;
        self.depth = outcome.depth;
        self.last_poll = Instant::now();
        let _ = self.client.drain_buffered_events();
        Ok(())
    }
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

#[allow(dead_code)] // reached by the direct/test undo helpers above
fn remote_undo_outcome(outcome: &WireOutcome, fps: Option<&nle_core::Seconds>) -> UndoOutcome {
    UndoOutcome {
        desc: outcome.desc.clone(),
        playhead: outcome
            .playhead
            .map(|at| frame_of(fps, secs_f64(&at)))
            .unwrap_or(0),
        provenance: nle_core::edit::Provenance {
            actor: Actor::Agent("nile-serve".into()),
            at: 0,
        },
    }
}

#[cfg(test)]
mod remote_tests {
    use super::*;
    use nle_core::timeline::{Clip, Segment};
    use nle_protocol::Server;
    use std::io::BufReader;
    use std::os::unix::net::UnixListener;

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
    fn a_remote_session_round_trips_actions_and_sequences() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        let project = project_with_clip();
        project.save(&path).unwrap();
        let socket = nle_core::state::session_socket_path(&path);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let direct = Session::from_history(project, History::default(), path.clone());
        let serve = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let reader = stream.try_clone().unwrap();
            let handler = crate::serve::ServeHandler {
                session: direct,
                events: Vec::new(),
                poisoned: None,
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

        drop(session);
        serve.join().unwrap();
        let _ = std::fs::remove_file(&socket);
    }
}
