//! Golden wire forms and an in-memory client/server round trip. These are
//! integration tests on purpose: they can only see the public API, so a
//! broken envelope or a typed call that cannot survive encoding fails here.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use nle_core::analysis::Word;
use nle_core::expect::{CheckOutcome, CheckReport, CheckSummary, ReportRef};
use nle_core::jobs::{ExportPreset, JobExport, JobProgress, JobRecord, JobState};
use nle_core::{
    Action, AnnotationStatus, AudioSettings, CheckResult, Project, ProjectDiff, Seconds,
    TransitionKind, TransitionLen,
};
use nle_protocol::{
    AnalysisHash, AnnotationList, Call, Client, ClientError, Event, Handler, INTERNAL_ERROR,
    INVALID_PARAMS, INVALID_REQUEST, Id, JobList, METHOD_NOT_FOUND, MediaAsset, MediaList,
    MediaUsage, Notification, Outcome, PARSE_ERROR, PROTOCOL_VERSION, PlayheadReply, Preview,
    REVISION_CONFLICT, Reply, Request, Response, RevisionConflict, RpcError, SequenceList,
    SequenceReply, SequenceSummary, Server,
};
use serde_json::{Value, json};

struct Fake {
    revision: u64,
    events: Vec<Event>,
    last_from: Option<Seconds>,
}

impl Fake {
    fn new(revision: u64) -> Self {
        Self {
            revision,
            events: Vec::new(),
            last_from: None,
        }
    }

    fn check(&self, base_revision: u64) -> Result<(), RpcError> {
        if base_revision == self.revision {
            Ok(())
        } else {
            Err(RpcError::revision_conflict(base_revision, self.revision))
        }
    }
}

impl Handler for Fake {
    fn handle(&mut self, call: &Call) -> Result<Reply, RpcError> {
        match call {
            Call::ProjectShow => {
                let mut project = Project::new();
                project.revision = self.revision;
                Ok(Reply::Project(project))
            }
            Call::Hello => Ok(Reply::Hello(nle_protocol::Hello::new(self.revision, 0, 1))),
            Call::TimelineEdl { track } => Ok(Reply::Edl(format!("TITLE: fake {track:?}"))),
            Call::ActionPreview {
                base_revision,
                actions,
                from,
            } => {
                self.check(*base_revision)?;
                self.last_from = *from;
                Ok(Reply::Preview(Preview {
                    outcome: Outcome {
                        revision: self.revision,
                        depth: 0,
                        desc: format!("preview {}", actions.first().map_or(0, Action::slot)),
                        changed: true,
                        playhead: None,
                    },
                    diff: ProjectDiff::default(),
                    warnings: Vec::new(),
                }))
            }
            Call::ActionApply {
                base_revision,
                actions,
                from,
            } => {
                self.check(*base_revision)?;
                self.last_from = *from;
                self.revision += 1;
                // The real handler computes this from nle_core::Outcome
                // frames (a blend midpoint, the trimmed edge); it must not
                // echo the request anchor.
                let playhead = Seconds::new(5, 2);
                self.events.push(Event::ProjectChanged {
                    revision: self.revision,
                });
                self.events.push(Event::Playhead { at: playhead });
                Ok(Reply::Outcome(Outcome {
                    revision: self.revision,
                    depth: 0,
                    desc: format!("applied {}", actions.first().map_or(0, Action::slot)),
                    changed: true,
                    playhead: Some(playhead),
                }))
            }
            Call::Undo { base_revision } => {
                self.check(*base_revision)?;
                self.revision = self.revision.saturating_sub(1);
                Ok(Reply::Outcome(Outcome {
                    revision: self.revision,
                    depth: 0,
                    desc: "undo".to_string(),
                    changed: true,
                    playhead: None,
                }))
            }
            Call::Redo { base_revision } => {
                self.check(*base_revision)?;
                self.revision += 1;
                Ok(Reply::Outcome(Outcome {
                    revision: self.revision,
                    depth: 0,
                    desc: "redo".to_string(),
                    changed: true,
                    playhead: None,
                }))
            }
            Call::SequenceList => Ok(Reply::Sequences(SequenceList {
                revision: self.revision,
                active: 1,
                sequences: vec![summary(1, "Sequence 1")],
            })),
            Call::SequenceUse { id } => Ok(Reply::Sequence(SequenceReply {
                revision: self.revision,
                active: *id,
                sequence: summary(*id, "Sequence 2"),
            })),
            Call::SequenceAdd {
                base_revision,
                name,
                ..
            } => {
                self.check(*base_revision)?;
                self.revision += 1;
                Ok(Reply::Sequence(SequenceReply {
                    revision: self.revision,
                    active: 1,
                    sequence: summary(2, name),
                }))
            }
            Call::SequenceAudio {
                base_revision,
                audio,
            } => {
                self.check(*base_revision)?;
                self.revision += 1;
                let mut sequence = summary(1, "Sequence 1");
                sequence.audio = *audio;
                self.events.push(Event::SequenceChanged {
                    active: 1,
                    revision: self.revision,
                });
                Ok(Reply::Sequence(SequenceReply {
                    revision: self.revision,
                    active: 1,
                    sequence,
                }))
            }
            Call::MediaList => Ok(Reply::Media(MediaList {
                revision: self.revision,
                assets: vec![media(1, "/media/red.mp4")],
            })),
            Call::MediaAdd {
                base_revision,
                paths,
            } => {
                self.check(*base_revision)?;
                self.revision += 1;
                Ok(Reply::Media(MediaList {
                    revision: self.revision,
                    assets: paths
                        .iter()
                        .enumerate()
                        .map(|(i, path)| media(i as u32 + 2, &path.display().to_string()))
                        .collect(),
                }))
            }
            Call::PlayheadSet { at } => Ok(Reply::Playhead(PlayheadReply {
                revision: self.revision,
                depth: 0,
                at: *at,
            })),
            Call::AnnotationList { sequence } => Ok(Reply::Annotations(AnnotationList {
                sequence: sequence.unwrap_or(1),
                annotations: Vec::new(),
            })),
            Call::AnnotationCheck {
                report,
                sequence,
                id,
            } => Ok(Reply::Checks(CheckReport {
                report: ReportRef {
                    output: report.display().to_string(),
                    sequence: sequence.unwrap_or(1),
                    revision: self.revision,
                    frame_count: 60,
                },
                checks: id
                    .iter()
                    .map(|id| CheckOutcome {
                        id: *id,
                        check: "cam".to_string(),
                        status: AnnotationStatus::Open,
                        result: CheckResult::Pass,
                        span: None,
                        frames: Some((0, 60)),
                        detail: None,
                    })
                    .collect(),
                summary: CheckSummary {
                    pass: id.len(),
                    ..CheckSummary::default()
                },
            })),
            Call::AnalysisAdd { asset, words } => Ok(Reply::Analysis(AnalysisHash {
                hash: format!("t1-{:016x}", u64::from(*asset) + words.len() as u64),
            })),
            Call::JobSubmit {
                base_revision,
                output,
                sequence,
                export: _,
            } => {
                self.check(*base_revision)?;
                Ok(Reply::Job(record(1, JobState::Queued, output, *sequence)))
            }
            Call::JobList => Ok(Reply::Jobs(JobList::new(
                self.revision,
                vec![record(
                    1,
                    JobState::Running,
                    &PathBuf::from("/media/job.mp4"),
                    1,
                )],
            ))),
            Call::JobGet { id } => Ok(Reply::Job(record(
                *id,
                JobState::Queued,
                &PathBuf::from("/media/job.mp4"),
                1,
            ))),
            Call::JobCancel { id } => {
                self.events.push(Event::JobChanged {
                    id: *id,
                    state: JobState::Cancelled,
                    report: None,
                });
                Ok(Reply::Job(record(
                    *id,
                    JobState::Cancelled,
                    &PathBuf::from("/media/job.mp4"),
                    1,
                )))
            }
            Call::JobRemove { id } => Ok(Reply::Job(record(
                *id,
                JobState::Done,
                &PathBuf::from("/media/job.mp4"),
                1,
            ))),
        }
    }

    fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
}

/// Send `call` to a server, then feed its output to a client. Returns the
/// client's reply plus the raw server output (response + any events), so a
/// test can assert ordering or replay the events.
fn exchange(handler: &mut impl Handler, call: Call) -> (Result<Reply, ClientError>, Vec<u8>) {
    let request = Request::new(
        Id::from(1u64),
        call.method(),
        call.params().expect("encode params"),
    );
    let line = serde_json::to_string(&request).expect("encode request");
    let mut server = Server::new(Cursor::new(format!("{line}\n")), Vec::new(), handler);
    server.serve().expect("serve");
    let (_, output, _) = server.into_inner();
    let mut client = Client::new(Cursor::new(output.clone()), Vec::new());
    (client.call(call), output)
}

fn responses(input: &str, handler: &mut impl Handler) -> Vec<Response> {
    let mut server = Server::new(Cursor::new(input.to_string()), Vec::new(), handler);
    server.serve().expect("serve");
    let (_, output, _) = server.into_inner();
    String::from_utf8(output)
        .expect("utf-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("a response envelope"))
        .collect()
}

fn media(id: u32, path: &str) -> MediaAsset {
    MediaAsset {
        id,
        path: PathBuf::from(path),
        duration: Some(Seconds::from_integer(6)),
        width: Some(320),
        height: Some(240),
        fps: Some(Seconds::from_integer(30)),
        audio: true,
        used_by: vec![MediaUsage {
            sequence: 1,
            segment: 1,
        }],
    }
}

fn record(id: u64, state: JobState, output: &Path, sequence: u32) -> JobRecord {
    JobRecord {
        version: nle_core::jobs::JOB_VERSION,
        id,
        state,
        revision: 7,
        sequence,
        output: output.to_path_buf(),
        export: JobExport::default(),
        created_at: 1,
        started_at: None,
        finished_at: None,
        progress: JobProgress {
            done: 10,
            total: 100,
        },
        report: None,
        error: None,
        snapshot: PathBuf::from("project.json"),
    }
}

fn summary(id: u32, name: &str) -> SequenceSummary {
    SequenceSummary {
        id,
        name: name.to_string(),
        fps: Some(Seconds::from_integer(30)),
        video: None,
        audio: AudioSettings::default(),
        playhead: Seconds::new(3, 2),
        clips: 2,
    }
}

fn all_calls() -> Vec<Call> {
    let action = Action::Undo;
    vec![
        Call::ProjectShow,
        Call::Hello,
        Call::TimelineEdl { track: None },
        Call::ActionPreview {
            base_revision: 0,
            actions: vec![action.clone()],
            from: None,
        },
        Call::ActionApply {
            base_revision: 0,
            actions: vec![action],
            from: None,
        },
        Call::Undo { base_revision: 0 },
        Call::Redo { base_revision: 0 },
        Call::SequenceList,
        Call::SequenceUse { id: 2 },
        Call::SequenceAdd {
            base_revision: 0,
            name: "Vertical".into(),
            fps: Some(Seconds::from_integer(30)),
            video: None,
        },
        Call::SequenceAudio {
            base_revision: 0,
            audio: AudioSettings {
                normalize: true,
                fade_in: Seconds::new(1, 2),
                fade_out: Seconds::from_integer(1),
            },
        },
        Call::MediaList,
        Call::MediaAdd {
            base_revision: 0,
            paths: vec![PathBuf::from("/media/red.mp4")],
        },
        Call::PlayheadSet {
            at: Seconds::new(3, 2),
        },
        Call::AnnotationList { sequence: Some(1) },
        Call::AnnotationCheck {
            report: PathBuf::from("/media/out.mp4"),
            sequence: Some(1),
            id: vec![2],
        },
        Call::AnalysisAdd {
            asset: 1,
            words: vec![Word {
                text: "take".to_string(),
                start: Seconds::from_integer(0),
                end: Seconds::from_integer(1),
            }],
        },
        Call::JobSubmit {
            base_revision: 0,
            output: PathBuf::from("/media/out.mp4"),
            sequence: 1,
            export: JobExport {
                preset: ExportPreset::Max,
                crf: Some(20),
                encoder_preset: None,
            },
        },
        Call::JobList,
        Call::JobGet { id: 3 },
        Call::JobCancel { id: 4 },
        Call::JobRemove { id: 5 },
    ]
}

#[test]
fn project_show_round_trips_through_the_typed_call() {
    let (reply, _) = exchange(&mut Fake::new(7), Call::ProjectShow);
    match reply.expect("show") {
        Reply::Project(project) => assert_eq!(project.revision, 7),
        other => panic!("expected project, got {other:?}"),
    }
}

#[test]
fn hello_advertises_the_protocol_and_methods() {
    let (reply, _) = exchange(&mut Fake::new(7), Call::Hello);
    let Reply::Hello(hello) = reply.expect("hello") else {
        panic!("expected hello");
    };
    assert_eq!(hello.protocol, PROTOCOL_VERSION);
    assert_eq!(
        hello.project_version,
        nle_core::project::PROJECT_VERSION,
        "hello reports the nile.json version this build reads"
    );
    assert_eq!(hello.revision, 7);
    assert_eq!(hello.depth, 0);
    assert_eq!(hello.active, 1);
    assert_eq!(
        hello.methods,
        nle_protocol::METHODS
            .iter()
            .map(|m| (*m).to_string())
            .collect::<Vec<_>>()
    );
}

#[test]
fn apply_moves_the_revision_and_preview_does_not() {
    let mut fake = Fake::new(7);
    let (preview, _) = exchange(
        &mut fake,
        Call::ActionPreview {
            base_revision: 7,
            actions: vec![Action::Split { track: 0, at: 1.5 }],
            from: None,
        },
    );
    assert_eq!(
        preview.expect("preview"),
        Reply::Preview(Preview {
            outcome: Outcome {
                revision: 7,
                depth: 0,
                desc: "preview 2".into(),
                changed: true,
                playhead: None,
            },
            diff: ProjectDiff::default(),
            warnings: Vec::new(),
        })
    );

    let (applied, _) = exchange(
        &mut fake,
        Call::ActionApply {
            base_revision: 7,
            actions: vec![Action::Split { track: 0, at: 1.5 }],
            from: None,
        },
    );
    assert_eq!(
        applied.expect("apply"),
        Reply::Outcome(Outcome {
            revision: 8,
            depth: 0,
            desc: "applied 2".into(),
            changed: true,
            playhead: Some(Seconds::new(5, 2)),
        })
    );
}

#[test]
fn the_anchor_is_an_input_and_the_playhead_is_the_result() {
    let mut fake = Fake::new(7);
    let (reply, _) = exchange(
        &mut fake,
        Call::ActionApply {
            base_revision: 7,
            actions: vec![Action::Split { track: 0, at: 1.5 }],
            from: Some(Seconds::new(3, 2)),
        },
    );
    assert_eq!(
        fake.last_from,
        Some(Seconds::new(3, 2)),
        "the handler sees the undo anchor"
    );
    match reply.expect("apply") {
        Reply::Outcome(outcome) => {
            assert_eq!(
                outcome.playhead,
                Some(Seconds::new(5, 2)),
                "the outcome is the edit's playhead, not the anchor"
            );
        }
        other => panic!("expected an outcome, got {other:?}"),
    }
}

#[test]
fn a_stale_base_revision_is_a_typed_conflict() {
    let (reply, _) = exchange(
        &mut Fake::new(7),
        Call::ActionApply {
            base_revision: 3,
            actions: vec![Action::Split { track: 0, at: 1.5 }],
            from: None,
        },
    );
    let error = reply.expect_err("stale base");
    let ClientError::Server(error) = error else {
        panic!("expected a server error, got {error:?}");
    };
    assert_eq!(error.code, REVISION_CONFLICT);
    assert_eq!(
        error.revision_conflict_data(),
        Some(RevisionConflict {
            expected: 3,
            actual: 7
        })
    );
}

#[test]
fn undo_and_redo_share_the_outcome_shape() {
    let mut fake = Fake::new(1);
    let (undone, _) = exchange(&mut fake, Call::Undo { base_revision: 1 });
    assert_eq!(
        undone.expect("undo"),
        Reply::Outcome(Outcome {
            revision: 0,
            depth: 0,
            desc: "undo".into(),
            changed: true,
            playhead: None,
        })
    );
    let (redone, _) = exchange(&mut fake, Call::Redo { base_revision: 0 });
    assert_eq!(
        redone.expect("redo"),
        Reply::Outcome(Outcome {
            revision: 1,
            depth: 0,
            desc: "redo".into(),
            changed: true,
            playhead: None,
        })
    );
}

#[test]
fn golden_action_apply_request() {
    let call = Call::ActionApply {
        base_revision: 7,
        actions: vec![Action::Split { track: 0, at: 1.5 }],
        from: None,
    };
    let request = Request::new(
        Id::from(1u64),
        call.method(),
        call.params().expect("params"),
    );
    assert_eq!(
        serde_json::to_string(&request).expect("serialize"),
        r#"{"jsonrpc":"2.0","id":1,"method":"action.apply","params":{"actions":[{"action":"split","at":1.5,"track":0}],"base_revision":7}}"#
    );
}

#[test]
fn the_anchor_is_rational_on_the_wire() {
    let call = Call::ActionApply {
        base_revision: 7,
        actions: vec![Action::Split { track: 0, at: 1.5 }],
        from: Some(Seconds::new(3, 2)),
    };
    let params = call.params().expect("params");
    assert_eq!(params["from"], "3/2");
    assert_eq!(Call::decode(call.method(), params).expect("decode"), call);
}

#[test]
fn golden_sequence_calls_and_events() {
    let call = Call::SequenceAdd {
        base_revision: 7,
        name: "Vertical".into(),
        fps: Some(Seconds::from_integer(30)),
        video: None,
    };
    let request = Request::new(
        Id::from(1u64),
        call.method(),
        call.params().expect("params"),
    );
    assert_eq!(
        serde_json::to_string(&request).expect("serialize"),
        r#"{"jsonrpc":"2.0","id":1,"method":"sequence.add","params":{"base_revision":7,"fps":"30/1","name":"Vertical"}}"#
    );
    assert_eq!(
        Call::decode(call.method(), call.params().expect("params")).expect("decode"),
        call
    );

    let call = Call::SequenceAudio {
        base_revision: 7,
        audio: AudioSettings {
            normalize: true,
            fade_in: Seconds::new(1, 2),
            fade_out: Seconds::from_integer(1),
        },
    };
    let request = Request::new(
        Id::from(2u64),
        call.method(),
        call.params().expect("params"),
    );
    assert_eq!(
        serde_json::to_string(&request).expect("serialize"),
        r#"{"jsonrpc":"2.0","id":2,"method":"sequence.audio","params":{"audio":{"fade_in":"1/2","fade_out":"1/1","normalize":true},"base_revision":7}}"#
    );
    assert_eq!(
        Call::decode(call.method(), call.params().expect("params")).expect("decode"),
        call
    );

    let event = Event::SequenceChanged {
        active: 2,
        revision: 4,
    };
    assert_eq!(
        serde_json::to_string(&event.notification().expect("notification")).expect("serialize"),
        r#"{"jsonrpc":"2.0","method":"sequence.changed","params":{"active":2,"revision":4}}"#
    );
}

#[test]
fn typed_sequence_helpers_match_their_calls() {
    let (_, output) = exchange(&mut Fake::new(7), Call::SequenceList);
    let mut client = Client::new(Cursor::new(output), Vec::new());
    let list = client.sequences().expect("sequence list");
    assert_eq!(list.active, 1);
    assert_eq!(list.sequences.len(), 1);
    assert_eq!(list.sequences[0].audio, AudioSettings::default());

    let (_, output) = exchange(&mut Fake::new(7), Call::SequenceUse { id: 2 });
    let mut client = Client::new(Cursor::new(output), Vec::new());
    let used = client.use_sequence(2).expect("sequence use");
    assert_eq!(used.active, 2);
    assert_eq!(used.sequence.id, 2);

    let (_, output) = exchange(
        &mut Fake::new(7),
        Call::SequenceAdd {
            base_revision: 7,
            name: "Vertical".into(),
            fps: Some(Seconds::from_integer(30)),
            video: None,
        },
    );
    let mut client = Client::new(Cursor::new(output), Vec::new());
    let added = client
        .add_sequence(7, "Vertical", Some(Seconds::from_integer(30)), None)
        .expect("sequence add");
    assert_eq!(added.revision, 8);
    assert_eq!(added.sequence.id, 2);

    let (_, output) = exchange(&mut Fake::new(7), Call::MediaList);
    let mut client = Client::new(Cursor::new(output), Vec::new());
    let list = client.media_list().expect("media list");
    assert_eq!(list.assets[0].id, 1);

    let audio = AudioSettings {
        normalize: true,
        fade_in: Seconds::new(1, 2),
        fade_out: Seconds::from_integer(1),
    };
    let (_, output) = exchange(
        &mut Fake::new(7),
        Call::SequenceAudio {
            base_revision: 7,
            audio,
        },
    );
    let mut client = Client::new(Cursor::new(output), Vec::new());
    let post = client.set_audio(7, audio).expect("sequence audio");
    assert_eq!(post.revision, 8);
    assert_eq!(post.sequence.audio, audio);
    assert_eq!(post.sequence.audio.fade_in, Seconds::new(1, 2));

    let (_, output) = exchange(
        &mut Fake::new(7),
        Call::MediaAdd {
            base_revision: 7,
            paths: vec![PathBuf::from("/media/extra.mp4")],
        },
    );
    let mut client = Client::new(Cursor::new(output), Vec::new());
    let added = client
        .add_media(7, vec![PathBuf::from("/media/extra.mp4")])
        .expect("media add");
    assert_eq!(added.revision, 8);
    assert_eq!(added.assets[0].id, 2);

    let summary = SequenceSummary {
        video: None,
        fps: None,
        ..summary(1, "Sequence 1")
    };
    let value = serde_json::to_value(summary).expect("summary");
    assert!(value.get("video").is_none(), "{value}");
    assert!(value.get("fps").is_none(), "{value}");
}

#[test]
fn golden_media_add_request() {
    let call = Call::MediaAdd {
        base_revision: 7,
        paths: vec![PathBuf::from("/media/red.mp4")],
    };
    let request = Request::new(
        Id::from(1u64),
        call.method(),
        call.params().expect("params"),
    );
    assert_eq!(
        serde_json::to_string(&request).expect("serialize"),
        r#"{"jsonrpc":"2.0","id":1,"method":"media.add","params":{"base_revision":7,"paths":["/media/red.mp4"]}}"#
    );
}

#[test]
fn golden_job_submit_request_and_round_trip() {
    let call = Call::JobSubmit {
        base_revision: 7,
        output: PathBuf::from("/media/out.mp4"),
        sequence: 2,
        export: JobExport {
            preset: ExportPreset::Max,
            crf: Some(20),
            encoder_preset: None,
        },
    };
    let request = Request::new(
        Id::from(1u64),
        call.method(),
        call.params().expect("params"),
    );
    assert_eq!(
        serde_json::to_string(&request).expect("serialize"),
        r#"{"jsonrpc":"2.0","id":1,"method":"job.submit","params":{"base_revision":7,"export":{"crf":20,"preset":"max"},"output":"/media/out.mp4","sequence":2}}"#
    );

    let (reply, _) = exchange(&mut Fake::new(7), call.clone());
    let Reply::Job(job) = reply.expect("submit") else {
        panic!("expected a job");
    };
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.sequence, 2);
    assert_eq!(job.output, PathBuf::from("/media/out.mp4"));

    let (reply, _) = exchange(&mut Fake::new(7), Call::JobList);
    let Reply::Jobs(list) = reply.expect("list") else {
        panic!("expected jobs");
    };
    assert_eq!(list.jobs.len(), 1);

    // Cancel is a job reply plus its terminal event, in that order.
    let (reply, output) = exchange(&mut Fake::new(7), Call::JobCancel { id: 4 });
    assert!(matches!(reply, Ok(Reply::Job(ref job)) if job.state == JobState::Cancelled));
    let lines: Vec<&str> = std::str::from_utf8(&output)
        .expect("utf-8")
        .lines()
        .collect();
    assert_eq!(lines.len(), 2, "one response plus one event: {lines:?}");
    let event: Value = serde_json::from_str(lines[1]).expect("event envelope");
    assert_eq!(
        event,
        json!({
            "jsonrpc": "2.0",
            "method": "job.changed",
            "params": {"id": 4, "state": "cancelled"}
        })
    );
    // Retention: removing a terminal job is an id-only call returning the
    // removed record, not a unit reply.
    let remove = Call::JobRemove { id: 5 };
    let request = Request::new(
        Id::from(1u64),
        remove.method(),
        remove.params().expect("params"),
    );
    assert_eq!(
        serde_json::to_string(&request).expect("serialize"),
        r#"{"jsonrpc":"2.0","id":1,"method":"job.remove","params":{"id":5}}"#
    );
    let (reply, _) = exchange(&mut Fake::new(7), remove);
    assert!(matches!(reply, Ok(Reply::Job(ref job)) if job.id == 5));
}

