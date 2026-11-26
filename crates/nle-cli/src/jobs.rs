//! The server-owned render queue.
//!
//! `nile serve` is the single writer, so it also owns the job queue: `submit`
//! writes a durable record + project snapshot, a dispatcher thread runs one job
//! at a time in submission order, and cancellation is a flag the export's
//! ffmpeg poll loop checks. The TUI/CLI never run a job themselves.
//!
//! ## Why a thread and a channel
//!
//! The protocol handler answers requests synchronously, but an export takes
//! minutes. The dispatcher blocks on `wake.recv()` between jobs and on
//! ffmpeg inside one; a submit just writes the record and sends a wake. No
//! async runtime, one worker, and the queue's order is the store's id order.
//!
//! ## Progress and events
//!
//! The export callback updates an in-memory `ActiveJob` and queues a
//! `job.progress` event. Progress is throttled and is **not** persisted on
//! every frame: a restart marks a running job interrupted, so the durable
//! record never needs a half-live progress value. Terminal transitions are
//! persisted before `job.changed` is emitted.
//!
//! ## Cancel and partial output
//!
//! The render writes to a hidden temp file beside `output` and renames it only
//! after ffmpeg exits successfully. A cancel kills the child on the worker's
//! thread, removes the temp file and marks the job `cancelled`, so a
//! cancelled job never leaves a half-written deliverable at the output path.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use nle_core::jobs::{
    JobError, JobExport, JobProgress, JobRecord, JobState, JobStore, now_secs, partial_path,
};
use nle_core::{Project, RenderReport, Seconds, secs_f64};

use nle_ffmpeg::FfmpegError;
use nle_protocol::Event;

use crate::cmd::export::{ExportOptions, QualityPreset};
use crate::log;

/// Why a single render stopped. Cancellation is not a failure: the queue maps
/// it to `JobState::Cancelled`.
#[derive(Debug)]
pub(crate) enum JobRunError {
    Cancelled,
    Failed(String),
}

