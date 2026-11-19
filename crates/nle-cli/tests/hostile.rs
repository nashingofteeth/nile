//! The hostile battery: whatever the CLI is handed it must
//! - not panic (a crash is always a bug),
//! - exit 0 (accepted), 1 (refused with a message) or 2 (usage error),
//! - leave a project that still loads with a finite, non-negative length.
//!
//! These are the old `scripts/fuzz.sh` cases. Nothing here asserts *what* a
//! command said — only that it held the invariant — so a deliberate wording
//! change never touches this file.

mod common;

use std::fs;
#[cfg(target_os = "linux")]
use std::process::Stdio;

use common::*;
use serde_json::json;

/// A labelled case: the argv sequences to run, in order, on a fresh hostile
/// base.
type Case = (&'static str, &'static [&'static [&'static str]]);

fn run(sandbox: &Sandbox, cases: &[Case]) {
    for &(label, steps) in cases {
        sandbox.invariant(label, steps);
    }
}

#[test]
fn addressing_and_ranges() {
    let s = Sandbox::new();
    run(
        &s,
        &[
            ("split at 0", &[&["split", "--at", "0"]]),
            ("split at the end", &[&["split", "--at", "7"]]),
            ("split on a cut", &[&["split", "--at", "3"]]),
            ("split inside a gap", &[&["split", "--at", "3.5"]]),
            ("split at -1", &[&["split", "--at", "-1"]]),
            ("split at 999", &[&["split", "--at", "999"]]),
            (
                "split at 1e18",
                &[&["split", "--at", "1000000000000000000"]],
            ),
            ("split at abc", &[&["split", "--at", "abc"]]),
            ("split at 1:2:3:4", &[&["split", "--at", "1:2:3:4"]]),
            ("split at an empty string", &[&["split", "--at", ""]]),
            ("trim-in at 0", &[&["trim-in", "--at", "0"]]),
            ("trim-in at the end", &[&["trim-in", "--at", "7"]]),
            ("trim-out at 0", &[&["trim-out", "--at", "0"]]),
            ("delete at 7", &[&["delete", "--at", "7"]]),
            ("delete --clip 999", &[&["delete", "--clip", "999"]]),
            ("delete --seg 99", &[&["delete", "--seg", "99"]]),
            (
                "delete --seg 2^64-1",
                &[&["delete", "--seg", "18446744073709551615"]],
            ),
            ("yank --clip 999", &[&["yank", "--clip", "999"]]),
            ("paste with an empty register", &[&["paste", "--at", "1"]]),
        ],
    );
}

#[test]
fn lengths_and_rates() {
    let s = Sandbox::new();
    run(
        &s,
        &[
            ("gap frames 0", &[&["gap", "--at", "1", "--frames", "0"]]),
            ("gap frames -5", &[&["gap", "--at", "1", "--frames", "-5"]]),
            ("gap dur 0", &[&["gap", "--at", "1", "--dur", "0"]]),
            ("gap dur -2", &[&["gap", "--at", "1", "--dur", "-2"]]),
            (
                "gap frames i64::MAX",
                &[&["gap", "--at", "1", "--frames", "9223372036854775807"]],
            ),
            (
                "gap at 1e18",
                &[&["gap", "--at", "1000000000000000000", "--frames", "30"]],
            ),
            (
                "gap at 999 (bridges a void)",
                &[&["gap", "--at", "999", "--frames", "30"]],
            ),
            ("speed 0", &[&["speed", "1", "0"]]),
            ("speed -1", &[&["speed", "1", "-1"]]),
            ("speed abc", &[&["speed", "1", "abc"]]),
            ("speed 1/0", &[&["speed", "1", "1/0"]]),
            ("speed 0.000001", &[&["speed", "1", "0.000001"]]),
            ("speed 10000 (sub-frame)", &[&["speed", "2", "10000"]]),
            ("speed 1e18", &[&["speed", "1", "1000000000000000000"]]),
            (
                "speed i64::MAX/1",
                &[&["speed", "1", "9223372036854775807/1"]],
            ),
            (
                "speed 1/i64::MAX",
                &[&["speed", "1", "1/9223372036854775807"]],
            ),
        ],
    );
}

