//! `nile serve --stdio`: the protocol handler over a loaded project.
//!
//! The server state is a [`Session`] — project, history, register, and path —
//! plus the queued events and the save-failure poison. Every action mutation
//! goes through [`ServeHandler::apply`], so compare-and-swap, the edit,
//! playhead conversion, persistence, and event emission cannot drift between
//! `action.apply` and the dedicated `undo`/`redo` aliases. Sequence and media
//! writes are separate model paths, but they use the same writeability,
//! revision and save/poison helpers; a failed save still poisons every later
//! write.
//!
//! stdout is the protocol channel. Nothing here may `say!` or print; errors
//! leave through the JSON-RPC response, and `main` sends command failures to
//! stderr.

use std::io;
#[cfg(unix)]
use std::io::BufReader;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::thread;

use anyhow::{Context, Result};

use crate::{log, say_err};

use nle_core::edit::Actor;
use nle_core::{
    Action, Asset, EditError, Outcome as ActionOutcome, Project, Seconds, TrackKind, frame_of,
    frames_to_seconds, secs_f64,
};
use nle_ffmpeg::probe;
use nle_protocol::{
    Call, Event, Handler, Hello, MediaAsset, MediaList, MediaUsage, Outcome as WireOutcome,
    PlayheadReply, Preview, Reply, RpcError, SequenceList, SequenceReply, SequenceSummary, Server,
};

use crate::session::{Session, playhead};

/// Serve the project over the per-project Unix socket, or stdin/stdout when
/// `--stdio` is set. The socket is the default so a TUI and an agent can
/// share one writer; the stdio mode is for pipes and tests.
pub(crate) fn cmd_serve(project_path: &Path, stdio: bool) -> Result<()> {
    let mut session = Session::load(project_path)?;
    session.set_actor(Actor::Agent("nile-serve".into()));
    let handler = ServeHandler {
        session,
        events: Vec::new(),
        poisoned: None,
    };
    if stdio {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut server = Server::new(stdin.lock(), stdout.lock(), handler);
        server.serve().context("serve the project protocol")?;
        return Ok(());
    }
    serve_socket(project_path, handler)
}

#[cfg(unix)]
struct SocketGuard(PathBuf);

#[cfg(unix)]
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
struct LockGuard(PathBuf);

#[cfg(unix)]
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .is_ok_and(|status| status.success())
}

/// Serialize startup before touching the socket path. The lock's PID lets a
/// later server distinguish a live/starting owner from a crash leftover.
#[cfg(unix)]
fn acquire_lock(project_path: &Path, socket: &Path, lock: &Path) -> Result<LockGuard> {
    use std::fs::{self, OpenOptions};
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;

    for _ in 0..4 {
        match OpenOptions::new().write(true).create_new(true).open(lock) {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id()).context("write the server lock")?;
                return Ok(LockGuard(lock.to_path_buf()));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                match UnixStream::connect(socket) {
                    Ok(_) => anyhow::bail!(
                        "already serving {} (socket {})",
                        project_path.display(),
                        socket.display()
                    ),
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                        ) =>
                    {
                        let stale = fs::read_to_string(lock)
                            .ok()
                            .and_then(|text| text.trim().parse::<u32>().ok());
                        if stale.is_some_and(pid_alive) {
                            anyhow::bail!(
                                "serving is starting for {} (pid {})",
                                project_path.display(),
                                stale.unwrap_or(0)
                            );
                        }
                        // Re-read before unlink so a lock re-created between
                        // the check and removal is not the one we discard.
                        let unchanged = fs::read_to_string(lock)
                            .ok()
                            .and_then(|text| text.trim().parse::<u32>().ok())
                            == stale;
                        if unchanged {
                            let _ = fs::remove_file(lock);
                        }
                    }
                    Err(e) => return Err(e).context("check the project socket"),
                }
            }
            Err(e) => return Err(e).context("create the server lock"),
        }
    }
    anyhow::bail!(
        "could not acquire the server lock for {} ({})",
        project_path.display(),
        lock.display()
    )
}

