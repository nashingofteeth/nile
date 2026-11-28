//! `nile serve --stdio` end to end: real process, real project, real history.
//!
//! The protocol crate covers envelopes and transport in isolation; this test
//! proves the handler wiring — CAS, saves, events, and the history the CLI
//! reads after the server exits.

mod common;

use common::{P, Sandbox};
use nle_core::Seconds;
use nle_core::analysis::Word;
use serde_json::{Value, json};

fn active(project: &Value) -> &Value {
    let id = project["active"].as_u64().expect("active sequence");
    project["sequences"]
        .as_array()
        .expect("sequences")
        .iter()
        .find(|sequence| sequence["id"].as_u64() == Some(id))
        .expect("active sequence")
}

fn call(serve: &mut common::Serve, id: u64, method: &str, params: Option<Value>) -> Value {
    let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method});
    if let Some(params) = params {
        request["params"] = params;
    }
    serve.request(request)
}

#[test]
fn job_submit_renders_the_snapshot_and_retains_the_report() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();

    let hello = {
        let mut client = serve.client();
        client.hello().expect("hello")
    };
    let output = sandbox.file("job-out.mp4");
    let job = {
        let mut client = serve.client();
        client
            .submit_job(
                hello.revision,
                output.clone(),
                1,
                nle_core::jobs::JobExport {
                    preset: nle_core::ExportPreset::Balanced,
                    crf: Some(30),
                    encoder_preset: None,
                },
            )
            .expect("submit")
    };
    assert_eq!(job.state, nle_core::JobState::Queued);
    assert_eq!(job.revision, hello.revision, "the snapshot's revision");

    // The export is a real ffmpeg run on a worker. Poll the durable record
    // until it leaves the queue; progress events may arrive in between and are
    // buffered by the typed client.
    let mut done = job.clone();
    for _ in 0..600 {
        if done.state.is_terminal() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        done = {
            let mut client = serve.client();
            client.job(job.id).expect("job.get")
        };
    }
    assert_eq!(done.state, nle_core::JobState::Done, "{done:?}");
    assert_eq!(done.revision, hello.revision);
    assert!(output.exists(), "the deliverable exists");
    let report = done.report.expect("a retained report");
    assert!(report.exists(), "report {}", report.display());
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(&report).expect("read report"))
            .expect("report JSON");
    assert_eq!(report["revision"], hello.revision);
    assert_eq!(report["sequence"], 1);

    // The list and get agree, and an unknown id is invalid params.
    let list = {
        let mut client = serve.client();
        client.jobs().expect("job.list")
    };
    assert_eq!(list.jobs.len(), 1);
    assert_eq!(list.jobs[0].id, job.id);
    let missing = call(&mut serve, 99, "job.get", Some(json!({"id": 99})));
    assert_eq!(missing["error"]["code"], -32602);

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn job_submit_cas_refuses_a_stale_revision() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();
    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");
    let out = sandbox.file("stale.mp4").display().to_string();
    let stale = call(
        &mut serve,
        2,
        "job.submit",
        Some(json!({
            "base_revision": revision.saturating_sub(1),
            "output": out,
            "sequence": 1
        })),
    );
    assert_eq!(stale["error"]["code"], -32001);
    let list = call(&mut serve, 3, "job.list", None);
    assert!(list["result"]["jobs"].as_array().unwrap().is_empty());
    assert!(serve.finish().success(), "serve exits cleanly");
}