#[test]
fn transitions() {
    let s = Sandbox::new();
    run(
        &s,
        &[
            (
                "transition frames 0",
                &[&["transition", "--at", "3", "--frames", "0"]],
            ),
            (
                "transition frames -5",
                &[&["transition", "--at", "3", "--frames", "-5"]],
            ),
            (
                "transition frames i64::MAX",
                &[&["transition", "--at", "3", "--frames", "9223372036854775807"]],
            ),
            (
                "transition dur 0",
                &[&["transition", "--at", "3", "--dur", "0"]],
            ),
            (
                "transition dur -1",
                &[&["transition", "--at", "3", "--dur", "-1"]],
            ),
            (
                "transition --frames and --dur",
                &[&["transition", "--at", "3", "--frames", "15", "--dur", "1"]],
            ),
            (
                "transition kind sideways",
                &[&["transition", "sideways", "--at", "3", "--frames", "15"]],
            ),
            (
                "transition at 0",
                &[&["transition", "--at", "0", "--frames", "15"]],
            ),
            (
                "transition at the end",
                &[&["transition", "--at", "7", "--frames", "15"]],
            ),
            (
                "transition at 1e18",
                &[&[
                    "transition",
                    "--at",
                    "1000000000000000000",
                    "--frames",
                    "15",
                ]],
            ),
            (
                "transition at 999",
                &[&["transition", "--at", "999", "--frames", "15"]],
            ),
            (
                "transition mid-clip",
                &[&["transition", "--at", "1.5", "--frames", "15"]],
            ),
            (
                "transition longer than the clip",
                &[&["transition", "--at", "3", "--dur", "99"]],
            ),
            (
                "transition at a clip|clip seam",
                &[
                    &["split", "--at", "1"],
                    &["transition", "--at", "1", "--frames", "15"],
                ],
            ),
            (
                "transition twice at one seam",
                &[
                    &["transition", "--at", "3", "--frames", "15"],
                    &["transition", "--at", "3", "--frames", "15"],
                ],
            ),
            (
                "transition then purge-stumps",
                &[
                    &["transition", "--at", "3", "--frames", "15"],
                    &["purge-stumps"],
                ],
            ),
            (
                "transition then export",
                &[
                    &["transition", "--at", "3", "--frames", "15"],
                    &["export", "-o", "tx.mp4"],
                ],
            ),
            (
                "transition then delete --seg",
                &[
                    &["transition", "--at", "3", "--frames", "15"],
                    &["delete", "--seg", "1"],
                ],
            ),
            (
                "transition then delete twice",
                &[
                    &["transition", "--at", "3", "--frames", "15"],
                    &["delete", "--at", "3.1"],
                    &["delete", "--at", "3.1"],
                ],
            ),
            (
                "transition between two gaps",
                &[
                    &["delete", "--seg", "0"],
                    &["delete", "--seg", "1"],
                    &["transition", "--at", "0", "--frames", "15"],
                ],
            ),
            (
                "transition --frames 2^63-1",
                &[&["transition", "--at", "3", "--frames", "9223372036854775807"]],
            ),
            (
                "transition --dur 1e300",
                &[&[
                    "transition",
                    "--at",
                    "3",
                    "--dur",
                    "1000000000000000000000000000000",
                ]],
            ),
            (
                "transition --frames -1",
                &[&["transition", "--at", "3", "--frames", "-1"]],
            ),
        ],
    );
}

#[test]
fn effects() {
    let s = Sandbox::new();
    run(
        &s,
        &[
            (
                "crop key without a crop",
                &[&["effect", "crop", "--clip", "1", "--at", "1"]],
            ),
            (
                "crop key outside the clip",
                &[&[
                    "effect", "crop", "--clip", "1", "--at", "9", "--x", "0", "--w", "100",
                ]],
            ),
            (
                "effect crop x 101",
                &[&["effect", "crop", "--clip", "1", "--x", "101"]],
            ),
            (
                "effect crop past the opposite edge",
                &[&["effect", "crop", "--clip", "1", "--x", "60", "--w", "60"]],
            ),
            (
                "effect scale w -1",
                &[&["effect", "scale", "--clip", "1", "--w", "-1", "--h", "10"]],
            ),
            (
                "effect scale w 2^64",
                &[&[
                    "effect",
                    "scale",
                    "--clip",
                    "1",
                    "--w",
                    "18446744073709551616",
                    "--h",
                    "10",
                ]],
            ),
            (
                "effect scale 1x1",
                &[&["effect", "scale", "--clip", "1", "--w", "1", "--h", "1"]],
            ),
            (
                "effect eq brightness abc",
                &[&["effect", "eq", "--clip", "1", "--brightness", "abc"]],
            ),
            (
                "effect eq contrast 1e18",
                &[&[
                    "effect",
                    "eq",
                    "--clip",
                    "1",
                    "--contrast",
                    "1000000000000000000",
                ]],
            ),
            (
                "effect eq brightness nan",
                &[&["effect", "eq", "--clip", "1", "--brightness", "nan"]],
            ),
            (
                "effect eq contrast inf",
                &[&["effect", "eq", "--clip", "1", "--contrast", "inf"]],
            ),
            (
                "effect eq saturation out of range",
                &[&["effect", "eq", "--clip", "1", "--saturation", "4"]],
            ),
            (
                "effect lut a missing file",
                &[&["effect", "lut", "--clip", "1", "--path", "/nope.cube"]],
            ),
            (
                "effect lut an empty path",
                &[&["effect", "lut", "--clip", "1", "--path", ""]],
            ),
            (
                "effect lut a path with commas",
                &[&["effect", "lut", "--clip", "1", "--path", "/tmp/a,b:c.cube"]],
            ),
            (
                "effect unknown kind",
                &[&["effect", "bogus", "--clip", "1"]],
            ),
            ("effect without a kind", &[&["effect", "--clip", "1"]]),
            ("effect clip 999", &[&["effect", "eq", "--clip", "999"]]),
            ("effect without --clip", &[&["effect", "eq"]]),
            (
                "effect on an audio track",
                &[
                    &["track", "add", "audio"],
                    &["effect", "eq", "--clip", "1", "--track", "2"],
                ],
            ),
            (
                "effect then purge-stumps",
                &[
                    &["effect", "eq", "--clip", "1", "--saturation", "0"],
                    &["purge-stumps"],
                ],
            ),
            (
                "effect then export a bad lut",
                &[
                    &["effect", "lut", "--clip", "1", "--path", "/nope.cube"],
                    &["export", "-o", "lx.mp4"],
                ],
            ),
            (
                "effect then speed then delete",
                &[
                    &["effect", "eq", "--clip", "1"],
                    &["speed", "1", "200"],
                    &["delete", "--clip", "1"],
                ],
            ),
        ],
    );
}

