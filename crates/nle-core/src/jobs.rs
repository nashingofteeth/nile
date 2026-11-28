//! Durable render jobs, owned by `nile serve`.
//!
//! A job is a record plus a **snapshot of the project at submit time**:
//! `.nile/<stem>/jobs/<id>/job.json` and `…/project.json`. The snapshot is
//! what the renderer reads, so an edit made while the job is queued or
//! running cannot change the deliverable (or its render report). Both files
//! are written through a temp file and renamed into place, so a crash leaves
//! either the old complete file or none — never a half-written one.
//!
//! This module is pure data and file I/O: no processes, no renderer. The
//! queue that runs jobs lives in `nle-cli`; the wire types reuse these
//! structs so a job has one spelling on disk and on the wire.
//!
//! ## States
//!
//! `queued` is durable; a restart keeps it. `running` is not resumed: on
//! startup [`JobStore::recover`] marks it `interrupted`, because a half-run
//! ffmpeg child is gone and silently retrying could overwrite a deliverable a
//! human already inspected. Terminal states (`done`, `failed`, `cancelled`,
//! `interrupted`) are final.
//!
//! ## Retention and crash temps
//!
//! [`prune_plan`] is a pure plan over **terminal** records (the queue executes
//! it, so `nile serve` stays the one writer). [`JobStore::sweep_temps`] runs
//! at startup: it deletes only the exact [`partial_path`] a terminal record
//! names, never a glob, so a live export's temp and an unexplained file are
//! both left alone.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::project::Project;
use crate::state;

/// The on-disk job schema. Local state, no migrations: a different version is
/// rejected with a clear error and the fix is to delete the job directory.
pub const JOB_VERSION: u32 = 1;
pub const JOB_FILE: &str = "job.json";
pub const SNAPSHOT_FILE: &str = "project.json";

/// The high-water mark for job ids. It lives beside the job directories so
/// removing (or pruning) the highest id cannot make `next_id` recycle it: a
/// stale client's `job.get 3` must not find a newer job.
const NEXT_ID_FILE: &str = ".next-id";

/// Seconds in a retention day (`--older-than`). A day is a calendar day of
/// 86 400 seconds; job timestamps are epoch seconds, so age is wall-clock.
pub const DAY_SECS: u64 = 86_400;

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("job store io ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("job store json ({path}): {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "job {path} has schema version {found}; this build reads {supported} — delete the job directory"
    )]
    Version {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("job #{0} not found")]
    NotFound(u64),
    #[error(
        "job id high-water mark {path} is not a positive integer — repair it before submitting jobs"
    )]
    NextId { path: PathBuf },
    #[error("sequence #{0} not found")]
    Sequence(u32),
}

impl JobError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    fn json(path: impl Into<PathBuf>, source: serde_json::Error) -> Self {
        Self::Json {
            path: path.into(),
            source,
        }
    }
}

/// One job's lifecycle. Serialized as `snake_case`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    #[default]
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
    Interrupted,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Done | JobState::Failed | JobState::Cancelled | JobState::Interrupted
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
            JobState::Interrupted => "interrupted",
        }
    }
}

/// A quality tier. The names match the CLI's `--preset` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportPreset {
    Preview,
    #[default]
    Balanced,
    Max,
}

impl ExportPreset {
    pub fn label(self) -> &'static str {
        match self {
            ExportPreset::Preview => "preview",
            ExportPreset::Balanced => "balanced",
            ExportPreset::Max => "max",
        }
    }
}

/// The export knobs a job carries.
///
/// Deliberately narrow: an explicit engine (`--encoder`), a hardware bitrate,
/// a chunk-worker count and the chunk cache are **not** part of the job DTO
/// yet. A job renders one graph (the cancellable path), so a parallel option
/// would be a promise the queue does not keep. Add them with the scheduler
/// rework, not speculatively.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobExport {
    #[serde(default)]
    pub preset: ExportPreset,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crf: Option<u8>,
    /// The encoder's own preset name (`fast`, `medium`, an SVT-AV1 number);
    /// the engine stays the tier's choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder_preset: Option<String>,
}

/// Render progress in output frames. `total` may be 0 before the first
/// probe; clients must not divide by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobProgress {
    pub done: u64,
    pub total: u64,
}

impl JobProgress {
    pub fn fraction(self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f64 / self.total as f64).clamp(0.0, 1.0)
        }
    }
}

