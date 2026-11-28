//! The hostile battery: whatever the CLI is handed it must
//! - not panic (a crash is always a bug),
//! - exit 0 (accepted), 1 (refused with a message) or 2 (usage error),
//! - leave a project that still loads with a finite, non-negative length.
//!
//! These are the old `scripts/fuzz.sh` cases. The invariant cases are
//! wording-agnostic — only whether the command held the invariant matters, so a
//! deliberate message change never touches this file. Three cases are
//! exceptions, each pinning typed text: the two exhausted-id refusals
//! (`no clip id left…`, `no asset id left…`), and
//! `a_duplicate_clip_id_across_tracks_refuses_load_before_any_edit`, which
//! deliberately hands in an invalid project: it pins `sequence #N: segment #M
//! is duplicated` and asserts the refused load leaves the file byte-identical
//! (so that case leaves a project that does not load, by design).

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

/// The 7 s asset is coprime with 86400: at this rate the old `snap` computed
/// `7 * i64::MAX` inside `Ratio<i64>` and panicked. The reconstruction now
/// keeps the unsnapped position, so the command succeeds and the project
/// still loads with the whole 7 s. (A 5 s asset reduces and hides the bug.)
#[test]
fn detect_edits_survives_an_extreme_frame_rate() {
    let s = Sandbox::new();
    let out = s.run(&[
        "detect-edits",
        "seven.mp4",
        "--force",
        "--fps",
        "9223372036854775807/86400",
    ]);
    let text = merged(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(!text.contains("panicked"), "{text}");
    s.assert_loads("detect-edits at an extreme frame rate", &text);
    let project = nle_core::Project::load(&s.file(P)).expect("the project loads");
    assert_eq!(
        project.timeline.end(),
        nle_core::Seconds::from_integer(7),
        "the full shot is reconstructed"
    );
}

#[test]
fn odd_media() {
    let s = Sandbox::new();
    s.write(
        "bad.edl",
        "TITLE: bad\nFCM: NON-DROP FRAME\n\n001 AX V C 00:00:00:00 nope\n",
    );
    s.write(
        "unknown.edl",
        "TITLE: unknown\nFCM: NON-DROP FRAME\n\n\
         001  AX  X  C  00:00:00:00 00:00:01:00 01:00:00:00 01:00:01:00\n",
    );
    s.write(
        "one.edl",
        "TITLE: one\nFCM: NON-DROP FRAME\n\n\
         001  AX  V  C  00:00:00:00 00:00:01:00 01:00:00:00 01:00:01:00\n\
         * FROM CLIP NAME: red.mp4\n",
    );
    // A two-second record gap after event 1: at an extreme (but accepted)
    // frame rate its frame count has no exact `Ratio<i64>` seconds.
    s.write(
        "gap.edl",
        "TITLE: gap\nFCM: NON-DROP FRAME\n\n\
         001  AX  V  C  00:00:00:00 00:00:01:00 01:00:00:00 01:00:01:00\n\
         * FROM CLIP NAME: red.mp4\n\
         002  AX  V  C  00:00:00:00 00:00:01:00 01:00:03:00 01:00:04:00\n\
         * FROM CLIP NAME: red.mp4\n",
    );
    // A 99-hour timecode: at an extreme (but accepted) frame rate its frame
    // count overflows i64 before the timeline is built.
    s.write(
        "long.edl",
        "TITLE: long\nFCM: NON-DROP FRAME\n\n\
         001  AX  V  C  99:00:00:00 99:00:01:00 99:00:00:00 99:00:01:00\n\
         * FROM CLIP NAME: red.mp4\n",
    );
    s.write("bad.otio", "not json");
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
                "detect-edits at an unrepresentable frame rate",
                &[&[
                    "detect-edits",
                    "seven.mp4",
                    "--force",
                    "--fps",
                    "9223372036854775807/86400",
                ]],
            ),
            (
                "import-edl at an unrepresentable frame rate",
                &[&[
                    "import-edl",
                    "gap.edl",
                    "--media",
                    ".",
                    "--force",
                    "--fps",
                    "9223372036854775807/86400",
                ]],
            ),
            (
                "import-edl a 99-hour timecode at an unrepresentable frame rate",
                &[&[
                    "import-edl",
                    "long.edl",
                    "--media",
                    ".",
                    "--force",
                    "--fps",
                    "9223372036854775807/86400",
                ]],
            ),
            (
                "import-edl a 99-hour --src-offset at an unrepresentable frame rate",
                &[&[
                    "import-edl",
                    "one.edl",
                    "--media",
                    ".",
                    "--force",
                    "--fps",
                    "9223372036854775807/86400",
                    "--src-offset",
                    "99:00:00:00",
                ]],
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
                "import-edl append a malformed EDL",
                &[&["import-edl", "bad.edl", "--media", ".", "--append"]],
            ),
            (
                "import-edl append an unknown channel",
                &[&["import-edl", "unknown.edl", "--media", ".", "--append"]],
            ),
            (
                "import-edl append --track 0",
                &[&[
                    "import-edl",
                    "one.edl",
                    "--media",
                    ".",
                    "--append",
                    "--track",
                    "0",
                ]],
            ),
            (
                "import-edl append a bad size",
                &[&[
                    "import-edl",
                    "one.edl",
                    "--media",
                    ".",
                    "--append",
                    "--size",
                    "0x0",
                ]],
            ),
            (
                "import-edl append and force",
                &[&[
                    "import-edl",
                    "unknown.edl",
                    "--media",
                    ".",
                    "--append",
                    "--force",
                ]],
            ),
            (
                "import-otio append malformed JSON",
                &[&["import-otio", "bad.otio", "--append"]],
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

/// A project saved while its media was valid can be reopened after that file
/// becomes 0 bytes (a truncated recording, a failed copy). Emptying the saved
/// path rather than repointing `media[0]` keeps the asset's identity, cache
/// and anchoring in play; `show` and `seek` still read the model, but `export`
/// must refuse and leave no render — the add/import probe is not the only path
/// to a zero-byte asset.
#[test]
fn a_saved_project_with_zero_byte_media_is_refused_cleanly() {
    let s = Sandbox::new();
    s.local_base(); // init + import red/green/blue + three clips
    fs::write(s.file("red.mp4"), b"").expect("truncate red.mp4");
    assert_eq!(
        fs::metadata(s.file("red.mp4")).expect("red.mp4").len(),
        0,
        "the saved asset is now 0 bytes"
    );

    for argv in [&["show"][..], &["seek", "1"][..]] {
        let out = s.run(argv);
        let text = merged(&out);
        assert!(!text.contains("panicked"), "{argv:?} panicked:\n{text}");
        assert_eq!(
            out.status.code(),
            Some(0),
            "{argv:?} still reads the model:\n{text}"
        );
    }

    let export = s.run(&["export", "-o", "z.mp4"]);
    let text = merged(&export);
    assert!(!text.contains("panicked"), "export panicked:\n{text}");
    assert_eq!(
        export.status.code(),
        Some(1),
        "export refuses zero-byte media:\n{text}"
    );
    assert!(!s.file("z.mp4").exists(), "the failed export left a file");
    s.assert_loads("zero-byte media", &text);
}

#[test]
fn import_append_requires_an_existing_project() {
    let s = Sandbox::new();
    s.write(
        "e.edl",
        "TITLE: e\nFCM: NON-DROP FRAME\n\n\
         001  AX  V  C  00:00:00:00 00:00:01:00 01:00:00:00 01:00:01:00\n\
         * FROM CLIP NAME: red.mp4\n",
    );
    s.write(
        "e.otio",
        r#"{ "OTIO_SCHEMA": "Timeline.1", "tracks": { "OTIO_SCHEMA": "Stack.1", "children": [] } }"#,
    );
    for argv in [
        &["import-edl", "e.edl", "--media", ".", "--append"][..],
        &["import-otio", "e.otio", "--append"][..],
    ] {
        let out = s.run_on("missing.json", argv);
        let text = merged(&out);
        assert!(!text.contains("panicked"), "{argv:?}:\n{text}");
        assert_eq!(out.status.code(), Some(1), "{argv:?}:\n{text}");
        assert!(
            text.contains("--append needs an existing project"),
            "{argv:?}:\n{text}"
        );
        assert!(!s.file("missing.json").exists(), "no project was written");
    }
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

/// The global `--project` is a path, so a raw-byte spelling must behave like
/// any other symlink to the project: commands and undo land on the real
/// `p.json`, and `state_dir`'s `canonicalize` keeps history and the register
/// under `.nile/p/` — never a lossy directory beside it. Linux-only because
/// APFS cannot store an invalid-UTF-8 name.
#[cfg(target_os = "linux")]
#[test]
fn a_non_utf8_project_path_resolves_to_the_real_state_dir() {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStrExt as _;

    let s = Sandbox::new();
    s.base();
    let bad = OsStr::from_bytes(b"bad-\xff.json");
    std::os::unix::fs::symlink("p.json", s.path().join(bad)).expect("a raw-byte symlink");
    let before: serde_json::Value = serde_json::from_str(&s.read(P)).expect("the base project");

    let show = s.run_os_on(bad, &[OsStr::new("show")]);
    let text = merged(&show);
    assert!(!text.contains("panicked"), "{text}");
    assert_eq!(
        show.status.code(),
        Some(0),
        "show through the raw name:\n{text}"
    );

    let split = s.run_os_on(
        bad,
        &[OsStr::new("split"), OsStr::new("--at"), OsStr::new("1")],
    );
    let text = merged(&split);
    assert!(!text.contains("panicked"), "{text}");
    assert_eq!(
        split.status.code(),
        Some(0),
        "split through the raw name:\n{text}"
    );
    let split_project: serde_json::Value =
        serde_json::from_str(&s.read(P)).expect("the split project");
    assert_ne!(
        split_project["sequences"][0]["timeline"]["tracks"],
        before["sequences"][0]["timeline"]["tracks"],
        "the split wrote the real p.json"
    );
    let history = s.path().join(".nile").join("p").join("history.json");
    assert!(
        history.exists(),
        "history landed under .nile/p: {history:?}"
    );

    let undo = s.run_os_on(bad, &[OsStr::new("undo")]);
    let text = merged(&undo);
    assert!(!text.contains("panicked"), "{text}");
    assert_eq!(
        undo.status.code(),
        Some(0),
        "undo through the raw name:\n{text}"
    );
    let after: serde_json::Value = serde_json::from_str(&s.read(P)).expect("the undone project");
    assert_eq!(
        after["sequences"][0]["timeline"]["tracks"], before["sequences"][0]["timeline"]["tracks"],
        "undo restored the real p.json"
    );

    let mut state: Vec<OsString> = fs::read_dir(s.path().join(".nile"))
        .expect(".nile")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    state.sort();
    assert_eq!(
        state,
        vec![OsString::from("p")],
        "state lives only under .nile/p, not a lossy copy"
    );
    s.assert_loads("non-UTF-8 --project", &text);
}

/// The path-flag half: `import` takes a `PathBuf`, but `nile.json` stores
/// paths as UTF-8, so a non-UTF-8 path must be refused *before* probing — no
/// asset line, no project or history write, and no half-import of a valid
/// sibling. Linux-only because APFS cannot store an invalid-UTF-8 filename,
/// so the copy below fails with `EILSEQ` on macOS.
#[cfg(target_os = "linux")]
#[test]
fn a_non_utf8_import_path_is_refused_before_any_write() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    let s = Sandbox::new();
    s.base();
    let bad = OsStr::from_bytes(b"bad-\xff.mp4");

    // A real file at the non-UTF-8 name: the refusal must come from the path
    // validation, not from a missing file or the save.
    std::fs::copy(s.file("red.mp4"), s.path().join(bad)).expect("copy media to a non-UTF-8 name");
    let before = s.read(P);
    let history = s.path().join(".nile").join("p").join("history.json");
    assert!(!history.exists(), "the fixture starts without history");

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
        !text.contains('✓'),
        "no asset may be reported before the refusal:\n{text}"
    );
    assert!(
        text.contains("UTF-8"),
        "the import refusal should name UTF-8:\n{text}"
    );
    assert!(
        text.contains(bad.to_string_lossy().as_ref()),
        "the refusal should name the lossy display:\n{text}"
    );
    assert_eq!(s.read(P), before, "a refused import wrote the project");
    assert!(!history.exists(), "a refused import wrote history");
    s.assert_loads("non-UTF-8 import", &text);

    // A valid sibling (`wide169.mp4` is not in the base pool) must not be
    // imported either: the batch is refused as a whole, before probing.
    let out = s.run_os(&[OsStr::new("import"), OsStr::new("wide169.mp4"), bad]);
    let text = merged(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "the batch with a non-UTF-8 path should be refused:\n{text}"
    );
    assert!(
        !text.contains('✓'),
        "the valid sibling must not be reported:\n{text}"
    );
    assert_eq!(
        s.read(P),
        before,
        "the valid sibling must not be half-imported:\n{text}"
    );
    assert!(!history.exists(), "a refused batch wrote history");
    let assets = s.ok(&["media", "ls"]);
    assert!(
        !assets.contains("wide169.mp4"),
        "the valid sibling entered the pool:\n{assets}"
    );
    s.assert_loads("non-UTF-8 batch", &text);
}

/// The EDL/OTIO `--media` scan pools files before building the timeline, so a
/// non-UTF-8 name there must be refused before *any* probe — otherwise it
/// half-pools and only the save complains. Linux-only for the same APFS
/// reason as the import case above.
#[cfg(target_os = "linux")]
#[test]
fn a_non_utf8_media_scan_is_refused_before_pooling() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    let s = Sandbox::new();
    s.base();
    let bad = OsStr::from_bytes(b"bad-\xff.mp4");
    std::fs::copy(s.file("red.mp4"), s.path().join(bad)).expect("copy media to a non-UTF-8 name");
    s.write(
        "e.edl",
        "TITLE: t\nFCM: NON-DROP FRAME\n\n\
         001  AX  V  C  00:00:00:00 00:00:01:00 01:00:00:00 01:00:01:00\n\
         * FROM CLIP NAME: red.mp4\n",
    );
    let before = s.read(P);
    let history = s.path().join(".nile").join("p").join("history.json");
    assert!(!history.exists(), "the fixture starts without history");

    let out = s.run(&["import-edl", "e.edl", "--media", ".", "--force"]);
    let text = merged(&out);
    assert!(
        !text.contains("panicked"),
        "a non-UTF-8 media scan panicked:\n{text}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "a non-UTF-8 media scan should be a clean refusal:\n{text}"
    );
    assert!(
        !text.contains('✓'),
        "no asset may be reported before the refusal:\n{text}"
    );
    assert!(
        text.contains("UTF-8"),
        "the media-scan refusal should name UTF-8:\n{text}"
    );
    assert!(
        text.contains(bad.to_string_lossy().as_ref()),
        "the refusal should name the lossy display:\n{text}"
    );
    assert_eq!(s.read(P), before, "a refused media scan wrote the project");
    assert!(!history.exists(), "a refused media scan wrote history");
    s.assert_loads("non-UTF-8 --media", &text);
}