#[test]
fn speeds_next_to_transitions() {
    let s = Sandbox::new();
    run(
        &s,
        &[
            (
                "speed a transition's neighbour 1e6",
                &[
                    &["transition", "--at", "3", "--frames", "15"],
                    &["speed", "1", "1000000"],
                ],
            ),
            (
                "speed a transition's neighbour 1e-6",
                &[
                    &["transition", "--at", "3", "--frames", "15"],
                    &["speed", "1", "0.000001"],
                ],
            ),
            (
                "speed the clip out of a transition",
                &[
                    &["transition", "--at", "3", "--frames", "15"],
                    &["speed", "2", "200"],
                ],
            ),
            (
                "speed a transition id",
                &[
                    &["transition", "--at", "3", "--frames", "15"],
                    &["speed", "4", "200"],
                ],
            ),
            (
                "transition percent 0",
                &[&["transition", "--at", "3", "--percent", "0"]],
            ),
            (
                "transition percent with no blend",
                &[&["transition", "--at", "1", "--percent", "50"]],
            ),
            (
                "tiny transition percent on a blend",
                &[
                    &["transition", "--at", "3", "--frames", "15"],
                    &["transition", "--at", "3", "--percent", "0.000001"],
                ],
            ),
            (
                "transition i64::MAX frames",
                &[&["transition", "--at", "3", "--frames", "9223372036854775807"]],
            ),
        ],
    );
}

#[test]
fn extend() {
    let s = Sandbox::new();
    run(
        &s,
        &[
            (
                "extend to the same edge",
                &[&["extend", "--clip", "1", "--edge", "out", "--to", "3"]],
            ),
            (
                "extend to the opposite end",
                &[&["extend", "--clip", "1", "--edge", "out", "--to", "0"]],
            ),
            (
                "extend beyond the material",
                &[&["extend", "--clip", "1", "--edge", "out", "--to", "99"]],
            ),
            (
                "extend to -5",
                &[&["extend", "--clip", "1", "--edge", "out", "--to", "-5"]],
            ),
            (
                "extend a gap's head back",
                &[&["extend", "--seg", "1", "--edge", "in", "--to", "-2"]],
            ),
            (
                "extend --edge sideways",
                &[&["extend", "--clip", "1", "--edge", "sideways", "--to", "2"]],
            ),
            (
                "extend a missing clip",
                &[&["extend", "--clip", "999", "--edge", "in", "--to", "1"]],
            ),
        ],
    );
}

#[test]
fn tracks_and_pool() {
    let s = Sandbox::new();
    run(
        &s,
        &[
            ("track rm the only video", &[&["track", "rm", "1"]]),
            ("track rm 99", &[&["track", "rm", "99"]]),
            ("track mute 99", &[&["track", "mute", "99"]]),
            ("track add 0", &[&["add", "red.mp4", "--track", "0"]]),
            ("track add 99", &[&["add", "red.mp4", "--track", "99"]]),
            ("add --track 0", &[&["add", "red.mp4", "--track", "0"]]),
            ("media rm an in-use asset", &[&["media", "rm", "1"]]),
            ("media rm 99", &[&["media", "rm", "99"]]),
            ("undo on an empty stack", &[&["undo"]]),
            ("redo on an empty stack", &[&["redo"]]),
            ("seek -1", &[&["seek", "-1"]]),
            ("seek 1e18", &[&["seek", "1000000000000000000"]]),
        ],
    );
}