#[test]
fn a_stale_job_submit_is_a_revision_conflict() {
    let (reply, _) = exchange(
        &mut Fake::new(7),
        Call::JobSubmit {
            base_revision: 6,
            output: PathBuf::from("/media/out.mp4"),
            sequence: 1,
            export: JobExport::default(),
        },
    );
    let error = reply.expect_err("stale");
    assert!(error.to_string().contains("revision"), "{error}");
}

#[test]
fn every_action_slot_walks_the_protocol() {
    let actions = nle_core::action::sample_actions();
    assert_eq!(actions.len(), nle_core::action::SLOTS);
    for (slot, action) in actions.into_iter().enumerate() {
        assert_eq!(action.slot(), slot, "samples follow Action::slot");
        let call = Call::ActionApply {
            base_revision: 0,
            actions: vec![action],
            from: None,
        };
        let params = call.params().expect("encode");
        assert_eq!(Call::decode(call.method(), params).expect("decode"), call);
        let (reply, _) = exchange(&mut Fake::new(0), call.clone());
        assert!(matches!(reply, Ok(Reply::Outcome(_))), "slot {slot}");
    }
}

#[test]
fn golden_response_and_error_envelopes() {
    let ok = Response::Result {
        id: Id::from(1u64),
        result: serde_json::to_value(Outcome {
            revision: 8,
            depth: 0,
            desc: "applied".into(),
            changed: true,
            playhead: Some(Seconds::new(1, 2)),
        })
        .expect("outcome"),
    };
    assert_eq!(
        serde_json::to_value(&ok).expect("serialize"),
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "changed": true,
                "depth": 0,
                "desc": "applied",
                "playhead": "1/2",
                "revision": 8
            }
        })
    );

    let err = Response::Error {
        id: None,
        error: RpcError::parse_error("expected value"),
    };
    assert_eq!(
        serde_json::to_string(&err).expect("serialize"),
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error: expected value"}}"#
    );
}