#[cfg(unix)]
#[test]
fn job_cli_submits_and_lists_through_the_socket() {
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    struct Guard(Option<Child>);
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    let sandbox = Sandbox::new();
    sandbox.base();
    let socket = nle_core::state::session_socket_path(&sandbox.file(P));
    let mut server = Guard(Some(
        Command::new(env!("CARGO_BIN_EXE_nile"))
            .current_dir(sandbox.path())
            .args(["--project", P, "serve"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn nile serve"),
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&socket).is_err() {
        assert!(
            Instant::now() < deadline,
            "server never bound {}",
            socket.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let out = sandbox.file("cli-job.mp4");
    let before = {
        let stream = UnixStream::connect(&socket).expect("connect");
        let write = stream.try_clone().expect("clone");
        let mut client = nle_protocol::Client::new(BufReader::new(stream), write);
        client.hello().expect("hello").revision
    };
    let submit = Command::new(env!("CARGO_BIN_EXE_nile"))
        .current_dir(sandbox.path())
        .args([
            "--project",
            P,
            "job",
            "submit",
            "-o",
            &out.display().to_string(),
            "--preset",
            "balanced",
            "--crf",
            "30",
        ])
        .output()
        .expect("job submit");
    assert!(submit.status.success(), "{submit:?}");
    let stdout = String::from_utf8_lossy(&submit.stdout);
    assert!(stdout.contains("queued job #1"), "{stdout}");

    let list = Command::new(env!("CARGO_BIN_EXE_nile"))
        .current_dir(sandbox.path())
        .args(["--project", P, "job", "ls"])
        .output()
        .expect("job ls");
    assert!(list.status.success(), "{list:?}");
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("#1"), "{stdout}");

    let show = Command::new(env!("CARGO_BIN_EXE_nile"))
        .current_dir(sandbox.path())
        .args(["--project", P, "job", "show", "99"])
        .output()
        .expect("job show");
    assert!(!show.status.success(), "an unknown id is refused");
    let stderr = String::from_utf8_lossy(&show.stderr);
    assert!(stderr.contains("not found"), "{stderr}");

    // Wait for the real render, then remove the terminal record.
    let mut state = nle_core::JobState::Queued;
    for _ in 0..600 {
        let job = {
            let stream = UnixStream::connect(&socket).expect("connect");
            let write = stream.try_clone().expect("clone");
            let mut client = nle_protocol::Client::new(BufReader::new(stream), write);
            client.job(1).expect("job.get")
        };
        state = job.state;
        if state.is_terminal() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(state.is_terminal(), "job finished: {state:?}");
    let rm = Command::new(env!("CARGO_BIN_EXE_nile"))
        .current_dir(sandbox.path())
        .args(["--project", P, "job", "rm", "1"])
        .output()
        .expect("job rm");
    assert!(rm.status.success(), "{rm:?}");
    let stdout = String::from_utf8_lossy(&rm.stdout);
    assert!(stdout.contains("removed #1"), "{stdout}");
    let gone = Command::new(env!("CARGO_BIN_EXE_nile"))
        .current_dir(sandbox.path())
        .args(["--project", P, "job", "show", "1"])
        .output()
        .expect("job show after rm");
    assert!(!gone.status.success(), "the removed id is gone");
    assert!(
        String::from_utf8_lossy(&gone.stderr).contains("not found"),
        "{gone:?}"
    );

    // Submit is a snapshot: the live project was not touched.
    let hello = {
        let stream = UnixStream::connect(&socket).expect("connect");
        let write = stream.try_clone().expect("clone");
        let mut client = nle_protocol::Client::new(BufReader::new(stream), write);
        client.hello().expect("hello")
    };
    assert_eq!(hello.revision, before, "no model write from a job submit");
    let _ = BufReader::new(UnixStream::connect(&socket).expect("second connect"));
    drop(server.0.take());
}

#[cfg(unix)]
#[test]
fn job_prune_keeps_the_newest_and_deletes_the_rest_through_the_cli() {
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    struct Guard(Option<Child>);
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    let sandbox = Sandbox::new();
    sandbox.base();

    // `--keep` is not optional: a count-less prune would be a surprise.
    let missing_keep = Command::new(env!("CARGO_BIN_EXE_nile"))
        .current_dir(sandbox.path())
        .args(["--project", P, "job", "prune"])
        .output()
        .expect("job prune without --keep");
    assert!(!missing_keep.status.success(), "{missing_keep:?}");

    let socket = nle_core::state::session_socket_path(&sandbox.file(P));
    let mut server = Guard(Some(
        Command::new(env!("CARGO_BIN_EXE_nile"))
            .current_dir(sandbox.path())
            .args(["--project", P, "serve"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn nile serve"),
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&socket).is_err() {
        assert!(
            Instant::now() < deadline,
            "server never bound {}",
            socket.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let out = sandbox.file("prune-job.mp4");
    let submit = Command::new(env!("CARGO_BIN_EXE_nile"))
        .current_dir(sandbox.path())
        .args([
            "--project",
            P,
            "job",
            "submit",
            "-o",
            &out.display().to_string(),
            "--preset",
            "preview",
            "--crf",
            "30",
        ])
        .output()
        .expect("job submit");
    assert!(submit.status.success(), "{submit:?}");

    let mut state = nle_core::JobState::Queued;
    for _ in 0..600 {
        let job = {
            let stream = UnixStream::connect(&socket).expect("connect");
            let write = stream.try_clone().expect("clone");
            let mut client = nle_protocol::Client::new(BufReader::new(stream), write);
            client.job(1).expect("job.get")
        };
        state = job.state;
        if state.is_terminal() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(state.is_terminal(), "job finished: {state:?}");

    // The newest terminal job survives `--keep 1`.
    let kept = Command::new(env!("CARGO_BIN_EXE_nile"))
        .current_dir(sandbox.path())
        .args(["--project", P, "job", "prune", "--keep", "1"])
        .output()
        .expect("job prune keep 1");
    assert!(kept.status.success(), "{kept:?}");
    assert!(
        String::from_utf8_lossy(&kept.stdout).contains("nothing to prune"),
        "{kept:?}"
    );

    // Keeping none removes the terminal record and its snapshot, and names it.
    let pruned = Command::new(env!("CARGO_BIN_EXE_nile"))
        .current_dir(sandbox.path())
        .args(["--project", P, "job", "prune", "--keep", "0"])
        .output()
        .expect("job prune keep 0");
    assert!(pruned.status.success(), "{pruned:?}");
    let stdout = String::from_utf8_lossy(&pruned.stdout);
    assert!(stdout.contains("pruned 1 job(s): #1"), "{stdout}");
    let job_dir = nle_core::state::jobs_dir(&sandbox.file(P)).join("1");
    assert!(!job_dir.exists(), "the job directory is gone");

    let gone = Command::new(env!("CARGO_BIN_EXE_nile"))
        .current_dir(sandbox.path())
        .args(["--project", P, "job", "show", "1"])
        .output()
        .expect("job show after prune");
    assert!(!gone.status.success(), "the pruned id is gone");
    assert!(
        String::from_utf8_lossy(&gone.stderr).contains("not found"),
        "{gone:?}"
    );

    drop(server.0.take());
}

#[cfg(unix)]
#[test]
fn job_batch_queues_one_export_per_sequence_through_the_cli() {
    use std::io::BufReader;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    struct Guard(Option<Child>);
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    let sandbox = Sandbox::new();
    sandbox.base();
    let socket = nle_core::state::session_socket_path(&sandbox.file(P));
    let mut server = Guard(Some(
        Command::new(env!("CARGO_BIN_EXE_nile"))
            .current_dir(sandbox.path())
            .args(["--project", P, "serve"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn nile serve"),
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&socket).is_err() {
        assert!(
            Instant::now() < deadline,
            "server never bound {}",
            socket.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let connect = || {
        let stream = UnixStream::connect(&socket).expect("connect");
        let write = stream.try_clone().expect("clone");
        nle_protocol::Client::new(BufReader::new(stream), write)
    };

    // A derived second sequence: both have material, so each job is a real
    // (cancellable) render rather than an empty-timeline edge case.
    {
        let mut client = connect();
        let revision = client.hello().expect("hello").revision;
        let derived = client
            .derive_sequence(revision, 1, "Second", None, None)
            .expect("derive");
        assert_eq!(derived.sequence.id, 2);
    }

    let before = {
        let mut client = connect();
        client.hello().expect("hello").revision
    };

    fn run_batch(sandbox: &Sandbox, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_nile"))
            .current_dir(sandbox.path())
            .args(["--project", P, "job", "batch"])
            .args(args)
            .output()
            .expect("nile job batch")
    }

    fn cancel_live(client: &mut nle_protocol::Client<std::io::BufReader<UnixStream>, UnixStream>) {
        for job in client.jobs().expect("job.list").jobs {
            if !job.state.is_terminal() {
                let _ = client.cancel_job(job.id);
            }
        }
    }

    // A bad selection refuses before the output directory exists.
    let refused = sandbox.file("batch-refused");
    let refused_arg = refused.display().to_string();
    let bad = run_batch(
        &sandbox,
        &["--output-dir", &refused_arg, "--sequence", "99"],
    );
    assert!(!bad.status.success(), "{bad:?}");
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("no sequence #99"),
        "{bad:?}"
    );
    assert!(
        !refused.exists(),
        "a selection refusal creates no directory"
    );

    // Duplicate ids are refused too, before any directory or job.
    let duplicate_dir = sandbox.file("batch-duplicate");
    let duplicate_arg = duplicate_dir.display().to_string();
    let duplicate = run_batch(
        &sandbox,
        &[
            "--output-dir",
            &duplicate_arg,
            "--sequence",
            "2",
            "--sequence",
            "2",
        ],
    );
    assert!(!duplicate.status.success(), "{duplicate:?}");
    assert!(
        String::from_utf8_lossy(&duplicate.stderr).contains("more than once"),
        "{duplicate:?}"
    );
    assert!(
        !duplicate_dir.exists(),
        "a duplicate refusal creates no directory"
    );

    // An existing render refuses the batch before any submit: no job appears
    // and the directory keeps only the file the user already had.
    let existing = sandbox.file("batch-existing");
    std::fs::create_dir(&existing).expect("mkdir");
    std::fs::write(existing.join("sequence-1.mp4"), b"an old render").expect("write");
    let existing_arg = existing.display().to_string();
    let collision = run_batch(&sandbox, &["--output-dir", &existing_arg]);
    assert!(!collision.status.success(), "{collision:?}");
    assert!(
        String::from_utf8_lossy(&collision.stderr).contains("already exists"),
        "{collision:?}"
    );
    {
        let mut client = connect();
        let jobs = client.jobs().expect("job.list");
        assert!(
            jobs.jobs.is_empty(),
            "the existing-file refusal queued nothing: {jobs:?}"
        );
    }
    assert_eq!(
        std::fs::read_dir(&existing).expect("read_dir").count(),
        1,
        "the refusal wrote no second output"
    );

    // No `--sequence`: every sequence, in listing order, into sequence-<id>.mp4.
    let dir = sandbox.file("batch");
    let dir_arg = dir.display().to_string();
    let batch = run_batch(
        &sandbox,
        &[
            "--output-dir",
            &dir_arg,
            "--preset",
            "preview",
            "--crf",
            "30",
        ],
    );
    assert!(batch.status.success(), "{batch:?}");
    let stdout = String::from_utf8_lossy(&batch.stdout);
    for (id, sequence) in [(1, 1), (2, 2)] {
        let output = dir.join(format!("sequence-{sequence}.mp4"));
        let line = format!(
            "queued job #{id} for sequence #{sequence} → {}",
            output.display()
        );
        assert!(stdout.contains(&line), "missing {line:?} in {stdout}");
    }

    let mut client = connect();
    let queued = client.jobs().expect("job.list");
    assert_eq!(queued.jobs.len(), 2, "{queued:?}");
    for (job, sequence) in queued.jobs.iter().zip([1u32, 2]) {
        assert_eq!(job.sequence, sequence);
        assert_eq!(job.output, dir.join(format!("sequence-{sequence}.mp4")));
    }
    assert!(dir.exists(), "the output directory was created");

    // Stop the renders instead of waiting for two full exports.
    cancel_live(&mut client);

    // A relative `--output-dir` resolves against the client's cwd.
    let relative = run_batch(
        &sandbox,
        &["--output-dir", "renders", "--preset", "preview"],
    );
    assert!(relative.status.success(), "{relative:?}");
    let relative_dir = sandbox.path().join("renders");
    assert!(
        relative_dir.is_dir(),
        "the relative directory resolved against the client cwd"
    );
    // The client resolves a relative `--output-dir` with `current_dir()`, which
    // returns the real path: on macOS that is `/private/var/...` even when
    // `TMPDIR` spells the same directory `/var/...`.
    let resolved_dir = relative_dir
        .canonicalize()
        .expect("the relative render directory exists");
    let relative_jobs = client.jobs().expect("job.list").jobs;
    assert_eq!(relative_jobs.len(), 4, "{relative_jobs:?}");
    for (job, (id, sequence)) in relative_jobs[2..].iter().zip([(3u64, 1u32), (4, 2)]) {
        assert_eq!(job.id, id);
        assert_eq!(job.sequence, sequence);
        assert_eq!(
            job.output,
            resolved_dir.join(format!("sequence-{sequence}.mp4"))
        );
    }
    cancel_live(&mut client);

    // Explicit ids keep the command-line order, not the listing order.
    let ordered_dir = sandbox.file("batch-ordered");
    let ordered_arg = ordered_dir.display().to_string();
    let ordered = run_batch(
        &sandbox,
        &[
            "--output-dir",
            &ordered_arg,
            "--preset",
            "preview",
            "--sequence",
            "2",
            "--sequence",
            "1",
        ],
    );
    assert!(ordered.status.success(), "{ordered:?}");
    let stdout = String::from_utf8_lossy(&ordered.stdout);
    for (id, sequence) in [(5, 2), (6, 1)] {
        let output = ordered_dir.join(format!("sequence-{sequence}.mp4"));
        let line = format!(
            "queued job #{id} for sequence #{sequence} → {}",
            output.display()
        );
        assert!(stdout.contains(&line), "missing {line:?} in {stdout}");
    }
    assert!(
        stdout.find("queued job #5").expect("#5") < stdout.find("queued job #6").expect("#6"),
        "sequence 2 was queued before sequence 1: {stdout}"
    );
    let ordered_jobs = client.jobs().expect("job.list").jobs;
    assert_eq!(ordered_jobs.len(), 6, "{ordered_jobs:?}");
    assert_eq!(
        (ordered_jobs[4].id, ordered_jobs[4].sequence),
        (5, 2),
        "the explicit order drives ids"
    );
    assert_eq!(
        (ordered_jobs[5].id, ordered_jobs[5].sequence),
        (6, 1),
        "the explicit order drives ids"
    );
    assert_eq!(ordered_jobs[4].output, ordered_dir.join("sequence-2.mp4"));
    assert_eq!(ordered_jobs[5].output, ordered_dir.join("sequence-1.mp4"));
    cancel_live(&mut client);

    // Let the cancellations land before the read-only probe: a terminal update
    // writes job.json, and a read-only jobs dir would stall it.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let jobs = client.jobs().expect("job.list").jobs;
        assert_eq!(jobs.len(), 6, "{jobs:?}");
        if jobs.iter().all(|job| job.state.is_terminal()) {
            break;
        }
        assert!(Instant::now() < deadline, "jobs did not stop: {jobs:?}");
        std::thread::sleep(Duration::from_millis(50));
    }

    // A submit can fail after the batch created its directory. A read-only
    // jobs dir makes that deterministic; with nothing queued, the directory
    // this batch created must go. Root ignores permissions, so probe first.
    let jobs_dir = nle_core::state::jobs_dir(&sandbox.file(P));
    assert!(jobs_dir.is_dir(), "the server made its jobs directory");
    let original = std::fs::metadata(&jobs_dir)
        .expect("jobs metadata")
        .permissions();
    std::fs::set_permissions(&jobs_dir, std::fs::Permissions::from_mode(0o555))
        .expect("chmod the jobs dir");
    let blocked = std::fs::create_dir(jobs_dir.join("probe")).is_err();
    if blocked {
        let nowrite = sandbox.file("batch-nowrite");
        let nowrite_arg = nowrite.display().to_string();
        let failed = run_batch(
            &sandbox,
            &["--output-dir", &nowrite_arg, "--preset", "preview"],
        );
        assert!(!failed.status.success(), "{failed:?}");
        assert!(
            String::from_utf8_lossy(&failed.stderr).contains("submit sequence #1"),
            "{failed:?}"
        );
        assert!(
            !nowrite.exists(),
            "a zero-queued batch removes the directory it created"
        );
        let jobs = client.jobs().expect("job.list").jobs;
        assert_eq!(jobs.len(), 6, "the failed submit queued nothing: {jobs:?}");
    } else {
        let _ = std::fs::remove_dir(jobs_dir.join("probe"));
    }
    std::fs::set_permissions(&jobs_dir, original).expect("restore the jobs dir");

    let hello = client.hello().expect("hello");
    assert_eq!(
        hello.revision, before,
        "a batch submit is not a model write"
    );
    drop(server.0.take());
}

#[test]
fn protocol_server_round_trip() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    assert_eq!(hello["result"]["protocol"], 5);
    assert_eq!(
        hello["result"]["project_version"],
        nle_core::project::PROJECT_VERSION
    );
    let revision = hello["result"]["revision"].as_u64().expect("revision");
    assert!(
        hello["result"]["methods"]
            .as_array()
            .expect("methods")
            .iter()
            .any(|m| m == "action.apply")
    );

    let show = call(&mut serve, 2, "project.show", None);
    let before = active(&show["result"])["timeline"]["tracks"][0]["segments"]
        .as_array()
        .expect("segments")
        .len();
    assert_eq!(show["result"]["revision"], revision);

    let edl = call(&mut serve, 3, "timeline.edl", None);
    let edl = edl["result"].as_str().expect("EDL string");
    assert!(edl.starts_with("edl://"), "{edl}");

    // Preview runs against a clone: live revision, no save.
    let preview = call(
        &mut serve,
        4,
        "action.preview",
        Some(json!({
            "base_revision": revision,
            "actions": [{"action": "split", "track": 0, "at": 1.0}],
            "from": "1/2"
        })),
    );
    assert_eq!(preview["result"]["revision"], revision);
    assert_eq!(preview["result"]["changed"], true);
    let show = call(&mut serve, 5, "project.show", None);
    assert_eq!(show["result"]["revision"], revision, "preview did not save");
    assert_eq!(
        active(&show["result"])["timeline"]["tracks"][0]["segments"]
            .as_array()
            .expect("segments")
            .len(),
        before,
        "preview did not change the timeline"
    );

    // Split: one undo entry, one project.changed event. Split has no core
    // playhead, so there is no playhead event.
    let applied = call(
        &mut serve,
        6,
        "action.apply",
        Some(json!({
            "base_revision": revision,
            "actions": [{"action": "split", "track": 0, "at": 1.0}],
            "from": "1/2"
        })),
    );
    let after = applied["result"]["revision"].as_u64().expect("revision");
    assert_eq!(after, revision + 1);
    assert_eq!(applied["result"]["depth"], 1);
    assert_eq!(applied["result"]["changed"], true);
    assert!(
        applied["result"].get("playhead").is_none(),
        "split has no explicit playhead"
    );
    let event = serve.read();
    assert_eq!(event["method"], "project.changed");
    assert_eq!(event["params"]["revision"], after);

    let show = call(&mut serve, 7, "project.show", None);
    assert_eq!(
        active(&show["result"])["timeline"]["tracks"][0]["segments"]
            .as_array()
            .expect("segments")
            .len(),
        before + 1
    );

    // Undo restores the split's anchor and emits both events.
    let undone = call(&mut serve, 8, "undo", Some(json!({"base_revision": after})));
    assert_eq!(undone["result"]["revision"], after + 1);
    assert_eq!(undone["result"]["playhead"], "1/2");
    let event = serve.read();
    assert_eq!(event["method"], "project.changed");
    let event = serve.read();
    assert_eq!(event["method"], "playhead");
    assert_eq!(event["params"]["at"], "1/2");

    // A stale CAS and an unknown action field are errors, not disconnects.
    let stale = call(
        &mut serve,
        9,
        "action.apply",
        Some(json!({
            "base_revision": revision,
            "actions": [{"action": "split", "track": 0, "at": 2.0}]
        })),
    );
    assert_eq!(stale["error"]["code"], -32001);

    let bad = call(
        &mut serve,
        10,
        "action.apply",
        Some(json!({
            "base_revision": after + 1,
            "actions": [{
                "action": "paste",
                "track": 0,
                "at": 1.0,
                "ripple": false,
                "segmnt": {}
            }]
        })),
    );
    assert_eq!(bad["error"]["code"], -32602);

    // Redo: the split is back, with both events.
    let redone = call(
        &mut serve,
        11,
        "redo",
        Some(json!({"base_revision": after + 1})),
    );
    assert_eq!(redone["result"]["revision"], after + 2);
    let _ = serve.read();
    let _ = serve.read();

    let status = serve.finish();
    assert!(status.success(), "nile serve exited: {status:?}");

    // The history sidecar is usable by the CLI after the server exits.
    let project: Value =
        serde_json::from_str(&sandbox.ok(&["show", "--json"])).expect("project JSON");
    assert_eq!(
        active(&project)["timeline"]["tracks"][0]["segments"]
            .as_array()
            .expect("segments")
            .len(),
        before + 1
    );
    sandbox.ok(&["undo"]);
    let project: Value =
        serde_json::from_str(&sandbox.ok(&["show", "--json"])).expect("project JSON");
    assert_eq!(
        active(&project)["timeline"]["tracks"][0]["segments"]
            .as_array()
            .expect("segments")
            .len(),
        before
    );
}

#[test]
fn sequence_list_use_and_add_round_trip() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");

    let list = call(&mut serve, 2, "sequence.list", None);
    assert_eq!(list["result"]["revision"], revision);
    assert_eq!(list["result"]["active"], 1);
    assert_eq!(list["result"]["sequences"].as_array().unwrap().len(), 1);
    assert_eq!(list["result"]["sequences"][0]["name"], "Sequence 1");
    assert_eq!(list["result"]["sequences"][0]["clips"], 3);

    // Add: a model write, so CAS and a revision bump.
    let added = call(
        &mut serve,
        3,
        "sequence.add",
        Some(json!({
            "base_revision": revision,
            "name": "Vertical",
            "fps": "30/1",
            "video": {"width": 1080, "height": 1920}
        })),
    );
    let after_add = added["result"]["revision"].as_u64().expect("revision");
    assert_eq!(after_add, revision + 1);
    assert_eq!(added["result"]["active"], 1, "add does not switch");
    assert_eq!(added["result"]["sequence"]["id"], 2);
    assert_eq!(added["result"]["sequence"]["name"], "Vertical");
    assert_eq!(added["result"]["sequence"]["clips"], 0);
    assert_eq!(added["result"]["sequence"]["video"]["width"], 1080);
    assert_eq!(serve.read()["method"], "project.changed");
    let event = serve.read();
    assert_eq!(event["method"], "sequence.changed");
    assert_eq!(event["params"]["active"], 1);

    // Use: session state, so the revision does not move.
    let used = call(&mut serve, 4, "sequence.use", Some(json!({"id": 2})));
    assert_eq!(used["result"]["revision"], after_add);
    assert_eq!(used["result"]["active"], 2);
    assert_eq!(used["result"]["sequence"]["id"], 2);
    let event = serve.read();
    assert_eq!(event["method"], "sequence.changed");
    assert_eq!(event["params"]["active"], 2);
    assert_eq!(event["params"]["revision"], after_add);

    let list = call(&mut serve, 5, "sequence.list", None);
    assert_eq!(list["result"]["active"], 2);
    assert_eq!(list["result"]["sequences"].as_array().unwrap().len(), 2);

    // Refusals keep serving.
    let bad_id = call(&mut serve, 6, "sequence.use", Some(json!({"id": 99})));
    assert_eq!(bad_id["error"]["code"], -32602);
    let bad_fps = call(
        &mut serve,
        7,
        "sequence.add",
        Some(json!({
            "base_revision": after_add,
            "name": "Zero",
            "fps": "0/1"
        })),
    );
    assert_eq!(bad_fps["error"]["code"], -32602);
    let stale = call(
        &mut serve,
        8,
        "sequence.add",
        Some(json!({"base_revision": revision, "name": "Stale"})),
    );
    assert_eq!(stale["error"]["code"], -32001);

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn sequence_derive_round_trips_through_the_server() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");
    let derived = call(
        &mut serve,
        2,
        "sequence.derive",
        Some(json!({
            "base_revision": revision,
            "from": 1,
            "name": "Vertical copy",
            "fps": "24/1",
            "video": {"width": 1080, "height": 1920}
        })),
    );
    assert_eq!(derived["result"]["revision"], revision + 1);
    assert_eq!(derived["result"]["active"], 1, "derive does not switch");
    assert_eq!(derived["result"]["sequence"]["id"], 2);
    assert_eq!(derived["result"]["sequence"]["name"], "Vertical copy");
    assert_eq!(
        derived["result"]["sequence"]["clips"], 3,
        "the copied timeline has the source clips"
    );
    assert_eq!(derived["result"]["sequence"]["fps"], "24/1");
    assert_eq!(derived["result"]["sequence"]["video"]["width"], 1080);
    assert_eq!(serve.read()["method"], "project.changed");
    let event = serve.read();
    assert_eq!(event["method"], "sequence.changed");
    assert_eq!(event["params"]["active"], 1);

    let list = call(&mut serve, 3, "sequence.list", None);
    assert_eq!(list["result"]["sequences"].as_array().unwrap().len(), 2);
    assert_eq!(list["result"]["sequences"][1]["id"], 2);

    let bad = call(
        &mut serve,
        4,
        "sequence.derive",
        Some(json!({
            "base_revision": revision + 1,
            "from": 99,
            "name": "Missing"
        })),
    );
    assert_eq!(bad["error"]["code"], -32602);
    let zero = call(
        &mut serve,
        5,
        "sequence.derive",
        Some(json!({
            "base_revision": revision + 1,
            "from": 1,
            "name": "Zero canvas",
            "video": {"width": 0, "height": 720}
        })),
    );
    assert_eq!(zero["error"]["code"], -32602, "{zero}");
    let stale = call(
        &mut serve,
        6,
        "sequence.derive",
        Some(json!({
            "base_revision": revision,
            "from": 1,
            "name": "Stale"
        })),
    );
    assert_eq!(stale["error"]["code"], -32001);

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn sequence_audio_round_trips_through_the_server() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");

    // A model write: CAS, revision bump, both sequence events.
    let set = call(
        &mut serve,
        2,
        "sequence.audio",
        Some(json!({
            "base_revision": revision,
            "audio": {
                "normalize": true,
                "fade_in": "1/2",
                "fade_out": "1/1"
            }
        })),
    );
    let after = set["result"]["revision"].as_u64().expect("revision");
    assert_eq!(after, revision + 1);
    assert_eq!(set["result"]["active"], 1);
    assert_eq!(set["result"]["sequence"]["id"], 1);
    assert_eq!(set["result"]["sequence"]["audio"]["normalize"], true);
    assert_eq!(set["result"]["sequence"]["audio"]["fade_in"], "1/2");
    assert_eq!(set["result"]["sequence"]["audio"]["fade_out"], "1/1");
    let event = serve.read();
    assert_eq!(event["method"], "project.changed");
    assert_eq!(event["params"]["revision"], after);
    let event = serve.read();
    assert_eq!(event["method"], "sequence.changed");
    assert_eq!(event["params"]["active"], 1);
    assert_eq!(event["params"]["revision"], after);

    // The persisted project and the sequence listing agree.
    let show = call(&mut serve, 3, "project.show", None);
    assert_eq!(
        active(&show["result"])["audio"]["normalize"],
        true,
        "project.show carries the field"
    );
    let list = call(&mut serve, 4, "sequence.list", None);
    assert_eq!(list["result"]["sequences"][0]["audio"]["normalize"], true);
    assert_eq!(list["result"]["sequences"][0]["audio"]["fade_in"], "1/2");

    // A stale CAS is an error, not a write.
    let stale = call(
        &mut serve,
        5,
        "sequence.audio",
        Some(json!({
            "base_revision": revision,
            "audio": {"normalize": false, "fade_in": "0/1", "fade_out": "0/1"}
        })),
    );
    assert_eq!(stale["error"]["code"], -32001);

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn sequence_use_reports_the_adopted_playhead() {
    let sandbox = Sandbox::new();
    sandbox.base();
    sandbox.ok(&["sequence", "add", "Second"]);
    sandbox.ok(&["sequence", "use", "2"]);
    sandbox.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    sandbox.ok(&["seek", "1.5"]);
    sandbox.ok(&["sequence", "use", "1"]);
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");
    call(&mut serve, 2, "sequence.use", Some(json!({"id": 2})));
    let event = serve.read();
    assert_eq!(event["method"], "sequence.changed");
    assert_eq!(event["params"]["revision"], revision);
    let event = serve.read();
    assert_eq!(event["method"], "playhead");
    assert_eq!(event["params"]["at"], "3/2");

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn a_failed_sequence_use_does_not_change_the_reported_active_sequence() {
    let sandbox = Sandbox::new();
    sandbox.base();
    sandbox.ok(&["sequence", "add", "Second"]);
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");

    // Make the project path unwritable: the switch cannot be persisted.
    let path = sandbox.file(P);
    std::fs::remove_file(&path).expect("remove the project");
    std::fs::create_dir(&path).expect("replace it with a directory");

    let failed = call(&mut serve, 2, "sequence.use", Some(json!({"id": 2})));
    assert_eq!(failed["error"]["code"], -32603, "{failed}");

    let list = call(&mut serve, 3, "sequence.list", None);
    assert_eq!(
        list["result"]["active"], 1,
        "the failed switch was rolled back"
    );
    assert_eq!(list["result"]["revision"], revision);
    assert_eq!(list["result"]["sequences"].as_array().unwrap().len(), 2);

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn protocol_undo_is_scoped_to_the_active_sequence() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");

    let split = call(
        &mut serve,
        2,
        "action.apply",
        Some(json!({
            "base_revision": revision,
            "actions": [{"action": "split", "track": 0, "at": 1.0}]
        })),
    );
    let after_split = split["result"]["revision"].as_u64().expect("revision");
    let _ = serve.read(); // project.changed

    let added = call(
        &mut serve,
        3,
        "sequence.add",
        Some(json!({"base_revision": after_split, "name": "Second"})),
    );
    let after_add = added["result"]["revision"].as_u64().expect("revision");
    let _ = serve.read(); // project.changed
    let _ = serve.read(); // sequence.changed

    call(&mut serve, 4, "sequence.use", Some(json!({"id": 2})));
    let _ = serve.read(); // sequence.changed

    let refused = call(
        &mut serve,
        5,
        "undo",
        Some(json!({"base_revision": after_add})),
    );
    assert_eq!(refused["error"]["code"], -32600, "nothing to undo on 2");

    call(&mut serve, 6, "sequence.use", Some(json!({"id": 1})));
    let _ = serve.read(); // sequence.changed
    let undone = call(
        &mut serve,
        7,
        "undo",
        Some(json!({"base_revision": after_add})),
    );
    assert_eq!(undone["result"]["changed"], true);
    let _ = serve.read(); // project.changed
    let _ = serve.read(); // playhead

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn media_list_and_add_round_trip() {
    let sandbox = Sandbox::new();
    sandbox.base();
    std::fs::copy(sandbox.file("red.mp4"), sandbox.file("extra.mp4")).expect("copy media");
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");

    let list = call(&mut serve, 2, "media.list", None);
    assert_eq!(list["result"]["revision"], revision);
    assert_eq!(list["result"]["assets"].as_array().unwrap().len(), 3);
    assert_eq!(
        list["result"]["assets"][0]["used_by"],
        json!([{"sequence": 1, "segment": 1}])
    );

    let added = call(
        &mut serve,
        3,
        "media.add",
        Some(json!({
            "base_revision": revision,
            "paths": [sandbox.file("extra.mp4").display().to_string()]
        })),
    );
    let after_add = added["result"]["revision"].as_u64().expect("revision");
    assert_eq!(after_add, revision + 1);
    let assets = added["result"]["assets"].as_array().unwrap();
    assert_eq!(assets.len(), 1);
    assert_eq!(assets[0]["id"], 4);
    assert_eq!(assets[0]["audio"], true);
    assert!(assets[0]["duration"].is_string(), "{}", assets[0]);
    let event = serve.read();
    assert_eq!(event["method"], "project.changed");

    // The same path is not pooled twice and does not bump the revision.
    let again = call(
        &mut serve,
        4,
        "media.add",
        Some(json!({
            "base_revision": after_add,
            "paths": [sandbox.file("extra.mp4").display().to_string()]
        })),
    );
    assert_eq!(again["result"]["revision"], after_add);
    assert_eq!(again["result"]["assets"][0]["id"], 4);

    // The import is one undo entry: the CLI sees the same history.
    let undone = call(
        &mut serve,
        5,
        "undo",
        Some(json!({"base_revision": after_add})),
    );
    assert_eq!(undone["result"]["changed"], true);
    let _ = serve.read(); // project.changed
    let _ = serve.read(); // playhead
    let list = call(&mut serve, 6, "media.list", None);
    assert_eq!(list["result"]["assets"].as_array().unwrap().len(), 3);

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn media_usage_is_scoped_per_sequence() {
    let sandbox = Sandbox::new();
    sandbox.base();
    // Yank red from sequence 1 and paste it into sequence 2: the same asset
    // id, the same clip id, different sequences.
    sandbox.ok(&["yank", "--clip", "1"]);
    sandbox.ok(&["sequence", "add", "Second"]);
    sandbox.ok(&["sequence", "use", "2"]);
    sandbox.ok(&["paste", "--at", "0"]);
    sandbox.ok(&["sequence", "use", "1"]);
    let mut serve = sandbox.serve();

    let list = call(&mut serve, 1, "media.list", None);
    let red = list["result"]["assets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|asset| {
            asset["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("red.mp4"))
        })
        .expect("red in the pool");
    let mut usage: Vec<(u64, u64)> = red["used_by"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            (
                u["sequence"].as_u64().expect("sequence"),
                u["segment"].as_u64().expect("segment"),
            )
        })
        .collect();
    usage.sort();
    assert_eq!(
        usage,
        vec![(1, 1), (2, 1)],
        "same clip id, different sequences"
    );

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn media_add_is_atomic_and_refuses_bad_input() {
    let sandbox = Sandbox::new();
    sandbox.base();
    std::fs::copy(sandbox.file("red.mp4"), sandbox.file("extra.mp4")).expect("copy media");
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");

    let empty = call(
        &mut serve,
        2,
        "media.add",
        Some(json!({"base_revision": revision, "paths": []})),
    );
    assert_eq!(empty["error"]["code"], -32602);

    // One good path, one missing: nothing is pooled.
    let partial = call(
        &mut serve,
        3,
        "media.add",
        Some(json!({
            "base_revision": revision,
            "paths": [
                sandbox.file("extra.mp4").display().to_string(),
                sandbox.file("missing.mp4").display().to_string()
            ]
        })),
    );
    assert_eq!(partial["error"]["code"], -32602, "{partial}");

    // Exists, but fails to probe: the probe loop must also run before any
    // mutation.
    let probe_fail = call(
        &mut serve,
        4,
        "media.add",
        Some(json!({
            "base_revision": revision,
            "paths": [
                sandbox.file("extra.mp4").display().to_string(),
                sandbox.file("notmedia.txt").display().to_string()
            ]
        })),
    );
    assert_eq!(probe_fail["error"]["code"], -32602, "{probe_fail}");

    let list = call(&mut serve, 5, "media.list", None);
    assert_eq!(list["result"]["assets"].as_array().unwrap().len(), 3);
    assert_eq!(list["result"]["revision"], revision);

    let stale = call(
        &mut serve,
        6,
        "media.add",
        Some(json!({
            "base_revision": revision.saturating_sub(1),
            "paths": [sandbox.file("extra.mp4").display().to_string()]
        })),
    );
    assert_eq!(stale["error"]["code"], -32001);

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn batched_actions_are_one_undo_step() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");

    let preview = call(
        &mut serve,
        2,
        "action.preview",
        Some(json!({
            "base_revision": revision,
            "actions": [
                {"action": "split", "track": 0, "at": 1.0},
                {"action": "split", "track": 0, "at": 1.5}
            ]
        })),
    );
    assert_eq!(preview["result"]["revision"], revision);
    assert_eq!(preview["result"]["changed"], true);
    assert!(
        preview["result"]["desc"]
            .as_str()
            .unwrap()
            .starts_with("batch:")
    );

    let applied = call(
        &mut serve,
        3,
        "action.apply",
        Some(json!({
            "base_revision": revision,
            "actions": [
                {"action": "split", "track": 0, "at": 1.0},
                {"action": "split", "track": 0, "at": 1.5}
            ]
        })),
    );
    let after = applied["result"]["revision"].as_u64().expect("revision");
    assert_eq!(after, revision + 2, "each action bumps the model revision");
    assert_eq!(applied["result"]["changed"], true);
    assert!(
        applied["result"]["desc"]
            .as_str()
            .unwrap()
            .starts_with("batch:")
    );
    assert_eq!(serve.read()["method"], "project.changed");

    let show = call(&mut serve, 4, "project.show", None);
    assert_eq!(
        active(&show["result"])["timeline"]["tracks"][0]["segments"]
            .as_array()
            .unwrap()
            .len(),
        5,
        "two cuts became four clips around the original three"
    );

    let undone = call(&mut serve, 5, "undo", Some(json!({"base_revision": after})));
    assert_eq!(undone["result"]["revision"], after + 1);
    assert_eq!(undone["result"]["changed"], true);
    let _ = serve.read(); // project.changed
    let _ = serve.read(); // playhead
    let show = call(&mut serve, 6, "project.show", None);
    assert_eq!(
        active(&show["result"])["timeline"]["tracks"][0]["segments"]
            .as_array()
            .unwrap()
            .len(),
        3,
        "one undo reverses both actions"
    );

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn preview_reports_clamp_warnings() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");
    let preview = call(
        &mut serve,
        2,
        "action.preview",
        Some(json!({
            "base_revision": revision,
            "actions": [{
                "action": "paste",
                "track": 0,
                "at": 6.0,
                "ripple": false,
                "segment": {"Clip": {
                    "id": 99,
                    "asset": 1,
                    "asset_in": "0/1",
                    "asset_out": "99/1",
                    "speed": "1/1",
                    "effects": []
                }}
            }]
        })),
    );
    assert_eq!(preview["result"]["changed"], true);
    let warnings = preview["result"]["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().is_some_and(|s| s.contains("out 99"))),
        "{warnings:?}"
    );
    let diff = &preview["result"]["diff"];
    assert_eq!(diff["sequences"][0]["id"], 1);
    assert_eq!(diff["sequences"][0]["tracks"][0]["before_segments"], 3);
    assert_eq!(
        diff["sequences"][0]["tracks"][0]["after_segments"], 4,
        "the pasted clip is in the preview diff"
    );
    assert_eq!(preview["result"]["revision"], revision);
    let show = call(&mut serve, 3, "project.show", None);
    assert_eq!(show["result"]["revision"], revision, "preview did not save");

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn a_failed_batch_leaves_no_trace() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");

    for (id, actions) in [
        (
            2,
            json!([
                {"action": "split", "track": 0, "at": 1.0},
                {"action": "split", "track": 0, "at": 99.0}
            ]),
        ),
        (3, json!([])),
        (
            4,
            json!([
                {"action": "undo"},
                {"action": "split", "track": 0, "at": 1.0}
            ]),
        ),
    ] {
        let failed = call(
            &mut serve,
            id,
            "action.apply",
            Some(json!({"base_revision": revision, "actions": actions})),
        );
        assert_eq!(failed["error"]["code"], -32602, "{failed}");
    }

    let show = call(&mut serve, 5, "project.show", None);
    assert_eq!(show["result"]["revision"], revision);
    assert_eq!(
        active(&show["result"])["timeline"]["tracks"][0]["segments"]
            .as_array()
            .unwrap()
            .len(),
        3,
        "the failed first action was rolled back"
    );

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[cfg(unix)]
#[test]
fn socket_is_the_default_and_is_single_writer() {
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    struct ChildGuard(Option<Child>);
    impl ChildGuard {
        fn new(child: Child) -> Self {
            Self(Some(child))
        }
        fn kill(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            self.kill();
        }
    }

    let sandbox = Sandbox::new();
    sandbox.base();
    let socket = nle_core::state::session_socket_path(&sandbox.file(P));
    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_nile"))
            .current_dir(sandbox.path())
            .args(["--project", P, "serve"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn nile serve")
    };
    let connect = |deadline: Instant| loop {
        match UnixStream::connect(&socket) {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("connect {}: {e}", socket.display()),
        }
    };

    let mut first = ChildGuard::new(spawn());
    let stream = connect(Instant::now() + Duration::from_secs(5));
    let write = stream.try_clone().expect("clone");
    let mut a = nle_protocol::Client::new(BufReader::new(stream), write);
    let hello = a.hello().expect("hello");
    assert_eq!(hello.protocol, nle_protocol::PROTOCOL_VERSION);

    // A second connection sees the same handler and history: one shared
    // session, not a per-connection clone.
    let applied = a
        .apply(
            hello.revision,
            vec![nle_core::Action::Split { track: 0, at: 1.0 }],
            None,
        )
        .expect("split");
    let after = applied.revision;
    assert_eq!(after, hello.revision + 1);

    let stream = connect(Instant::now() + Duration::from_secs(5));
    let write = stream.try_clone().expect("clone");
    let mut b = nle_protocol::Client::new(BufReader::new(stream), write);
    assert_eq!(b.show().expect("show").revision, after);
    let applied = b
        .apply(
            after,
            vec![nle_core::Action::Split { track: 0, at: 0.5 }],
            None,
        )
        .expect("split from the second connection");
    assert_eq!(applied.revision, after + 1);
    assert_eq!(a.show().expect("show").revision, after + 1);

    // A second server on the same project refuses before binding.
    let second = spawn();
    let output = second.wait_with_output().expect("second server");
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already serving"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Killing the owner leaves its socket/lock behind; the next server
    // recognizes the dead owner, clears both and binds.
    drop(a);
    drop(b);
    first.kill();
    assert!(socket.exists(), "a killed server leaves a stale socket");

    let mut third = ChildGuard::new(spawn());
    let _stream = connect(Instant::now() + Duration::from_secs(5));
    third.kill();
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(nle_core::state::session_lock_path(&sandbox.file(P)));
}

/// The stdio transport has no `set_read_timeout`, but it must enforce the same
/// partial-frame policy as a socket: bytes with no newline end the process,
/// not an idle wait. The outer bound is generous because the real deadline is 5 s.
#[test]
fn stdio_partial_frame_cannot_wedge() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let sandbox = Sandbox::new();
    sandbox.ok(&["init"]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_nile"))
        .current_dir(sandbox.path())
        .args(["--project", P, "serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nile serve --stdio");
    let mut stdin = child.stdin.take().expect("piped stdin");
    // Bytes, no newline. Keep the pipe open so only the deadline can end it.
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1")
        .expect("write a partial frame");
    stdin.flush().expect("flush");

    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    drop(stdin);
    let status = status.expect("serve must not wedge on a partial frame");
    assert!(
        status.success(),
        "a partial frame ends the stdio server cleanly: {status:?}"
    );
}

#[test]
fn playhead_set_moves_the_shared_cursor() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");
    let reply = call(&mut serve, 2, "playhead.set", Some(json!({"at": "3/2"})));
    assert_eq!(reply["result"]["at"], "3/2");
    assert_eq!(reply["result"]["revision"], revision);
    assert_eq!(reply["result"]["depth"], 0);
    let event = serve.read();
    assert_eq!(event["method"], "playhead");
    assert_eq!(event["params"]["at"], "3/2");
    let show = call(&mut serve, 3, "project.show", None);
    assert_eq!(active(&show["result"])["playhead"], "3/2");

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn edl_track_is_zero_based() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();

    let ok = call(&mut serve, 1, "timeline.edl", Some(json!({"track": 0})));
    let edl = ok["result"].as_str().expect("track EDL");
    assert!(edl.starts_with("edl://"), "{edl}");

    let bad = call(&mut serve, 2, "timeline.edl", Some(json!({"track": 99})));
    assert_eq!(bad["error"]["code"], -32602);

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn action_apply_accepts_undo_as_an_alias() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");
    let applied = call(
        &mut serve,
        2,
        "action.apply",
        Some(json!({
            "base_revision": revision,
            "actions": [{"action": "split", "track": 0, "at": 1.0}],
            "from": "1/2"
        })),
    );
    let after = applied["result"]["revision"].as_u64().expect("revision");
    let _ = serve.read(); // project.changed

    // `from` is ignored for undo: the history restores the split's anchor.
    let undone = call(
        &mut serve,
        3,
        "action.apply",
        Some(json!({
            "base_revision": after,
            "actions": [{"action": "undo"}],
            "from": "3/1"
        })),
    );
    assert_eq!(undone["result"]["revision"], after + 1);
    assert_eq!(undone["result"]["playhead"], "1/2");
    let event = serve.read();
    assert_eq!(event["method"], "project.changed");
    let event = serve.read();
    assert_eq!(event["method"], "playhead");
    assert_eq!(event["params"]["at"], "1/2");

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn yank_and_paste_share_the_register_across_the_server() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");

    let yanked = call(
        &mut serve,
        2,
        "action.apply",
        Some(json!({
            "base_revision": revision,
            "actions": [{"action": "yank", "track": 0, "target": {"clip": 1}}]
        })),
    );
    assert_eq!(
        yanked["result"]["revision"], revision,
        "a yank is not a model change"
    );
    assert_eq!(yanked["result"]["changed"], false);

    let pasted = call(
        &mut serve,
        3,
        "action.apply",
        Some(json!({
            "base_revision": revision,
            "actions": [{"action": "paste", "track": 0, "at": 6.0, "ripple": false}]
        })),
    );
    assert_eq!(pasted["result"]["revision"], revision + 1);
    assert_eq!(pasted["result"]["changed"], true);
    let event = serve.read();
    assert_eq!(event["method"], "project.changed");

    let show = call(&mut serve, 4, "project.show", None);
    assert_eq!(
        active(&show["result"])["timeline"]["tracks"][0]["segments"]
            .as_array()
            .expect("segments")
            .len(),
        4
    );

    assert!(serve.finish().success(), "serve exits cleanly");

    // The server saved the register; the CLI can paste another copy.
    sandbox.ok(&["paste", "--at", "8"]);
    let project: Value =
        serde_json::from_str(&sandbox.ok(&["show", "--json"])).expect("project JSON");
    assert_eq!(
        active(&project)["timeline"]["tracks"][0]["segments"]
            .as_array()
            .expect("segments")
            .len(),
        5
    );
}

#[test]
fn nothing_to_undo_is_an_invalid_request_and_serving_continues() {
    let sandbox = Sandbox::new();
    sandbox.ok(&["init"]);
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");
    let undone = call(
        &mut serve,
        2,
        "undo",
        Some(json!({"base_revision": revision})),
    );
    assert_eq!(undone["error"]["code"], -32600);

    let after = call(&mut serve, 3, "hello", None);
    assert_eq!(after["result"]["revision"], revision);

    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn serve_reanchors_the_playhead_like_the_tui() {
    let sandbox = Sandbox::new();
    sandbox.hostile_base();
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");
    let show = call(&mut serve, 2, "project.show", None);
    assert_eq!(active(&show["result"])["playhead"], "3/2");

    // The clip's out-edge moves before the playhead; the shared media anchor
    // resolves to the new out-edge (1 s), just as the TUI would.
    let trimmed = call(
        &mut serve,
        3,
        "action.apply",
        Some(json!({
            "base_revision": revision,
            "actions": [{"action": "trim_out", "track": 0, "at": 1.0, "ripple": false}],
            "from": "3/2"
        })),
    );
    assert_eq!(trimmed["result"]["changed"], true);
    assert_eq!(trimmed["result"]["playhead"], "1/1");
    let event = serve.read();
    assert_eq!(event["method"], "project.changed");
    let event = serve.read();
    assert_eq!(event["method"], "playhead");
    assert_eq!(event["params"]["at"], "1/1");

    let show = call(&mut serve, 4, "project.show", None);
    assert_eq!(active(&show["result"])["playhead"], "1/1");
    assert!(serve.finish().success(), "serve exits cleanly");
}

#[test]
fn annotation_list_and_add_over_the_protocol() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");
    let empty = call(&mut serve, 2, "annotation.list", None);
    assert_eq!(empty["result"]["sequence"], 1);
    assert!(
        empty["result"]["annotations"]
            .as_array()
            .expect("annotations")
            .is_empty()
    );

    let applied = call(
        &mut serve,
        3,
        "action.apply",
        Some(json!({
            "base_revision": revision,
            "actions": [{
                "action": "annotation_add",
                "anchor": {
                    "on": "source",
                    "source": {"of": "asset", "id": 1},
                    "time": "1/2"
                },
                "kind": {"kind": "note", "text": "check the cut"}
            }]
        })),
    );
    assert_eq!(applied["result"]["changed"], true);
    let event = serve.read();
    assert_eq!(event["method"], "project.changed");
    assert_eq!(event["params"]["revision"], revision + 1);

    let listed = call(
        &mut serve,
        4,
        "annotation.list",
        Some(json!({"sequence": 1})),
    );
    let annotations = listed["result"]["annotations"]
        .as_array()
        .expect("annotations");
    assert_eq!(annotations.len(), 1);
    assert_eq!(annotations[0]["kind"]["text"], "check the cut");
    assert_eq!(annotations[0]["anchor"]["source"]["id"], 1);
    assert_eq!(annotations[0]["status"], "open");

    let missing = call(
        &mut serve,
        5,
        "annotation.list",
        Some(json!({"sequence": 99})),
    );
    assert_eq!(missing["error"]["code"], -32602);

    let status = serve.finish();
    assert!(status.success(), "nile serve exited: {status:?}");
    assert!(sandbox.ok(&["annotation", "ls"]).contains("check the cut"));
}

#[test]
fn analysis_add_stores_a_transcript_and_words_complete_reads_it() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();
    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");

    let hash = serve
        .client()
        .add_transcript(
            1,
            vec![Word {
                text: "hello".to_string(),
                start: Seconds::from_integer(0),
                end: Seconds::from_integer(1),
            }],
        )
        .expect("analysis.add");
    assert!(hash.hash.starts_with("t1-"), "{hash:?}");

    // Analysis state is not a project write: the revision does not move.
    let after = call(&mut serve, 2, "hello", None);
    assert_eq!(after["result"]["revision"], revision);

    let applied = call(
        &mut serve,
        3,
        "action.apply",
        Some(json!({
            "base_revision": revision,
            "actions": [{
                "action": "annotation_add_at",
                "track": 0,
                "at": 0.0,
                "to": 2.0,
                "kind": {
                    "kind": "expectation",
                    "expect": {
                        "check": "words_complete",
                        "min_coverage": 1.0,
                        "transcript": hash.hash
                    }
                }
            }]
        })),
    );
    assert_eq!(applied["result"]["depth"], 1);
    let revision = applied["result"]["revision"].as_u64().expect("revision");
    let event = serve.read();
    assert_eq!(event["method"], "project.changed");

    // Export after the expectation so the report's revision matches.
    sandbox.export("words.mp4");
    let out = sandbox.file("words.mp4").display().to_string();
    let checked = call(
        &mut serve,
        4,
        "annotation.check",
        Some(json!({"report": out})),
    );
    assert_eq!(
        checked["result"]["checks"][0]["check"], "words_complete",
        "{checked}"
    );
    assert_eq!(
        checked["result"]["checks"][0]["result"], "pass",
        "{checked}"
    );
    assert_eq!(checked["result"]["summary"]["pass"], 1);

    // A missing asset is invalid params, not a store failure.
    let bad = call(
        &mut serve,
        5,
        "analysis.add",
        Some(json!({"asset": 99, "words": []})),
    );
    assert_eq!(bad["error"]["code"], -32602);

    // The check is a read: the revision is still the one after the add.
    let final_hello = call(&mut serve, 6, "hello", None);
    assert_eq!(final_hello["result"]["revision"], revision);
}

#[test]
fn annotation_check_reads_a_report_without_writing() {
    let sandbox = Sandbox::new();
    sandbox.base();
    // Export before serving: the report is an artifact of a render, and the
    // read must work on whatever is on disk.
    sandbox.export("out.mp4");
    let out = sandbox.file("out.mp4");
    let out_text = out.display().to_string();
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    let revision = hello["result"]["revision"].as_u64().expect("revision");

    // One expectation covering the first clip (red, asset #1) at 0–2 s.
    let applied = call(
        &mut serve,
        2,
        "action.apply",
        Some(json!({
            "base_revision": revision,
            "actions": [{
                "action": "annotation_add_at",
                "track": 0,
                "at": 0.0,
                "to": 2.0,
                "kind": {
                    "kind": "expectation",
                    "expect": {"check": "cam", "asset": 1}
                }
            }]
        })),
    );
    assert_eq!(applied["result"]["depth"], 1);
    let after = applied["result"]["revision"].as_u64().expect("revision");
    assert_eq!(after, revision + 1);
    let event = serve.read();
    assert_eq!(event["method"], "project.changed");

    let listed = call(&mut serve, 3, "annotation.list", None);
    let id = listed["result"]["annotations"][0]["id"]
        .as_u64()
        .expect("annotation id");

    // A relative path is refused even when it names the file beside the
    // server's cwd: the server cannot know the client's cwd.
    let relative = call(
        &mut serve,
        4,
        "annotation.check",
        Some(json!({"report": "out.mp4"})),
    );
    assert_eq!(relative["error"]["code"], -32602);
    assert!(
        relative["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("must be absolute"),
        "{relative}"
    );

    // Absolute report named as the exported video; sequence and id omitted.
    let checked = call(
        &mut serve,
        5,
        "annotation.check",
        Some(json!({"report": out_text})),
    );
    assert_eq!(checked["result"]["report"]["sequence"], 1);
    assert!(
        checked["result"]["report"]["frame_count"]
            .as_i64()
            .is_some_and(|frames| frames > 0),
        "{checked}"
    );
    assert_eq!(checked["result"]["checks"][0]["id"], id);
    assert_eq!(checked["result"]["checks"][0]["check"], "cam");
    assert_eq!(checked["result"]["checks"][0]["status"], "open");
    assert_eq!(checked["result"]["checks"][0]["result"], "pass");
    assert_eq!(checked["result"]["checks"][0]["frames"], json!([0, 60]));
    assert!(
        checked["result"]["checks"][0].get("detail").is_none(),
        "a clean pass has no caveat: {checked}"
    );
    assert_eq!(
        checked["result"]["summary"],
        json!({"pass": 1, "fail": 0, "unverifiable": 0})
    );

    // The sidecar path is the other accepted spelling.
    let from_sidecar = call(
        &mut serve,
        6,
        "annotation.check",
        Some(json!({"report": format!("{out_text}.report.json"), "sequence": 1})),
    );
    assert_eq!(from_sidecar["result"], checked["result"]);

    // An id filter selects one annotation; a stale id selects nothing.
    let one = call(
        &mut serve,
        7,
        "annotation.check",
        Some(json!({"report": out_text, "id": [id]})),
    );
    assert_eq!(one["result"]["checks"].as_array().expect("checks").len(), 1);
    let none = call(
        &mut serve,
        8,
        "annotation.check",
        Some(json!({"report": out_text, "id": [99]})),
    );
    assert!(
        none["result"]["checks"]
            .as_array()
            .expect("checks")
            .is_empty()
    );
    assert_eq!(none["result"]["summary"]["pass"], 0);

    // The typed helper decodes the same reply.
    let typed = {
        let mut client = serve.client();
        client
            .checks(out.clone(), Some(1), Vec::new())
            .expect("typed checks")
    };
    assert_eq!(
        serde_json::to_value(&typed).expect("check report json"),
        checked["result"]
    );

    // A read leaves the model alone: no revision bump, no undo entry and no
    // event. Reading hello here also proves no event was queued — it would
    // have arrived before this response.
    let after_read = call(&mut serve, 9, "hello", None);
    assert_eq!(after_read["result"]["revision"], after);
    assert_eq!(after_read["result"]["depth"], 1);

    // A report of another sequence is refused, not answered with a fake pass.
    let mismatch = call(
        &mut serve,
        10,
        "annotation.check",
        Some(json!({"report": out_text, "sequence": 2})),
    );
    assert_eq!(mismatch["error"]["code"], -32602);
    let message = mismatch["error"]["message"]
        .as_str()
        .expect("error message");
    assert!(message.contains("#2"), "{message}");
    assert!(message.contains("#1"), "{message}");

    // A missing report is the request's fault, not a server failure.
    let missing = call(
        &mut serve,
        11,
        "annotation.check",
        Some(json!({"report": sandbox.file("missing.mp4").display().to_string()})),
    );
    assert_eq!(missing["error"]["code"], -32602);
    assert!(
        missing["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("does not exist"),
        "{missing}"
    );

    // Refusals are reads too: serving continues, and the next call answers.
    let show = call(&mut serve, 12, "project.show", None);
    assert_eq!(show["result"]["revision"], after);

    let status = serve.finish();
    assert!(status.success(), "nile serve exited: {status:?}");

    // From a cwd that does not hold the artifact, a relative path is still
    // refused and the absolute one still resolves to the same file.
    std::fs::create_dir_all(sandbox.file("elsewhere")).expect("mkdir");
    let mut elsewhere = sandbox.serve_in(&sandbox.file("elsewhere"));
    let relative = call(
        &mut elsewhere,
        1,
        "annotation.check",
        Some(json!({"report": "out.mp4"})),
    );
    assert_eq!(relative["error"]["code"], -32602);
    assert!(
        relative["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("must be absolute"),
        "{relative}"
    );
    let absolute = call(
        &mut elsewhere,
        2,
        "annotation.check",
        Some(json!({"report": out_text})),
    );
    assert_eq!(absolute["result"]["checks"][0]["result"], "pass");
    assert_eq!(absolute["result"], checked["result"]);
    let status = elsewhere.finish();
    assert!(status.success(), "nile serve exited: {status:?}");
}