#[test]
fn odd_media() {
    let s = Sandbox::new();
    run(
        &s,
        &[
            (
                "add video-only onto an audio track",
                &[
                    &["track", "add", "audio"],
                    &[
                        "add",
                        "video_only.mp4",
                        "--track",
                        "2",
                        "--in",
                        "0",
                        "--out",
                        "1",
                    ],
                ],
            ),
            (
                "add audio-only onto a video track",
                &[&["add", "audio_only.m4a", "--in", "0", "--out", "1"]],
            ),
            (
                "add a 0.1 s file as 1 s",
                &[&["add", "tiny.mp4", "--in", "0", "--out", "1"]],
            ),
            ("add an empty file", &[&["add", "empty.mp4"]]),
            ("add a text file", &[&["add", "notmedia.txt"]]),
            ("add a directory", &[&["add", "."]]),
            (
                "add --in 5 --out 2",
                &[&["add", "red.mp4", "--in", "5", "--out", "2"]],
            ),
            (
                "add --out 99",
                &[&["add", "red.mp4", "--in", "0", "--out", "99"]],
            ),
            ("add --at -1", &[&["add", "red.mp4", "--at", "-1"]]),
            ("import a missing file", &[&["import", "nope.mp4"]]),
            ("import an empty file", &[&["import", "empty.mp4"]]),
            ("import a text file", &[&["import", "notmedia.txt"]]),
            ("import a directory", &[&["import", "."]]),
            (
                "detect-edits a missing file",
                &[&["detect-edits", "nope.mov", "--force"]],
            ),
            (
                "detect-edits an audio-only file",
                &[&["detect-edits", "audio_only.m4a", "--force"]],
            ),
            (
                "import-edl a missing file",
                &[&["import-edl", "nope.edl", "--media", ".", "--force"]],
            ),
            (
                "import-otio a missing file",
                &[&["import-otio", "nope.otio", "--force"]],
            ),
            (
                "import-edl a nameless group",
                &[&["import-edl", "nope.edl", "--media", ".", "--group", "=a,b"]],
            ),
            (
                "import-edl a one-part group",
                &[&["import-edl", "nope.edl", "--media", ".", "--group", "CAM=a"]],
            ),
            (
                "import-otio an empty group part",
                &[&["import-otio", "nope.otio", "--group", "CAM=a,,b"]],
            ),
            (
                "annotation add a missing asset",
                &[&["annotation", "add", "note", "--asset", "99", "--time", "0"]],
            ),
            (
                "annotation add a bad time",
                &[&[
                    "annotation",
                    "add",
                    "note",
                    "--asset",
                    "1",
                    "--time",
                    "nope",
                ]],
            ),
            (
                "annotation add two sources",
                &[&[
                    "annotation",
                    "add",
                    "note",
                    "--asset",
                    "1",
                    "--group",
                    "1",
                    "--time",
                    "0",
                ]],
            ),
            (
                "annotation add empty text",
                &[&["annotation", "add", "   ", "--asset", "1", "--time", "0"]],
            ),
            (
                "annotation add timeline without a position",
                &[&["annotation", "add", "pin", "--timeline"]],
            ),
            (
                "annotation add timeline on a gap",
                &[&["annotation", "add", "pin", "--timeline", "--at", "3.5"]],
            ),
            (
                "annotation add --to without --timeline",
                &[&["annotation", "add", "x", "--to", "5"]],
            ),
            (
                "annotation range ends before it starts",
                &[&[
                    "annotation",
                    "add",
                    "span",
                    "--timeline",
                    "--at",
                    "2",
                    "--to",
                    "1",
                ]],
            ),
            (
                "annotation range ends in a gap",
                &[&[
                    "annotation",
                    "add",
                    "span",
                    "--timeline",
                    "--at",
                    "1",
                    "--to",
                    "3.5",
                ]],
            ),
            (
                "annotation add timeline on a blend",
                &[
                    &["transition", "--at", "2"],
                    &["annotation", "add", "pin", "--timeline", "--at", "2"],
                ],
            ),
            (
                "annotation add timeline track 99",
                &[&[
                    "annotation",
                    "add",
                    "pin",
                    "--timeline",
                    "--at",
                    "1",
                    "--track",
                    "99",
                ]],
            ),
            (
                "annotation expect both asset and group",
                &[&[
                    "annotation",
                    "add",
                    "--expect",
                    "cam",
                    "--timeline",
                    "--at",
                    "1",
                    "--asset",
                    "1",
                    "--group",
                    "1",
                ]],
            ),
            (
                "annotation expect a flag from another check",
                &[&[
                    "annotation",
                    "add",
                    "--expect",
                    "cam",
                    "--timeline",
                    "--at",
                    "1",
                    "--asset",
                    "1",
                    "--black",
                ]],
            ),
            (
                "annotation expect an unknown check",
                &[&[
                    "annotation",
                    "add",
                    "--expect",
                    "nope",
                    "--timeline",
                    "--at",
                    "1",
                ]],
            ),
            (
                "annotation expect without --timeline",
                &[&[
                    "annotation",
                    "add",
                    "--expect",
                    "cam",
                    "--at",
                    "1",
                    "--asset",
                    "1",
                ]],
            ),
            (
                "annotation expect with text",
                &[&[
                    "annotation",
                    "add",
                    "note",
                    "--expect",
                    "cam",
                    "--timeline",
                    "--at",
                    "1",
                    "--asset",
                    "1",
                ]],
            ),
            (
                "annotation expect a negative tolerance",
                &[&[
                    "annotation",
                    "add",
                    "--expect",
                    "cam",
                    "--timeline",
                    "--at",
                    "1",
                    "--asset",
                    "1",
                    "--tolerance-frames",
                    "-1",
                ]],
            ),
            (
                "annotation expect a margin out of range",
                &[&[
                    "annotation",
                    "add",
                    "--expect",
                    "work-in-frame",
                    "--timeline",
                    "--at",
                    "1",
                    "--margin",
                    "99",
                ]],
            ),
            (
                "annotation expect on a gap",
                &[&[
                    "annotation",
                    "add",
                    "--expect",
                    "cam",
                    "--timeline",
                    "--at",
                    "3.5",
                    "--asset",
                    "1",
                ]],
            ),
            (
                "annotation check a missing report",
                &[&["annotation", "check", "--report", "nope.mp4"]],
            ),
            ("report a missing output", &[&["report", "nope.mp4"]]),
            ("annotation rm 99", &[&["annotation", "rm", "99"]]),
            ("annotation resolve 99", &[&["annotation", "resolve", "99"]]),
            (
                "annotation add then rm",
                &[
                    &["annotation", "add", "note", "--asset", "1", "--time", "0"],
                    &["annotation", "rm", "1"],
                ],
            ),
            (
                "trim --out past the file",
                &[&["trim", "red.mp4", "--in", "0", "--out", "99", "-o", "t.mp4"]],
            ),
            (
                "trim inverted",
                &[&["trim", "red.mp4", "--in", "3", "--out", "1", "-o", "t.mp4"]],
            ),
        ],
    );
}