/// One durable job. Paths are absolute when created by the server.
///
/// Unknown fields are tolerated on purpose: a record written by a newer build
/// must still parse far enough to reach the [`JOB_VERSION`] check instead of
/// failing with a generic JSON error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRecord {
    pub version: u32,
    pub id: u64,
    pub state: JobState,
    /// The project revision the snapshot was taken at: what the render report
    /// records, not whatever the live project says when the job finishes.
    pub revision: u64,
    /// The sequence the snapshot was rendering.
    pub sequence: u32,
    pub output: PathBuf,
    #[serde(default)]
    pub export: JobExport,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(default)]
    pub progress: JobProgress,
    /// The retained render report (`<output>.report.json`), when a render
    /// produced one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<PathBuf>,
    /// Why the job failed, was cancelled or was interrupted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The project snapshot beside the record.
    pub snapshot: PathBuf,
}

impl JobRecord {
    /// A short one-line summary for `nile job ls` and the TUI row.
    pub fn summary(&self) -> String {
        let pct = (self.progress.fraction() * 100.0).round() as u32;
        match self.state {
            JobState::Running => format!("#{} running {pct}%", self.id),
            JobState::Queued => format!("#{} queued", self.id),
            state => format!("#{} {}", self.id, state.label()),
        }
    }
}

/// The result of [`JobStore::recover`].
#[derive(Debug)]
pub struct Recovery {
    /// Every readable record after recovery, oldest id first.
    pub jobs: Vec<JobRecord>,
    /// Only the records this call flipped `running` → `interrupted`.
    pub interrupted: Vec<JobRecord>,
    /// Corrupt or newer records the store skipped (directories untouched).
    pub skipped: Vec<JobError>,
}

/// The per-project job directory. One store instance is cheap; it is just the
/// path.
#[derive(Debug, Clone)]
pub struct JobStore {
    dir: PathBuf,
}

impl JobStore {
    pub fn at(project_path: &Path) -> Self {
        Self {
            dir: state::jobs_dir(project_path),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn job_dir(&self, id: u64) -> PathBuf {
        self.dir.join(id.to_string())
    }

    pub fn snapshot_path(&self, id: u64) -> PathBuf {
        self.job_dir(id).join(SNAPSHOT_FILE)
    }

    /// The next unused id: one past the highest directory **and** one past the
    /// persisted high-water mark, whichever is greater. Ids are never reused:
    /// a stale client's `job.get 3` must not find a newer job. The directory
    /// scan alone is not enough, because `remove`/`prune` can delete the
    /// highest id.
    fn next_id(&self) -> Result<u64, JobError> {
        let mut next = self.stored_next_id()?.unwrap_or(1);
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(next),
            Err(e) => return Err(JobError::io(&self.dir, e)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| JobError::io(&self.dir, e))?;
            if let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u64>().ok())
            {
                next = next.max(id + 1);
            }
        }
        Ok(next)
    }

    /// Read the persisted high-water mark. A missing file is normal (a project
    /// that predates the marker); an unreadable or non-positive one is an
    /// error, because silently falling back to the directory scan could
    /// recycle an id deleted before the marker was written.
    fn stored_next_id(&self) -> Result<Option<u64>, JobError> {
        let path = self.dir.join(NEXT_ID_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(JobError::io(&path, e)),
        };
        match text.trim().parse::<u64>().ok().filter(|id| *id > 0) {
            Some(id) => Ok(Some(id)),
            None => Err(JobError::NextId { path }),
        }
    }

    /// Persist the high-water mark before the record is written: a crash
    /// between the two skips an id (harmless) instead of recycling one.
    fn write_next_id(&self, next: u64) -> Result<(), JobError> {
        std::fs::create_dir_all(&self.dir).map_err(|e| JobError::io(&self.dir, e))?;
        let path = self.dir.join(NEXT_ID_FILE);
        let mut tmp =
            tempfile::NamedTempFile::new_in(&self.dir).map_err(|e| JobError::io(&self.dir, e))?;
        writeln!(tmp, "{next}").map_err(|e| JobError::io(&path, e))?;
        tmp.persist(&path)
            .map_err(|e| JobError::io(&path, e.error))?;
        Ok(())
    }