/// The seam between the queue and the renderer. A test runner replaces this so
/// FIFO/cancel/recovery can be tested without ffmpeg or media.
pub(crate) type JobRunner = dyn Fn(
        &JobStore,
        &JobRecord,
        &AtomicBool,
        &mut dyn FnMut(u64, u64),
    ) -> Result<Option<PathBuf>, JobRunError>
    + Send
    + Sync;

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueError {
    #[error(transparent)]
    Store(#[from] JobError),
    #[error("{0}")]
    Invalid(String),
}

struct ActiveJob {
    id: u64,
    cancel: Arc<AtomicBool>,
    progress: JobProgress,
}

#[derive(Default)]
struct QueueState {
    active: Option<ActiveJob>,
    events: Vec<Event>,
}

pub(crate) struct JobQueue {
    store: JobStore,
    state: Mutex<QueueState>,
    wake: Sender<()>,
    run: Arc<JobRunner>,
}

impl JobQueue {
    /// Open the durable store, recover it, and start the dispatcher. The
    /// caller keeps the `Arc` for the server's lifetime.
    pub(crate) fn start(project_path: &Path) -> Result<Arc<Self>, QueueError> {
        Self::start_with(project_path, Arc::new(run_export_job))
    }

    /// [`JobQueue::start`] with an injected renderer (tests).
    pub(crate) fn start_with(
        project_path: &Path,
        run: Arc<JobRunner>,
    ) -> Result<Arc<Self>, QueueError> {
        let store = JobStore::at(project_path);
        let recovery = store.recover()?;
        for skipped in &recovery.skipped {
            log::line(&format!(
                "job queue: skipping unreadable record ({skipped}); directory kept"
            ));
        }
        // Recovery has flipped every `running` record terminal, so its temp is
        // now provably stale. Queued records were never started (no temp); the
        // sweep still refuses to touch them by construction.
        let sweep = store.sweep_temps(&recovery.jobs);
        for path in &sweep.removed {
            log::line(&format!(
                "job queue: removed orphan render temp {}",
                path.display()
            ));
        }
        for (path, error) in &sweep.failed {
            log::line(&format!(
                "job queue: could not remove orphan temp {}: {error}",
                path.display()
            ));
        }
        let (wake, rx) = mpsc::channel();
        let queue = Arc::new(Self {
            store,
            state: Mutex::new(QueueState::default()),
            wake,
            run,
        });
        {
            let mut state = queue.lock();
            // Only this startup's flips: a historical `interrupted` record is
            // not news and must not be re-announced on every serve.
            for job in recovery.interrupted {
                state.events.push(Event::JobChanged {
                    id: job.id,
                    state: job.state,
                    report: job.report,
                });
            }
        }
        let worker = Arc::clone(&queue);
        std::thread::Builder::new()
            .name("nile-jobs".into())
            .spawn(move || worker.dispatch(rx))
            .map_err(|e| QueueError::Invalid(format!("could not start the job worker: {e}")))?;
        Ok(queue)
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        // A panicked render must not brick status reads; the worker's own
        // panic is caught in `dispatch`.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue a snapshot and wake the dispatcher. The snapshot is captured
    /// before the record, so a record always names a complete project.
    pub(crate) fn submit(
        &self,
        project: &Project,
        sequence: u32,
        output: PathBuf,
        export: JobExport,
    ) -> Result<JobRecord, QueueError> {
        let job = self.store.create(project, sequence, output, export)?;
        let _ = self.wake.send(());
        Ok(job)
    }

    pub(crate) fn list(&self) -> Result<Vec<JobRecord>, QueueError> {
        let mut jobs = self.store.list()?;
        let state = self.lock();
        if let Some(active) = &state.active
            && let Some(job) = jobs.iter_mut().find(|job| job.id == active.id)
        {
            job.state = JobState::Running;
            job.progress = active.progress;
        }
        Ok(jobs)
    }

    pub(crate) fn get(&self, id: u64) -> Result<JobRecord, QueueError> {
        let mut job = self
            .store
            .get(id)?
            .ok_or_else(|| QueueError::Invalid(format!("job #{id} not found")))?;
        let state = self.lock();
        if let Some(active) = &state.active
            && active.id == id
        {
            job.state = JobState::Running;
            job.progress = active.progress;
        }
        Ok(job)
    }

    /// Drop a queued job, or ask the running job's ffmpeg child to die. The
    /// worker owns the final state transition; cancellation of a running job
    /// returns `running` and reports `cancelled` via `job.changed` once the
    /// child has been reaped and the temp file removed.
    pub(crate) fn cancel(&self, id: u64) -> Result<JobRecord, QueueError> {
        let mut state = self.lock();
        if let Some(active) = &state.active
            && active.id == id
        {
            active.cancel.store(true, Ordering::Relaxed);
            drop(state);
            return self.get(id);
        }
        let mut job = self
            .store
            .get(id)?
            .ok_or_else(|| QueueError::Invalid(format!("job #{id} not found")))?;
        // A running job always has an `active` entry. Seeing `running` here
        // means the worker has finished the child and is publishing the
        // terminal record; killing it now would race the rename.
        if job.state == JobState::Running {
            return Err(QueueError::Invalid(format!(
                "job #{id} is finishing; try again"
            )));
        }
        if job.state.is_terminal() {
            return Err(QueueError::Invalid(format!(
                "job #{id} is already {}",
                job.state.label()
            )));
        }
        job.state = JobState::Cancelled;
        job.finished_at = Some(now_secs());
        job.error = Some("cancelled while queued".to_string());
        self.store.update(&job)?;
        state.events.push(Event::JobChanged {
            id,
            state: job.state,
            report: job.report.clone(),
        });
        Ok(job)
    }

    /// Retain a terminal job's record and snapshot, or delete them. A
    /// queued/running job is not removable: cancel it first, so the worker
    /// cannot write into a directory that vanished underneath it.
    pub(crate) fn remove(&self, id: u64) -> Result<JobRecord, QueueError> {
        let mut state = self.lock();
        if let Some(active) = &state.active
            && active.id == id
        {
            return Err(QueueError::Invalid(format!(
                "job #{id} is running; cancel it first"
            )));
        }
        let job = self
            .store
            .get(id)?
            .ok_or_else(|| QueueError::Invalid(format!("job #{id} not found")))?;
        if job.state == JobState::Running {
            return Err(QueueError::Invalid(format!(
                "job #{id} is finishing; try again"
            )));
        }
        if !job.state.is_terminal() {
            return Err(QueueError::Invalid(format!(
                "job #{id} is {}; cancel it first",
                job.state.label()
            )));
        }
        self.store.remove(id)?;
        // A terminal event for a record that no longer exists would confuse a
        // poller; drop only this id's queued notifications.
        state.events.retain(|event| event_job_id(event) != Some(id));
        Ok(job)
    }

    /// Retention: delete terminal jobs beyond the newest `keep` (and, when
    /// set, older than `older_than_days`). Returns the removed ids oldest
    /// first. Queued/running jobs are never candidates; holding the state lock
    /// keeps `begin_next` from promoting a queued job between the plan and the
    /// deletions, so a job cannot start inside a directory being removed.
    pub(crate) fn prune(
        &self,
        keep: u64,
        older_than_days: Option<u64>,
    ) -> Result<Vec<u64>, QueueError> {
        let mut state = self.lock();
        let jobs = self.store.list()?;
        let keep = usize::try_from(keep).unwrap_or(usize::MAX);
        let removed = nle_core::jobs::prune_plan(&jobs, keep, older_than_days, now_secs());
        for id in &removed {
            self.store.remove(*id)?;
            state
                .events
                .retain(|event| event_job_id(event) != Some(*id));
        }
        Ok(removed)
    }

    pub(crate) fn take_events(&self) -> Vec<Event> {
        std::mem::take(&mut self.lock().events)
    }

    /// Pick the oldest queued job, mark it running and hand back its cancel
    /// flag. The store write happens under the same lock as cancellation, so
    /// a cancel cannot slip between "queued" and "running".
    fn begin_next(&self) -> Option<(JobRecord, Arc<AtomicBool>, u64)> {
        let mut state = self.lock();
        if state.active.is_some() {
            return None;
        }
        let jobs = match self.store.list() {
            Ok(jobs) => jobs,
            Err(e) => {
                log::line(&format!("job queue: {e}"));
                return None;
            }
        };
        for mut job in jobs.into_iter().filter(|j| j.state == JobState::Queued) {
            let mut snapshot = match self.store.load_snapshot(&job) {
                Ok(project) => project,
                Err(e) => {
                    // A broken snapshot fails just this job; the next queued
                    // one must still run.
                    job.state = JobState::Failed;
                    job.finished_at = Some(now_secs());
                    job.error = Some(format!("snapshot could not be loaded: {e}"));
                    let _ = self.store.update(&job);
                    state.events.push(Event::JobChanged {
                        id: job.id,
                        state: job.state,
                        report: None,
                    });
                    continue;
                }
            };
            // `total_frames` reads `timeline.end()` of the *active* sequence;
            // the snapshot's stored active sequence need not be the job's.
            if let Err(e) = snapshot.select_sequence(job.sequence) {
                job.state = JobState::Failed;
                job.finished_at = Some(now_secs());
                job.error = Some(format!("sequence #{}: {e}", job.sequence));
                let _ = self.store.update(&job);
                state.events.push(Event::JobChanged {
                    id: job.id,
                    state: job.state,
                    report: None,
                });
                continue;
            }
            let total = total_frames(&snapshot);
            job.state = JobState::Running;
            job.started_at = Some(now_secs());
            job.finished_at = None;
            job.progress = JobProgress { done: 0, total };
            job.error = None;
            if let Err(e) = self.store.update(&job) {
                log::line(&format!("job queue: mark running: {e}"));
                return None;
            }
            let cancel = Arc::new(AtomicBool::new(false));
            state.active = Some(ActiveJob {
                id: job.id,
                cancel: Arc::clone(&cancel),
                progress: job.progress,
            });
            push_progress(&mut state, job.id, job.progress);
            return Some((job, cancel, total));
        }
        None
    }

    fn finish(&self, mut job: JobRecord, total: u64, result: Result<Option<PathBuf>, JobRunError>) {
        let mut state = self.lock();
        state.active = None;
        job.finished_at = Some(now_secs());
        // The total computed from the job's own snapshot sequence is the only
        // one finish may report; a stale record value would drift from the
        // progress events.
        job.progress.total = total;
        match result {
            Ok(report) => {
                job.state = JobState::Done;
                job.progress.done = total;
                job.report = report;
                job.error = None;
            }
            Err(JobRunError::Cancelled) => {
                job.state = JobState::Cancelled;
                job.error = Some("cancelled while running".to_string());
            }
            Err(JobRunError::Failed(why)) => {
                job.state = JobState::Failed;
                job.error = Some(why);
            }
        }
        if let Err(e) = self.store.update(&job) {
            log::line(&format!("job queue: finish #{}: {e}", job.id));
        }
        state.events.push(Event::JobChanged {
            id: job.id,
            state: job.state,
            report: job.report.clone(),
        });
    }

    fn progress(&self, id: u64, done: u64, total: u64) {
        let mut state = self.lock();
        let progress = match state.active.as_mut() {
            Some(active) if active.id == id => {
                active.progress = JobProgress { done, total };
                active.progress
            }
            _ => return,
        };
        push_progress(&mut state, id, progress);
    }

    fn dispatch(self: Arc<Self>, rx: Receiver<()>) {
        loop {
            while let Some((job, cancel, total)) = self.begin_next() {
                let id = job.id;
                let report = {
                    let run = Arc::clone(&self.run);
                    let mut progress = |done: u64, total: u64| self.progress(id, done, total);
                    // A buggy renderer panic becomes a failed job, not a dead
                    // queue; the store is the source of truth.
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        (run)(&self.store, &job, &cancel, &mut progress)
                    }))
                    .unwrap_or_else(|_| Err(JobRunError::Failed("render worker panicked".into())))
                };
                self.finish(job, total, report);
            }
            match rx.recv() {
                Ok(()) => {}
                // The sender lives in the queue this worker owns, so this is
                // only reachable if the channel is closed by a future stop
                // path; either way no one can enqueue more work.
                Err(_) => return,
            }
        }
    }
}