#[test]
fn golden_event_notification() {
    let event = Event::Playhead {
        at: Seconds::new(1, 2),
    };
    assert_eq!(
        serde_json::to_string(&event.notification().expect("notification")).expect("serialize"),
        r#"{"jsonrpc":"2.0","method":"playhead","params":{"at":"1/2"}}"#
    );
}

#[test]
fn events_round_trip_through_notifications() {
    let events = [
        Event::ProjectChanged { revision: 4 },
        Event::Playhead {
            at: Seconds::new(30000, 1001),
        },
        Event::SequenceChanged {
            active: 2,
            revision: 4,
        },
        Event::JobProgress {
            id: 3,
            state: JobState::Running,
            progress: JobProgress {
                done: 30,
                total: 120,
            },
        },
        Event::JobChanged {
            id: 3,
            state: JobState::Done,
            report: Some(PathBuf::from("/media/out.mp4.report.json")),
        },
    ];
    assert_eq!(nle_protocol::EVENTS.len(), events.len());
    for event in events {
        let back =
            Event::from_notification(event.notification().expect("notification")).expect("decode");
        assert_eq!(back, event);
    }

    let unknown = Notification::new("job.telemetry", json!({}));
    let error = Event::from_notification(unknown).expect_err("unknown event");
    assert_eq!(error.code, METHOD_NOT_FOUND);
}