#[cfg(unix)]
fn serve_socket(project_path: &Path, handler: ServeHandler) -> Result<()> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};

    let socket = nle_core::state::session_socket_path(project_path);
    let lock = nle_core::state::session_lock_path(project_path);
    let dir = socket
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).context("create the project state directory")?;
    let _lock = acquire_lock(project_path, &socket, &lock)?;
    // With the lock held, an unconnectable socket is stale and safe to
    // replace; a live one belongs to an older/foreign server.
    match UnixStream::connect(&socket) {
        Ok(_) => anyhow::bail!(
            "already serving {} (socket {})",
            project_path.display(),
            socket.display()
        ),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            let _ = fs::remove_file(&socket);
        }
        Err(e) => return Err(e).context("check the project socket"),
    }
    let listener = UnixListener::bind(&socket).context("bind the project socket")?;
    let _guard = SocketGuard(socket.clone());
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .context("set socket permissions")?;
    say_err!("serving {} on {}", project_path.display(), socket.display());

    let shared = Arc::new(Mutex::new(handler));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => spawn_connection(Arc::clone(&shared), stream),
            Err(e) => log::line(&format!("project socket accept failed: {e}")),
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn serve_socket(_project_path: &Path, _handler: ServeHandler) -> Result<()> {
    anyhow::bail!("socket serving is Unix-only; use --stdio")
}

#[cfg(unix)]
fn spawn_connection(shared: Arc<Mutex<ServeHandler>>, stream: std::os::unix::net::UnixStream) {
    let reader = match stream.try_clone() {
        Ok(reader) => reader,
        Err(e) => {
            log::line(&format!("project socket clone failed: {e}"));
            return;
        }
    };
    thread::spawn(move || {
        let handler = ConnectionHandler {
            shared,
            events: Vec::new(),
        };
        let mut server = Server::new(BufReader::new(reader), stream, handler);
        if let Err(e) = server.serve() {
            log::line(&format!("project socket connection ended: {e}"));
        }
    });
}

/// One connection's event buffer. Calls serialize on the shared handler, so
/// events cannot be routed to another client.
#[cfg(unix)]
struct ConnectionHandler {
    shared: Arc<Mutex<ServeHandler>>,
    events: Vec<Event>,
}

#[cfg(unix)]
impl Handler for ConnectionHandler {
    fn handle(&mut self, call: &Call) -> Result<Reply, RpcError> {
        let mut handler = match self.shared.lock() {
            Ok(handler) => handler,
            Err(poisoned) => {
                let mut handler = poisoned.into_inner();
                handler.poisoned = Some("a connection handler panicked".into());
                return Err(RpcError::internal(
                    "server is read-only after a handler panic",
                ));
            }
        };
        let reply = handler.handle(call);
        self.events.extend(handler.take_events());
        reply
    }

    fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
}

/// The live server state. One instance serves one project on one connection.
pub(crate) struct ServeHandler {
    pub(crate) session: Session,
    pub(crate) events: Vec<Event>,
    pub(crate) poisoned: Option<String>,
}

impl Handler for ServeHandler {
    fn handle(&mut self, call: &Call) -> Result<Reply, RpcError> {
        match call {
            Call::Hello => Ok(Reply::Hello(Hello::new(
                self.session.revision(),
                self.session.depth() as u64,
                self.session.project().active_id(),
            ))),
            Call::ProjectShow => Ok(Reply::Project(self.session.project().clone())),
            Call::TimelineEdl { track } => self.edl(*track),
            Call::ActionPreview {
                base_revision,
                actions,
                from,
            } => self.preview(*base_revision, actions, *from),
            Call::ActionApply {
                base_revision,
                actions,
                from,
            } => self.apply(*base_revision, actions, *from),
            Call::Undo { base_revision } => self.apply(*base_revision, &[Action::Undo], None),
            Call::Redo { base_revision } => self.apply(*base_revision, &[Action::Redo], None),
            Call::SequenceList => self.sequence_list(),
            Call::SequenceUse { id } => self.sequence_use(*id),
            Call::SequenceAdd {
                base_revision,
                name,
                fps,
                video,
            } => self.sequence_add(*base_revision, name, *fps, video.clone()),
            Call::MediaList => Ok(Reply::Media(self.media_list())),
            Call::MediaAdd {
                base_revision,
                paths,
            } => self.media_add(*base_revision, paths),
            Call::PlayheadSet { at } => self.playhead_set(*at),
        }
    }

    fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
}

impl ServeHandler {
    /// The base video track is the protocol's active track for playhead
    /// anchoring; the TUI passes the user's active track instead.
    fn active_track(&self) -> usize {
        self.session
            .project()
            .timeline
            .tracks
            .iter()
            .position(|track| track.kind == TrackKind::Video)
            .unwrap_or(0)
    }

    fn check_revision(&self, base_revision: u64) -> Result<(), RpcError> {
        if base_revision == self.session.revision() {
            Ok(())
        } else {
            Err(RpcError::revision_conflict(
                base_revision,
                self.session.revision(),
            ))
        }
    }

    fn ensure_writable(&self) -> Result<(), RpcError> {
        match &self.poisoned {
            Some(reason) => Err(RpcError::internal(format!(
                "server is read-only after a failed save: {reason}"
            ))),
            None => Ok(()),
        }
    }

