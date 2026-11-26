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

    /// Highest existing id, or 0. Ids are never reused: a stale client's
    /// `job.get 3` must not find a newer job.
    fn next_id(&self) -> Result<u64, JobError> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(1),
            Err(e) => return Err(JobError::io(&self.dir, e)),
        };
        let mut max = 0;
        for entry in entries {
            let entry = entry.map_err(|e| JobError::io(&self.dir, e))?;
            if let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u64>().ok())
            {
                max = max.max(id);
            }
        }
        Ok(max + 1)
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
    pub fn remove(&self, id: u64) -> Result<(), JobError> {
        let dir = self.job_dir(id);
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
        project.add_sequence("Second", None, None);
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
