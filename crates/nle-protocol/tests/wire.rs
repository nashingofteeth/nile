//! Golden wire forms and an in-memory client/server round trip. These are
//! integration tests on purpose: they can only see the public API, so a
//! broken envelope or a typed call that cannot survive encoding fails here.

use std::io::Cursor;

use nle_core::{Action, Project, Seconds, TransitionKind, TransitionLen};
use nle_protocol::{
    Call, Client, ClientError, Event, Handler, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, Id,
    METHOD_NOT_FOUND, Notification, Outcome, PARSE_ERROR, REVISION_CONFLICT, Reply, Request,
    Response, RevisionConflict, RpcError, Server,
};
use serde_json::json;

struct Fake {
    revision: u64,
}

impl Fake {
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
            Call::TimelineEdl { track } => Ok(Reply::Edl(format!("TITLE: fake {track:?}"))),
            Call::ActionPreview { base_revision, .. } => {
                self.check(*base_revision)?;
                Ok(Reply::Outcome(Outcome {
                    revision: self.revision,
                    desc: "preview".to_string(),
                    changed: true,
                    playhead: None,
                }))
            }
            Call::ActionApply {
                base_revision,
                actions,
            } => {
                self.check(*base_revision)?;
                self.revision += 1;
                Ok(Reply::Outcome(Outcome {
                    revision: self.revision,
                    desc: format!("applied {}", actions.len()),
                    changed: true,
                    playhead: Some(30),
                }))
            }
            Call::Undo { base_revision } => {
                self.check(*base_revision)?;
                self.revision = self.revision.saturating_sub(1);
                Ok(Reply::Outcome(Outcome {
                    revision: self.revision,
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
                    desc: "redo".to_string(),
                    changed: true,
                    playhead: None,
                }))
            }
        }
    }
}

/// Send `call` to a server, then feed its one-line response to a client.
/// This exercises both halves without a second thread or a real pipe.
fn exchange(handler: &mut impl Handler, call: Call) -> Result<Reply, ClientError> {
    let request = Request::new(
        Id::from(1u64),
        call.method(),
        call.params().expect("encode params"),
    );
    let line = serde_json::to_string(&request).expect("encode request");
    let mut server = Server::new(Cursor::new(format!("{line}\n")), Vec::new(), handler);
    server.serve().expect("serve");
    let (_, output, _) = server.into_inner();
    let mut client = Client::new(Cursor::new(output), Vec::new());
    client.call(call)
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

#[test]
fn project_show_round_trips_through_the_typed_call() {
    let mut fake = Fake { revision: 7 };
    match exchange(&mut fake, Call::ProjectShow).expect("show") {
        Reply::Project(project) => assert_eq!(project.revision, 7),
        other => panic!("expected project, got {other:?}"),
    }
}

#[test]
fn apply_moves_the_revision_and_preview_does_not() {
    let mut fake = Fake { revision: 7 };
    let preview = exchange(
        &mut fake,
        Call::ActionPreview {
            base_revision: 7,
            actions: vec![Action::Split { track: 0, at: 1.5 }],
        },
    )
    .expect("preview");
    assert_eq!(
        preview,
        Reply::Outcome(Outcome {
            revision: 7,
            desc: "preview".into(),
            changed: true,
            playhead: None,
        })
    );

    let applied = exchange(
        &mut fake,
        Call::ActionApply {
            base_revision: 7,
            actions: vec![Action::Split { track: 0, at: 1.5 }],
        },
    )
    .expect("apply");
    assert_eq!(
        applied,
        Reply::Outcome(Outcome {
            revision: 8,
            desc: "applied 1".into(),
            changed: true,
            playhead: Some(30),
        })
    );
}

#[test]
fn a_stale_base_revision_is_a_typed_conflict() {
    let mut fake = Fake { revision: 7 };
    let error = exchange(
        &mut fake,
        Call::ActionApply {
            base_revision: 3,
            actions: Vec::new(),
        },
    )
    .expect_err("stale base");
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
    let mut fake = Fake { revision: 1 };
    let undone = exchange(&mut fake, Call::Undo { base_revision: 1 }).expect("undo");
    assert_eq!(
        undone,
        Reply::Outcome(Outcome {
            revision: 0,
            desc: "undo".into(),
            changed: true,
            playhead: None,
        })
    );
    let redone = exchange(&mut fake, Call::Redo { base_revision: 0 }).expect("redo");
    assert_eq!(
        redone,
        Reply::Outcome(Outcome {
            revision: 1,
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
fn golden_response_and_error_envelopes() {
    let ok = Response::Result {
        id: Id::from(1u64),
        result: json!({"revision": 8, "desc": "applied", "changed": true}),
    };
    assert_eq!(
        serde_json::to_string(&ok).expect("serialize"),
        r#"{"jsonrpc":"2.0","id":1,"result":{"changed":true,"desc":"applied","revision":8}}"#
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
        serde_json::to_string(&event.notification()).expect("serialize"),
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
    ];
    for event in events {
        let back = Event::from_notification(event.notification()).expect("decode");
        assert_eq!(back, event);
    }

    let unknown = Notification::new("job.progress", json!({}));
    let error = Event::from_notification(unknown).expect_err("unknown event");
    assert_eq!(error.code, METHOD_NOT_FOUND);
}

#[test]
fn a_malformed_line_is_a_parse_error_and_serving_continues() {
    let mut fake = Fake { revision: 0 };
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
fn unknown_method_and_bad_params_are_distinct() {
    let mut fake = Fake { revision: 0 };
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":2,"method":"timeline.rewind"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"action.apply","params":{"base_revision":"x","actions":[]}}"#,
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
    let mut fake = Fake { revision: 0 };
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
fn blank_lines_are_ignored() {
    let mut fake = Fake { revision: 0 };
    let responses = responses(
        "\n  \n{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"project.show\"}\n\n",
        &mut fake,
    );
    assert_eq!(responses.len(), 1);
}

#[test]
fn every_call_has_a_known_method() {
    let calls = [
        Call::ProjectShow,
        Call::TimelineEdl { track: None },
        Call::ActionPreview {
            base_revision: 0,
            actions: Vec::new(),
        },
        Call::ActionApply {
            base_revision: 0,
            actions: Vec::new(),
        },
        Call::Undo { base_revision: 0 },
        Call::Redo { base_revision: 0 },
    ];
    for call in &calls {
        assert!(
            nle_protocol::METHODS.contains(&call.method()),
            "{} is missing from METHODS",
            call.method()
        );
    }
    assert_eq!(nle_protocol::METHODS.len(), calls.len());
}

#[test]
fn catalog_actions_survive_the_call_encoding() {
    let call = Call::ActionApply {
        base_revision: 3,
        actions: vec![
            Action::SetSpeed {
                track: 0,
                clip: 2,
                rate: Seconds::new(30000, 1001),
            },
            Action::Transition {
                track: 0,
                at: 4.0,
                kind: TransitionKind::Dissolve,
                len: TransitionLen::Frames(12),
            },
        ],
    };
    let params = call.params().expect("params");
    assert_eq!(params["actions"][0]["rate"], "30000/1001");
    assert_eq!(Call::decode(call.method(), params).expect("decode"), call);
}

#[test]
fn timeline_edl_is_a_plain_string_reply() {
    let mut fake = Fake { revision: 0 };
    let text = match exchange(&mut fake, Call::TimelineEdl { track: Some(1) }).expect("edl") {
        Reply::Edl(text) => text,
        other => panic!("expected an EDL, got {other:?}"),
    };
    assert_eq!(text, "TITLE: fake Some(1)");
}