#[test]
fn a_corrupt_report_is_refused() {
    let s = Sandbox::new();
    s.write("bad.mp4.report.json", "{ not json");
    let out = s.run(&["report", "bad.mp4"]);
    assert!(!out.status.success(), "a corrupt report must fail");
    assert!(!merged(&out).contains("panicked"), "{}", merged(&out));
}

#[test]
fn odd_projects() {
    let s = Sandbox::new();
    // The untouched hostile base is the "still loads" reference below.
    s.hostile_base();
    s.ok_on("empty.json", &["init", "--force"]);

    let cases: &[(&str, &str, &[&str])] = &[
        (
            "split on an empty timeline",
            "empty.json",
            &["split", "--at", "1"],
        ),
        (
            "gap on an empty timeline",
            "empty.json",
            &["gap", "--at", "1", "--frames", "30"],
        ),
        (
            "export an empty timeline",
            "empty.json",
            &["export", "-o", "x.mp4"],
        ),
        ("seek an empty timeline", "empty.json", &["seek", "5"]),
        ("track ls an empty timeline", "empty.json", &["track", "ls"]),
        (
            "media rm on an empty pool",
            "empty.json",
            &["media", "rm", "1"],
        ),
    ];
    for &(label, project, argv) in cases {
        s.insist(label, project, argv);
    }

    s.write("bad.json", "{ not json");
    s.insist("show a corrupt project", "bad.json", &["show"]);
    s.write("old.json", r#"{"version": 3}"#);
    s.insist("show an old-version project", "old.json", &["show"]);
    s.insist("show a missing project", "nope.json", &["show"]);
    s.assert_loads("odd projects", "");
}

/// A transition as the project JSON stores it (externally tagged).
fn transition(
    dur: &str,
    kind: &str,
    a: serde_json::Value,
    b: serde_json::Value,
) -> serde_json::Value {
    json!({ "Transition": { "id": 9, "dur": dur, "kind": kind, "a": a, "b": b } })
}

fn source(asset: u32, from: &str, to: &str) -> serde_json::Value {
    json!({ "Clip": { "asset": asset, "src_in": from, "src_out": to } })
}

fn clip(id: u32, out: &str, speed: &str) -> serde_json::Value {
    json!({ "Clip": {
        "id": id, "asset": 1, "asset_in": "0/1", "asset_out": out,
        "speed": speed, "effects": [],
    }})
}

#[test]
fn hostile_projects() {
    let s = Sandbox::new();
    s.hostile_base();
    let base: serde_json::Value = serde_json::from_str(&s.read(P)).expect("the base project");

    // Rewrite the single video track's segments, optionally emptying the pool.
    let write = |name: &str, segments: serde_json::Value, empty_pool: bool| {
        let mut project = base.clone();
        project["sequences"][0]["timeline"]["tracks"][0]["segments"] = segments;
        if empty_pool {
            project["media"] = json!([]);
        }
        s.write(name, &serde_json::to_string_pretty(&project).expect("json"));
    };

    let black = json!("Black");
    write(
        "h_dur0.json",
        json!([transition(
            "0/1",
            "Dissolve",
            source(1, "0/1", "1/1"),
            black.clone()
        )]),
        false,
    );
    write(
        "h_span.json",
        json!([transition(
            "1/1",
            "Dissolve",
            source(1, "2/1", "1/1"),
            black.clone()
        )]),
        false,
    );
    write(
        "h_black.json",
        json!([transition("1/1", "Dissolve", black.clone(), black.clone())]),
        false,
    );
    write(
        "h_asset.json",
        json!([transition(
            "1/1",
            "Dissolve",
            source(99, "0/1", "1/1"),
            black.clone()
        )]),
        false,
    );
    write(
        "h_media.json",
        json!([transition(
            "1/1",
            "Dissolve",
            source(1, "4/1", "9/1"),
            black.clone()
        )]),
        false,
    );
    write(
        "h_kind.json",
        json!([transition("1/1", "Wipe", black.clone(), black)]),
        false,
    );
    write("h_speed0.json", json!([clip(1, "3/1", "0/1")]), false);
    write(
        "h_speed_tiny.json",
        json!([clip(1, "3/1", "1/9223372036854775807")]),
        false,
    );
    write("h_speed_neg.json", json!([clip(1, "3/1", "-1/1")]), false);
    write(
        "h_len_overflow.json",
        json!([
            clip(1, "9223372036854775807/1", "1/1"),
            clip(2, "9223372036854775807/1", "1/1")
        ]),
        false,
    );
    // Two tiny values whose exact sum overflows the rational's numerator.
    write(
        "h_len_frac.json",
        json!([
            clip(1, "1/9223372036854775807", "1/1"),
            clip(2, "1/9223372036854775806", "1/1")
        ]),
        false,
    );
    write(
        "h_empty_pool.json",
        json!([transition(
            "1/1",
            "Dissolve",
            source(1, "0/1", "1/1"),
            json!("Black")
        )]),
        true,
    );

    let cases: &[(&str, &str, &[&str])] = &[
        ("show a transition with dur 0", "h_dur0.json", &["show"]),
        (
            "show a transition with a reversed side",
            "h_span.json",
            &["show"],
        ),
        (
            "show a transition with no clip side",
            "h_black.json",
            &["show"],
        ),
        (
            "show a transition on a missing asset",
            "h_asset.json",
            &["show"],
        ),
        (
            "show a transition past its media",
            "h_media.json",
            &["show"],
        ),
        (
            "show a transition with an unknown kind",
            "h_kind.json",
            &["show"],
        ),
        ("show a clip with speed 0", "h_speed0.json", &["show"]),
        (
            "show a clip with a tiny speed",
            "h_speed_tiny.json",
            &["show"],
        ),
        (
            "show a clip with a negative speed",
            "h_speed_neg.json",
            &["show"],
        ),
        (
            "show a timeline whose length overflows",
            "h_len_overflow.json",
            &["show"],
        ),
        (
            "show a timeline of huge-denominator slivers",
            "h_len_frac.json",
            &["show"],
        ),
        (
            "show a transition with an empty pool",
            "h_empty_pool.json",
            &["show"],
        ),
        (
            "export a transition past its media",
            "h_media.json",
            &["export", "-o", "hx.mp4"],
        ),
        (
            "speed a project with a broken transition",
            "h_span.json",
            &["speed", "1", "200"],
        ),
    ];
    for &(label, project, argv) in cases {
        s.insist(label, project, argv);
    }
    s.assert_loads("hostile projects", "");
}

#[test]
fn hostile_sequence_shapes() {
    fn no_sequences(project: &mut serde_json::Value) {
        project["sequences"] = json!([]);
    }
    fn missing_active(project: &mut serde_json::Value) {
        project["active"] = json!(99);
    }
    fn duplicate_id(project: &mut serde_json::Value) {
        let mut copy = project["sequences"][0].clone();
        copy["id"] = json!(1);
        project["sequences"].as_array_mut().unwrap().push(copy);
    }
    fn zero_fps(project: &mut serde_json::Value) {
        project["sequences"][0]["fps"] = json!("0/1");
    }

    let s = Sandbox::new();
    type Case = (&'static str, fn(&mut serde_json::Value), &'static str);
    let cases: &[Case] = &[
        ("empty sequence list", no_sequences, "no sequences"),
        ("active id is missing", missing_active, "not found"),
        ("duplicate sequence ids", duplicate_id, "duplicated"),
        ("zero sequence fps", zero_fps, "non-positive"),
    ];
    for &(label, mutate, expected) in cases {
        s.hostile_base();
        let mut project: serde_json::Value =
            serde_json::from_str(&s.read(P)).expect("the base project");
        mutate(&mut project);
        s.write(P, &serde_json::to_string_pretty(&project).expect("json"));
        for argv in [&["show"][..], &["seek", "1"][..]] {
            let text = s.insist(label, P, argv);
            assert!(text.contains(expected), "[{label}] {argv:?}: {text}");
        }
    }
}

/// Argv is bytes on Unix, but a filename that is not UTF-8 is only half
/// portable: the string-flag half (clap) runs everywhere Unix, while a *real*
/// file at that name can only be created on Linux — macOS APFS rejects the
/// name with `EILSEQ`, so the raw-path property is tested on Linux alone.
/// Either way the bytes must never be rewritten lossily on the way in.
#[cfg(unix)]
#[test]
fn non_utf8_argv_is_refused_cleanly() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    let s = Sandbox::new();
    s.base();
    let bad = OsStr::from_bytes(b"bad-\xff.mp4");

    // A string flag: clap rejects the value as not UTF-8 (usage, exit 2)
    // instead of handing `to_action` a replacement character. No file is
    // needed, so this holds on every Unix, macOS included.
    let out = s.run_os(&[OsStr::new("split"), OsStr::new("--at"), bad]);
    let text = merged(&out);
    assert!(
        !text.contains("panicked"),
        "a non-UTF-8 --at panicked:\n{text}"
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "a non-UTF-8 --at is a clap usage error:\n{text}"
    );
    assert!(
        text.contains("invalid UTF-8"),
        "clap's refusal should name UTF-8:\n{text}"
    );
    s.assert_loads("non-UTF-8 --at", &text);
}