#[test]
fn the_server_writes_the_response_before_its_events() {
    let (reply, output) = exchange(
        &mut Fake::new(7),
        Call::ActionApply {
            base_revision: 7,
            actions: vec![Action::Split { track: 0, at: 1.5 }],
            from: None,
        },
    );
    assert!(matches!(reply, Ok(Reply::Outcome(_))));

    let lines: Vec<&str> = std::str::from_utf8(&output)
        .expect("utf-8")
        .lines()
        .collect();
    assert_eq!(lines.len(), 3, "one response plus two events");
    let first: Value = serde_json::from_str(lines[0]).expect("response");
    assert!(first.get("result").is_some(), "the result comes first");
    for line in &lines[1..] {
        let event: Value = serde_json::from_str(line).expect("event");
        assert!(event.get("method").is_some(), "then notifications");
    }

    let mut client = Client::new(Cursor::new(output), Vec::new());
    client
        .call(Call::ActionApply {
            base_revision: 7,
            actions: vec![Action::Split { track: 0, at: 1.5 }],
            from: None,
        })
        .expect("apply");
    assert_eq!(
        client.next_event().expect("event"),
        Some(Event::ProjectChanged { revision: 8 })
    );
    assert_eq!(
        client.next_event().expect("event"),
        Some(Event::Playhead {
            at: Seconds::new(5, 2)
        })
    );
}

#[test]
fn events_before_a_response_are_buffered() {
    let event = Event::Playhead {
        at: Seconds::new(1, 2),
    };
    let event_line =
        serde_json::to_string(&event.notification().expect("notification")).expect("event line");
    let response = r#"{"jsonrpc":"2.0","id":1,"result":"TITLE: t"}"#;
    let input = format!("{event_line}\n{response}\n");

    let mut client = Client::new(Cursor::new(input), Vec::new());
    assert_eq!(client.edl(None).expect("edl"), "TITLE: t");
    assert_eq!(client.next_event().expect("event"), Some(event));
}