    fn poison(&mut self, error: impl std::fmt::Display) -> RpcError {
        let message = error.to_string();
        self.poisoned = Some(message.clone());
        RpcError::internal(format!("save failed: {message}"))
    }

    fn save_or_poison(&mut self, changed: bool, playhead_moved: bool) -> Result<(), RpcError> {
        if let Err(e) = self.session.save_if(changed, playhead_moved) {
            return Err(self.poison(e));
        }
        Ok(())
    }

    fn save_project_or_poison(&mut self) -> Result<(), RpcError> {
        if let Err(e) = self.session.save_project() {
            return Err(self.poison(e));
        }
        Ok(())
    }

    /// One place for the wire's common outcome fields. `playhead` is passed
    /// explicitly because apply may reanchor it while preview reports the
    /// action's own result.
    fn wire_outcome(
        &self,
        outcome: ActionOutcome,
        revision: u64,
        playhead: Option<Seconds>,
    ) -> WireOutcome {
        WireOutcome {
            revision,
            depth: self.session.depth() as u64,
            desc: outcome.desc,
            changed: outcome.changed,
            playhead,
        }
    }

    /// The undo anchor: the request's `from`, or the shared playhead.
    fn anchor_seconds(&self, from: Option<Seconds>) -> f64 {
        from.map(|t| secs_f64(&t))
            .unwrap_or_else(|| secs_f64(&self.session.project().playhead))
    }

    /// A history frame as exact timeline seconds. Without an fps the model's
    /// frame is a second (the same fallback `frame_to_secs` uses).
    fn frame_seconds(&self, frame: i64) -> Seconds {
        match self.session.project().fps.as_ref() {
            Some(fps) => frames_to_seconds(frame, *fps),
            None => Seconds::from_integer(frame),
        }
    }

    /// Move the shared cursor. Session state: no revision/CAS, but the
    /// project file is saved so a later CLI/TUI opens there.
    fn playhead_set(&mut self, at: Seconds) -> Result<Reply, RpcError> {
        self.ensure_writable()?;
        self.session.set_playhead(secs_f64(&at));
        self.save_project_or_poison()?;
        let at = self.session.project().playhead;
        self.events.push(Event::Playhead { at });
        Ok(Reply::Playhead(PlayheadReply {
            revision: self.session.revision(),
            depth: self.session.depth() as u64,
            at,
        }))
    }

    fn sequence_list(&self) -> Result<Reply, RpcError> {
        Ok(Reply::Sequences(SequenceList::of(self.session.project())))
    }

    /// Switch the edit scope. `active` is session state: the revision does
    /// not move, but the project file is written so a later CLI/TUI session
    /// opens on the same sequence. The adopted playhead is reported.
    fn sequence_use(&mut self, id: u32) -> Result<Reply, RpcError> {
        self.ensure_writable()?;
        let before_id = self.session.project().active_id();
        let before = self.session.project().playhead;
        if let Err(e) = self.session.select_sequence(id) {
            return Err(RpcError::invalid_params(e.to_string()));
        }
        let after = self.session.project().playhead;
        if let Err(error) = self.save_project_or_poison() {
            // The call failed, so reads must keep reporting the sequence the
            // client was already editing. The server remains poisoned.
            let _ = self.session.select_sequence(before_id);
            return Err(error);
        }
        let revision = self.session.revision();
        let active = self.session.project().active_id();
        let sequence = SequenceSummary::of(self.session.project().active_sequence());
        self.events
            .push(Event::SequenceChanged { active, revision });
        if after != before {
            self.events.push(Event::Playhead { at: after });
        }
        Ok(Reply::Sequence(SequenceReply {
            revision,
            active,
            sequence,
        }))
    }

    /// Add an empty sequence. A model write: CAS, revision bump, save.
    fn sequence_add(
        &mut self,
        base_revision: u64,
        name: &str,
        fps: Option<Seconds>,
        video: Option<nle_core::VideoSettings>,
    ) -> Result<Reply, RpcError> {
        self.ensure_writable()?;
        self.check_revision(base_revision)?;
        let id = self
            .session
            .add_sequence(name, fps, video)
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        self.save_or_poison(true, false)?;
        let revision = self.session.revision();
        let active = self.session.project().active_id();
        let sequence = SequenceSummary::of(
            self.session
                .project()
                .sequence(id)
                .expect("the sequence was just added"),
        );
        self.events.push(Event::ProjectChanged { revision });
        self.events
            .push(Event::SequenceChanged { active, revision });
        Ok(Reply::Sequence(SequenceReply {
            revision,
            active,
            sequence,
        }))
    }