/// The job id a notification refers to, for retention cleanup.
fn event_job_id(event: &Event) -> Option<u64> {
    match event {
        Event::JobProgress { id, .. } | Event::JobChanged { id, .. } => Some(*id),
        _ => None,
    }
}

/// Replace a pending progress notification for `id` instead of growing the
/// queue without bound between calls. Terminal `job.changed` entries are
/// never touched, so their order is preserved.
fn push_progress(state: &mut QueueState, id: u64, progress: JobProgress) {
    if let Some(Event::JobProgress {
        progress: pending, ..
    }) = state
        .events
        .iter_mut()
        .find(|event| matches!(event, Event::JobProgress { id: pending, .. } if *pending == id))
    {
        *pending = progress;
        return;
    }
    state.events.push(Event::JobProgress {
        id,
        state: JobState::Running,
        progress,
    });
}

/// The number of output frames a snapshot should produce.
pub(crate) fn total_frames(project: &Project) -> u64 {
    let Some(fps) = project.fps else {
        return 0;
    };
    let seconds = secs_f64(&project.timeline.end());
    let rate = secs_f64(&fps);
    (seconds * rate).round().max(0.0) as u64
}

/// Map the narrow job DTO onto the CLI's export knobs.
pub(crate) fn export_options(export: &JobExport) -> ExportOptions {
    ExportOptions {
        preset: match export.preset {
            nle_core::jobs::ExportPreset::Preview => QualityPreset::Preview,
            nle_core::jobs::ExportPreset::Balanced => QualityPreset::Balanced,
            nle_core::jobs::ExportPreset::Max => QualityPreset::Max,
        },
        crf: export.crf,
        encoder_preset: export.encoder_preset.clone(),
        ..Default::default()
    }
}