#[test]
fn an_unknown_event_is_skipped_and_the_call_succeeds() {
    let bad = r#"{"jsonrpc":"2.0","method":"job.telemetry","params":{}}"#;
    let response = r#"{"jsonrpc":"2.0","id":1,"result":"TITLE: t"}"#;
    let input = format!("{bad}\n{response}\n");
    let mut client = Client::new(Cursor::new(input), Vec::new());
    assert_eq!(client.edl(None).expect("edl"), "TITLE: t");
    assert_eq!(
        client.skipped(),
        1,
        "the unknown notification is counted, not fatal"
    );
}

#[test]
fn next_event_skips_undecodable_notifications() {
    let bad = r#"{"jsonrpc":"2.0","method":"job.telemetry","params":{}}"#;
    let good = Event::Playhead {
        at: Seconds::new(1, 2),
    };
    let good_line =
        serde_json::to_string(&good.notification().expect("notification")).expect("event line");
    let input = format!("{bad}\n{good_line}\n");
    let mut client = Client::new(Cursor::new(input), Vec::new());
    assert_eq!(client.next_event().expect("event"), Some(good));
    assert_eq!(client.skipped(), 1);
}

#[test]
fn a_null_id_error_is_reported_as_the_server_error() {
    let line =
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error: bad"}}"#;
    let mut client = Client::new(Cursor::new(format!("{line}\n")), Vec::new());
    match client.call(Call::ProjectShow).expect_err("null-id error") {
        ClientError::Server(error) => assert_eq!(error.code, PARSE_ERROR),
        other => panic!("expected the server error, got {other:?}"),
    }
}

#[test]
fn a_client_notification_is_ignored() {
    let mut fake = Fake::new(0);
    let input = concat!(
        r#"{"jsonrpc":"2.0","method":"project.show"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"project.show"}"#,
        "\n"
    );
    let responses = responses(input, &mut fake);
    assert_eq!(responses.len(), 1, "notifications get no response");
    assert_eq!(responses[0].id(), Some(&Id::from(2u64)));
}

#[test]
fn a_null_id_notification_is_ignored() {
    let mut fake = Fake::new(0);
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":null,"method":"project.show"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"project.show"}"#,
        "\n"
    );
    let responses = responses(input, &mut fake);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].id(), Some(&Id::from(2u64)));
}

#[test]
fn a_batch_array_is_an_invalid_request() {
    let mut fake = Fake::new(0);
    let responses = responses("[]\n", &mut fake);
    match &responses[0] {
        Response::Error { id, error } => {
            assert_eq!(id, &None);
            assert_eq!(error.code, INVALID_REQUEST);
        }
        other => panic!("expected an invalid request, got {other:?}"),
    }
}

#[test]
fn a_string_id_is_echoed() {
    let mut fake = Fake::new(0);
    let responses = responses(
        r#"{"jsonrpc":"2.0","id":"abc","method":"project.show"}"#,
        &mut fake,
    );
    assert_eq!(responses[0].id(), Some(&Id::String("abc".into())));
}

#[test]
fn a_malformed_line_is_a_parse_error_and_serving_continues() {
    let mut fake = Fake::new(0);
    let input = concat!(
        "not json\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"project.show"}"#,
        "\n"
    );
    let responses = responses(input, &mut fake);
    assert_eq!(responses.len(), 2);
    match &responses[0] {
        Response::Error { id: None, error } => assert_eq!(error.code, PARSE_ERROR),
        other => panic!("expected a null-id parse error, got {other:?}"),
    }
    match &responses[1] {
        Response::Result { id, .. } => assert_eq!(id, &Id::from(2u64)),
        other => panic!("serving should continue, got {other:?}"),
    }
}