/// `add` auto-imports into the pool, so it stages an asset before the edit.
/// A non-UTF-8 path must be refused before that stage: no clip, no
/// `auto-imported` line, no project or history write. Linux-only for the same
/// APFS reason as the import case above.
#[cfg(target_os = "linux")]
#[test]
fn a_non_utf8_add_path_is_refused_before_auto_import() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    let s = Sandbox::new();
    s.base();
    let bad = OsStr::from_bytes(b"bad-\xff.mp4");
    std::fs::copy(s.file("red.mp4"), s.path().join(bad)).expect("copy media to a non-UTF-8 name");
    let before = s.read(P);
    let history = s.path().join(".nile").join("p").join("history.json");
    assert!(!history.exists(), "the fixture starts without history");

    let out = s.run_os(&[OsStr::new("add"), bad]);
    let text = merged(&out);
    assert!(
        !text.contains("panicked"),
        "a non-UTF-8 add panicked:\n{text}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "a non-UTF-8 add should be a clean refusal:\n{text}"
    );
    assert!(
        !text.contains('✓'),
        "no clip may be reported before the refusal:\n{text}"
    );
    assert!(
        !text.contains("auto-imported"),
        "the asset must not be auto-imported:\n{text}"
    );
    assert!(
        text.contains("UTF-8"),
        "the add refusal should name UTF-8:\n{text}"
    );
    assert!(
        text.contains(bad.to_string_lossy().as_ref()),
        "the refusal should name the lossy display:\n{text}"
    );
    assert_eq!(s.read(P), before, "a refused add wrote the project");
    assert!(!history.exists(), "a refused add wrote history");
    s.assert_loads("non-UTF-8 add", &text);
}