/// Render one job from its snapshot. Writes a hidden temp file, renames on
/// success, and removes it on every failure — including cancellation.
pub(crate) fn run_export_job(
    store: &JobStore,
    job: &JobRecord,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Option<PathBuf>, JobRunError> {
    let mut project = store
        .load_snapshot(job)
        .map_err(|e| JobRunError::Failed(e.to_string()))?;
    project
        .select_sequence(job.sequence)
        .map_err(|e| JobRunError::Failed(e.to_string()))?;
    let total = total_frames(&project);
    let options = export_options(&job.export);
    let temp = partial_path(&job.output, job.id);
    if let Some(parent) = temp.parent().filter(|p| !p.as_os_str().is_empty())
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return Err(JobRunError::Failed(format!(
            "create {}: {e}",
            parent.display()
        )));
    }
    let spec = options.spec(&temp);
    let fps = project.fps.unwrap_or_else(|| Seconds::from_integer(30));
    let mut last = Instant::now() - Duration::from_secs(1);
    let mut report = |seconds: f64| {
        if last.elapsed() < Duration::from_millis(250) {
            return;
        }
        last = Instant::now();
        let done = (seconds * secs_f64(&fps)).round().max(0.0) as u64;
        progress(done.min(total), total);
    };
    let result = nle_ffmpeg::export_quiet_progress_cancel(&project, &spec, &mut report, cancel);
    let (used, result) = match result {
        Err(nle_ffmpeg::graph::ExportError::Ffmpeg(FfmpegError::Cancelled)) => {
            let _ = std::fs::remove_file(&temp);
            return Err(JobRunError::Cancelled);
        }
        Err(e) if spec.encoder.is_hardware() => {
            log::line(&format!("job #{} hardware export failed: {e}", job.id));
            let cpu = options.cpu_fallback(&spec);
            let retry =
                nle_ffmpeg::export_quiet_progress_cancel(&project, &cpu, &mut report, cancel);
            (cpu.encoder, retry)
        }
        Ok(plan) => (spec.encoder, Ok(plan)),
        Err(e) => (spec.encoder, Err(e)),
    };
    if let Err(e) = result {
        let _ = std::fs::remove_file(&temp);
        return match e {
            nle_ffmpeg::graph::ExportError::Ffmpeg(FfmpegError::Cancelled) => {
                Err(JobRunError::Cancelled)
            }
            other => Err(JobRunError::Failed(other.to_string())),
        };
    }
    // Cancellation may have been requested between the child's exit and the
    // rename: still honour it, and never publish a file after a cancel.
    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(&temp);
        return Err(JobRunError::Cancelled);
    }
    std::fs::rename(&temp, &job.output).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        JobRunError::Failed(format!("publish {}: {e}", job.output.display()))
    })?;
    let frames = nle_ffmpeg::frame_count(&job.output).unwrap_or(total as i64);
    let built = RenderReport::build(
        &project,
        &job.output,
        frames,
        Some(used.label().to_string()),
    );
    let report_path = RenderReport::sidecar_path(&job.output);
    match built.save(&report_path) {
        Ok(()) => Ok(Some(report_path)),
        Err(e) => {
            // The deliverable is complete; a report failure is a warning in
            // the job record, not a reason to throw the render away.
            log::line(&format!("job #{} report not written: {e}", job.id));
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, Sender};

    fn seeded() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nile.json");
        let mut project = Project::new();
        project.fps = Some(Seconds::from_integer(30));
        project.save(&path).expect("save project");
        (dir, path)
    }

    /// A runner that reports its job id, then waits for `release`.
    fn gated_runner() -> (Arc<JobRunner>, Receiver<u64>, Sender<()>, Arc<AtomicBool>) {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let overlap = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(AtomicBool::new(false));
        let overlap_seen = Arc::clone(&overlap);
        let overlap_flag = Arc::clone(&seen);
        let run: Arc<JobRunner> = Arc::new(move |_store, job, cancel, _progress| {
            if overlap_flag.swap(true, Ordering::SeqCst) {
                overlap_seen.store(true, Ordering::SeqCst);
            }
            let _ = started_tx.send(job.id);
            loop {
                if cancel.load(Ordering::Relaxed) {
                    overlap_flag.store(false, Ordering::SeqCst);
                    return Err(JobRunError::Cancelled);
                }
                let rx = release_rx.lock().unwrap();
                match rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(()) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            overlap_flag.store(false, Ordering::SeqCst);
            Ok(None)
        });
        (run, started_rx, release_tx, overlap)
    }

    fn wait_for<T>(rx: &Receiver<T>, what: &str) -> T {
        rx.recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    fn quick_runner() -> Arc<JobRunner> {
        Arc::new(|_store, _job, _cancel, _progress| Ok(None))
    }

    fn wait_done(queue: &JobQueue, id: u64) -> JobRecord {
        for _ in 0..200 {
            let job = queue.get(id).expect("get");
            if job.state.is_terminal() {
                return job;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("job #{id} never finished");
    }

    #[test]
    fn jobs_run_one_at_a_time_in_submission_order() {
        let (_dir, path) = seeded();
        let store = JobStore::at(&path);
        let project = Project::load(&path).unwrap();
        for n in 0..3 {
            store
                .create(
                    &project,
                    1,
                    PathBuf::from(format!("/tmp/{n}.mp4")),
                    JobExport::default(),
                )
                .unwrap();
        }
        let (run, started, release, overlap) = gated_runner();
        let queue = JobQueue::start_with(&path, run).expect("queue");
        assert_eq!(wait_for(&started, "first job"), 1);
        release.send(()).unwrap();
        assert_eq!(wait_for(&started, "second job"), 2);
        release.send(()).unwrap();
        assert_eq!(wait_for(&started, "third job"), 3);
        release.send(()).unwrap();
        // Let the last finish.
        let mut states = Vec::new();
        for _ in 0..200 {
            states = queue.list().unwrap().into_iter().map(|j| j.state).collect();
            if states.iter().all(|s| s.is_terminal()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(states, vec![JobState::Done; 3]);
        assert!(!overlap.load(Ordering::SeqCst), "two jobs ran at once");
        let events = queue.take_events();
        assert!(events.iter().any(|e| matches!(
            e,
            Event::JobChanged {
                id: 3,
                state: JobState::Done,
                ..
            }
        )));
    }

    #[test]
    fn cancelling_a_queued_job_never_runs_it() {
        let (_dir, path) = seeded();
        let store = JobStore::at(&path);
        let project = Project::load(&path).unwrap();
        for n in 0..2 {
            store
                .create(
                    &project,
                    1,
                    PathBuf::from(format!("/tmp/{n}.mp4")),
                    JobExport::default(),
                )
                .unwrap();
        }
        let (run, started, release, _overlap) = gated_runner();
        let queue = JobQueue::start_with(&path, run).expect("queue");
        assert_eq!(wait_for(&started, "first job"), 1);
        let cancelled = queue.cancel(2).expect("cancel queued");
        assert_eq!(cancelled.state, JobState::Cancelled);
        assert!(cancelled.error.as_deref().unwrap().contains("queued"));
        release.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(queue.get(2).unwrap().state, JobState::Cancelled);
        assert!(
            queue.cancel(2).is_err(),
            "terminal jobs refuse another cancel"
        );
    }

    #[test]
    fn cancelling_a_running_job_kills_it_and_reports_cancelled() {
        let (_dir, path) = seeded();
        let store = JobStore::at(&path);
        let project = Project::load(&path).unwrap();
        store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/run.mp4"),
                JobExport::default(),
            )
            .unwrap();
        let (run, started, _release, _overlap) = gated_runner();
        let queue = JobQueue::start_with(&path, run).expect("queue");
        assert_eq!(wait_for(&started, "running job"), 1);
        let reply = queue.cancel(1).expect("cancel running");
        assert_eq!(reply.state, JobState::Running, "kill is asynchronous");
        let mut state = JobState::Running;
        for _ in 0..200 {
            state = queue.get(1).unwrap().state;
            if state == JobState::Cancelled {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(state, JobState::Cancelled);
        assert!(queue.take_events().iter().any(|e| matches!(
            e,
            Event::JobChanged {
                id: 1,
                state: JobState::Cancelled,
                ..
            }
        )));
    }

    #[test]
    fn an_unknown_job_is_an_invalid_request() {
        let (_dir, path) = seeded();
        let queue = JobQueue::start(&path).expect("queue");
        let error = queue.get(99).expect_err("unknown");
        assert!(matches!(error, QueueError::Invalid(_)));
        assert!(error.to_string().contains("not found"));
        assert!(queue.cancel(99).is_err());
        assert!(queue.remove(99).is_err());
    }

    #[test]
    fn progress_total_and_finish_use_the_jobs_own_sequence() {
        let (_dir, path) = seeded();
        let store = JobStore::at(&path);
        let mut project = Project::load(&path).unwrap();
        let second = project.add_sequence("Second", Some(Seconds::from_integer(24)), None);
        let sequence = project.sequence_mut(second).unwrap();
        sequence.timeline.video_track_mut().unwrap().segments =
            vec![nle_core::timeline::Segment::Gap(nle_core::timeline::Gap {
                duration: Seconds::from_integer(2),
            })];
        // The snapshot's active sequence is 1; the job renders 2 (24 fps × 2 s
        // = 48 frames, not 30 fps × 0 s from the active sequence).
        let job = store
            .create(
                &project,
                second,
                PathBuf::from("/tmp/second.mp4"),
                JobExport::default(),
            )
            .unwrap();
        let (run, started, release, _overlap) = gated_runner();
        let queue = JobQueue::start_with(&path, run).expect("queue");
        assert_eq!(wait_for(&started, "job"), job.id);
        assert_eq!(queue.get(job.id).unwrap().progress.total, 48);
        release.send(()).unwrap();
        let done = wait_done(&queue, job.id);
        assert_eq!(done.state, JobState::Done);
        assert_eq!(done.progress.total, 48);
        assert_eq!(done.progress.done, 48);
    }

    #[test]
    fn repeated_progress_coalesces_and_terminal_events_stay_ordered() {
        let (_dir, path) = seeded();
        let store = JobStore::at(&path);
        let project = Project::load(&path).unwrap();
        let job = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/p.mp4"),
                JobExport::default(),
            )
            .unwrap();
        let (run, started, release, _overlap) = gated_runner();
        let queue = JobQueue::start_with(&path, run).expect("queue");
        assert_eq!(wait_for(&started, "job"), job.id);
        for done in 1..=5 {
            queue.progress(job.id, done * 10, 100);
        }
        let events = queue.take_events();
        let progress: Vec<_> = events
            .iter()
            .filter(|event| matches!(event, Event::JobProgress { id, .. } if *id == job.id))
            .collect();
        assert_eq!(progress.len(), 1, "one coalesced progress row: {events:?}");
        assert!(
            matches!(
                progress[0],
                Event::JobProgress { progress, .. } if progress.done == 50 && progress.total == 100
            ),
            "{events:?}"
        );

        // A progress event pending when the job finishes keeps its place
        // before the terminal one.
        queue.progress(job.id, 90, 100);
        release.send(()).unwrap();
        let _ = wait_done(&queue, job.id);
        let events = queue.take_events();
        let progress_at = events
            .iter()
            .position(|event| matches!(event, Event::JobProgress { id, .. } if *id == job.id))
            .expect("pending progress");
        let changed = events
            .iter()
            .position(|event| matches!(event, Event::JobChanged { id, .. } if *id == job.id))
            .expect("terminal event");
        assert!(
            progress_at < changed,
            "progress must precede the terminal event: {events:?}"
        );
        assert!(
            !events[changed + 1..]
                .iter()
                .any(|event| matches!(event, Event::JobProgress { id, .. } if *id == job.id)),
            "no progress after the terminal event: {events:?}"
        );
    }
    #[test]
    fn remove_only_terminal_jobs() {
        let (_dir, path) = seeded();
        let store = JobStore::at(&path);
        let project = Project::load(&path).unwrap();
        for n in 0..2 {
            store
                .create(
                    &project,
                    1,
                    PathBuf::from(format!("/tmp/rm-{n}.mp4")),
                    JobExport::default(),
                )
                .unwrap();
        }
        let (run, started, release, _overlap) = gated_runner();
        let queue = JobQueue::start_with(&path, run).expect("queue");
        assert_eq!(wait_for(&started, "first job"), 1);

        let running = queue.remove(1).expect_err("running");
        assert!(running.to_string().contains("running"), "{running}");
        let queued = queue.remove(2).expect_err("queued");
        assert!(queued.to_string().contains("queued"), "{queued}");
        let unknown = queue.remove(9).expect_err("unknown");
        assert!(unknown.to_string().contains("not found"), "{unknown}");

        let cancelled = queue.cancel(2).expect("cancel queued");
        assert_eq!(cancelled.state, JobState::Cancelled);
        let removed = queue.remove(2).expect("remove cancelled");
        assert_eq!(removed.id, 2);
        assert!(!store.job_dir(2).exists());
        assert!(queue.get(2).is_err(), "removed jobs are gone");

        release.send(()).unwrap();
        let _ = wait_done(&queue, 1);
        queue.remove(1).expect("remove done");
        assert!(!store.job_dir(1).exists());
    }

    #[test]
    fn prune_removes_terminal_jobs_but_never_running_or_queued() {
        let (_dir, path) = seeded();
        let store = JobStore::at(&path);
        let project = Project::load(&path).unwrap();
        for n in 0..2 {
            store
                .create(
                    &project,
                    1,
                    PathBuf::from(format!("/tmp/prune-{n}.mp4")),
                    JobExport::default(),
                )
                .unwrap();
        }
        // A directory without a record is not a job: it is skippable, not
        // prunable. A non-numeric name keeps the id high-water mark honest.
        std::fs::create_dir_all(store.dir().join("scratch")).unwrap();

        let (run, started, release, _overlap) = gated_runner();
        let queue = JobQueue::start_with(&path, run).expect("queue");
        assert_eq!(wait_for(&started, "running job"), 1);
        let queued = queue
            .submit(
                &project,
                1,
                PathBuf::from("/tmp/prune-2.mp4"),
                JobExport::default(),
            )
            .expect("submit");
        assert_eq!(queued.id, 3);
        let cancelled = queue.cancel(3).expect("cancel queued");
        assert_eq!(cancelled.state, JobState::Cancelled);
        let _ = queue.take_events(); // drop the cancel notification

        // Only the cancelled (terminal) job is a candidate: #1 is running and
        // #2 is queued, so a keep of zero still spares them.
        assert_eq!(queue.prune(0, None).expect("prune"), vec![3]);
        assert!(!store.job_dir(3).exists());
        assert!(store.job_dir(1).exists(), "a running job is kept");
        assert!(store.job_dir(2).exists(), "a queued job is kept");
        assert!(
            store.dir().join("scratch").exists(),
            "an unrelated dir is kept"
        );
        assert!(
            queue.take_events().is_empty(),
            "a pruned job's queued events are dropped"
        );
        assert!(queue.prune(0, None).expect("nothing left").is_empty());

        // Finish both, then keep the newest terminal job only.
        release.send(()).unwrap();
        assert_eq!(wait_done(&queue, 1).state, JobState::Done);
        assert_eq!(wait_for(&started, "second job"), 2);
        release.send(()).unwrap();
        assert_eq!(wait_done(&queue, 2).state, JobState::Done);
        assert_eq!(queue.prune(1, None).expect("keep newest"), vec![1]);
        assert!(!store.job_dir(1).exists());
        assert!(store.job_dir(2).exists());
        // Ids are not recycled after a prune emptied the highest ids.
        let next = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/prune-next.mp4"),
                JobExport::default(),
            )
            .unwrap();
        assert_eq!(next.id, 4);
    }

    #[test]
    fn startup_sweeps_a_crashed_temp_and_spares_live_work() {
        let (dir, path) = seeded();
        let store = JobStore::at(&path);
        let project = Project::load(&path).unwrap();
        let done = store
            .create(
                &project,
                1,
                dir.path().join("crashed.mp4"),
                JobExport::default(),
            )
            .unwrap();
        let mut done = done;
        done.state = JobState::Done;
        store.update(&done).unwrap();
        let crashed = partial_path(&done.output, done.id);
        std::fs::write(&crashed, b"half a render").unwrap();
        let queued = store
            .create(
                &project,
                1,
                dir.path().join("queued.mp4"),
                JobExport::default(),
            )
            .unwrap();
        let queued_temp = partial_path(&queued.output, queued.id);
        std::fs::write(&queued_temp, b"not started").unwrap();

        let _queue = JobQueue::start_with(&path, quick_runner()).expect("queue");
        assert!(!crashed.exists(), "a terminal record's temp is swept");
        assert!(queued_temp.exists(), "a queued record's temp is kept");
    }

    #[test]
    fn startup_announces_only_this_runs_interruptions() {
        let (_dir, path) = seeded();
        let store = JobStore::at(&path);
        let project = Project::load(&path).unwrap();
        let historical = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/old.mp4"),
                JobExport::default(),
            )
            .unwrap();
        let mut historical = historical;
        historical.state = JobState::Interrupted;
        historical.error = Some("an older crash".into());
        store.update(&historical).unwrap();
        let running = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/now.mp4"),
                JobExport::default(),
            )
            .unwrap();
        let mut running = running;
        running.state = JobState::Running;
        store.update(&running).unwrap();

        let queue = JobQueue::start_with(&path, quick_runner()).expect("queue");
        let events = queue.take_events();
        let changed: Vec<u64> = events
            .iter()
            .filter_map(|event| match event {
                Event::JobChanged {
                    id,
                    state: JobState::Interrupted,
                    ..
                } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(changed, vec![running.id], "historical rows are not news");
    }

    #[test]
    fn startup_skips_a_corrupt_record_and_runs_the_rest() {
        let (_dir, path) = seeded();
        let store = JobStore::at(&path);
        let project = Project::load(&path).unwrap();
        let good = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/good.mp4"),
                JobExport::default(),
            )
            .unwrap();
        let corrupt = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/bad.mp4"),
                JobExport::default(),
            )
            .unwrap();
        std::fs::write(store.job_dir(corrupt.id).join("job.json"), "{ nope").unwrap();

        let queue = JobQueue::start_with(&path, quick_runner()).expect("queue");
        let done = wait_done(&queue, good.id);
        assert_eq!(done.state, JobState::Done);
        assert!(store.job_dir(corrupt.id).exists(), "kept for inspection");
        assert!(queue.get(corrupt.id).is_err());
    }

    #[test]
    fn a_cancelled_executor_leaves_no_deliverable_or_temp() {
        let (_dir, path) = seeded();
        let store = JobStore::at(&path);
        let mut project = Project::load(&path).unwrap();
        project.video = Some(nle_core::VideoSettings {
            width: 320,
            height: 180,
        });
        project.timeline.video_track_mut().unwrap().segments =
            vec![nle_core::timeline::Segment::Gap(nle_core::timeline::Gap {
                duration: Seconds::from_integer(1),
            })];
        let out = path.with_file_name("cancelled.mp4");
        let job = store
            .create(&project, 1, out.clone(), JobExport::default())
            .unwrap();
        let cancel = AtomicBool::new(true);
        let mut progress = |_done: u64, _total: u64| {};
        let result = run_export_job(&store, &job, &cancel, &mut progress);
        assert!(matches!(result, Err(JobRunError::Cancelled)));
        assert!(!out.exists(), "no output after a cancel");
        assert!(!partial_path(&out, job.id).exists(), "no temp left behind");
    }

    #[test]
    fn partial_output_keeps_the_extension_and_is_not_the_deliverable() {
        let out = Path::new("/tmp/show.mp4");
        let temp = partial_path(out, 7);
        assert_eq!(temp, Path::new("/tmp/.show.job7.mp4"));
        assert_ne!(temp, out);
        assert_eq!(
            partial_path(Path::new("/tmp/noext"), 2).extension(),
            Some("mp4".as_ref())
        );
    }

    #[test]
    fn progress_counts_output_frames() {
        let mut project = Project::new();
        project.fps = Some(Seconds::from_integer(30));
        project.timeline.video_track_mut().unwrap().segments =
            vec![nle_core::timeline::Segment::Gap(nle_core::timeline::Gap {
                duration: Seconds::new(5, 2),
            })];
        assert_eq!(total_frames(&project), 75);
        project.fps = None;
        assert_eq!(total_frames(&project), 0);
    }
}
