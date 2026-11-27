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

use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::{log, say_err};

use nle_core::analysis::{AnalysisError, Transcript, TranscriptStore, Word};
use nle_core::edit::Actor;
use nle_core::{
    Action, Asset, AudioSettings, EditError, Outcome as ActionOutcome, Project, Seconds, TrackKind,
    frame_of, frames_to_seconds, secs_f64,
};
use nle_ffmpeg::probe;
use nle_protocol::{
    AnalysisHash, AnnotationList, Call, Event, Handler, Hello, JobList, MediaAsset, MediaList,
    MediaUsage, Outcome as WireOutcome, PlayheadReply, Preview, Reply, RpcError, SequenceList,
    SequenceReply, SequenceSummary, Server,
};

use crate::jobs::{JobQueue, QueueError};
use crate::session::{Session, playhead};

/// Serve the project over the per-project Unix socket, or stdin/stdout when
/// `--stdio` is set. The socket is the default so a TUI and an agent can
/// share one writer; the stdio mode is for pipes and tests.
///
/// Socket mode binds and locks **before** the handler exists, so a second
/// server cannot recover jobs or start a dispatcher just to fail at bind.
/// `--stdio` is deliberately unguarded: it has no singleton gate, and callers
/// (tests, pipes) are responsible for using separate projects.
pub(crate) fn cmd_serve(project_path: &Path, stdio: bool) -> Result<()> {
    if stdio {
        let handler = build_handler(project_path)?;
        let stdout = io::stdout();
        // Same partial-frame policy as a socket connection: the pump makes
        // stdin pollable, so a frame with no newline cannot wedge the process.
        let reader = BufReader::new(FrameDeadline::new(
            StdinPump::spawn()?,
            PARTIAL_FRAME_DEADLINE,
            FRAME_POLL,
        ));
        let mut server = Server::new(reader, stdout.lock(), handler);
        match server.serve() {
            Ok(()) => {}
            Err(nle_protocol::ServerError::Io(e)) if e.kind() == io::ErrorKind::TimedOut => {
                log::line(&format!(
                    "stdio connection closed: partial frame exceeded \
                     {PARTIAL_FRAME_DEADLINE:?} without a newline"
                ));
            }
            Err(e) => return Err(e).context("serve the project protocol"),
        }
        return Ok(());
    }
    #[cfg(unix)]
    {
        let bound = bind_socket(project_path)?;
        let handler = build_handler(project_path)?;
        serve_bound(bound, handler)
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("socket serving is Unix-only; use --stdio")
    }
}

/// Load the session and start its queue. Only called once the socket is ours
/// (or immediately for `--stdio`).
fn build_handler(project_path: &Path) -> Result<ServeHandler> {
    let mut session = Session::load(project_path)?;
    session.set_actor(Actor::Agent("nile-serve".into()));
    let jobs = JobQueue::start(project_path).context("open the job queue")?;
    Ok(ServeHandler {
        session,
        events: Vec::new(),
        poisoned: None,
        jobs,
    })
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

/// Default grace period between the first byte of a frame and its newline.
const PARTIAL_FRAME_DEADLINE: Duration = Duration::from_secs(5);

/// Read timeout. The adapter wakes this often to poll for the partial
/// deadline; it is not an idle timeout.
const FRAME_POLL: Duration = Duration::from_millis(100);

/// A `Read` adapter that leaves idle connections alone but gives up on a
/// half-received frame.
///
/// A blocking `UnixStream` read with no timeout wedges its connection thread
/// forever when a peer sends `{"id":1` and then stops. A coarse read timeout
/// is wrong: it would also disconnect an idle TUI or agent. So the socket
/// gets a short `set_read_timeout` (the poll), and this adapter swallows
/// `WouldBlock`/`TimedOut` while no frame bytes are pending. Once bytes since
/// the last newline have arrived, their `deadline` starts ticking; on
/// expiry it returns `TimedOut`, which ends only this connection's serve loop.
///
/// The deadline is checked after a successful read as well as after a poll:
/// a peer that trickles a byte just inside `poll` would otherwise keep every
/// read in the `Ok` arm and never reach the check.
///
/// The inner reader is a type parameter so tests can script reads instead of
/// waiting on a real peer. `new` takes both durations so tests use tens of
/// milliseconds.
struct FrameDeadline<S> {
    inner: S,
    deadline: Duration,
    poll: Duration,
    /// Start of the current unterminated frame, or `None` between frames.
    partial_since: Option<Instant>,
}

impl<S> FrameDeadline<S> {
    fn new(inner: S, deadline: Duration, poll: Duration) -> Self {
        Self {
            inner,
            deadline,
            poll,
            partial_since: None,
        }
    }

    /// Track frame state from the bytes just read. A chunk ending in a newline
    /// starts a fresh (empty) frame; one with a newline in the middle starts
    /// the trailing frame now; a chunk with no newline keeps the original
    /// start, so a trickle cannot postpone the deadline.
    fn observe(&mut self, bytes: &[u8], now: Instant) {
        let Some(&last) = bytes.last() else { return };
        if last == b'\n' {
            self.partial_since = None;
        } else if bytes.contains(&b'\n') || self.partial_since.is_none() {
            self.partial_since = Some(now);
        }
    }

    /// Whether the pending frame has outlived its grace period. Time-based,
    /// not read-count-based, so a byte every 50 ms cannot reset it.
    fn expired(&self) -> bool {
        self.partial_since
            .is_some_and(|start| start.elapsed() >= self.deadline)
    }

    fn deadline_error(&self) -> io::Error {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "partial frame: no newline within {:?} (closing this connection)",
                self.deadline
            ),
        )
    }
}

