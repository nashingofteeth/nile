//! `nile serve --stdio` end to end: real process, real project, real history.
//!
//! The protocol crate covers envelopes and transport in isolation; this test
//! proves the handler wiring — CAS, saves, events, and the history the CLI
//! reads after the server exits.

mod common;

use common::{P, Sandbox};
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
fn protocol_server_round_trip() {
    let sandbox = Sandbox::new();
    sandbox.base();
    let mut serve = sandbox.serve();

    let hello = call(&mut serve, 1, "hello", None);
    assert_eq!(hello["result"]["protocol"], 5);
    assert_eq!(hello["result"]["project_version"], 12);
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