/// The path-flag half: `import` takes a `PathBuf`, so the raw bytes reach the
/// pool and the JSON save refuses to write them back. Linux-only because APFS
/// cannot store an invalid-UTF-8 filename, so the copy below fails with
/// `EILSEQ` on macOS.
#[cfg(target_os = "linux")]
#[test]
fn a_non_utf8_import_path_is_refused_at_save() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    let s = Sandbox::new();
    s.base();
    let bad = OsStr::from_bytes(b"bad-\xff.mp4");

    // A real file at the non-UTF-8 name: the import must get as far as the
    // project save, so the refusal names UTF-8 rather than a missing file.
    std::fs::copy(s.file("red.mp4"), s.path().join(bad)).expect("copy media to a non-UTF-8 name");

    let out = s.run_os(&[OsStr::new("import"), bad]);
    let text = merged(&out);
    assert!(
        !text.contains("panicked"),
        "a non-UTF-8 import panicked:\n{text}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "a non-UTF-8 import should be a clean refusal:\n{text}"
    );
    assert!(
        text.contains("invalid UTF-8"),
        "the import refusal should name UTF-8:\n{text}"
    );
    s.assert_loads("non-UTF-8 import", &text);
}

/// Clip ids are meant to be unique across tracks, but a hand-edited or
/// imported project can repeat one. `--clip N` must resolve deterministically
/// (the first track that has it) and edit only there.
#[test]
fn a_duplicate_clip_id_across_tracks_edits_the_first_track() {
    let s = Sandbox::new();
    s.hostile_base();
    let mut project: serde_json::Value =
        serde_json::from_str(&s.read(P)).expect("the base project");
    let first = project["sequences"][0]["timeline"]["tracks"][0].clone();
    let id = first["segments"][0]["Clip"]["id"]
        .as_u64()
        .expect("the first segment is a clip");
    project["sequences"][0]["timeline"]["tracks"]
        .as_array_mut()
        .expect("tracks")
        .push(first);
    s.write(P, &serde_json::to_string_pretty(&project).expect("json"));

    s.insist("duplicate clip id (show)", P, &["show"]);
    let id_arg = id.to_string();
    s.insist(
        "duplicate clip id (delete)",
        P,
        &["delete", "--clip", &id_arg],
    );
    s.assert_loads("duplicate clip id", "");

    let after: serde_json::Value = serde_json::from_str(&s.read(P)).expect("the project reloads");
    let tracks = after["sequences"][0]["timeline"]["tracks"]
        .as_array()
        .expect("tracks");
    assert_eq!(tracks.len(), 2, "the cloned track must still be there");
    let clip_ids = |track: &serde_json::Value| -> Vec<u64> {
        track["segments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|seg| seg.get("Clip"))
            .filter_map(|clip| clip["id"].as_u64())
            .collect()
    };
    assert!(
        !clip_ids(&tracks[0]).contains(&id),
        "track 0 still has clip {id}: {}",
        tracks[0]
    );
    assert!(
        clip_ids(&tracks[1]).contains(&id),
        "the edit did not land on the first-matching track 0; track 1 lost clip {id}: {}",
        tracks[1]
    );
}

/// Frame fields that JSON can express but no CLI edit writes: the rational
/// parser is the first gate (a negative is not a rate it accepts), and a huge
/// but positive fps must not overflow the frame-grid arithmetic.
#[test]
fn hostile_frame_fields() {
    fn negative_playhead(project: &mut serde_json::Value) {
        project["sequences"][0]["playhead"] = json!("-3/1");
    }
    fn negative_source_span(project: &mut serde_json::Value) {
        let clip = &mut project["sequences"][0]["timeline"]["tracks"][0]["segments"][0]["Clip"];
        clip["asset_in"] = json!("-3/1");
        clip["asset_out"] = json!("-1/1");
    }
    // i64::MAX is the extreme the old `last_frame` subtraction overflowed
    // `Rational64` on. The i128 math in the `last_frame`/clamp fix keeps the
    // save path finite, so the battery can hold the real extreme.
    fn absurd_fps(project: &mut serde_json::Value) {
        project["sequences"][0]["fps"] = json!("9223372036854775807/1");
    }

    let s = Sandbox::new();
    type Refusal = (&'static str, fn(&mut serde_json::Value), &'static str);
    let refused: &[Refusal] = &[
        (
            "negative playhead",
            negative_playhead,
            "invalid rational time",
        ),
        (
            "negative source span",
            negative_source_span,
            "invalid rational time",
        ),
    ];
    for &(label, mutate, expected) in refused {
        s.hostile_base();
        let mut project: serde_json::Value =
            serde_json::from_str(&s.read(P)).expect("the base project");
        mutate(&mut project);
        s.write(P, &serde_json::to_string_pretty(&project).expect("json"));
        for argv in [&["show"][..], &["seek", "1"][..]] {
            let text = s.insist(label, P, argv);
            assert!(text.contains(expected), "[{label}] {argv:?}: {text}");
        }
    }

    // An absurd but positive rate is accepted: `show` reads it, and `seek`
    // saves through the `last_frame` clamp. Both must succeed — no panic and a
    // finite playhead on disk — not merely refuse cleanly.
    s.hostile_base();
    let mut project: serde_json::Value =
        serde_json::from_str(&s.read(P)).expect("the base project");
    absurd_fps(&mut project);
    s.write(P, &serde_json::to_string_pretty(&project).expect("json"));
    let shown = s.ok(&["show"]);
    let saved = s.ok(&["seek", "1"]);
    // "Finite" means the saved playhead is a real position inside the
    // timeline, not just a value that happens to serialize.
    let loaded =
        nle_core::Project::load(&s.file(P)).expect("the saving seek left a loadable project");
    assert!(
        loaded.playhead >= nle_core::Seconds::from_integer(0)
            && loaded.playhead <= loaded.timeline.end(),
        "saved playhead {} is outside [0, {}]",
        nle_core::format_seconds(&loaded.playhead),
        nle_core::format_seconds(&loaded.timeline.end())
    );
    s.assert_loads("absurd fps", &format!("{shown}\n{saved}"));
}

#[test]
fn a_truncated_cache_entry_is_rerendered() {
    let s = Sandbox::new();
    s.base();
    // Render a proxy (the effect clip) and the black filler (the timeline has
    // no gap — so add one first, or the black file is never made).
    s.ok(&["gap", "--at", "2", "--frames", "15"]);
    s.ok(&["effect", "eq", "--clip", "1", "--saturation", "0"]);
    s.ok(&["edl", "--track", "1"]);
    let cached = s.cached_mp4s();
    assert!(
        cached.len() >= 2,
        "expected a proxy and a black filler, cached {:?}",
        cached
    );
    // A stub left behind by a full disk must not be served forever: the hash
    // never changes, so nothing would re-render it.
    for file in &cached {
        fs::write(file, b"").expect("truncate a cache entry");
    }
    s.ok(&["edl", "--track", "1"]);
    for file in s.cached_mp4s() {
        let len = fs::metadata(&file).expect("a cache entry").len();
        assert!(len > 0, "{} is still empty", file.display());
    }
}

#[test]
fn output_plumbing() {
    let s = Sandbox::new();
    s.base();
    // A failed stdout write (a full disk) must exit cleanly. `/dev/full` is
    // Linux-only; the closed-pipe half below covers EPIPE on every platform.
    #[cfg(target_os = "linux")]
    {
        let sink = fs::File::create("/dev/full").expect("/dev/full");
        let status = s.run_with_stdout(&["media", "ls"], Stdio::from(sink));
        assert!(
            status.success(),
            "a failed stdout write crashed: {status:?}"
        );
    }
    // …and an early-closed pipe must not crash it either.
    let status = s.run_with_closed_pipe(&["media", "ls"]);
    assert!(
        status.code().is_some_and(|code| code <= 2),
        "a broken pipe crashed the CLI ({status:?})"
    );
}

// The diagnostic path runs without a monitor, but it still creates a scratch
// copy. A TMPDIR it cannot use must be refused cleanly (1), never panicked
// (101). Non-unix `temp_dir()` ignores TMPDIR, so this only means something
// on unix.
#[cfg(all(unix, feature = "tui"))]
#[test]
fn an_unwritable_tmpdir_cannot_panic_the_dump() {
    let s = Sandbox::new();
    s.base();
    let blocker = s.file("not-a-dir");
    fs::write(&blocker, b"x").expect("a regular file is not a directory");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nile"))
        .current_dir(s.path())
        .args(["--project", P, "edit", "--dump"])
        .env("TMPDIR", blocker.join("child"))
        .output()
        .expect("spawn nile");
    let code = out.status.code().expect("an exit code");
    assert_eq!(code, 1, "a bad TMPDIR is a clean refusal: {}", merged(&out));
    assert!(
        !merged(&out).contains("panicked"),
        "a diagnostic must not panic: {}",
        merged(&out)
    );
}