impl<S: Read> Read for FrameDeadline<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.inner.read(buf) {
                Ok(read) if read > 0 => {
                    // Check the frame that was pending *before* this read: a
                    // newline arriving after the deadline does not rescue it,
                    // and bytes without one cannot move the start.
                    if self.expired() {
                        return Err(self.deadline_error());
                    }
                    self.observe(&buf[..read], Instant::now());
                    return Ok(read);
                }
                Ok(_) => return Ok(0),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if self.expired() {
                        return Err(self.deadline_error());
                    }
                    thread::sleep(self.poll);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// A pollable stdin. `Stdin` has no `set_read_timeout`, so a helper thread owns
/// the blocking read and forwards each non-empty chunk over a one-slot channel;
/// an empty channel is `WouldBlock` and a closed one is EOF. That lets
/// [`FrameDeadline`] apply the same partial-frame policy to `--stdio` as to a
/// socket without ever letting an idle pipe wedge the process.
struct StdinPump {
    chunks: std::sync::mpsc::Receiver<io::Result<Vec<u8>>>,
    pending: Vec<u8>,
    eof: bool,
}

impl StdinPump {
    fn spawn() -> Result<Self> {
        let (tx, chunks) = std::sync::mpsc::sync_channel(1);
        thread::Builder::new()
            .name("nile-stdin".into())
            .spawn(move || {
                let stdin = io::stdin();
                let mut stdin = stdin.lock();
                let mut buf = [0u8; 8192];
                loop {
                    match stdin.read(&mut buf) {
                        Ok(0) => break,
                        Ok(read) => {
                            if tx.send(Ok(buf[..read].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            break;
                        }
                    }
                }
            })
            .context("spawn the stdin pump")?;
        Ok(Self {
            chunks,
            pending: Vec::new(),
            eof: false,
        })
    }
}

impl Read for StdinPump {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if !self.pending.is_empty() {
                let read = buf.len().min(self.pending.len());
                buf[..read].copy_from_slice(&self.pending[..read]);
                self.pending.drain(..read);
                return Ok(read);
            }
            if self.eof {
                return Ok(0);
            }
            match self.chunks.try_recv() {
                Ok(Ok(chunk)) => self.pending = chunk,
                Ok(Err(e)) => return Err(e),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.eof = true,
            }
        }
    }
}

#[cfg(unix)]
struct BoundSocket {
    listener: std::os::unix::net::UnixListener,
    /// Removes the socket/lock on drop, including a handler-build failure.
    _socket: SocketGuard,
    _lock: LockGuard,
}

#[cfg(unix)]
fn bind_socket(project_path: &Path) -> Result<BoundSocket> {
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
    let lock = acquire_lock(project_path, &socket, &lock)?;
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
    let guard = SocketGuard(socket.clone());
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .context("set socket permissions")?;
    say_err!("serving {} on {}", project_path.display(), socket.display());
    Ok(BoundSocket {
        listener,
        _socket: guard,
        _lock: lock,
    })
}

#[cfg(unix)]
fn serve_bound(bound: BoundSocket, handler: ServeHandler) -> Result<()> {
    let shared = Arc::new(Mutex::new(handler));
    for stream in bound.listener.incoming() {
        match stream {
            Ok(stream) => spawn_connection(Arc::clone(&shared), stream),
            Err(e) => log::line(&format!("project socket accept failed: {e}")),
        }
    }
    Ok(())
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
    // The poll interval is the socket read timeout; the adapter retries while
    // the connection is idle and enforces the partial-frame deadline itself.
    if let Err(e) = reader.set_read_timeout(Some(FRAME_POLL)) {
        log::line(&format!("project socket timeout setup failed: {e}"));
        return;
    }
    thread::spawn(move || {
        let handler = ConnectionHandler {
            shared,
            events: Vec::new(),
        };
        let reader = FrameDeadline::new(reader, PARTIAL_FRAME_DEADLINE, FRAME_POLL);
        let mut server = Server::new(BufReader::new(reader), stream, handler);
        match server.serve() {
            Ok(()) => {}
            Err(nle_protocol::ServerError::Io(e)) if e.kind() == io::ErrorKind::TimedOut => {
                log::line(&format!(
                    "project socket connection closed: partial frame exceeded \
                     {PARTIAL_FRAME_DEADLINE:?} without a newline"
                ));
            }
            Err(e) => log::line(&format!("project socket connection ended: {e}")),
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
    /// The durable job queue this server owns. `nile serve` is the only
    /// process that writes it, so no file lock is needed here.
    pub(crate) jobs: std::sync::Arc<JobQueue>,
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
            Call::SequenceAudio {
                base_revision,
                audio,
            } => self.sequence_audio(*base_revision, *audio),
            Call::MediaList => Ok(Reply::Media(self.media_list())),
            Call::MediaAdd {
                base_revision,
                paths,
            } => self.media_add(*base_revision, paths),
            Call::PlayheadSet { at } => self.playhead_set(*at),
            Call::AnnotationList { sequence } => self.annotation_list(*sequence),
            Call::AnnotationCheck {
                report,
                sequence,
                id,
            } => self.annotation_check(report, *sequence, id),
            Call::AnalysisAdd { asset, words } => self.analysis_add(*asset, words.clone()),
            Call::JobSubmit {
                base_revision,
                output,
                sequence,
                export,
            } => self.job_submit(*base_revision, output, *sequence, export.clone()),
            Call::JobList => self.job_list(),
            Call::JobGet { id } => self.job_get(*id),
            Call::JobCancel { id } => self.job_cancel(*id),
            Call::JobRemove { id } => self.job_remove(*id),
            Call::JobPrune {
                keep,
                older_than_days,
            } => self.job_prune(*keep, *older_than_days),
        }
    }

    fn take_events(&mut self) -> Vec<Event> {
        // The queue's worker emits progress between calls; merge those into
        // this call's reply. An error reply drops them (the Server contract),
        // but `job.list` re-reads the durable state, so nothing is lost.
        let mut events = std::mem::take(&mut self.events);
        events.extend(self.jobs.take_events());
        events
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

    fn annotation_list(&self, sequence: Option<u32>) -> Result<Reply, RpcError> {
        let project = self.session.project();
        let sequence = sequence.unwrap_or_else(|| project.active_id());
        if project.sequence(sequence).is_none() {
            return Err(RpcError::invalid_params(format!(
                "sequence #{sequence} not found"
            )));
        }
        Ok(Reply::Annotations(AnnotationList::of(project, sequence)))
    }

    /// Evaluate a render report against the sequence's expectations. A read:
    /// no save, no revision bump, no history entry and no event. The seam's
    /// errors (missing/corrupt report, a sequence the report did not render,
    /// an unknown sequence) are the request's fault, not the server's.
    ///
    /// The path must be absolute (after `~` expansion): the server's cwd is
    /// its own, so a relative path would silently name a different file than
    /// the client meant.
    fn annotation_check(
        &self,
        report: &Path,
        sequence: Option<u32>,
        ids: &[u32],
    ) -> Result<Reply, RpcError> {
        let report = crate::cmd::import::expand_tilde(report);
        if !report.is_absolute() {
            return Err(RpcError::invalid_params(
                "report path must be absolute (the server's cwd may differ from the client's)",
            ));
        }
        let checks = crate::cmd::check::check_with(
            self.session.project(),
            self.session.path(),
            &report,
            sequence,
            ids,
        )
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        Ok(Reply::Checks(checks))
    }

    /// Store a transcript in the project's analysis state and return its
    /// content address. Analysis state is beside the project, not in it: no
    /// save, revision bump, history entry or event.
    fn analysis_add(&self, asset: u32, words: Vec<Word>) -> Result<Reply, RpcError> {
        let media = self.session.project().asset(asset).ok_or_else(|| {
            RpcError::invalid_params(format!("asset #{asset} is not in the media pool"))
        })?;
        let transcript = Transcript::new(asset, words);
        transcript
            .validate(media.probe.duration())
            .map_err(analysis_error)?;
        let hash = TranscriptStore::at(self.session.path())
            .import(&transcript)
            .map_err(analysis_error)?;
        Ok(Reply::Analysis(AnalysisHash { hash }))
    }

    /// Queue an export of the project **as it is now**. CAS guards that the
    /// client named the revision it saw; the snapshot is written before the
    /// record, so an edit after submit cannot change this render.
    fn job_submit(
        &mut self,
        base_revision: u64,
        output: &Path,
        sequence: u32,
        export: nle_core::jobs::JobExport,
    ) -> Result<Reply, RpcError> {
        self.check_revision(base_revision)?;
        let output = crate::cmd::import::expand_tilde(output);
        if !output.is_absolute() {
            return Err(RpcError::invalid_params(
                "job output must be absolute (the server's cwd may differ from the client's)",
            ));
        }
        if self.session.project().sequence(sequence).is_none() {
            return Err(RpcError::invalid_params(format!(
                "sequence #{sequence} not found"
            )));
        }
        if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty())
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Err(RpcError::internal(format!(
                "create {}: {e}",
                parent.display()
            )));
        }
        let job = self
            .jobs
            .submit(self.session.project(), sequence, output, export)
            .map_err(queue_rpc)?;
        Ok(Reply::Job(job))
    }

    fn job_list(&self) -> Result<Reply, RpcError> {
        let jobs = self.jobs.list().map_err(queue_rpc)?;
        Ok(Reply::Jobs(JobList::new(self.session.revision(), jobs)))
    }

    fn job_get(&self, id: u64) -> Result<Reply, RpcError> {
        Ok(Reply::Job(self.jobs.get(id).map_err(queue_rpc)?))
    }

    fn job_cancel(&self, id: u64) -> Result<Reply, RpcError> {
        Ok(Reply::Job(self.jobs.cancel(id).map_err(queue_rpc)?))
    }

    /// Retention: delete a terminal job's record + snapshot. The removed
    /// record is returned so a client can report what went away.
    fn job_remove(&self, id: u64) -> Result<Reply, RpcError> {
        Ok(Reply::Job(self.jobs.remove(id).map_err(queue_rpc)?))
    }

    /// Retention across jobs: the queue owns the plan (terminal only), so a
    /// stale client cannot name a running job here. A failed directory is
    /// reported alongside the ids that were removed, not as a lost result.
    fn job_prune(&self, keep: u64, older_than_days: Option<u64>) -> Result<Reply, RpcError> {
        let report = self.jobs.prune(keep, older_than_days).map_err(queue_rpc)?;
        Ok(Reply::Pruned(nle_protocol::JobPruneReply {
            removed: report.removed,
            failed: report
                .failed
                .into_iter()
                .map(|(id, error)| nle_protocol::JobPruneFailure { id, error })
                .collect(),
        }))
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

    /// Set the active sequence's audio post. A model write: CAS, revision
    /// bump, save, and the same `sequence.changed` event as use/add.
    fn sequence_audio(
        &mut self,
        base_revision: u64,
        audio: AudioSettings,
    ) -> Result<Reply, RpcError> {
        self.ensure_writable()?;
        self.check_revision(base_revision)?;
        self.session
            .set_audio(audio)
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        self.save_or_poison(true, false)?;
        let revision = self.session.revision();
        let active = self.session.project().active_id();
        let sequence = SequenceSummary::of(self.session.project().active_sequence());
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

        let batch = self
            .session
            .apply_actions(actions, self.anchor_seconds(from))
            .map_err(edit_rpc)?;
        let mut outcome = batch.outcome;
        if !batch.warnings.is_empty() {
            outcome.desc = format!("{} — {}", outcome.desc, batch.warnings.join("; "));
        }

        // A single action keeps the media-frame reanchor policy. A batch has
        // no one edge to follow, so it lands on its last explicit playhead or
        // clamps the cursor into the new length.
        let single = (actions.len() == 1).then(|| &actions[0]);
        // An explicit frame is always reported; a re-anchored or clamped
        // cursor only when it actually moved (a split reports no playhead).
        let explicit = outcome.playhead.is_some();
        let target = if let Some(action) = single {
            // One policy for every front-end: the TUI, this server and the CLI.
            playhead::land(
                self.session.project(),
                current,
                track,
                anchor,
                action,
                &outcome,
            )
        } else if let Some(frame) = outcome.playhead {
            // A batch has no one edge to follow: its last explicit playhead.
            Some(secs_f64(&self.frame_seconds(frame)))
        } else if outcome.changed {
            Some(current.clamp(0.0, secs_f64(&self.session.project().last_frame())))
        } else {
            None
        };
        let playhead = target.and_then(|at| {
            self.session.set_playhead(at);
            let now = self.session.project().playhead;
            (explicit || now != before).then_some(now)
        });

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

/// A store failure is the server's; an unknown/terminal job id is the
/// caller's (the same split as `media.add` and `annotation.check`).
fn queue_rpc(error: QueueError) -> RpcError {
    match error {
        QueueError::Invalid(message) => RpcError::invalid_params(message),
        QueueError::Store(e) => RpcError::internal(e.to_string()),
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

/// Analysis input errors are the caller's; store failures are the server's.
/// The split matches `media.add`: a bad word is invalid params, a read-only or
/// corrupt analysis store is internal, not a request to rewrite the words.
fn analysis_error(error: AnalysisError) -> RpcError {
    let message = error.to_string();
    match error {
        AnalysisError::Invalid(_) => RpcError::invalid_params(message),
        _ => RpcError::internal(message),
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
        let jobs = JobQueue::start(&path).expect("queue");
        ServeHandler {
            session,
            events: Vec::new(),
            poisoned: None,
            jobs,
        }
    }

    #[test]
    fn a_stale_base_revision_is_a_conflict() {
        let h = handler();
        let error = h.check_revision(9).expect_err("stale");
        assert_eq!(error.code, nle_protocol::REVISION_CONFLICT);
    }

    #[test]
    fn analysis_store_failures_are_internal_not_the_callers_fault() {
        let invalid = analysis_error(AnalysisError::Invalid("bad word".to_string()));
        assert_eq!(invalid.code, nle_protocol::INVALID_PARAMS);
        assert!(invalid.message.contains("bad word"));

        let io = analysis_error(AnalysisError::Io(std::io::Error::other("read-only")));
        assert_eq!(io.code, nle_protocol::INTERNAL_ERROR);
    }

    #[test]
    fn frames_convert_with_the_project_rate() {
        let h = handler();
        assert_eq!(h.frame_seconds(30), Seconds::from_integer(1));
        assert_eq!(h.frame_seconds(15), Seconds::new(1, 2));
    }

    fn handler_with_runner(
        run: std::sync::Arc<crate::jobs::JobRunner>,
    ) -> (tempfile::TempDir, ServeHandler) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.json");
        let mut project = Project::new();
        project.fps = Some(Seconds::from_integer(30));
        project.save(&path).expect("save");
        let session = Session::load(&path).expect("load");
        let jobs = JobQueue::start_with(&path, run).expect("queue");
        let handler = ServeHandler {
            session,
            events: Vec::new(),
            poisoned: None,
            jobs,
        };
        (dir, handler)
    }

    fn quick_runner() -> std::sync::Arc<crate::jobs::JobRunner> {
        std::sync::Arc::new(|_store, _job, _cancel, _progress| Ok(None))
    }

    fn wait_done(handler: &ServeHandler, id: u64) -> nle_core::JobRecord {
        for _ in 0..200 {
            let job = handler.jobs.get(id).expect("get");
            if job.state.is_terminal() {
                return job;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("job #{id} never reached a terminal state");
    }

    #[test]
    fn a_job_submit_snapshots_the_project_and_reports_progress() {
        let (dir, mut handler) = handler_with_runner(quick_runner());
        let output = dir.path().join("out.mp4");
        let revision = handler.session.revision();
        let active = handler.session.project().active_id();
        let reply = handler
            .job_submit(
                revision,
                &output,
                active,
                nle_core::jobs::JobExport::default(),
            )
            .expect("submit");
        let Reply::Job(queued) = reply else {
            panic!("expected a job");
        };
        assert_eq!(queued.state, nle_core::JobState::Queued);
        assert_eq!(queued.revision, revision);
        let store = nle_core::JobStore::at(handler.session.path());
        let snapshot = store.load_snapshot(&queued).expect("snapshot");
        assert_eq!(snapshot.revision, revision);
        assert_eq!(snapshot.active_id(), active);

        let done = wait_done(&handler, queued.id);
        assert_eq!(done.state, nle_core::JobState::Done);
        let events = handler.take_events();
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::JobChanged {
                    id,
                    state: nle_core::JobState::Done,
                    ..
                } if *id == queued.id
            )),
            "the terminal event is emitted: {events:?}"
        );

        let Reply::Jobs(list) = handler.job_list().expect("list") else {
            panic!("expected jobs");
        };
        assert_eq!(list.jobs.len(), 1);
        assert_eq!(list.jobs[0].id, queued.id);
    }

    #[test]
    fn job_submit_is_compare_and_swap_and_needs_an_absolute_output() {
        let (dir, mut handler) = handler_with_runner(quick_runner());
        let error = handler
            .job_submit(
                handler.session.revision() + 1,
                &dir.path().join("out.mp4"),
                1,
                nle_core::jobs::JobExport::default(),
            )
            .expect_err("stale revision");
        assert_eq!(error.code, nle_protocol::REVISION_CONFLICT);

        let error = handler
            .job_submit(
                handler.session.revision(),
                Path::new("relative.mp4"),
                1,
                nle_core::jobs::JobExport::default(),
            )
            .expect_err("relative output");
        assert_eq!(error.code, nle_protocol::INVALID_PARAMS);
        assert!(error.message.contains("absolute"));
    }

    #[test]
    fn unknown_job_ids_are_invalid_params() {
        let (_dir, handler) = handler_with_runner(quick_runner());
        let error = handler.job_get(99).expect_err("unknown");
        assert_eq!(error.code, nle_protocol::INVALID_PARAMS);
        assert!(error.message.contains("not found"));
        let error = handler.job_cancel(99).expect_err("unknown");
        assert_eq!(error.code, nle_protocol::INVALID_PARAMS);
        let error = handler.job_remove(99).expect_err("unknown");
        assert_eq!(error.code, nle_protocol::INVALID_PARAMS);
    }

    #[test]
    fn removing_a_terminal_job_deletes_it_and_refuses_a_queued_one() {
        let (dir, mut handler) = handler_with_runner(quick_runner());
        let output = dir.path().join("remove.mp4");
        let created = handler
            .job_submit(
                handler.session.revision(),
                &output,
                handler.session.project().active_id(),
                nle_core::jobs::JobExport::default(),
            )
            .expect("submit");
        let Reply::Job(job) = created else {
            panic!("expected a job");
        };
        let done = wait_done(&handler, job.id);
        assert!(done.state.is_terminal());
        let Reply::Job(removed) = handler.job_remove(job.id).expect("remove") else {
            panic!("expected a job");
        };
        assert_eq!(removed.id, job.id);
        let store = nle_core::JobStore::at(handler.session.path());
        assert!(!store.job_dir(job.id).exists());
        assert!(handler.jobs.get(job.id).is_err(), "the record is gone");
    }

    /// A runner that blocks until the test releases it, so one job is live
    /// while others queue behind it.
    fn gated_runner() -> (
        std::sync::Arc<crate::jobs::JobRunner>,
        std::sync::mpsc::Receiver<u64>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));
        let run: std::sync::Arc<crate::jobs::JobRunner> =
            std::sync::Arc::new(move |_store, job, _cancel, _progress| {
                let _ = started_tx.send(job.id);
                let rx = release_rx.lock().unwrap();
                let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
                Ok(None)
            });
        (run, started_rx, release_tx)
    }

    #[test]
    fn job_prune_removes_terminal_jobs_and_spares_live_ones() {
        let (run, started, release) = gated_runner();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.json");
        let mut project = Project::new();
        project.fps = Some(Seconds::from_integer(30));
        project.save(&path).expect("save");
        let session = Session::load(&path).expect("load");
        let jobs = JobQueue::start_with(&path, run).expect("queue");
        let mut handler = ServeHandler {
            session,
            events: Vec::new(),
            poisoned: None,
            jobs,
        };
        let store = nle_core::JobStore::at(handler.session.path());

        let submit = |handler: &mut ServeHandler, name: &str| {
            let Reply::Job(job) = handler
                .job_submit(
                    handler.session.revision(),
                    &dir.path().join(name),
                    handler.session.project().active_id(),
                    nle_core::jobs::JobExport::default(),
                )
                .expect("submit")
            else {
                panic!("expected a job");
            };
            job
        };
        let running = submit(&mut handler, "running.mp4");
        assert_eq!(
            started
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("job started"),
            running.id
        );
        let queued = submit(&mut handler, "queued.mp4");
        let cancelled = submit(&mut handler, "cancelled.mp4");
        handler.job_cancel(cancelled.id).expect("cancel queued");
        let _ = handler.take_events();

        // Zero keep still spares the running and queued jobs; only the
        // cancelled terminal job goes.
        let Reply::Pruned(reply) = handler.job_prune(0, None).expect("prune") else {
            panic!("expected a prune reply");
        };
        assert_eq!(reply.removed, vec![cancelled.id]);
        assert!(reply.failed.is_empty());
        assert!(!store.job_dir(cancelled.id).exists());
        assert!(store.job_dir(running.id).exists());
        assert!(store.job_dir(queued.id).exists());
        assert!(
            handler.take_events().is_empty(),
            "the pruned job's events are dropped"
        );

        // Finish both live jobs, then keep only the newest.
        release.send(()).unwrap();
        assert_eq!(
            wait_done(&handler, running.id).state,
            nle_core::JobState::Done
        );
        assert_eq!(
            started
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("second job started"),
            queued.id
        );
        release.send(()).unwrap();
        assert_eq!(
            wait_done(&handler, queued.id).state,
            nle_core::JobState::Done
        );
        let Reply::Pruned(reply) = handler.job_prune(1, None).expect("prune again") else {
            panic!("expected a prune reply");
        };
        assert_eq!(reply.removed, vec![running.id]);
        assert!(!store.job_dir(running.id).exists());
        assert!(store.job_dir(queued.id).exists());

        // The wire shape: a successful `job.prune` call carries the reply.
        assert!(matches!(
            handler.handle(&Call::JobPrune {
                keep: 1,
                older_than_days: None
            }),
            Ok(Reply::Pruned(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_second_bind_refuses_before_recovering_jobs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.json");
        let project = Project::new();
        project.save(&path).expect("save");
        let first = bind_socket(&path).expect("first bind");

        // A leftover `running` record is only touched by `JobQueue::start`;
        // the refused second server must not reach it.
        let store = nle_core::JobStore::at(&path);
        let mut job = store
            .create(
                &project,
                1,
                dir.path().join("running.mp4"),
                nle_core::jobs::JobExport::default(),
            )
            .expect("create");
        job.state = nle_core::JobState::Running;
        store.update(&job).expect("update");

        let error = match bind_socket(&path) {
            Ok(_) => panic!("a second bind must refuse"),
            Err(e) => e,
        };
        assert!(error.to_string().contains("already serving"), "{error}");
        assert_eq!(
            store.get(job.id).unwrap().unwrap().state,
            nle_core::JobState::Running,
            "the failed server did not recover the queue"
        );
        drop(first);
    }

    #[test]
    fn a_save_failure_poisons_mutations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.json");
        Project::new().save(&path).expect("save");
        let session = Session::load(&path).expect("load");
        drop(dir); // the project directory is gone, so the next save fails
        let jobs = JobQueue::start(&path).expect("queue");
        let mut handler = ServeHandler {
            session,
            events: Vec::new(),
            poisoned: None,
            jobs,
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

    /// `FrameDeadline` over a scripted peer: `Block` is a socket read timing
    /// out, `Bytes` a short read. Exhausting the script keeps blocking, which
    /// is what an idle peer looks like.
    #[cfg(unix)]
    struct Scripted {
        steps: std::collections::VecDeque<Step>,
    }

    #[cfg(unix)]
    enum Step {
        Block,
        Bytes(&'static [u8]),
    }

    #[cfg(unix)]
    impl Scripted {
        fn new(steps: impl IntoIterator<Item = Step>) -> Self {
            Self {
                steps: steps.into_iter().collect(),
            }
        }
    }

    #[cfg(unix)]
    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.steps.pop_front() {
                Some(Step::Bytes(bytes)) => {
                    buf[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                }
                Some(Step::Block) | None => Err(io::ErrorKind::WouldBlock.into()),
            }
        }
    }

    /// Twenty polls at 2 ms outlive a 10 ms deadline, yet an idle connection
    /// (no frame byte) must never be dropped.
    #[cfg(unix)]
    #[test]
    fn an_idle_connection_never_hits_the_deadline() {
        let mut reader = FrameDeadline::new(
            Scripted::new(
                (0..20)
                    .map(|_| Step::Block)
                    .chain([Step::Bytes(b"hello\n")]),
            ),
            Duration::from_millis(10),
            Duration::from_millis(2),
        );
        let mut buf = [0u8; 64];
        let read = reader.read(&mut buf).expect("idle is allowed");
        assert_eq!(&buf[..read], b"hello\n");
    }

    #[cfg(unix)]
    #[test]
    fn partial_bytes_without_a_newline_time_out() {
        let mut reader = FrameDeadline::new(
            Scripted::new([Step::Bytes(b"{\"id\":1")]),
            Duration::from_millis(20),
            Duration::from_millis(2),
        );
        let mut buf = [0u8; 64];
        let read = reader.read(&mut buf).expect("first chunk");
        assert_eq!(read, 7);
        let error = reader.read(&mut buf).expect_err("half a frame must expire");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("partial frame"), "{error}");
    }

    /// The newline clears the partial state, so the idle stretch after it is
    /// unlimited: a deadline that never reset would fire here.
    #[cfg(unix)]
    #[test]
    fn a_newline_resets_the_partial_deadline() {
        let mut reader = FrameDeadline::new(
            Scripted::new(
                [Step::Bytes(b"{}"), Step::Bytes(b"\n")]
                    .into_iter()
                    .chain((0..20).map(|_| Step::Block))
                    .chain([Step::Bytes(b"next\n")]),
            ),
            Duration::from_millis(10),
            Duration::from_millis(2),
        );
        let mut buf = [0u8; 64];
        assert_eq!(reader.read(&mut buf).expect("first chunk"), 2);
        assert_eq!(reader.read(&mut buf).expect("newline"), 1);
        let read = reader
            .read(&mut buf)
            .expect("idle after a complete line is allowed");
        assert_eq!(&buf[..read], b"next\n");
    }

    /// A peer that writes one byte just inside the poll interval. Every inner
    /// read succeeds, so the `Ok` arm must enforce the deadline itself; only
    /// the `WouldBlock` path would never run.
    #[cfg(unix)]
    struct Trickle {
        gap: Duration,
    }

    #[cfg(unix)]
    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            thread::sleep(self.gap);
            buf[0] = b' ';
            Ok(1)
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_trickle_without_a_newline_still_hits_the_deadline() {
        let mut reader = FrameDeadline::new(
            Trickle {
                gap: Duration::from_millis(2),
            },
            Duration::from_millis(10),
            Duration::from_millis(1),
        );
        let mut buf = [0u8; 64];
        let mut result = Ok(0);
        for _ in 0..200 {
            result = reader.read(&mut buf);
            if result.is_err() {
                break;
            }
        }
        let error = result.expect_err("a trickle cannot postpone the deadline");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("partial frame"), "{error}");
    }
}