    /// Capture the project and queue a new job. The snapshot is written before
    /// the record: a crash between them leaves an ignored directory, never a
    /// record that points at a missing snapshot.
    pub fn create(
        &self,
        project: &Project,
        sequence: u32,
        output: PathBuf,
        export: JobExport,
    ) -> Result<JobRecord, JobError> {
        if project.sequence(sequence).is_none() {
            return Err(JobError::Sequence(sequence));
        }
        let id = self.next_id()?;
        self.write_next_id(id + 1)?;
        let dir = self.job_dir(id);
        std::fs::create_dir_all(&dir).map_err(|e| JobError::io(&dir, e))?;
        let snapshot = PathBuf::from(SNAPSHOT_FILE);
        write_json(&dir.join(&snapshot), project)?;
        let record = JobRecord {
            version: JOB_VERSION,
            id,
            state: JobState::Queued,
            revision: project.revision,
            sequence,
            output,
            export,
            created_at: now_secs(),
            started_at: None,
            finished_at: None,
            progress: JobProgress::default(),
            report: None,
            error: None,
            snapshot,
        };
        self.update(&record)?;
        Ok(record)
    }

    /// Load one record; `Ok(None)` when the id has no record.
    pub fn get(&self, id: u64) -> Result<Option<JobRecord>, JobError> {
        let path = self.job_dir(id).join(JOB_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(JobError::io(path, e)),
        };
        let record: JobRecord =
            serde_json::from_str(&text).map_err(|e| JobError::json(&path, e))?;
        if record.version != JOB_VERSION {
            return Err(JobError::Version {
                path,
                found: record.version,
                supported: JOB_VERSION,
            });
        }
        Ok(Some(record))
    }

    /// Every complete record, oldest id first. A directory without a
    /// `job.json` (a crash between mkdir and snapshot, or a hand-made dir) is
    /// skipped, as is a corrupt or newer record: one bad job must not hide the
    /// rest of the queue. IO failures still surface.
    pub fn list(&self) -> Result<Vec<JobRecord>, JobError> {
        Ok(self.list_inner()?.0)
    }

    /// `(records, skipped)`: a corrupt/newer `job.json` is reported for logging
    /// at startup and otherwise ignored; the directory stays on disk.
    fn list_inner(&self) -> Result<(Vec<JobRecord>, Vec<JobError>), JobError> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new()));
            }
            Err(e) => return Err(JobError::io(&self.dir, e)),
        };
        let mut ids: Vec<u64> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| JobError::io(&self.dir, e))?;
            if let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u64>().ok())
            {
                ids.push(id);
            }
        }
        ids.sort_unstable();
        let mut jobs = Vec::with_capacity(ids.len());
        let mut skipped = Vec::new();
        for id in ids {
            match self.get(id) {
                Ok(Some(record)) => jobs.push(record),
                Ok(None) => {}
                Err(e @ (JobError::Json { .. } | JobError::Version { .. })) => skipped.push(e),
                Err(e) => return Err(e),
            }
        }
        Ok((jobs, skipped))
    }

    /// Rewrite one record atomically. The record's id names its directory.
    pub fn update(&self, record: &JobRecord) -> Result<(), JobError> {
        let dir = self.job_dir(record.id);
        std::fs::create_dir_all(&dir).map_err(|e| JobError::io(&dir, e))?;
        write_json(&dir.join(JOB_FILE), record)
    }

    /// Delete a job directory (record + snapshot). The caller decides whether
    /// the state allows it; an unknown id is [`JobError::NotFound`].
    ///
    /// The high-water mark is advanced *before* the directory disappears. A
    /// project written before `.next-id` existed has no marker, so removing its
    /// highest id would otherwise make `next_id` scan back over it and recycle
    /// the id on the next submit.
    pub fn remove(&self, id: u64) -> Result<(), JobError> {
        let dir = self.job_dir(id);
        if !dir.exists() {
            return Err(JobError::NotFound(id));
        }
        let next = self.next_id()?.max(id.saturating_add(1));
        self.write_next_id(next)?;
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(JobError::NotFound(id)),
            Err(e) => Err(JobError::io(dir, e)),
        }
    }

    /// Startup recovery: a `running` job has no live ffmpeg child any more.
    /// Mark it `interrupted` (never silently resume). `interrupted` holds only
    /// the records flipped by **this** call, so a client is not sent events
    /// for historical interruptions; `skipped` carries corrupt/newer records
    /// the caller should log (their directories are left alone).
    pub fn recover(&self) -> Result<Recovery, JobError> {
        let (mut jobs, skipped) = self.list_inner()?;
        let now = now_secs();
        let mut interrupted = Vec::new();
        for job in &mut jobs {
            if job.state == JobState::Running {
                job.state = JobState::Interrupted;
                job.finished_at = Some(now);
                job.error = Some("server stopped while the job was running".to_string());
                self.update(job)?;
                interrupted.push(job.clone());
            }
        }
        Ok(Recovery {
            jobs,
            interrupted,
            skipped,
        })
    }

    /// The snapshot project a worker should render. The snapshot path is
    /// relative to the job directory (local state, but a hand-crafted
    /// `job.json` should not be able to point outside it).
    pub fn load_snapshot(&self, record: &JobRecord) -> Result<Project, JobError> {
        if record.snapshot.is_absolute()
            || record
                .snapshot
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(JobError::Io {
                path: self.job_dir(record.id).join(&record.snapshot),
                source: std::io::Error::other("snapshot path must stay inside the job directory"),
            });
        }
        let path = self.job_dir(record.id).join(&record.snapshot);
        Project::load(&path).map_err(|e| JobError::Io {
            path: path.clone(),
            source: std::io::Error::other(e.to_string()),
        })
    }

    /// Startup sweep: delete hidden render temps a crash left behind.
    ///
    /// A temp is only deleted when it is exactly [`partial_path`] of a
    /// **terminal** record. The renderer publishes or removes that path before
    /// the terminal record is written, so one still on disk cannot belong to a
    /// live export; a queued/running record is never considered. The path set
    /// comes from the records, not a glob, so no unexplained file is removed
    /// (a temp whose record was already deleted stays for a human). A path that
    /// is also some record's deliverable is skipped even if the names collide.
    pub fn sweep_temps(&self, jobs: &[JobRecord]) -> TempSweep {
        let mut sweep = TempSweep::default();
        for job in jobs.iter().filter(|job| job.state.is_terminal()) {
            let temp = partial_path(&job.output, job.id);
            if jobs.iter().any(|other| other.output == temp) {
                continue;
            }
            match std::fs::remove_file(&temp) {
                Ok(()) => sweep.removed.push(temp),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => sweep.failed.push((temp, e)),
            }
        }
        sweep
    }
}

