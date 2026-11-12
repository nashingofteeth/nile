//! Golden wire forms and an in-memory client/server round trip. These are
//! integration tests on purpose: they can only see the public API, so a
//! broken envelope or a typed call that cannot survive encoding fails here.

use std::io::Cursor;

use nle_core::{Action, Project, Seconds, TransitionKind, TransitionLen};
use nle_protocol::{
    Call, Client, ClientError, Event, Handler, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, Id,
    METHOD_NOT_FOUND, Notification, Outcome, PARSE_ERROR, PROTOCOL_VERSION, REVISION_CONFLICT,
    Reply, Request, Response, RevisionConflict, RpcError, Server,
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
            Call::Hello => Ok(Reply::Hello(nle_protocol::Hello::new(self.revision))),
            Call::TimelineEdl { track } => Ok(Reply::Edl(format!("TITLE: fake {track:?}"))),
            Call::ActionPreview {
                base_revision,
                action,
                from,
            } => {
                self.check(*base_revision)?;
                self.last_from = *from;
                Ok(Reply::Outcome(Outcome {
                    revision: self.revision,
                    desc: format!("preview {}", action.slot()),
                    changed: true,
                    playhead: None,
                }))
            }
            Call::ActionApply {
                base_revision,
                action,
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
                    desc: format!("applied {}", action.slot()),
                    changed: true,
                    playhead: Some(playhead),
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

fn all_calls() -> Vec<Call> {
    let action = Action::Undo;
    vec![
        Call::ProjectShow,
        Call::Hello,
        Call::TimelineEdl { track: None },
        Call::ActionPreview {
            base_revision: 0,
            action: action.clone(),
            from: None,
        },
        Call::ActionApply {
            base_revision: 0,
            action,
            from: None,
        },
        Call::Undo { base_revision: 0 },
        Call::Redo { base_revision: 0 },
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
            action: Action::Split { track: 0, at: 1.5 },
            from: None,
        },
    );
    assert_eq!(
        preview.expect("preview"),
        Reply::Outcome(Outcome {
            revision: 7,
            desc: "preview 2".into(),
            changed: true,
            playhead: None,
        })
    );

    let (applied, _) = exchange(
        &mut fake,
        Call::ActionApply {
            base_revision: 7,
            action: Action::Split { track: 0, at: 1.5 },
            from: None,
        },
    );
    assert_eq!(
        applied.expect("apply"),
        Reply::Outcome(Outcome {
            revision: 8,
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
            action: Action::Split { track: 0, at: 1.5 },
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
            action: Action::Split { track: 0, at: 1.5 },
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
        action: Action::Split { track: 0, at: 1.5 },
        from: None,
    };
    let request = Request::new(
        Id::from(1u64),
        call.method(),
        call.params().expect("params"),
    );
    assert_eq!(
        serde_json::to_string(&request).expect("serialize"),
        r#"{"jsonrpc":"2.0","id":1,"method":"action.apply","params":{"action":{"action":"split","at":1.5,"track":0},"base_revision":7}}"#
    );
}

#[test]
fn the_anchor_is_rational_on_the_wire() {
    let call = Call::ActionApply {
        base_revision: 7,
        action: Action::Split { track: 0, at: 1.5 },
        from: Some(Seconds::new(3, 2)),
    };
    let params = call.params().expect("params");
    assert_eq!(params["from"], "3/2");
    assert_eq!(Call::decode(call.method(), params).expect("decode"), call);
}

#[test]
fn golden_response_and_error_envelopes() {
    let ok = Response::Result {
        id: Id::from(1u64),
        result: serde_json::to_value(Outcome {
            revision: 8,
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
    ];
    assert_eq!(nle_protocol::EVENTS.len(), events.len());
    for event in events {
        let back =
            Event::from_notification(event.notification().expect("notification")).expect("decode");
        assert_eq!(back, event);
    }

    let unknown = Notification::new("job.progress", json!({}));
    let error = Event::from_notification(unknown).expect_err("unknown event");
    assert_eq!(error.code, METHOD_NOT_FOUND);
}

#[test]
fn the_server_writes_the_response_before_its_events() {
    let (reply, output) = exchange(
        &mut Fake::new(7),
        Call::ActionApply {
            base_revision: 7,
            action: Action::Split { track: 0, at: 1.5 },
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
            action: Action::Split { track: 0, at: 1.5 },
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
    let bad = r#"{"jsonrpc":"2.0","method":"job.progress","params":{}}"#;
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
    let bad = r#"{"jsonrpc":"2.0","method":"job.progress","params":{}}"#;
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
        r#"{"jsonrpc":"2.0","id":3,"method":"action.apply","params":{"base_revision":"x","action":{"action":"undo"}}}"#,
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
            action,
            from: None,
        };
        let params = call.params().expect("params");
        assert_eq!(Call::decode(call.method(), params).expect("decode"), call);
    }
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
        action: Action::SetSpeed {
            track: 0,
            clip: 2,
            rate: Seconds::new(30000, 1001),
        },
        from: Some(Seconds::new(1, 2)),
    };
    let params = call.params().expect("params");
    assert_eq!(params["action"]["rate"], "30000/1001");
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
        action: Action::Transition {
            track: 0,
            at: 4.0,
            kind: TransitionKind::Dissolve,
            len: TransitionLen::Frames(12),
        },
        from: None,
    };
    let params = call.params().expect("params");
    assert_eq!(Call::decode(call.method(), params).expect("decode"), call);
}