/// Clip ids are unique across every track of a sequence, so a hand-edited
/// duplicate is refused at load: `--clip N` would otherwise be ambiguous.
#[test]
fn a_duplicate_clip_id_across_tracks_refuses_load_before_any_edit() {
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
    let text = serde_json::to_string_pretty(&project).expect("json");
    s.write(P, &text);

    let shown = s.insist("duplicate clip id (show)", P, &["show"]);
    let id_arg = id.to_string();
    let edited = s.insist(
        "duplicate clip id (delete)",
        P,
        &["delete", "--clip", &id_arg],
    );
    for output in [&shown, &edited] {
        assert!(
            output.contains(&format!("sequence #1: segment #{id} is duplicated")),
            "the refusal must name the duplicated sequence/segment id: {output}"
        );
    }
    assert_eq!(s.read(P), text, "a refused load does not rewrite the file");
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
    // A rate finer than `Ratio<i64>` can encode a whole second: `snap` and
    // the frame→seconds helpers must refuse, not panic.
    fn overflow_fps(project: &mut serde_json::Value) {
        project["sequences"][0]["fps"] = json!("9223372036854775807/86400");
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
    // Edit ops cross the frame grid; at an extreme rate they must refuse
    // cleanly (exit 1) or land — never panic. `insist` is the invariant.
    for argv in [
        &["gap", "--at", "1", "--frames", "1"][..],
        &["split", "--at", "1.5"][..],
        &["speed", "1", "200"][..],
        &["transition", "--at", "3", "--frames", "1"][..],
        &["yank", "--seg", "0"][..],
        &["paste", "--at", "1"][..],
    ] {
        s.insist("absurd fps edit", P, argv);
    }
    s.assert_loads("absurd fps", &format!("{shown}\n{saved}"));

    // An extreme rate's `snap` saturates `t >= 1` to 1 s, so the seam has to
    // be one it can address: at 0 the right clip's head trim leaves a
    // duration (`2 - 1/i64::MAX`) whose exact prefix sum the rational can't
    // hold. The transition must refuse in `track_shift` (the timeline-limit
    // `TooLong`), not panic in `Track::start_of`.
    let t = Sandbox::new();
    t.local_base();
    let mut project: serde_json::Value =
        serde_json::from_str(&t.read(P)).expect("the local base project");
    absurd_fps(&mut project);
    t.write(P, &serde_json::to_string_pretty(&project).expect("json"));
    let text = t.insist(
        "absurd fps track-start transition",
        P,
        &["transition", "--at", "0", "--frames", "1"],
    );
    assert!(
        text.contains("longer than"),
        "the transition must reach the typed TooLong refusal, not \
         `nothing to trim`:\n{text}"
    );
    t.assert_loads("absurd fps track-start transition", "");

    // A whole second at a rate whose frame is finer than `Ratio<i64>` can
    // encode: every frame→seconds crossing (`snap`, the clamp's frame grid,
    // `grid_time`) must refuse or fall back, never panic.
    s.hostile_base();
    let mut project: serde_json::Value =
        serde_json::from_str(&s.read(P)).expect("the base project");
    overflow_fps(&mut project);
    s.write(P, &serde_json::to_string_pretty(&project).expect("json"));
    for argv in [
        &["show"][..],
        &["seek", "1"][..],
        &["split", "--at", "1"][..],
        &["yank", "--seg", "0"][..],
        &["gap", "--at", "0", "--frames", "9223372036854775806"][..],
    ] {
        s.insist("overflow fps", P, argv);
    }
    s.assert_loads("overflow fps", "");

    // A tiny positive rate is the inverse extreme: `dur.den * rate.den`
    // overflows the frame-grid product, so the checked comparison must not
    // fall back to a raw multiply.
    s.hostile_base();
    let mut project: serde_json::Value =
        serde_json::from_str(&s.read(P)).expect("the base project");
    project["sequences"][0]["fps"] = json!("1/9223372036854775807");
    s.write(P, &serde_json::to_string_pretty(&project).expect("json"));
    for argv in [
        &["speed", "1", "200"][..],
        &["gap", "--at", "1", "--frames", "1"][..],
        &["split", "--at", "1.5"][..],
    ] {
        s.insist("tiny fps edit", P, argv);
    }
    s.assert_loads("tiny fps edits", "");
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

#[test]
fn analysis_import_refuses_bad_input() {
    let s = Sandbox::new();
    s.base();
    s.write(
        "take.json",
        r#"{"words":[{"text":"take","start":"0/1","end":"1/1"}]}"#,
    );
    s.write("bad.json", "{");
    s.write(
        "late.json",
        r#"{"words":[{"text":"late","start":"0/1","end":"99/1"}]}"#,
    );
    let cases: &[(&str, &[&str])] = &[
        (
            "bad json",
            &["analysis", "import", "--asset", "1", "bad.json"],
        ),
        (
            "missing file",
            &["analysis", "import", "--asset", "1", "missing.json"],
        ),
        (
            "missing asset",
            &["analysis", "import", "--asset", "99", "take.json"],
        ),
        (
            "word past the asset",
            &["analysis", "import", "--asset", "1", "late.json"],
        ),
    ];
    for &(label, argv) in cases {
        let out = s.run(argv);
        let text = merged(&out);
        assert_eq!(out.status.code(), Some(1), "{label}: {text}");
        assert!(!text.contains("panicked"), "{label}: {text}");
    }
    s.assert_loads("analysis import garbage", "");
}

/// The hidden analyzer is a process boundary, so garbage on its stdin must be
/// answered or refused, never a panic. A malformed line gets a typed
/// `bad_request`; an over-cap line kills it before it can allocate the rest.
#[test]
fn the_analysis_analyzer_survives_malformed_and_oversized_lines() {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    fn spawn() -> std::process::Child {
        Command::new(env!("CARGO_BIN_EXE_nile"))
            .arg("analysis-analyzer")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn analysis-analyzer")
    }

    let mut child = spawn();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{ not a json object\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text = merged(&out);
    assert!(!text.contains("panicked"), "{text}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("bad_request"),
        "{text}"
    );

    let mut child = spawn();
    // One byte past the 1 MiB cap, no newline until the end.
    let huge = vec![b'a'; 1024 * 1024 + 1];
    let _ = child.stdin.as_mut().unwrap().write_all(&huge);
    let _ = child.stdin.as_mut().unwrap().write_all(b"\n");
    let out = child.wait_with_output().unwrap();
    let text = merged(&out);
    assert!(!text.contains("panicked"), "{text}");
    assert_eq!(
        out.status.code(),
        Some(1),
        "an over-cap line is fatal: {text}"
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

// The last legal id is `u32::MAX - 1`; `u32::MAX` is reserved. Both the
// detect and import paths mirror the project allocator, so a hand-edited
// project at the ceiling must refuse in the ordinary 0/1/2 range, never panic
// the process (101) or wrap an id to 0.
fn set_first_clip_id(project: &mut nle_core::Project, id: u32) {
    for track in &mut project.timeline.tracks {
        for segment in &mut track.segments {
            if let nle_core::timeline::Segment::Clip(clip) = segment {
                clip.id = id;
                return;
            }
        }
    }
    panic!("the fixture has no clip");
}

#[test]
fn an_exhausted_clip_id_refuses_detect_append_without_panicking() {
    let s = Sandbox::new();
    let last = u32::MAX - 1;
    s.hostile_base();
    let path = s.file(P);
    let mut project = nle_core::Project::load(&path).expect("the hostile base loads");
    set_first_clip_id(&mut project, last);
    project
        .save(&path)
        .expect("the last legal clip id still saves");

    let out = s.run(&["detect-edits", "red.mp4", "--append"]);
    let text = merged(&out);
    let code = out.status.code().expect("an exit code");
    assert!(
        (0..=2).contains(&code),
        "an exhausted clip id must be a clean refusal, got {code}:\n{text}"
    );
    assert!(!text.contains("panicked"), "{text}");
    assert!(
        text.contains(&format!("no clip id left at #{last}")),
        "the refusal must name the exhausted clip id, not a missing probe: {text}"
    );
    s.assert_loads("detect-edits append at the last clip id", &text);
}

#[test]
fn an_exhausted_asset_pool_refuses_import_without_panicking() {
    let s = Sandbox::new();
    let last = u32::MAX - 1;
    s.ok(&["init"]);
    s.ok(&["import", "red.mp4"]);
    let path = s.file(P);
    let mut project = nle_core::Project::load(&path).expect("the pool loads");
    project.media[0].id = last;
    project
        .save(&path)
        .expect("the last legal asset id still saves");

    let out = s.run(&["import", "green.mp4", "blue.mp4"]);
    let text = merged(&out);
    let code = out.status.code().expect("an exit code");
    assert!(
        (0..=2).contains(&code),
        "an exhausted asset pool must be a clean refusal, got {code}:\n{text}"
    );
    assert!(!text.contains("panicked"), "{text}");
    assert!(
        text.contains(&format!("no asset id left at #{last}")),
        "the refusal must name the exhausted asset id, not a missing file: {text}"
    );
    s.assert_loads("import at the last asset id", &text);
}