/// What [`JobStore::sweep_temps`] found.
#[derive(Debug, Default)]
pub struct TempSweep {
    /// Stale temps that were deleted.
    pub removed: Vec<PathBuf>,
    /// Stale temps that could not be deleted, with the reason.
    pub failed: Vec<(PathBuf, std::io::Error)>,
}

/// The hidden temp path the renderer writes before publishing.
///
/// It keeps the output's extension (ffmpeg picks the muxer from it) and lives
/// beside the output so the publish stays on one filesystem. This is the one
/// spelling the executor and the startup sweep share; a different shape here
/// would make a crash leftover invisible to the sweep.
pub fn partial_path(output: &Path, id: u64) -> PathBuf {
    let name = output
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out.mp4".to_string());
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => (stem, ext),
        _ => (name.as_str(), "mp4"),
    };
    output.with_file_name(format!(".{stem}.job{id}.{ext}"))
}

/// Which terminal jobs a retention pass removes, oldest first.
///
/// Retention order is the time a job **finished**, or its creation time for a
/// record that never started, with the id as the tiebreak (ids are monotonic,
/// so equal timestamps keep submission order). The newest `keep` terminal jobs
/// are always retained; beyond those, `older_than_days` (when set) further
/// requires the same timestamp to be older than `now - days`, so a fresh job
/// is never pruned just because `keep` is small. Queued and running jobs are
/// not candidates at all.
pub fn prune_plan(
    jobs: &[JobRecord],
    keep: usize,
    older_than_days: Option<u64>,
    now: u64,
) -> Vec<u64> {
    let mut terminal: Vec<&JobRecord> = jobs.iter().filter(|job| job.state.is_terminal()).collect();
    terminal.sort_by_key(|job| {
        (
            std::cmp::Reverse(retention_time(job)),
            std::cmp::Reverse(job.id),
        )
    });
    let cutoff = older_than_days.map(|days| now.saturating_sub(days.saturating_mul(DAY_SECS)));
    terminal
        .into_iter()
        .skip(keep)
        .filter(|job| cutoff.is_none_or(|cutoff| retention_time(job) < cutoff))
        // The plan is oldest first: the natural order to report deletions.
        .rev()
        .map(|job| job.id)
        .collect()
}

