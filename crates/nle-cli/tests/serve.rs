//! `nile serve --stdio` end to end: real process, real project, real history.
//!
//! The protocol crate covers envelopes and transport in isolation; this test
//! proves the handler wiring — CAS, saves, events, and the history the CLI
//! reads after the server exits.

mod common;

use common::Sandbox;
use serde_json::{Value, json};

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
    assert_eq!(hello["result"]["protocol"], 1);
    assert_eq!(hello["result"]["project_version"], 10);
    let revision = hello["result"]["revision"].as_u64().expect("revision");
    assert!(
        hello["result"]["methods"]
            .as_array()
            .expect("methods")
            .iter()
            .any(|m| m == "action.apply")
    );

    let show = call(&mut serve, 2, "project.show", None);
    let before = show["result"]["timeline"]["tracks"][0]["segments"]
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
            "action": {"action": "split", "track": 0, "at": 1.0},
            "from": "1/2"
        })),
    );
    assert_eq!(preview["result"]["revision"], revision);
    assert_eq!(preview["result"]["changed"], true);
    let show = call(&mut serve, 5, "project.show", None);
    assert_eq!(show["result"]["revision"], revision, "preview did not save");
    assert_eq!(
        show["result"]["timeline"]["tracks"][0]["segments"]
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
            "action": {"action": "split", "track": 0, "at": 1.0},
            "from": "1/2"
        })),
    );
    let after = applied["result"]["revision"].as_u64().expect("revision");
    assert_eq!(after, revision + 1);
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
        show["result"]["timeline"]["tracks"][0]["segments"]
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
            "action": {"action": "split", "track": 0, "at": 2.0}
        })),
    );
    assert_eq!(stale["error"]["code"], -32001);

    let bad = call(
        &mut serve,
        10,
        "action.apply",
        Some(json!({
            "base_revision": after + 1,
            "action": {
                "action": "paste",
                "track": 0,
                "at": 1.0,
                "ripple": false,
                "segmnt": {}
            }
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
        project["timeline"]["tracks"][0]["segments"]
            .as_array()
            .expect("segments")
            .len(),
        before + 1
    );
    sandbox.ok(&["undo"]);
    let project: Value =
        serde_json::from_str(&sandbox.ok(&["show", "--json"])).expect("project JSON");
    assert_eq!(
        project["timeline"]["tracks"][0]["segments"]
            .as_array()
            .expect("segments")
            .len(),
        before
    );
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
            "action": {"action": "split", "track": 0, "at": 1.0},
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
            "action": {"action": "undo"},
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
            "action": {"action": "yank", "track": 0, "target": {"clip": 1}}
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
            "action": {"action": "paste", "track": 0, "at": 6.0, "ripple": false}
        })),
    );
    assert_eq!(pasted["result"]["revision"], revision + 1);
    assert_eq!(pasted["result"]["changed"], true);
    let event = serve.read();
    assert_eq!(event["method"], "project.changed");

    let show = call(&mut serve, 4, "project.show", None);
    assert_eq!(
        show["result"]["timeline"]["tracks"][0]["segments"]
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
        project["timeline"]["tracks"][0]["segments"]
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