#[test]
fn invalid_utf8_is_a_parse_error_and_serving_continues() {
    let mut fake = Fake::new(0);
    let mut input = Vec::new();
    input.extend_from_slice(&[0xff, b'\n']);
    input.extend_from_slice(br#"{"jsonrpc":"2.0","id":9,"method":"project.show"}"#);
    input.push(b'\n');
    let mut server = Server::new(Cursor::new(input), Vec::new(), &mut fake);
    server.serve().expect("serve");
    let (_, output, _) = server.into_inner();
    let lines: Vec<&str> = std::str::from_utf8(&output)
        .expect("utf-8")
        .lines()
        .collect();
    assert_eq!(lines.len(), 2, "parse error, then the valid result");
    let first: Response = serde_json::from_str(lines[0]).expect("parse error");
    match first {
        Response::Error { id: None, error } => assert_eq!(error.code, PARSE_ERROR),
        other => panic!("expected a null-id parse error, got {other:?}"),
    }
    let second: Response = serde_json::from_str(lines[1]).expect("result");
    assert_eq!(second.id(), Some(&Id::from(9u64)));
}

#[test]
fn a_response_must_be_exactly_one_of_result_or_error() {
    assert!(
        serde_json::from_str::<Response>(
            r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"x"}}"#
        )
        .is_err()
    );
    assert!(serde_json::from_str::<Response>(r#"{"jsonrpc":"2.0","id":1}"#).is_err());
    assert!(serde_json::from_str::<Response>(r#"{"jsonrpc":"1.0","id":1,"result":{}}"#).is_err());
}

#[test]
fn unknown_method_and_bad_params_are_distinct() {
    let mut fake = Fake::new(0);
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":2,"method":"timeline.rewind"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"action.apply","params":{"base_revision":"x","actions":[{"action":"undo"}]}}"#,
        "\n"
    );
    let responses = responses(input, &mut fake);
    match &responses[0] {
        Response::Error { id, error } => {
            assert_eq!(id, &Some(Id::from(2u64)));
            assert_eq!(error.code, METHOD_NOT_FOUND);
        }
        other => panic!("expected method not found, got {other:?}"),
    }
    match &responses[1] {
        Response::Error { id, error } => {
            assert_eq!(id, &Some(Id::from(3u64)));
            assert_eq!(error.code, INVALID_PARAMS);
        }
        other => panic!("expected invalid params, got {other:?}"),
    }
}

#[test]
fn an_invalid_request_echoes_the_id() {
    let mut fake = Fake::new(0);
    let responses = responses(r#"{"jsonrpc":"2.0","id":4}"#, &mut fake);
    match &responses[0] {
        Response::Error { id, error } => {
            assert_eq!(id, &Some(Id::from(4u64)));
            assert_eq!(error.code, INVALID_REQUEST);
        }
        other => panic!("expected invalid request, got {other:?}"),
    }
}

#[test]
fn a_client_rejects_a_wrong_response_id() {
    let line = r#"{"jsonrpc":"2.0","id":99,"result":{"revision":1}}"#;
    let mut client = Client::new(Cursor::new(format!("{line}\n")), Vec::new());
    let error = client.call(Call::ProjectShow).expect_err("wrong id");
    match error {
        ClientError::IdMismatch { expected, got } => {
            assert_eq!(expected, "1");
            assert_eq!(got, "99");
        }
        other => panic!("expected an id mismatch, got {other:?}"),
    }
}

#[test]
fn a_client_rejects_invalid_utf8_from_the_server() {
    let mut client = Client::new(Cursor::new(vec![0xff, b'\n']), Vec::new());
    let error = client.show().expect_err("invalid utf-8");
    assert!(matches!(error, ClientError::Decode(_)), "got {error:?}");
}

#[test]
fn a_client_reports_eof() {
    let mut client = Client::new(Cursor::new(""), Vec::new());
    let error = client.call(Call::ProjectShow).expect_err("eof");
    assert!(matches!(error, ClientError::Eof), "got {error:?}");
}

#[test]
fn a_handler_with_the_wrong_reply_kind_is_an_internal_error() {
    struct Wrong;
    impl Handler for Wrong {
        fn handle(&mut self, _: &Call) -> Result<Reply, RpcError> {
            Ok(Reply::Edl("not a project".into()))
        }
    }
    let mut wrong = Wrong;
    let responses = responses(
        r#"{"jsonrpc":"2.0","id":5,"method":"project.show"}"#,
        &mut wrong,
    );
    match &responses[0] {
        Response::Error { id, error } => {
            assert_eq!(id, &Some(Id::from(5u64)));
            assert_eq!(error.code, INTERNAL_ERROR);
        }
        other => panic!("expected an internal error, got {other:?}"),
    }
}

#[test]
fn failed_calls_do_not_ship_events() {
    struct Failing;
    impl Handler for Failing {
        fn handle(&mut self, _: &Call) -> Result<Reply, RpcError> {
            Err(RpcError::internal("boom"))
        }

        fn take_events(&mut self) -> Vec<Event> {
            vec![Event::ProjectChanged { revision: 1 }]
        }
    }
    let mut failing = Failing;
    let responses = responses(
        r#"{"jsonrpc":"2.0","id":8,"method":"project.show"}"#,
        &mut failing,
    );
    assert_eq!(responses.len(), 1, "only the error line, not its events");
    match &responses[0] {
        Response::Error { id, error } => {
            assert_eq!(id, &Some(Id::from(8u64)));
            assert_eq!(error.code, INTERNAL_ERROR);
        }
        other => panic!("expected an internal error, got {other:?}"),
    }
}

#[test]
fn blank_lines_are_ignored() {
    let mut fake = Fake::new(0);
    let responses = responses(
        "\n  \n{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"project.show\"}\n\n",
        &mut fake,
    );
    assert_eq!(responses.len(), 1);
}

#[test]
fn unit_calls_and_unknown_fields_are_validated() {
    assert_eq!(
        Call::decode("project.show", json!({})).expect("empty params"),
        Call::ProjectShow
    );
    assert_eq!(
        Call::decode("hello", Value::Null).expect("null params"),
        Call::Hello
    );
    assert_eq!(
        Call::decode("timeline.edl", Value::Null).expect("omitted params"),
        Call::TimelineEdl { track: None },
        "every field is optional, so omitted params is an empty object"
    );
    assert_eq!(
        Call::decode("timeline.edl", json!({})).expect("empty params"),
        Call::TimelineEdl { track: None }
    );
    assert_eq!(
        Call::decode("maybe", json!({}))
            .expect_err("unknown method")
            .code,
        METHOD_NOT_FOUND
    );
    assert_eq!(
        Call::decode("timeline.edl", json!({"trak": 0}))
            .expect_err("a typo is not silently ignored")
            .code,
        INVALID_PARAMS
    );
    assert_eq!(
        Call::decode("action.apply", json!({}))
            .expect_err("missing fields")
            .code,
        INVALID_PARAMS
    );
}

#[test]
fn a_call_without_params_uses_an_empty_object() {
    let mut fake = Fake::new(0);
    let responses = responses(
        r#"{"jsonrpc":"2.0","id":7,"method":"timeline.edl"}"#,
        &mut fake,
    );
    match &responses[0] {
        Response::Result { id, result } => {
            assert_eq!(id, &Id::from(7u64));
            assert_eq!(result.as_str(), Some("TITLE: fake None"));
        }
        other => panic!("expected an EDL, got {other:?}"),
    }
}

#[test]
fn undo_and_redo_are_accepted_as_apply_actions() {
    for action in [Action::Undo, Action::Redo] {
        let call = Call::ActionApply {
            base_revision: 1,
            actions: vec![action],
            from: None,
        };
        let params = call.params().expect("params");
        assert_eq!(Call::decode(call.method(), params).expect("decode"), call);
    }
}

#[test]
fn annotation_check_round_trips_with_and_without_ids() {
    let bare = Call::AnnotationCheck {
        report: PathBuf::from("/media/out.mp4"),
        sequence: None,
        id: Vec::new(),
    };
    let params = bare.params().expect("params");
    assert_eq!(params["report"], "/media/out.mp4");
    assert_eq!(
        Call::decode("annotation.check", params).expect("decode"),
        bare
    );
    // Both filters are optional, so a hand-written request may omit them and
    // still land on the same typed call.
    assert_eq!(
        Call::decode("annotation.check", json!({"report": "/media/out.mp4"}))
            .expect("omitted filters"),
        bare
    );

    let call = Call::AnnotationCheck {
        report: PathBuf::from("/media/out.mp4"),
        sequence: Some(1),
        id: vec![3, 4],
    };
    let (reply, output) = exchange(&mut Fake::new(7), call);
    let Reply::Checks(checks) = reply.expect("checks") else {
        panic!("expected a check report");
    };
    assert_eq!(checks.report().sequence, 1);
    assert_eq!(checks.report().frame_count, 60);
    assert_eq!(checks.checks().len(), 2);
    assert_eq!(checks.summary().pass, 2);

    let output = String::from_utf8(output).expect("utf-8");
    let response: Value =
        serde_json::from_str(output.lines().next().expect("one response")).expect("json");
    assert_eq!(
        response["result"]["report"],
        json!({
            "output": "/media/out.mp4",
            "sequence": 1,
            "revision": 7,
            "frame_count": 60
        })
    );
    assert_eq!(
        response["result"]["checks"][0],
        json!({
            "id": 3,
            "check": "cam",
            "status": "open",
            "result": "pass",
            "frames": [0, 60]
        })
    );
    assert_eq!(
        response["result"]["summary"],
        json!({"pass": 2, "fail": 0, "unverifiable": 0})
    );
}

#[test]
fn the_checks_helper_sends_and_decodes_the_report() {
    let result = json!({
        "report": {"output": "/media/out.mp4", "sequence": 1, "revision": 7, "frame_count": 60},
        "checks": [{"id": 2, "check": "cam", "status": "open", "result": "pass",
                    "frames": [0, 60]}],
        "summary": {"pass": 1, "fail": 0, "unverifiable": 0}
    });
    let input = format!("{}\n", json!({"jsonrpc": "2.0", "id": 1, "result": result}));
    let mut output = Vec::new();
    let checks = {
        let mut client = Client::new(Cursor::new(input), &mut output);
        client
            .checks(PathBuf::from("/media/out.mp4"), Some(1), vec![2])
            .expect("checks")
    };
    assert_eq!(checks.report().output, "/media/out.mp4");
    assert_eq!(checks.checks()[0].id, 2);
    assert_eq!(checks.summary().pass, 1);

    let request: Request = serde_json::from_slice(&output).expect("request");
    assert_eq!(request.method, "annotation.check");
    assert_eq!(request.params["report"], "/media/out.mp4");
    assert_eq!(request.params["sequence"], 1);
    assert_eq!(request.params["id"], json!([2]));
}

#[test]
fn analysis_add_round_trips_words_and_returns_an_address() {
    let call = Call::AnalysisAdd {
        asset: 1,
        words: vec![Word {
            text: "take".to_string(),
            start: Seconds::from_integer(0),
            end: Seconds::from_integer(1),
        }],
    };
    let params = call.params().expect("params");
    assert_eq!(params["asset"], 1);
    assert_eq!(params["words"][0]["text"], "take");
    assert_eq!(params["words"][0]["start"], "0/1");
    assert_eq!(Call::decode("analysis.add", params).expect("decode"), call);

    let (reply, output) = exchange(&mut Fake::new(7), call);
    let Reply::Analysis(hash) = reply.expect("analysis") else {
        panic!("expected an analysis hash");
    };
    assert!(hash.hash.starts_with("t1-"), "{hash:?}");
    let output = String::from_utf8(output).expect("utf-8");
    let response: Value =
        serde_json::from_str(output.lines().next().expect("one response")).expect("json");
    assert_eq!(response["result"]["hash"], hash.hash);
}

#[test]
fn every_call_has_a_known_method_and_decodes() {
    let calls = all_calls();
    assert_eq!(
        nle_protocol::METHODS.len(),
        calls.len(),
        "the sample list must cover every call"
    );
    for call in calls {
        assert!(
            nle_protocol::METHODS.contains(&call.method()),
            "{} is missing from METHODS",
            call.method()
        );
        let params = call.params().expect("params");
        assert_eq!(
            Call::decode(call.method(), params).expect("decode"),
            call,
            "{} must survive encode/decode",
            call.method()
        );
    }
}

#[test]
fn the_client_writes_one_request_per_call_and_numbers_them() {
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"first\"}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":\"second\"}\n"
    );
    let mut output = Vec::new();
    {
        let mut client = Client::new(Cursor::new(input), &mut output);
        assert_eq!(client.edl(None).expect("first"), "first");
        assert_eq!(client.edl(None).expect("second"), "second");
    }
    let lines: Vec<&str> = std::str::from_utf8(&output)
        .expect("utf-8")
        .lines()
        .collect();
    assert_eq!(lines.len(), 2, "one request line per call");
    let first: Request = serde_json::from_str(lines[0]).expect("first request");
    let second: Request = serde_json::from_str(lines[1]).expect("second request");
    assert_eq!(first.id, Id::from(1u64));
    assert_eq!(second.id, Id::from(2u64));
    assert_eq!(first.method, "timeline.edl");
    assert_eq!(second.method, "timeline.edl");
    assert_eq!(output.last(), Some(&b'\n'), "the request ends in a newline");
}

#[test]
fn catalog_actions_survive_the_call_encoding() {
    let call = Call::ActionApply {
        base_revision: 3,
        actions: vec![Action::SetSpeed {
            track: 0,
            clip: 2,
            rate: Seconds::new(30000, 1001),
        }],
        from: Some(Seconds::new(1, 2)),
    };
    let params = call.params().expect("params");
    assert_eq!(params["actions"][0]["rate"], "30000/1001");
    assert_eq!(params["from"], "1/2");
    assert_eq!(Call::decode(call.method(), params).expect("decode"), call);
}

#[test]
fn timeline_edl_is_a_plain_string_reply() {
    let (reply, _) = exchange(&mut Fake::new(0), Call::TimelineEdl { track: Some(1) });
    let text = match reply.expect("edl") {
        Reply::Edl(text) => text,
        other => panic!("expected an EDL, got {other:?}"),
    };
    assert_eq!(text, "TITLE: fake Some(1)");
}

#[test]
fn a_transition_action_crosses_the_wire() {
    let call = Call::ActionApply {
        base_revision: 0,
        actions: vec![Action::Transition {
            track: 0,
            at: 4.0,
            kind: TransitionKind::Dissolve,
            len: TransitionLen::Frames(12),
        }],
        from: None,
    };
    let params = call.params().expect("params");
    assert_eq!(Call::decode(call.method(), params).expect("decode"), call);
}