/// The timestamp a job is aged by: its finish if it ran, else its creation.
fn retention_time(job: &JobRecord) -> u64 {
    job.finished_at.unwrap_or(job.created_at)
}

/// Seconds since the epoch; 0 if the clock is before it (never panics).
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Serialize pretty JSON and rename a temp file over `path`. The temp lives in
/// the same directory so the rename is atomic (same filesystem).
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), JobError> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| JobError::io(dir, e))?;
    let mut json = serde_json::to_string_pretty(value).map_err(|e| JobError::json(path, e))?;
    json.push('\n');
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| JobError::io(dir, e))?;
    tmp.write_all(json.as_bytes())
        .map_err(|e| JobError::io(path, e))?;
    tmp.persist(path).map_err(|e| JobError::io(path, e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Seconds;

    fn project() -> Project {
        let mut project = Project::new();
        project.fps = Some(Seconds::from_integer(30));
        project
    }

    fn store() -> (tempfile::TempDir, JobStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nile.json");
        Project::new().save(&path).expect("save project");
        (dir, JobStore::at(&path))
    }

    #[test]
    fn a_job_round_trips_with_its_snapshot() {
        let (_dir, store) = store();
        let mut project = project();
        project.revision = 7;
        project.add_sequence("Second", None, None).unwrap();
        let job = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/out.mp4"),
                JobExport {
                    preset: ExportPreset::Max,
                    crf: Some(19),
                    encoder_preset: Some("medium".into()),
                },
            )
            .expect("create");
        assert_eq!(job.id, 1);
        assert_eq!(job.state, JobState::Queued);
        assert_eq!(job.revision, 7);
        assert_eq!(store.get(1).unwrap().as_ref(), Some(&job));
        assert_eq!(store.list().unwrap(), vec![job.clone()]);
        // The snapshot is a full project, not a pointer at the live file.
        let snapshot = store.load_snapshot(&job).expect("snapshot");
        assert_eq!(snapshot.revision, 7);
        assert_eq!(snapshot.sequences.len(), 2);
        // Ids never repeat, even after the first job is terminal.
        let second = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/two.mp4"),
                JobExport::default(),
            )
            .expect("create");
        assert_eq!(second.id, 2);
    }

    #[test]
    fn recovery_keeps_queued_and_interrupts_running() {
        let (_dir, store) = store();
        let project = project();
        let queued = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/q.mp4"),
                JobExport::default(),
            )
            .expect("create");
        let running = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/r.mp4"),
                JobExport::default(),
            )
            .expect("create");
        let mut running = running;
        running.state = JobState::Running;
        running.started_at = Some(1);
        store.update(&running).expect("update");

        let recovered = store.recover().expect("recover");
        assert_eq!(recovered.jobs.len(), 2);
        assert_eq!(recovered.jobs[0].state, JobState::Queued);
        assert_eq!(recovered.jobs[1].state, JobState::Interrupted);
        assert_eq!(recovered.interrupted.len(), 1, "only the flipped record");
        assert_eq!(recovered.interrupted[0].id, running.id);
        assert!(recovered.jobs[1].finished_at.is_some());
        assert!(
            recovered.jobs[1]
                .error
                .as_deref()
                .unwrap()
                .contains("stopped")
        );
        assert!(recovered.skipped.is_empty());
        // A second recovery is a no-op: interrupted is terminal, so it must
        // not be reported as a *new* interruption.
        let again = store.recover().expect("recover again");
        assert_eq!(again.jobs, recovered.jobs);
        assert!(again.interrupted.is_empty());
        assert_eq!(
            store.get(queued.id).unwrap().unwrap().state,
            JobState::Queued
        );
    }

    #[test]
    fn a_newer_job_schema_is_rejected_not_misread() {
        let (_dir, store) = store();
        let job = store
            .create(
                &project(),
                1,
                PathBuf::from("/tmp/x.mp4"),
                JobExport::default(),
            )
            .expect("create");
        let path = store.job_dir(job.id).join(JOB_FILE);
        let mut text: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        text["version"] = serde_json::json!(JOB_VERSION + 1);
        // A newer record may add fields; the version error must win over a
        // generic unknown-field parse error.
        text["future_field"] = serde_json::json!(true);
        std::fs::write(&path, text.to_string()).unwrap();
        let error = store.get(job.id).expect_err("version differs");
        assert!(matches!(error, JobError::Version { .. }), "{error:?}");
        assert!(error.to_string().contains("delete the job directory"));
        // The queue skips it instead of failing every other job.
        assert!(store.list().unwrap().is_empty());
        let recovery = store.recover().expect("recover skips it");
        assert!(recovery.jobs.is_empty());
        assert_eq!(recovery.skipped.len(), 1);
        assert!(store.job_dir(job.id).exists(), "the directory is kept");
    }

    #[test]
    fn recovery_skips_a_corrupt_record_and_keeps_it_on_disk() {
        let (_dir, store) = store();
        let project = project();
        let good = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/good.mp4"),
                JobExport::default(),
            )
            .expect("create");
        let corrupt = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/bad.mp4"),
                JobExport::default(),
            )
            .expect("create");
        let path = store.job_dir(corrupt.id).join(JOB_FILE);
        std::fs::write(&path, "{ not json").unwrap();

        let recovery = store.recover().expect("recover");
        assert_eq!(recovery.jobs.len(), 1);
        assert_eq!(recovery.jobs[0].id, good.id);
        assert_eq!(recovery.skipped.len(), 1);
        assert!(store.job_dir(corrupt.id).exists());
        assert_eq!(store.list().unwrap().len(), 1, "list also skips it");
    }

    #[test]
    fn removing_a_job_deletes_its_directory() {
        let (_dir, store) = store();
        let job = store
            .create(
                &project(),
                1,
                PathBuf::from("/tmp/rm.mp4"),
                JobExport::default(),
            )
            .expect("create");
        assert!(store.job_dir(job.id).exists());
        store.remove(job.id).expect("remove");
        assert!(!store.job_dir(job.id).exists());
        assert!(store.get(job.id).unwrap().is_none());
        assert!(matches!(
            store.remove(job.id).expect_err("already gone"),
            JobError::NotFound(_)
        ));
    }

    /// Create a job, then stamp the fields retention reads.
    fn aged(
        store: &JobStore,
        project: &Project,
        state: JobState,
        created_at: u64,
        finished_at: Option<u64>,
    ) -> JobRecord {
        let mut job = store
            .create(
                project,
                1,
                PathBuf::from(format!("/tmp/{created_at}.mp4")),
                JobExport::default(),
            )
            .expect("create");
        job.state = state;
        job.created_at = created_at;
        job.finished_at = finished_at;
        store.update(&job).expect("update");
        job
    }

    #[test]
    fn retention_prunes_only_terminal_jobs_beyond_the_keep_count() {
        let (_dir, store) = store();
        let project = project();
        let oldest_done = aged(&store, &project, JobState::Done, 100, Some(110));
        let queued = aged(&store, &project, JobState::Queued, 200, None);
        let running = aged(&store, &project, JobState::Running, 300, None);
        let failed = aged(&store, &project, JobState::Failed, 400, Some(420));
        let newest_done = aged(&store, &project, JobState::Cancelled, 500, Some(510));
        let jobs = vec![
            oldest_done.clone(),
            queued.clone(),
            running.clone(),
            failed.clone(),
            newest_done.clone(),
        ];

        // Newest terminal first: cancelled (id 5), failed (id 4), done (id 1).
        assert_eq!(prune_plan(&jobs, 2, None, 1_000), vec![oldest_done.id]);
        assert_eq!(
            prune_plan(&jobs, 1, None, 1_000),
            vec![oldest_done.id, failed.id]
        );
        assert_eq!(
            prune_plan(&jobs, 0, None, 1_000),
            vec![oldest_done.id, failed.id, newest_done.id],
            "oldest first"
        );

        // A live job is not a candidate however small `keep` is.
        for keep in 0..=3 {
            let pruned = prune_plan(&jobs, keep, None, 1_000);
            assert!(
                !pruned.contains(&queued.id),
                "queued job pruned: {pruned:?}"
            );
            assert!(
                !pruned.contains(&running.id),
                "running job pruned: {pruned:?}"
            );
        }
    }

    #[test]
    fn retention_age_uses_the_finish_time_then_creation_and_keeps_the_newest() {
        let now = 100 * DAY_SECS;
        let (_dir, store) = store();
        let project = project();
        let ten_days = aged(
            &store,
            &project,
            JobState::Done,
            now - 40 * DAY_SECS,
            Some(now - 10 * DAY_SECS),
        );
        let thirty_days = aged(
            &store,
            &project,
            JobState::Failed,
            now - 30 * DAY_SECS,
            None,
        );
        let one_day = aged(
            &store,
            &project,
            JobState::Cancelled,
            now - 90 * DAY_SECS,
            Some(now - DAY_SECS),
        );
        let jobs = vec![ten_days.clone(), thirty_days.clone(), one_day.clone()];

        // `keep` protects the newest terminal job; age prunes the rest.
        assert_eq!(
            prune_plan(&jobs, 1, Some(7), now),
            vec![thirty_days.id, ten_days.id]
        );
        // An age-only window never prunes inside the keep window.
        assert_eq!(prune_plan(&jobs, 3, Some(7), now), Vec::<u64>::new());
        // A job that never started is aged by its creation time.
        assert_eq!(prune_plan(&jobs, 0, Some(20), now), vec![thirty_days.id]);
        // Zero days means "strictly older than now", oldest first.
        assert_eq!(
            prune_plan(&jobs, 0, Some(0), now),
            vec![thirty_days.id, ten_days.id, one_day.id]
        );
    }

    #[test]
    fn equal_retention_times_break_the_tie_by_id() {
        let now = 1_000;
        let (_dir, store) = store();
        let project = project();
        let first = aged(&store, &project, JobState::Done, 100, Some(900));
        let second = aged(&store, &project, JobState::Done, 100, Some(900));
        let jobs = vec![first.clone(), second.clone()];
        assert_eq!(prune_plan(&jobs, 1, None, now), vec![first.id]);
        assert_eq!(prune_plan(&jobs, 0, None, now), vec![first.id, second.id]);
    }

    #[test]
    fn the_startup_sweep_removes_only_stale_render_temps() {
        let (dir, store) = store();
        let project = project();
        let renders = dir.path().join("renders");
        std::fs::create_dir_all(&renders).expect("renders dir");
        let output = |name: &str| renders.join(name);

        let done = store
            .create(&project, 1, output("done.mp4"), JobExport::default())
            .expect("create");
        let mut done = done;
        done.state = JobState::Done;
        store.update(&done).expect("update");
        let queued = store
            .create(&project, 1, output("queued.mp4"), JobExport::default())
            .expect("create");
        let running = store
            .create(&project, 1, output("running.mp4"), JobExport::default())
            .expect("create");
        let mut running = running;
        running.state = JobState::Running;
        store.update(&running).expect("update");

        let done_temp = partial_path(&done.output, done.id);
        let queued_temp = partial_path(&queued.output, queued.id);
        let running_temp = partial_path(&running.output, running.id);
        std::fs::write(&done_temp, b"crashed render").unwrap();
        std::fs::write(&queued_temp, b"queued").unwrap();
        std::fs::write(&running_temp, b"live").unwrap();
        std::fs::write(&done.output, b"deliverable").unwrap();
        let unexplained = renders.join(".unknown.job99.mp4");
        std::fs::write(&unexplained, b"no record names me").unwrap();
        let not_a_temp = renders.join(".notes.txt");
        std::fs::write(&not_a_temp, b"keep").unwrap();

        let sweep = store.sweep_temps(&[done.clone(), queued.clone(), running.clone()]);
        assert_eq!(sweep.removed, vec![done_temp.clone()]);
        assert!(sweep.failed.is_empty(), "{:?}", sweep.failed);
        assert!(!done_temp.exists(), "a terminal temp is removed");
        assert!(queued_temp.exists(), "a queued temp is untouched");
        assert!(running_temp.exists(), "a running temp is untouched");
        assert!(done.output.exists(), "the deliverable is untouched");
        assert!(unexplained.exists(), "no record explains it, so keep it");
        assert!(not_a_temp.exists());
    }

    #[test]
    fn the_sweep_never_deletes_a_path_that_is_another_records_output() {
        let (dir, store) = store();
        let mut project = project();
        project.fps = Some(Seconds::from_integer(30));
        let first = store
            .create(
                &project,
                1,
                dir.path().join("out.mp4"),
                JobExport::default(),
            )
            .expect("create");
        let mut first = first;
        first.state = JobState::Done;
        store.update(&first).expect("update");
        // A hand-made collision: the second job delivers exactly the first
        // job's hidden temp name.
        let collision = partial_path(&first.output, first.id);
        let second = store
            .create(&project, 1, collision.clone(), JobExport::default())
            .expect("create");
        std::fs::write(&collision, b"a real deliverable").unwrap();

        let sweep = store.sweep_temps(&[first, second]);
        assert!(sweep.removed.is_empty(), "{:?}", sweep.removed);
        assert!(
            collision.exists(),
            "another record's output is a deliverable"
        );
    }

    #[test]
    fn removing_the_highest_id_does_not_recycle_it() {
        let (_dir, store) = store();
        let project = project();
        let first = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/one.mp4"),
                JobExport::default(),
            )
            .expect("create");
        assert_eq!(first.id, 1);
        store.remove(first.id).expect("remove");
        let second = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/two.mp4"),
                JobExport::default(),
            )
            .expect("create");
        assert_eq!(second.id, 2, "a removed id is not reused");
        store.remove(second.id).expect("remove");
        let third = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/three.mp4"),
                JobExport::default(),
            )
            .expect("create");
        assert_eq!(third.id, 3);
    }

    #[test]
    fn an_upgraded_store_advances_the_marker_when_the_highest_id_is_removed() {
        let (_dir, store) = store();
        let project = project();
        let first = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/one.mp4"),
                JobExport::default(),
            )
            .expect("create");
        let second = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/two.mp4"),
                JobExport::default(),
            )
            .expect("create");
        assert_eq!((first.id, second.id), (1, 2));
        // Simulate a project written before `.next-id` existed: only the
        // directories carry the high-water mark. Removing the highest id must
        // still advance it, or the next create recycles 2.
        std::fs::remove_file(store.dir().join(NEXT_ID_FILE)).expect("remove marker");
        store.remove(second.id).expect("remove");
        assert_eq!(store.next_id().expect("next id"), 3);
        let third = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/three.mp4"),
                JobExport::default(),
            )
            .expect("create");
        assert_eq!(third.id, 3, "an upgraded store does not recycle ids");
    }

    #[test]
    fn a_corrupt_high_water_mark_is_an_error_not_a_rebuild() {
        let (_dir, store) = store();
        let project = project();
        let job = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/one.mp4"),
                JobExport::default(),
            )
            .expect("create");
        for text in ["not a number\n", "0\n", "-1\n", "\n"] {
            std::fs::write(store.dir().join(NEXT_ID_FILE), text).expect("write marker");
            let error = store
                .create(
                    &project,
                    1,
                    PathBuf::from("/tmp/two.mp4"),
                    JobExport::default(),
                )
                .expect_err("corrupt marker");
            assert!(
                matches!(error, JobError::NextId { .. }),
                "{text:?}: {error:?}"
            );
            assert!(error.to_string().contains("high-water"), "{error}");
            // Removal needs a trustworthy marker too: deleting without one
            // could recycle the id.
            let error = store.remove(job.id).expect_err("corrupt marker");
            assert!(
                matches!(error, JobError::NextId { .. }),
                "{text:?}: {error:?}"
            );
        }
        // Repairing the marker recovers the store without a directory scan.
        std::fs::write(store.dir().join(NEXT_ID_FILE), "2\n").expect("repair");
        assert!(store.get(job.id).expect("get").is_some());
        let next = store
            .create(
                &project,
                1,
                PathBuf::from("/tmp/two.mp4"),
                JobExport::default(),
            )
            .expect("create after repair");
        assert_eq!(next.id, 2);
    }

    #[test]
    fn partial_path_keeps_the_extension_and_is_hidden() {
        let out = Path::new("/tmp/show.mp4");
        assert_eq!(partial_path(out, 7), Path::new("/tmp/.show.job7.mp4"));
        assert_eq!(
            partial_path(Path::new("/tmp/noext"), 2).extension(),
            Some("mp4".as_ref())
        );
    }

    #[test]
    fn progress_reports_a_clamped_fraction() {
        assert_eq!(JobProgress::default().fraction(), 0.0);
        assert_eq!(
            JobProgress {
                done: 50,
                total: 100
            }
            .fraction(),
            0.5
        );
        assert_eq!(
            JobProgress {
                done: 200,
                total: 100
            }
            .fraction(),
            1.0
        );
    }
}