    fn media_list(&self) -> MediaList {
        let project = self.session.project();
        MediaList {
            revision: project.revision,
            assets: project
                .media
                .iter()
                .map(|asset| media_asset(project, asset))
                .collect(),
        }
    }

    /// Probe and pool files as one undo step. Everything is validated and
    /// probed before the first mutation, so one bad path leaves the pool
    /// untouched. Already-pooled paths are returned without another add.
    fn media_add(&mut self, base_revision: u64, paths: &[PathBuf]) -> Result<Reply, RpcError> {
        self.ensure_writable()?;
        self.check_revision(base_revision)?;
        if paths.is_empty() {
            return Err(RpcError::invalid_params(
                "media.add needs at least one path",
            ));
        }
        let mut canonical: Vec<PathBuf> = Vec::new();
        for raw in paths {
            let expanded = crate::cmd::import::expand_tilde(raw);
            let path = std::fs::canonicalize(&expanded)
                .map_err(|e| RpcError::invalid_params(format!("{}: {e}", expanded.display())))?;
            if !canonical.contains(&path) {
                canonical.push(path);
            }
        }
        let mut fresh = Vec::new();
        for path in &canonical {
            if self.session.project().asset_by_path(path).is_some() {
                continue;
            }
            let report = probe(path)
                .map_err(|e| RpcError::invalid_params(format!("{}: {e}", path.display())))?;
            fresh.push((path.clone(), report));
        }
        let playhead = frame_of(
            self.session.project().fps.as_ref(),
            secs_f64(&self.session.project().playhead),
        );
        let ids = self
            .session
            .add_assets(fresh, playhead)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        if !ids.is_empty() {
            self.save_or_poison(true, false)?;
            self.events.push(Event::ProjectChanged {
                revision: self.session.revision(),
            });
        }
        let project = self.session.project();
        let assets = canonical
            .iter()
            .filter_map(|path| project.asset_by_path(path))
            .map(|asset| media_asset(project, asset))
            .collect();
        Ok(Reply::Media(MediaList {
            revision: project.revision,
            assets,
        }))
    }

    fn edl(&self, track: Option<usize>) -> Result<Reply, RpcError> {
        let text = match track {
            Some(ti) => {
                if self.session.project().timeline.track(ti).is_none() {
                    return Err(RpcError::invalid_params(format!(
                        "no track {ti} (the protocol uses 0-based tracks)"
                    )));
                }
                crate::preview::build_edl(self.session.project(), self.session.path(), ti)
            }
            None => {
                crate::preview::build_composite_edl(self.session.project(), self.session.path())
            }
        }
        .map_err(|e| RpcError::internal(format!("EDL: {e}")))?;
        Ok(Reply::Edl(text))
    }

    /// Run actions against a copy. `Session::preview_actions` clones the
    /// history so an undo/redo preview is real, and the register so a preview
    /// yank cannot touch the live clipboard. Nothing is saved and no events
    /// are queued.
    fn preview(
        &mut self,
        base_revision: u64,
        actions: &[Action],
        from: Option<Seconds>,
    ) -> Result<Reply, RpcError> {
        self.check_revision(base_revision)?;
        let from = self.anchor_seconds(from);
        let preview = match actions {
            [action] => self.session.preview(action, from),
            _ => self.session.preview_actions(actions, from),
        }
        .map_err(edit_rpc)?;
        let playhead = preview
            .outcome
            .playhead
            .map(|frame| self.frame_seconds(frame));
        let outcome = self.wire_outcome(preview.outcome, self.session.revision(), playhead);
        Ok(Reply::Preview(Preview {
            outcome,
            diff: preview.diff,
            warnings: preview.warnings,
        }))
    }

