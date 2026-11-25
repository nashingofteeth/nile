//! The server-owned job queue panel: rendering, list refresh, cancel.
//!
//! The queue itself is tested in `crate::jobs`; these tests pin the TUI side:
//! a row per job with id/state/progress, and `x` routed through the remote
//! session to `job.cancel`.

use std::io::BufReader;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nle_core::jobs::{JOB_VERSION, JobExport, JobProgress, JobRecord, JobState};
use nle_protocol::{Call, Event, Handler, JobList, Reply, RpcError, Server};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::super::panels::jobs::render_jobs;
use super::super::{JobsState, Mode};
use super::*;
use crate::session::Session;

fn job(id: u64, state: JobState, done: u64, total: u64, report: Option<&str>) -> JobRecord {
    JobRecord {
        version: JOB_VERSION,
        id,
        state,
        revision: 7,
        sequence: 1,
        output: PathBuf::from(format!("/media/out-{id}.mp4")),
        export: JobExport::default(),
        created_at: 1,
        started_at: None,
        finished_at: None,
        progress: JobProgress { done, total },
        report: report.map(PathBuf::from),
        error: None,
        snapshot: PathBuf::from("project.json"),
    }
}

#[test]
fn the_panel_lists_ids_states_progress_and_reports() {
    let mut app = App::new(asset_project(), temp_project_path("jobs-panel"));
    app.mode = Mode::Jobs(JobsState { sel: 0 });
    app.jobs = vec![
        job(1, JobState::Running, 43, 100, None),
        job(
            2,
            JobState::Done,
            100,
            100,
            Some("/media/out-2.mp4.report.json"),
        ),
    ];
    let mut term = Terminal::new(TestBackend::new(90, 8)).unwrap();
    term.draw(|f| render_jobs(f, f.area(), &app)).unwrap();
    let buf = term.backend().buffer();
    let text: String = (0..8u16)
        .map(|y| (0..90u16).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("jobs"), "{text}");
    assert!(text.contains("#1"), "{text}");
    assert!(text.contains("running"), "{text}");
    assert!(text.contains("43%"), "{text}");
    assert!(text.contains("#2"), "{text}");
    assert!(text.contains("done"), "{text}");
    assert!(
        text.contains("report"),
        "the retained report is visible:\n{text}"
    );
    assert!(text.contains("out-1.mp4"), "{text}");
}

/// A server that only knows the job methods the panel uses.
struct FakeJobs {
    project: nle_core::Project,
    jobs: Vec<JobRecord>,
    cancelled: Arc<Mutex<Vec<u64>>>,
}

impl Handler for FakeJobs {
    fn handle(&mut self, call: &Call) -> Result<Reply, RpcError> {
        match call {
            Call::Hello => Ok(Reply::Hello(nle_protocol::Hello::new(7, 0, 1))),
            Call::ProjectShow => Ok(Reply::Project(self.project.clone())),
            Call::JobList => Ok(Reply::Jobs(JobList::new(7, self.jobs.clone()))),
            Call::JobCancel { id } => {
                self.cancelled.lock().unwrap().push(*id);
                let mut job = self
                    .jobs
                    .iter()
                    .find(|job| job.id == *id)
                    .cloned()
                    .ok_or_else(|| RpcError::invalid_params("job not found"))?;
                job.state = JobState::Cancelled;
                Ok(Reply::Job(job))
            }
            other => Err(RpcError::method_not_found(other.method())),
        }
    }

    fn take_events(&mut self) -> Vec<Event> {
        Vec::new()
    }
}

#[test]
fn x_cancels_the_selected_job_through_the_server() {
    let dir = std::env::temp_dir().join(format!("nle-tui-jobs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.json");
    let project = asset_project();
    project.save(&path).unwrap();
    let socket = nle_core::state::session_socket_path(&path);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let cancelled = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&cancelled);
    let jobs = vec![
        job(1, JobState::Queued, 0, 100, None),
        job(2, JobState::Queued, 0, 100, None),
    ];
    let serve = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let reader = stream.try_clone().unwrap();
        let handler = FakeJobs {
            project,
            jobs,
            cancelled: seen,
        };
        let mut server = Server::new(BufReader::new(reader), stream, handler);
        let _ = server.serve();
    });

    let mut app = App::with_session(Session::open(&path).expect("remote session"));
    super::super::jobs::open_jobs(&mut app);
    assert_eq!(app.jobs.len(), 2, "open lists the queue");
    assert!(matches!(app.mode, Mode::Jobs(_)));

    // Select the second row and cancel it.
    app.mode = Mode::Jobs(JobsState { sel: 1 });
    super::super::jobs::cancel_selected(&mut app);
    assert!(
        app.message.contains("cancelled #2"),
        "message: {}",
        app.message
    );
    assert_eq!(*cancelled.lock().unwrap(), vec![2]);

    // The panel is still open after cancelling.
    assert!(matches!(app.mode, Mode::Jobs(_)));
    drop(app);
    let _ = serve.join();
}