    /// The only action mutation path: CAS, edit, playhead, save, events.
    fn apply(
        &mut self,
        base_revision: u64,
        actions: &[Action],
        from: Option<Seconds>,
    ) -> Result<Reply, RpcError> {
        self.ensure_writable()?;
        self.check_revision(base_revision)?;

        // Match the TUI's media-frame policy on the base video track: an edit
        // that moves material under the playhead keeps the same source frame,
        // falling back to the seam when the clip disappears.
        let before = self.session.project().playhead;
        let current = secs_f64(&before);
        let track = self.active_track();
        let anchor = playhead::edit_anchor(self.session.project(), current, track);

        let outcome = self
            .session
            .apply_actions(actions, self.anchor_seconds(from))
            .map_err(edit_rpc)?
            .outcome;

        // A single action keeps the media-frame reanchor policy. A batch has
        // no one edge to follow, so it lands on its last explicit playhead or
        // clamps the cursor into the new length.
        let single = (actions.len() == 1).then(|| &actions[0]);
        let mut landed = None;
        if let Some(frame) = outcome.playhead {
            let at = self.frame_seconds(frame);
            self.session.set_playhead(secs_f64(&at));
            landed = Some(self.session.project().playhead);
        } else if outcome.changed && single.is_some_and(playhead::reanchors) {
            let pos = playhead::reanchor(self.session.project(), current, track, anchor);
            self.session.set_playhead(pos);
            if self.session.project().playhead != before {
                landed = Some(self.session.project().playhead);
            }
        } else if outcome.changed {
            // Track/pool edits leave the active media alone; just clamp.
            let clamped = current.clamp(0.0, secs_f64(&self.session.project().last_frame()));
            if clamped != current {
                self.session.set_playhead(clamped);
                landed = Some(self.session.project().playhead);
            }
        }
        let playhead = landed;

        self.save_or_poison(outcome.changed, playhead.is_some())?;

        let revision = self.session.revision();
        if outcome.changed {
            self.events.push(Event::ProjectChanged { revision });
        }
        if let Some(at) = playhead {
            self.events.push(Event::Playhead { at });
        }
        Ok(Reply::Outcome(
            self.wire_outcome(outcome, revision, playhead),
        ))
    }
}

fn media_asset(project: &Project, asset: &Asset) -> MediaAsset {
    let (width, height) = match asset.video_size() {
        Some((w, h)) => (Some(w), Some(h)),
        None => (None, None),
    };
    MediaAsset {
        id: asset.id,
        path: asset.path.clone(),
        duration: asset.duration(),
        width,
        height,
        fps: asset.fps(),
        audio: asset.probe.has_audio(),
        used_by: project
            .asset_usage_scoped(asset.id)
            .into_iter()
            .map(|(sequence, segment)| MediaUsage { sequence, segment })
            .collect(),
    }
}

fn edit_rpc(error: EditError) -> RpcError {
    match &error {
        EditError::NothingToUndo | EditError::NothingToRedo => {
            RpcError::invalid_request(error.to_string())
        }
        EditError::History(_) => RpcError::internal(error.to_string()),
        _ => RpcError::invalid_params(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_core::Project;

    fn handler() -> ServeHandler {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.json");
        let mut project = Project::new();
        project.fps = Some(Seconds::from_integer(30));
        project.save(&path).expect("save");
        let session = Session::load(&path).expect("load");
        ServeHandler {
            session,
            events: Vec::new(),
            poisoned: None,
        }
    }

    #[test]
    fn a_stale_base_revision_is_a_conflict() {
        let h = handler();
        let error = h.check_revision(9).expect_err("stale");
        assert_eq!(error.code, nle_protocol::REVISION_CONFLICT);
    }

    #[test]
    fn frames_convert_with_the_project_rate() {
        let h = handler();
        assert_eq!(h.frame_seconds(30), Seconds::from_integer(1));
        assert_eq!(h.frame_seconds(15), Seconds::new(1, 2));
    }

    #[test]
    fn a_save_failure_poisons_mutations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.json");
        Project::new().save(&path).expect("save");
        let session = Session::load(&path).expect("load");
        drop(dir); // the project directory is gone, so the next save fails
        let mut handler = ServeHandler {
            session,
            events: Vec::new(),
            poisoned: None,
        };

        let error = handler
            .apply(
                0,
                &[Action::TrackAdd {
                    kind: nle_core::TrackKind::Audio,
                }],
                None,
            )
            .expect_err("the save must fail");
        assert_eq!(error.code, nle_protocol::INTERNAL_ERROR);
        assert!(handler.poisoned.is_some(), "mutations are poisoned");

        let error = handler
            .apply(
                1,
                &[Action::TrackAdd {
                    kind: nle_core::TrackKind::Audio,
                }],
                None,
            )
            .expect_err("poisoned");
        assert_eq!(error.code, nle_protocol::INTERNAL_ERROR);
        assert!(error.message.contains("read-only"), "{}", error.message);

        assert!(matches!(
            handler.handle(&Call::ProjectShow),
            Ok(Reply::Project(_))
        ));
    }

    #[test]
    fn nothing_to_undo_is_an_invalid_request() {
        assert_eq!(
            edit_rpc(EditError::NothingToUndo).code,
            nle_protocol::INVALID_REQUEST
        );
        assert_eq!(
            edit_rpc(EditError::NoClip).code,
            nle_protocol::INVALID_PARAMS
        );
    }
}
