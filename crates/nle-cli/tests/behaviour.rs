//! The expected-behaviour battery: normal workflows, checked against the
//! *result* — exported pixels, durations, frame counts, structure. These are
//! the old `scripts/expect.sh` sections.
//!
//! Each test builds its own sandbox (see `common`), so they run in parallel.
//! On failure the assertion names the expectation in its message.

mod common;

use std::fs;

use common::*;

#[test]
fn baseline_export() {
    let s = Sandbox::new();
    s.base();
    s.export("b.mp4");
    s.assert_px("b.mp4", 1.0, RED, "red 0–2");
    s.assert_px("b.mp4", 3.0, GREEN, "green 2–4");
    s.assert_px("b.mp4", 5.0, BLUE, "blue 4–6");
    s.assert_dur("b.mp4", 6.0, "6 s");
}

#[test]
fn two_sequences_export_at_their_own_sizes() {
    let s = Sandbox::new();
    s.base(); // sequence 1: 320x240, three clips

    s.ok(&[
        "sequence", "add", "Vertical", "--width", "1080", "--height", "1920", "--fps", "30/1",
    ]);
    let list = s.ok(&["sequence", "ls"]);
    assert!(list.contains("* #1"), "{list}");
    assert!(list.contains("#2  Vertical"), "{list}");
    assert!(list.contains("1080×1920"), "{list}");

    s.ok(&["sequence", "use", "2"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["export", "-o", "vertical.mp4"]);
    assert_eq!(
        s.size("vertical.mp4"),
        (1080, 1920),
        "the active sequence's size"
    );

    s.ok(&["export", "--sequence", "1", "-o", "landscape.mp4"]);
    assert_eq!(
        s.size("landscape.mp4"),
        (320, 240),
        "--sequence picks the other timeline without switching"
    );
    // The explicit export did not change the stored active sequence.
    assert!(s.ok(&["sequence", "ls"]).contains("* #2"));
    assert!(s.ok(&["show"]).contains("#2 Vertical"), "{}", s.show());
    assert!(s.ok(&["show", "--sequence", "1"]).contains("320×240"));
}

#[test]
fn sequence_add_rejects_a_non_positive_frame_rate() {
    let s = Sandbox::new();
    s.base();
    let out = s.run(&["sequence", "add", "Bad", "--fps", "0"]);
    assert!(!out.status.success(), "--fps 0 must be refused");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("positive"), "{text}");
    assert!(!text.contains("panicked"), "{text}");
    assert_eq!(s.ok(&["sequence", "ls"]).matches("#").count(), 1);
    // A zero-rate property in the file is rejected on load, not a later panic.
    let mut project: serde_json::Value = serde_json::from_str(&s.ok(&["show", "--json"])).unwrap();
    project["sequences"][0]["fps"] = serde_json::json!("0/1");
    s.write(P, &serde_json::to_string_pretty(&project).unwrap());
    for argv in [&["show"][..], &["seek", "1"][..]] {
        let out = s.run(argv);
        assert!(!out.status.success(), "{argv:?} must reject the file");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(text.contains("non-positive"), "{argv:?}: {text}");
        assert!(!text.contains("panicked"), "{argv:?}: {text}");
    }
}

#[test]
fn sequence_audio_settings_round_trip_and_undo() {
    let s = Sandbox::new();
    s.base();

    // Defaults are off/zero, and reading them is a no-op command.
    let shown = s.ok(&["sequence", "audio"]);
    assert!(shown.contains("normalize off"), "{shown}");
    assert!(shown.contains("fade-in 0 s"), "{shown}");

    s.ok(&[
        "sequence",
        "audio",
        "--normalize",
        "--fade-in",
        "0.5",
        "--fade-out",
        "1",
    ]);
    let project: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    assert_eq!(project["sequences"][0]["audio"]["normalize"], true);
    assert_eq!(project["sequences"][0]["audio"]["fade_in"], "1/2");
    assert_eq!(project["sequences"][0]["audio"]["fade_out"], "1/1");
    assert!(
        s.ok(&["sequence", "ls"]).contains("audio normalize on"),
        "the listing exposes the setting"
    );
    assert!(
        s.show().contains("audio normalize on"),
        "the show header exposes the setting:\n{}",
        s.show()
    );

    // One undo restores defaults, redo brings the setting back.
    s.ok(&["undo"]);
    let project: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    assert_eq!(project["sequences"][0]["audio"]["normalize"], false);
    assert_eq!(project["sequences"][0]["audio"]["fade_in"], "0/1");
    s.ok(&["redo"]);
    let project: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    assert_eq!(project["sequences"][0]["audio"]["normalize"], true);

    // A negative fade is refused by the time parser, not clamped silently.
    let out = s.run(&["sequence", "audio", "--fade-in", "-1"]);
    assert!(!out.status.success(), "--fade-in -1 must be refused");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("fade-in"), "{text}");
    assert!(!text.contains("panicked"), "{text}");
}

#[test]
fn show_json_does_not_rewrite_the_active_sequence() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["sequence", "add", "Second"]);
    let before: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    let out = s.run(&["show", "--json", "--sequence", "1"]);
    assert!(!out.status.success(), "--json and --sequence conflict");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("omit --sequence"), "{text}");
    let after: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    assert_eq!(before, after, "the rejected command changed the file");
}

#[test]
fn import_group_creates_one_undoable_recording() {
    let s = Sandbox::new();
    s.ok(&["init"]);
    s.ok(&["import", "--group", "CAM 3", "red.mp4", "green.mp4"]);
    let project: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    assert_eq!(project["groups"][0]["name"], "CAM 3");
    assert_eq!(project["groups"][0]["parts"].as_array().unwrap().len(), 2);
    assert_eq!(project["media"].as_array().unwrap().len(), 2);
    assert!(s.ok(&["group", "ls"]).contains("CAM 3"));

    // One undo removes the group and both imported assets.
    s.ok(&["undo"]);
    let project: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    assert!(project["groups"].as_array().unwrap().is_empty());
    assert!(project["media"].as_array().unwrap().is_empty());

    // Redo restores both, and removing the group leaves the assets pooled.
    s.ok(&["redo"]);
    s.ok(&["group", "rm", "1"]);
    let project: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    assert!(project["groups"].as_array().unwrap().is_empty());
    assert_eq!(project["media"].as_array().unwrap().len(), 2);
    s.ok(&["undo"]);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&s.ok(&["show", "--json"])).unwrap()["groups"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

/// One cut from each of two files, for the importer `--group` cases.
const PAIR_EDL: &str = "\
TITLE: pair
FCM: NON-DROP FRAME

001  AX  V  C  00:00:00:00 00:00:01:00 01:00:00:00 01:00:01:00
* FROM CLIP NAME: red.mp4
002  AX  V  C  00:00:00:00 00:00:01:00 01:00:01:00 01:00:02:00
* FROM CLIP NAME: green.mp4
";

#[test]
fn import_edl_group_resolves_bare_part_names() {
    let s = Sandbox::new();
    s.write("pair.edl", PAIR_EDL);
    s.ok(&[
        "import-edl",
        "pair.edl",
        "--media",
        ".",
        "--group",
        "PAIR=red.mp4,green.mp4",
    ]);
    let listing = s.ok(&["group", "ls"]);
    assert!(listing.contains("PAIR"), "{listing}");
    assert!(listing.contains("2 part(s)"), "{listing}");
    let project: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    assert_eq!(project["groups"][0]["name"], "PAIR");
}

#[test]
fn import_edl_group_refuses_a_missing_part() {
    let s = Sandbox::new();
    s.write("pair.edl", PAIR_EDL);
    let out = s.run(&[
        "import-edl",
        "pair.edl",
        "--media",
        ".",
        "--group",
        "PAIR=red.mp4,missing.mp4",
    ]);
    let text = merged(&out);
    assert!(!out.status.success(), "a missing part is fatal:\n{text}");
    assert!(text.contains("missing.mp4"), "the part is named:\n{text}");
    assert!(!s.file(P).exists(), "no project was written");
}

#[test]
fn import_edl_group_parts_can_fill_an_empty_media_dir() {
    let s = Sandbox::new();
    std::fs::create_dir(s.path().join("empty")).unwrap();
    s.write("pair.edl", PAIR_EDL);
    s.ok(&[
        "import-edl",
        "pair.edl",
        "--media",
        "empty",
        "--group",
        "PAIR=red.mp4,green.mp4",
    ]);
    let project: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    assert_eq!(project["groups"][0]["name"], "PAIR");
    assert_eq!(
        project["media"].as_array().unwrap().len(),
        2,
        "the explicit parts are the pool"
    );
    assert_eq!(
        project["sequences"][0]["timeline"]["tracks"][0]["segments"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "the reels resolve from the pooled parts"
    );
}

/// The clip paths moved, but `--group` names the real files: the OTIO
/// importer pools group parts before it resolves clips, so the pooled-name
/// fallback finds them (as the EDL importer already did).
#[test]
fn import_otio_group_parts_resolve_moved_clips() {
    let s = Sandbox::new();
    s.write(
        "moved.otio",
        r#"{
  "OTIO_SCHEMA": "Timeline.1",
  "name": "moved",
  "tracks": {
    "OTIO_SCHEMA": "Stack.1",
    "children": [{
      "OTIO_SCHEMA": "Track.1",
      "name": "Program",
      "kind": "Video",
      "children": [
        {
          "OTIO_SCHEMA": "Clip.1",
          "name": "red",
          "media_reference": {
            "OTIO_SCHEMA": "ExternalReference.1",
            "target_url": "/moved/red.mp4"
          },
          "source_range": {
            "OTIO_SCHEMA": "TimeRange.1",
            "start_time": {"OTIO_SCHEMA": "RationalTime.1", "value": 0, "rate": 30},
            "duration": {"OTIO_SCHEMA": "RationalTime.1", "value": 30, "rate": 30}
          }
        },
        {
          "OTIO_SCHEMA": "Clip.1",
          "name": "green",
          "media_reference": {
            "OTIO_SCHEMA": "ExternalReference.1",
            "target_url": "/moved/green.mp4"
          },
          "source_range": {
            "OTIO_SCHEMA": "TimeRange.1",
            "start_time": {"OTIO_SCHEMA": "RationalTime.1", "value": 0, "rate": 30},
            "duration": {"OTIO_SCHEMA": "RationalTime.1", "value": 30, "rate": 30}
          }
        }
      ]
    }]
  }
}"#,
    );
    s.ok(&[
        "import-otio",
        "moved.otio",
        "--group",
        "PAIR=red.mp4,green.mp4",
    ]);
    let project: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    assert_eq!(project["groups"][0]["name"], "PAIR");
    assert_eq!(
        project["sequences"][0]["timeline"]["tracks"][0]["segments"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "both moved clips resolved through the pooled parts"
    );
}

#[test]
fn annotations_attach_to_source_and_survive_edits() {
    let s = Sandbox::new();
    s.base();
    s.ok(&[
        "annotation",
        "add",
        "check the cut",
        "--asset",
        "1",
        "--time",
        "1",
    ]);
    let listing = s.ok(&["annotation", "ls"]);
    assert!(listing.contains("check the cut"), "{listing}");
    assert!(listing.contains("[open]"), "{listing}");
    assert!(listing.contains("asset #1 @ 1 s"), "{listing}");

    let anchor = |s: &Sandbox| -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(&s.ok(&["show", "--json"])).unwrap()["annotations"]
            [0]["anchor"]
            .clone()
    };
    let before = anchor(&s);

    // A source anchor is a media time: splitting the timeline cannot move it.
    s.ok(&["split", "--at", "1"]);
    assert_eq!(anchor(&s), before, "split left the source anchor alone");

    s.ok(&["annotation", "resolve", "1"]);
    assert!(s.ok(&["annotation", "ls"]).contains("[resolved]"));
    s.ok(&["undo"]);
    assert!(
        s.ok(&["annotation", "ls"]).contains("[open]"),
        "undo reopens it"
    );

    s.ok(&["annotation", "rm", "1"]);
    assert!(s.ok(&["annotation", "ls"]).contains("no annotations"));
    s.ok(&["undo"]);
    assert!(
        s.ok(&["annotation", "ls"]).contains("check the cut"),
        "undo restores a removed note"
    );
}

#[test]
fn timeline_annotations_follow_their_clip_across_edits() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["annotation", "add", "pin", "--timeline", "--at", "1"]);
    assert!(s.ok(&["annotation", "ls"]).contains("timeline 1 s"));

    // A ripple gap moves the material (and the note) one second right.
    s.ok(&["gap", "--at", "0", "--frames", "30"]);
    assert!(
        s.ok(&["annotation", "ls"]).contains("timeline 2 s"),
        "the note follows the clip"
    );
    s.ok(&["undo"]);
    assert!(s.ok(&["annotation", "ls"]).contains("timeline 1 s"));

    // Split at the note: the right piece owns source 1, so the anchor moves
    // to it without changing the timeline position.
    s.ok(&["split", "--at", "1"]);
    let project: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    let anchor = &project["annotations"][0]["anchor"];
    let piece = anchor["start"]["clip"].as_u64().expect("clip id");
    assert_ne!(piece, 1, "the note re-anchored to the split-off piece");
    assert_eq!(anchor["start"]["source"], "1/1");
    assert!(s.ok(&["annotation", "ls"]).contains("timeline 1 s"));

    // Deleting the clip drops the note; the one-shot edit reports it, and
    // undo brings both back.
    let dropped = s.ok(&["delete", "--clip", &piece.to_string()]);
    assert!(dropped.contains("dropped annotation"), "{dropped}");
    assert!(s.ok(&["annotation", "ls"]).contains("no annotations"));
    s.ok(&["undo"]);
    assert!(s.ok(&["annotation", "ls"]).contains("timeline 1 s"));
}

#[test]
fn timeline_ranges_follow_ripples_and_drop_with_a_clip() {
    let s = Sandbox::new();
    s.base();
    s.ok(&[
        "annotation",
        "add",
        "span",
        "--timeline",
        "--at",
        "0.5",
        "--to",
        "3.5",
    ]);
    let listing = s.ok(&["annotation", "ls"]);
    assert!(listing.contains("timeline 0.5–3.5 s"), "{listing}");

    s.ok(&["gap", "--at", "0", "--frames", "30"]);
    assert!(
        s.ok(&["annotation", "ls"]).contains("timeline 1.5–4.5 s"),
        "both endpoints follow the ripple"
    );
    s.ok(&["undo"]);
    assert!(s.ok(&["annotation", "ls"]).contains("timeline 0.5–3.5 s"));

    // The range ends in the green clip; deleting it drops the whole note.
    let project: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    let end_clip = project["annotations"][0]["anchor"]["end"]["clip"]
        .as_u64()
        .expect("end clip");
    let dropped = s.ok(&["delete", "--clip", &end_clip.to_string()]);
    assert!(dropped.contains("dropped annotation"), "{dropped}");
    assert!(s.ok(&["annotation", "ls"]).contains("no annotations"));
    s.ok(&["undo"]);
    assert!(s.ok(&["annotation", "ls"]).contains("timeline 0.5–3.5 s"));
}

#[test]
fn a_range_end_on_a_cut_belongs_to_the_left_clip() {
    let s = Sandbox::new();
    s.base(); // red 0–2, green 2–4, blue 4–6
    s.ok(&[
        "annotation",
        "add",
        "cut",
        "--timeline",
        "--at",
        "0.5",
        "--to",
        "2",
    ]);
    let project: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    let clips = project["sequences"][0]["timeline"]["tracks"][0]["segments"]
        .as_array()
        .unwrap();
    let left = clips[0]["Clip"]["id"].as_u64().expect("left clip");
    let right = clips[1]["Clip"]["id"].as_u64().expect("right clip");
    let anchor = &project["annotations"][0]["anchor"];
    assert_eq!(anchor["end"]["clip"], left, "the cut end binds left");
    assert_eq!(anchor["end"]["source"], "2/1");

    // Deleting the right clip leaves the left tail (and the note) intact.
    s.ok(&["delete", "--clip", &right.to_string()]);
    assert!(
        s.ok(&["annotation", "ls"]).contains("timeline 0.5–2 s"),
        "the note belongs to the surviving left clip"
    );
    s.ok(&["undo"]);

    // Deleting the left clip drops it.
    let dropped = s.ok(&["delete", "--clip", &left.to_string()]);
    assert!(dropped.contains("dropped annotation"), "{dropped}");
    assert!(s.ok(&["annotation", "ls"]).contains("no annotations"));
    s.ok(&["undo"]);
    assert!(s.ok(&["annotation", "ls"]).contains("timeline 0.5–2 s"));
}

#[test]
fn annotation_check_passes_a_matching_render_and_fails_a_wrong_camera() {
    let s = Sandbox::new();
    s.base(); // red 0–2, green 2–4, blue 4–6 at 30 fps
    s.ok(&[
        "annotation",
        "add",
        "--expect",
        "cam",
        "--timeline",
        "--at",
        "0.5",
        "--to",
        "1.5",
        "--asset",
        "1",
    ]);
    let listing = s.ok(&["annotation", "ls"]);
    assert!(listing.contains("expect"), "{listing}");
    assert!(listing.contains("cam"), "{listing}");
    s.export("out.mp4");

    let out = s.run(&["annotation", "check", "--report", "out.mp4"]);
    assert!(
        out.status.success(),
        "a matching cam passes:\n{}",
        merged(&out)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains('✓'), "{text}");
    assert!(
        text.contains("1 check(s): 1 pass, 0 fail, 0 unverifiable"),
        "{text}"
    );

    // The other camera fails at the first checked frame, and `--json` still
    // prints the whole report when the exit code is 1.
    s.ok(&["annotation", "rm", "1"]);
    s.ok(&[
        "annotation",
        "add",
        "--expect",
        "cam",
        "--timeline",
        "--at",
        "0.5",
        "--to",
        "1.5",
        "--asset",
        "2",
    ]);
    let out = s.run(&["annotation", "check", "--report", "out.mp4", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", merged(&out));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("json on exit 1: {e}\n{}", merged(&out)));
    assert_eq!(report["summary"]["fail"], 1, "{report}");
    let detail = report["checks"][0]["detail"].as_str().expect("detail");
    assert!(detail.contains("frame 15"), "{detail}");
    assert!(detail.contains("asset #1"), "{detail}");
}

#[test]
fn analysis_import_stores_a_transcript_and_words_complete_uses_it() {
    let s = Sandbox::new();
    s.base(); // red 0–2, green 2–4, blue 4–6; red's asset is #1
    s.write(
        "take.json",
        r#"{"words":[{"text":"hello","start":"0/1","end":"1/1"}]}"#,
    );
    let hash = s
        .ok(&["analysis", "import", "--asset", "1", "take.json"])
        .trim()
        .to_string();
    assert!(hash.starts_with("t1-"), "{hash}");
    let store = nle_core::state::state_dir(&s.file(P)).join("analysis/transcripts");
    assert!(store.join(&hash).is_file(), "stored {hash} in {store:?}");

    // A whole word in the first clip.
    s.ok(&[
        "annotation",
        "add",
        "--expect",
        "words-complete",
        "--timeline",
        "--at",
        "0",
        "--to",
        "2",
        "--transcript",
        &hash,
    ]);
    // A word cut by the anchor's end.
    s.write(
        "cut.json",
        r#"{"words":[{"text":"cut","start":"1/1","end":"3/1"}]}"#,
    );
    let cut = s
        .ok(&["analysis", "import", "--asset", "1", "cut.json"])
        .trim()
        .to_string();
    s.ok(&[
        "annotation",
        "add",
        "--expect",
        "words-complete",
        "--timeline",
        "--at",
        "0",
        "--to",
        "2",
        "--transcript",
        &cut,
    ]);
    // Both expectations precede the export, so the report's revision matches.
    s.export("words.mp4");

    let out = s.run(&["annotation", "check", "--report", "words.mp4", "--id", "1"]);
    assert!(out.status.success(), "{}", merged(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("1/1 words complete"), "{text}");

    let out = s.run(&["annotation", "check", "--report", "words.mp4", "--id", "2"]);
    assert_eq!(out.status.code(), Some(1), "{}", merged(&out));
    let text = merged(&out);
    assert!(text.contains("0/1 words complete"), "{text}");
    assert!(text.contains("at source 1 s is cut"), "{text}");
}

#[test]
fn analysis_analyze_stores_a_mean_luma_product_and_reuses_the_sample_offline() {
    let s = Sandbox::new();
    s.base(); // red 0–2, green 2–4, blue 4–6; red's asset is #1
    let project_before = s.read(P);
    let args = [
        "analysis",
        "analyze",
        "--asset",
        "1",
        "--duration",
        "1",
        "--fps",
        "5",
        "--width",
        "32",
        "--height",
        "24",
    ];
    let hash = s.ok(&args).trim().to_string();
    assert!(hash.starts_with("a1-"), "{hash}");
    assert_eq!(hash.len(), 19, "{hash}");

    let state = nle_core::state::state_dir(&s.file(P));
    let product_path = state.join("analysis/products").join(&hash);
    let product: serde_json::Value =
        serde_json::from_slice(&fs::read(&product_path).expect("the product file")).unwrap();
    assert_eq!(product["version"], 1);
    assert_eq!(product["analyzer"]["name"], "mean-luma");
    assert_eq!(product["sample"]["key"].as_str().map(str::len), Some(16));
    assert_eq!(product["result"]["kind"], "mean_luma");
    let luma = product["result"]["luma_milli"]
        .as_array()
        .expect("per-frame luma");
    assert_eq!(luma.len(), 5, "1 s at 5 fps");
    for frame in luma {
        let value = frame.as_u64().expect("integer luma");
        assert!(
            (50_000..60_000).contains(&value),
            "red luma {value} out of range"
        );
    }
    assert_eq!(
        s.read(P),
        project_before,
        "an analysis run must not rewrite the project"
    );

    // One packed sample (`.raw` + `.json`), and no staging directory left.
    let cache = state.join("cache/analysis");
    let entries = || {
        fs::read_dir(&cache)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect::<Vec<_>>()
    };
    assert_eq!(entries().len(), 2, "{:?}", entries());
    let staging = cache.join("staging");
    assert!(
        !staging.exists() || staging.read_dir().unwrap().next().is_none(),
        "the per-run staging directory is removed"
    );

    // The source is gone, but the packed sample is enough: same address, no
    // new cache entries, no project write.
    fs::remove_file(s.file("red.mp4")).unwrap();
    let again = s.ok(&args).trim().to_string();
    assert_eq!(again, hash, "the offline sample produced the same product");
    assert_eq!(entries().len(), 2, "the sample hit did not grow the cache");
    assert_eq!(s.read(P), project_before);

    // A zero dimension is a bad grid, not a window shorter than one frame.
    let out = s.run(&[
        "analysis", "analyze", "--asset", "1", "--width", "0", "--height", "24",
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", merged(&out));
    assert!(
        merged(&out).contains("width and height must be non-zero"),
        "{}",
        merged(&out)
    );
}

#[test]
fn annotation_check_counts_unverifiable_and_resolved_failures_as_failed() {
    let s = Sandbox::new();
    s.base();
    // A transcript hash missing from the store cannot answer.
    s.ok(&[
        "annotation",
        "add",
        "--expect",
        "words-complete",
        "--timeline",
        "--at",
        "0.5",
        "--transcript",
        "t1-0000000000000000",
    ]);
    s.export("out.mp4");
    let out = s.run(&["annotation", "check", "--report", "out.mp4"]);
    assert_eq!(out.status.code(), Some(1), "{}", merged(&out));
    let text = merged(&out);
    assert!(text.contains('?'), "{text}");
    assert!(text.contains("unverifiable"), "{text}");

    // Resolving acknowledges a failure; it does not waive it.
    s.ok(&["annotation", "rm", "1"]);
    s.ok(&[
        "annotation",
        "add",
        "--expect",
        "cam",
        "--timeline",
        "--at",
        "0.5",
        "--asset",
        "2",
    ]);
    s.ok(&["annotation", "resolve", "1"]);
    let out = s.run(&["annotation", "check", "--report", "out.mp4"]);
    assert_eq!(out.status.code(), Some(1), "{}", merged(&out));
    let text = merged(&out);
    assert!(text.contains("[resolved]"), "{text}");
    assert!(text.contains("0 pass, 1 fail"), "{text}");
}

#[test]
fn annotation_check_narrows_to_ids_and_follows_the_reports_sequence() {
    let s = Sandbox::new();
    s.base();
    s.ok(&[
        "annotation",
        "add",
        "--expect",
        "cam",
        "--timeline",
        "--at",
        "0.5",
        "--asset",
        "1",
    ]);
    s.ok(&[
        "annotation",
        "add",
        "--expect",
        "cam",
        "--timeline",
        "--at",
        "0.5",
        "--asset",
        "2",
    ]);
    // A second, empty sequence becomes active: the report still names #1, so
    // its expectations are the ones checked.
    s.ok(&["sequence", "add", "Second"]);
    s.ok(&["sequence", "use", "2"]);
    s.ok(&["export", "--sequence", "1", "-o", "one.mp4"]);

    let out = s.run(&["annotation", "check", "--report", "one.mp4", "--id", "1"]);
    assert!(out.status.success(), "{}", merged(&out));
    let out = s.run(&["annotation", "check", "--report", "one.mp4", "--id", "2"]);
    assert_eq!(out.status.code(), Some(1), "{}", merged(&out));
    let out = s.run(&["annotation", "check", "--report", "one.mp4"]);
    assert_eq!(out.status.code(), Some(1), "both checks: {}", merged(&out));

    // `--report` takes the video or its sidecar.
    let out = s.run(&[
        "annotation",
        "check",
        "--report",
        "one.mp4.report.json",
        "--id",
        "1",
    ]);
    assert!(out.status.success(), "{}", merged(&out));

    // Asking for another sequence is a seam error (2), not a silent answer
    // about the wrong frame map; so is a missing report.
    let out = s.run(&[
        "annotation",
        "check",
        "--report",
        "one.mp4",
        "--sequence",
        "2",
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", merged(&out));
    let out = s.run(&["annotation", "check", "--report", "nope.mp4"]);
    assert_eq!(out.status.code(), Some(2), "{}", merged(&out));
}

#[test]
fn an_expectation_selector_guards_media_and_group_removal() {
    let s = Sandbox::new();
    s.base(); // red/green/blue on the timeline, assets #1–#3
    s.ok(&["import", "tiny.mp4"]);
    let project: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    let tiny = project["media"]
        .as_array()
        .expect("media")
        .iter()
        .find(|asset| {
            asset["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("tiny.mp4"))
        })
        .and_then(|asset| asset["id"].as_u64())
        .expect("tiny.mp4 is pooled");
    let tiny_id = tiny;
    let tiny = tiny.to_string();
    let pooled_ids = || -> Vec<u64> {
        let project: serde_json::Value =
            serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
        project["media"]
            .as_array()
            .expect("media")
            .iter()
            .filter_map(|asset| asset["id"].as_u64())
            .collect()
    };

    // The asset is pool-only (not on any timeline), but the check names it.
    s.ok(&[
        "annotation",
        "add",
        "--expect",
        "cam",
        "--timeline",
        "--at",
        "0.5",
        "--asset",
        &tiny,
    ]);
    // Resolving is not a waiver: a resolved check still holds the reference.
    s.ok(&["annotation", "resolve", "1"]);
    let err = s.err(&["media", "rm", &tiny]);
    let expected = format!("asset #{tiny} is used by annotation(s) [1]");
    assert!(err.contains(&expected), "{err}");
    assert!(err.contains("nile annotation rm"), "{err}");
    assert!(
        pooled_ids().contains(&tiny_id),
        "the refused asset is still pooled"
    );

    // The group equivalent: a group of pool-only parts named by a check.
    s.ok(&["import", "--group", "PAIR", "wide169.mp4", "video_only.mp4"]);
    let project: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    let group = project["groups"][0]["id"]
        .as_u64()
        .expect("group id")
        .to_string();
    s.ok(&[
        "annotation",
        "add",
        "--expect",
        "cam",
        "--timeline",
        "--at",
        "0.5",
        "--group",
        &group,
    ]);
    let err = s.err(&["group", "rm", &group]);
    let expected = format!("media group #{group} is used by annotation(s) [2]");
    assert!(err.contains(&expected), "{err}");
    assert!(s.ok(&["group", "ls"]).contains("PAIR"), "still present");

    s.ok(&["annotation", "rm", "2"]);
    s.ok(&["group", "rm", &group]);
    // The asset check remains and still blocks the pooled file.
    let err = s.err(&["media", "rm", &tiny]);
    assert!(
        err.contains(&format!("asset #{tiny} is used by annotation(s) [1]")),
        "{err}"
    );
    s.ok(&["annotation", "rm", "1"]);
    s.ok(&["media", "rm", &tiny]);
    assert!(!pooled_ids().contains(&tiny_id));
}

#[test]
fn annotation_check_refuses_an_id_that_selects_nothing() {
    let s = Sandbox::new();
    s.base();
    // #1 is an expectation on sequence #1; #2 is a note there.
    s.ok(&[
        "annotation",
        "add",
        "--expect",
        "cam",
        "--timeline",
        "--at",
        "0.5",
        "--asset",
        "1",
    ]);
    s.ok(&["annotation", "add", "pin", "--timeline", "--at", "1"]);
    // #3 is an expectation on another sequence.
    s.ok(&["sequence", "add", "Second", "--fps", "30/1"]);
    s.ok(&["sequence", "use", "2"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&[
        "annotation",
        "add",
        "--expect",
        "cam",
        "--timeline",
        "--at",
        "0.5",
        "--asset",
        "1",
    ]);
    s.ok(&["export", "--sequence", "1", "-o", "one.mp4"]);

    for id in ["99", "2", "3"] {
        let out = s.run(&["annotation", "check", "--report", "one.mp4", "--id", id]);
        assert_eq!(out.status.code(), Some(2), "id {id}: {}", merged(&out));
        let text = merged(&out);
        assert!(
            text.contains(&format!("no expectation with id #{id} on sequence #1")),
            "id {id}: {text}"
        );
    }
    // The real expectation still checks.
    let out = s.run(&["annotation", "check", "--report", "one.mp4", "--id", "1"]);
    assert!(out.status.success(), "{}", merged(&out));
}

#[test]
fn show_json_exposes_time_maps_and_groups() {
    let s = Sandbox::new();
    s.base();
    let project: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    assert_eq!(
        project["version"],
        nle_core::project::PROJECT_VERSION,
        "the JSON names the build's project version"
    );
    assert_eq!(project["groups"], serde_json::json!([]));
    assert!(
        project.get("time_maps").is_none(),
        "plain show --json is the persisted project"
    );

    let with_maps: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json", "--maps"])).expect("project + maps");
    let maps = with_maps["time_maps"].as_array().expect("time maps");
    assert!(!maps.is_empty(), "the clips produce a map");
    assert_eq!(maps[0]["track_index"], 0);
    assert_eq!(maps[0]["asset"], 1);
    assert!(
        maps[0]["spans"]
            .as_array()
            .is_some_and(|spans| !spans.is_empty())
    );

    // A group referencing a pooled asset round-trips through show.
    let mut with_group = project.clone();
    with_group["groups"] = serde_json::json!([{
        "id": 1,
        "name": "CAM 3",
        "parts": [{
            "asset": 1,
            "group_start": "0/1",
            "file_start": "0/1",
            "duration": "2/1"
        }]
    }]);
    s.write(P, &serde_json::to_string_pretty(&with_group).unwrap());
    let shown: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
    assert_eq!(shown["groups"][0]["name"], "CAM 3");
    assert!(s.ok(&["show"]).contains("group #1 CAM 3"));
    let maps: serde_json::Value =
        serde_json::from_str(&s.ok(&["show", "--json", "--maps"])).expect("project + maps");
    let group_maps = maps["group_maps"].as_array().expect("group maps");
    assert_eq!(group_maps[0]["group"], 1);
    assert_eq!(group_maps[0]["asset"], 1);
    assert!(
        group_maps[0]["spans"]
            .as_array()
            .is_some_and(|spans| !spans.is_empty())
    );
}

#[test]
fn a_chunked_export_writes_a_report_too() {
    let s = Sandbox::new();
    s.ok(&["init"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["add", "green.mp4", "--at", "2", "--in", "0", "--out", "2"]);
    s.ok(&["add", "red.mp4", "--at", "4", "--in", "2", "--out", "4"]);
    let out = s.run(&["export", "-o", "chunked.mp4", "--parallel", "2"]);
    let text = merged(&out);
    assert!(out.status.success(), "chunked export failed:\n{text}");
    let sidecar = nle_core::RenderReport::sidecar_path(&s.file("chunked.mp4"));
    let report = nle_core::RenderReport::load(&sidecar)
        .unwrap_or_else(|e| panic!("report missing after chunked export: {e}\n{text}"));
    assert_eq!(report.frame_count, s.frame_count("chunked.mp4"));
    assert!(!report.windows.is_empty(), "{text}");
    assert!(
        text.contains("2-way"),
        "the scheduler split the export: {text}"
    );
    assert!(
        report.windows.len() >= 2,
        "chunked export records its windows: {text}"
    );
}

#[test]
fn export_writes_a_frame_map_beside_the_output() {
    let s = Sandbox::new();
    s.base();
    let summary = s.export_report("mapped.mp4");
    assert!(
        summary.contains("report mapped.mp4.report.json"),
        "{summary}"
    );

    let sidecar = nle_core::RenderReport::sidecar_path(&s.file("mapped.mp4"));
    let report = nle_core::RenderReport::load(&sidecar).expect("report sidecar");
    assert_eq!(report.sequence, 1);
    assert_eq!(report.frame_count, s.frame_count("mapped.mp4"));
    assert_eq!(
        report
            .frame_map
            .last()
            .map(|span| span.start_frame + span.frames),
        Some(report.frame_count),
        "the map covers the output"
    );

    let frame = report.locate(15).expect("frame 15");
    assert!(
        matches!(
            frame,
            nle_core::FrameLocation::Clip { asset: 1, source, .. }
                if source == nle_core::Seconds::new(1, 2)
        ),
        "{frame:?}"
    );

    let run_report = |args: &[&str]| -> String {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_nile"))
            .current_dir(s.path())
            .arg("report")
            .args(args)
            .output()
            .expect("run nile report");
        assert!(out.status.success(), "{}", merged(&out));
        merged(&out)
    };
    let listing = run_report(&["mapped.mp4"]);
    assert!(listing.contains("180 frames @ 30/1"), "{listing}");
    assert!(listing.contains("6 s"), "{listing}");
    let json = run_report(&["mapped.mp4", "--json"]);
    let value: serde_json::Value = serde_json::from_str(&json).expect("report JSON");
    assert_eq!(value["frame_count"], report.frame_count);
    assert!(
        value["frame_map"]
            .as_array()
            .is_some_and(|spans| !spans.is_empty())
    );

    // The report belongs to the output, not the project: remove the project
    // and it still reads.
    std::fs::remove_file(s.file(P)).unwrap();
    assert!(run_report(&["mapped.mp4"]).contains("180 frames"));

    // The report is output-relative: a stray --project is refused, like
    // `nile default`.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nile"))
        .current_dir(s.path())
        .args(["--project", "typo.json", "report", "mapped.mp4"])
        .output()
        .expect("run nile report");
    assert!(!out.status.success());
    assert!(
        merged(&out).contains("--project is not used"),
        "{}",
        merged(&out)
    );
}

#[test]
fn cli_undo_follows_the_active_sequence() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["split", "--at", "1"]);

    let segments = |s: &Sandbox| {
        let project: serde_json::Value =
            serde_json::from_str(&s.ok(&["show", "--json"])).expect("project JSON");
        let active = project["active"].as_u64().expect("active");
        project["sequences"]
            .as_array()
            .unwrap()
            .iter()
            .find(|sequence| sequence["id"].as_u64() == Some(active))
            .unwrap()["timeline"]["tracks"][0]["segments"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(segments(&s), 4, "the split landed on sequence 1");

    s.ok(&["sequence", "add", "Second"]);
    s.ok(&["sequence", "use", "2"]);
    let refused = s.run(&["undo"]);
    assert!(!refused.status.success(), "nothing to undo on sequence 2");
    assert_eq!(segments(&s), 0, "sequence 2 is still empty");

    s.ok(&["sequence", "use", "1"]);
    s.ok(&["undo"]);
    assert_eq!(
        segments(&s),
        3,
        "undo on sequence 1 replayed its own split only"
    );
}

#[test]
fn split_and_ripple_delete() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["split", "--at", "1"]);
    s.ok(&["delete", "--clip", "1", "--ripple"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 0.5, RED, "the red half survives");
    s.assert_px("b.mp4", 2.0, GREEN, "green slid left");
    s.assert_dur("b.mp4", 5.0, "5 s");
}

#[test]
fn range_actions_round_trip_through_the_cli() {
    let s = Sandbox::new();
    s.base(); // red 0–2, green 2–4, blue 4–6
    s.ok(&["yank", "--from-seg", "0", "--to-seg", "1"]);
    s.ok(&["paste", "--range", "--at", "6"]);
    s.export("b.mp4");
    s.assert_dur("b.mp4", 10.0, "base + copied range");
    s.assert_px("b.mp4", 6.5, RED, "pasted red head");
    s.assert_px("b.mp4", 8.5, GREEN, "pasted green tail");

    s.base();
    s.ok(&["delete", "--from-seg", "0", "--to-seg", "1"]);
    s.export("b.mp4");
    s.assert_dur("b.mp4", 2.0, "only blue remains");
    s.assert_px("b.mp4", 1.0, BLUE, "blue at the new start");
}

#[test]
fn trim_in_ripples_trim_out_leaves_a_gap() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["trim-in", "--at", "1", "--ripple"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 0.5, RED, "red head pulled in");
    s.assert_dur("b.mp4", 5.0, "5 s");

    s.base();
    s.ok(&["trim-out", "--at", "1"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 0.5, RED, "red 0–1");
    s.assert_px("b.mp4", 1.5, BLACK, "the trimmed material is a gap");
    s.assert_dur("b.mp4", 6.0, "length preserved");
}

#[test]
fn gaps_insert_merge_delete() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["gap", "--at", "2", "--frames", "30"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 2.5, BLACK, "the inserted gap");
    s.assert_px("b.mp4", 4.0, GREEN, "green pushed to 3–5");
    s.assert_dur("b.mp4", 7.0, "7 s");

    // Two adjacent gaps read as one row.
    s.base();
    s.ok(&["gap", "--at", "2", "--frames", "30"]);
    s.ok(&["gap", "--at", "3", "--frames", "30"]);
    // Count the row's `gap` token, not a substring: a checkout path can itself
    // contain "gap" (`test-review-gaps`), which every clip path repeats.
    let gaps = s
        .show()
        .lines()
        .filter(|line| line.split_whitespace().any(|word| word == "gap"))
        .count();
    assert_eq!(gaps, 1, "adjacent gaps coalesced ({gaps} gap rows)");

    s.base();
    s.ok(&["gap", "--at", "2", "--frames", "30"]);
    s.ok(&["delete", "--at", "2.5"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 2.5, GREEN, "deleting the gap pulled green back");
    s.assert_dur("b.mp4", 6.0, "6 s");
}

#[test]
fn delete_keeps_the_length_the_tail_is_black() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["delete", "--clip", "2"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 3.0, BLACK, "green's span is a gap");
    s.assert_dur("b.mp4", 6.0, "length preserved");
}

// Clip ids and segment indices are different spaces: the overlay clip is
// id 4 on track 2 *index 0*, deep in the ids but first in its track. Acting
// by `--clip` must find it by id — reading the number as a track, or as a
// 0-/1-based segment index on the base track, hits something else (or
// nothing) and leaves the wrong picture.
#[test]
fn clip_addresses_are_ids_not_indices() {
    let s = Sandbox::new();
    s.base(); // red 0–2 (id 1), green 2–4 (id 2), blue 4–6 (id 3)
    s.ok(&["track", "add", "video"]);
    s.ok(&["add", "blue.mp4", "--track", "2", "--in", "0", "--out", "2"]); // id 4, track 2 index 0
    s.ok(&["delete", "--clip", "4"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 0.5, RED, "the overlay blue is gone");
    s.assert_dur("b.mp4", 6.0, "the base track is untouched");

    // The same rule for effects: id 4 edits the overlay, not base index 4.
    s.base();
    s.ok(&["track", "add", "video"]);
    s.ok(&["add", "blue.mp4", "--track", "2", "--in", "0", "--out", "2"]);
    s.ok(&["effect", "eq", "--clip", "4", "--saturation", "0"]);
    s.export("g.mp4");
    let grey = s.px("g.mp4", 0.5);
    assert!(
        grey[0].abs_diff(grey[2]) < 8 && grey[1].abs_diff(grey[2]) < 8,
        "the overlay is desaturated, not the base: {grey:?}"
    );
    s.assert_px("g.mp4", 3.0, GREEN, "the base green is untouched");
}

#[test]
fn paste_over_vs_ripple() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["yank", "--clip", "1"]);
    s.ok(&["paste", "--at", "1"]);
    let show = s.show();
    assert!(
        show.contains("1 – 3"),
        "the paste covers exactly 1–3:\n{show}"
    );
    assert!(
        show.contains("(src 1–2)"),
        "the covered green was trimmed to its tail:\n{show}"
    );
    s.export("b.mp4");
    s.assert_px("b.mp4", 2.0, RED, "the pasted red");
    s.assert_dur("b.mp4", 6.0, "length unchanged");

    s.base();
    s.ok(&["yank", "--clip", "1"]);
    s.ok(&["paste", "--at", "0", "--ripple"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 1.0, RED, "the ripple paste sits first");
    s.assert_px("b.mp4", 3.0, RED, "the original red follows");
    s.assert_dur("b.mp4", 8.0, "8 s");
}

#[test]
fn speed_source_timeline_math() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "4"]);
    s.ok(&["speed", "1", "200"]);
    s.ok(&["trim-in", "--at", "1"]);
    let show = s.show();
    assert!(
        show.contains("(src 2–4)"),
        "1 timeline s at 2× consumes 2 source s:\n{show}"
    );
    s.ok(&["extend", "--clip", "1", "--edge", "out", "--to", "2.5"]);
    let show = s.show();
    assert!(
        show.contains("(src 2–5)"),
        "0.5 timeline s at 2× adds 1 source s:\n{show}"
    );
    s.export("s.mp4");
    s.assert_dur("s.mp4", 2.5, "2.5 s");
    let report = s.export_report("s2.mp4");
    assert!(report.contains("75 frames"), "30 fps × 2.5 s:\n{report}");
}

#[test]
fn audio_retiming_keeps_the_audio() {
    let s = Sandbox::new();
    for (rate, want) in [("200", 1.0), ("50", 4.0), ("40", 5.0), ("300", 0.666667)] {
        s.ok(&["init", "--force"]);
        s.ok(&["import", "red.mp4"]);
        s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
        s.ok(&["speed", "1", rate]);
        s.export("r.mp4");
        s.assert_dur("r.mp4", want, &format!("a 2 s clip at {rate}%"));
        assert!(s.has_audio("r.mp4"), "{rate}% keeps its audio");
    }
}

#[test]
fn export_summary_reports_actual_frames() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "blue.mp4", "red.mp4"]);
    s.ok(&["add", "blue.mp4", "--at", "0", "--in", "0", "--out", "2.5"]);
    s.ok(&["add", "red.mp4", "--at", "2.5", "--in", "0", "--out", "1.5"]);
    s.ok(&["add", "blue.mp4", "--at", "4", "--in", "0", "--out", "2.5"]);
    s.ok(&["split", "--at", "5.013"]);
    s.ok(&["split", "--at", "3.795"]);
    s.ok(&["split", "--at", "5.031"]);
    s.ok(&["gap", "--at", "0.635", "--frames", "15"]);
    let report = s.export_report("actual.mp4");
    let actual = s.frame_count("actual.mp4");
    assert!(
        report.contains(&format!("{actual} frames")),
        "summary should report {actual} actual frames:\n{report}"
    );
}

#[test]
fn parallel_export_reports_when_it_must_stay_single() {
    let s = Sandbox::new();
    s.base();
    let out = s.ok(&["export", "-o", "single.mp4", "--parallel", "2"]);
    assert!(
        out.contains("single (chunks need frame-aligned cuts)"),
        "the fallback must be visible:\n{out}"
    );
    assert!(!out.contains("2-way"), "scheduler should not run:\n{out}");
}

#[test]
fn sixty_fps_source_follows_the_timeline_rate() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "blue.mp4"]);
    s.ok(&["add", "blue.mp4", "--in", "0", "--out", "2"]);
    let report = s.export_report("f.mp4");
    assert!(
        report.contains("120 frames @ 60000/1001"),
        "2 s of 59.94 = 120 frames:\n{report}"
    );
}

#[test]
fn multi_track_overlay_mute_base_promotion() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "4"]);
    s.ok(&["track", "add", "video"]);
    s.ok(&[
        "add",
        "green.mp4",
        "--track",
        "2",
        "--at",
        "1",
        "--in",
        "0",
        "--out",
        "2",
    ]);
    s.export("m.mp4");
    s.assert_px("m.mp4", 2.0, GREEN, "the overlay is on top");
    s.assert_px(
        "m.mp4",
        0.5,
        RED,
        "the base shows through the overlay's gap",
    );

    s.ok(&["track", "mute", "2"]);
    s.export("m2.mp4");
    s.assert_px("m2.mp4", 2.0, RED, "a muted overlay reveals the base");

    s.ok(&["track", "mute", "2"]); // unmute the overlay again
    s.ok(&["track", "mute", "1"]);
    s.export("m3.mp4");
    s.assert_px("m3.mp4", 2.0, GREEN, "muting the base promotes the overlay");
    s.assert_px(
        "m3.mp4",
        0.5,
        BLACK,
        "…and it is black where the overlay is a gap",
    );
}

#[test]
fn a_keyed_crop_moves_between_passes() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["effect", "crop", "--clip", "1", "--x", "0", "--w", "100"]);
    s.ok(&[
        "effect", "crop", "--clip", "1", "--at", "1", "--x", "50", "--w", "50",
    ]);

    s.export("keyed.mp4");
    assert_eq!(s.frame_count("keyed.mp4"), 60, "2 s at 30 fps");
    // The lead-in keeps the authored full-size window from frame 0.
    let first = s.px_at("keyed.mp4", 0.0, 2, 120);
    assert!(
        first[0] > 200 && first[1] < 20 && first[2] < 20,
        "frame 0 is full width: {first:?}"
    );
    // Before the key, the window starts at the left edge and slides right;
    // by 0.5 s it already masks the left quarter.
    s.assert_px_at("keyed.mp4", 0.5, 20, 120, BLACK, "the moving head masks");
    let kept = s.px_at("keyed.mp4", 0.5, 200, 120);
    assert!(
        kept[0] > 200 && kept[1] < 20 && kept[2] < 20,
        "the kept side stays red: {kept:?}"
    );
    // After the key, the window is half width at the centre line.
    s.assert_px_at("keyed.mp4", 1.5, 40, 120, BLACK, "the keyed window masks");
    let kept = s.px_at("keyed.mp4", 1.5, 280, 120);
    assert!(
        kept[0] > 200 && kept[1] < 20 && kept[2] < 20,
        "the keyed window keeps picture: {kept:?}"
    );
}

#[test]
fn a_crop_masks_in_place_and_reveals_what_is_below() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["effect", "crop", "--clip", "1", "--x", "50", "--w", "50"]);
    s.export("solo.mp4");
    // 320×240: the left half is masked, the kept half does not zoom.
    s.assert_px_at(
        "solo.mp4",
        0.5,
        20,
        120,
        BLACK,
        "a crop with nothing below is black",
    );
    s.assert_px_at(
        "solo.mp4",
        0.5,
        300,
        120,
        RED,
        "the kept half stays where it was",
    );

    // A scale-down is transparent too: black on its own.
    s.ok(&["effect", "clear", "--clip", "1"]);
    s.ok(&["effect", "scale", "--clip", "1", "--w", "50", "--h", "50"]);
    s.export("small.mp4");
    s.assert_px_at(
        "small.mp4",
        0.5,
        20,
        120,
        BLACK,
        "a scale-down with nothing below is black",
    );
    s.assert_px_at(
        "small.mp4",
        0.5,
        160,
        120,
        RED,
        "the shrunk picture is centred",
    );

    // The same effects on an overlay: the masked area shows the track below.
    s.ok(&["effect", "clear", "--clip", "1"]);
    s.ok(&["track", "add", "video"]);
    s.ok(&[
        "add",
        "green.mp4",
        "--track",
        "2",
        "--in",
        "0",
        "--out",
        "2",
    ]);
    s.ok(&["effect", "crop", "--clip", "2", "--x", "50", "--w", "50"]);
    s.export("over.mp4");
    s.assert_px_at(
        "over.mp4",
        0.5,
        20,
        120,
        RED,
        "the cropped overlay reveals the base",
    );
    s.assert_px_at(
        "over.mp4",
        0.5,
        300,
        120,
        GREEN,
        "the overlay keeps its right half",
    );

    s.ok(&["effect", "clear", "--clip", "2"]);
    s.ok(&["effect", "scale", "--clip", "2", "--w", "50", "--h", "50"]);
    s.export("scaled.mp4");
    s.assert_px_at(
        "scaled.mp4",
        0.5,
        20,
        120,
        RED,
        "the scale pad reveals the base",
    );
    s.assert_px_at(
        "scaled.mp4",
        0.5,
        160,
        120,
        GREEN,
        "the shrunk overlay is centred",
    );

    s.ok(&["effect", "clear", "--clip", "2"]);
    s.ok(&["effect", "offset", "--clip", "2", "--x", "50"]);
    s.export("offset.mp4");
    s.assert_px_at(
        "offset.mp4",
        0.5,
        20,
        120,
        RED,
        "the offset vacates the base",
    );
    s.assert_px_at(
        "offset.mp4",
        0.5,
        300,
        120,
        GREEN,
        "the moved overlay is still visible",
    );

    // ±100% of the canvas is fully off screen: the base shows everywhere.
    for x in ["100", "-100"] {
        s.ok(&["effect", "offset", "--clip", "2", "--x", x]);
        let out = format!("off{}.mp4", x.replace('-', "l"));
        s.export(&out);
        s.assert_px_at(
            &out,
            0.5,
            20,
            120,
            RED,
            &format!("offset {x}% leaves the base at the left"),
        );
        s.assert_px_at(
            &out,
            0.5,
            300,
            120,
            RED,
            &format!("offset {x}% leaves the base at the right"),
        );
    }
}

#[test]
fn a_muted_sole_video_track_has_nothing_to_export() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["track", "mute", "1"]);
    let out = s.err(&["export", "-o", "mm.mp4"]);
    assert!(
        out.contains("every video track is muted"),
        "the export refuses instead of ignoring the mute:\n{out}"
    );
}

#[test]
fn an_empty_upper_video_track_is_transparent() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["track", "add", "video"]);
    s.export("e.mp4");
    s.assert_px("e.mp4", 1.0, RED, "the base still shows");
}

#[test]
fn audio_track_mixes_in() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["track", "add", "audio"]);
    s.ok(&[
        "add",
        "green.mp4",
        "--track",
        "2",
        "--in",
        "0",
        "--out",
        "1",
    ]);
    s.export("a.mp4");
    s.assert_dur("a.mp4", 2.0, "2 s");
    assert!(s.has_audio("a.mp4"), "audio stream present");
}

#[test]
fn audio_post_exports_one_aac_stream_and_keeps_the_duration() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["add", "green.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["sequence", "audio", "--normalize"]);

    // Audio post is a whole-programme stage, so the chunk scheduler renders
    // plain PCM chunks and the final mux applies loudnorm/fades once over the
    // joined programme. One final AAC encode, not one per chunk.
    let summary = s.ok(&["export", "--parallel", "2", "-o", "post.mp4"]);
    assert!(
        summary.contains("2-way"),
        "scheduler was not used: {summary}"
    );
    assert!(
        !summary.contains("single ("),
        "audio post must not force one graph any more: {summary}"
    );
    s.assert_dur(
        "post.mp4",
        4.0,
        "the requested duration survives audio post",
    );
    assert_eq!(
        s.audio_streams("post.mp4"),
        ["aac"],
        "one mixed AAC stream, encoded once"
    );
    assert_eq!(
        s.audio_sample_rate("post.mp4"),
        48_000,
        "loudnorm's 192 kHz output is resampled back to the graph rate"
    );
}

#[test]
fn chunked_audio_post_matches_the_single_graph() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["add", "green.mp4", "--in", "0", "--out", "2"]);
    s.ok(&[
        "sequence",
        "audio",
        "--normalize",
        "--fade-in",
        "0.5",
        "--fade-out",
        "0.5",
    ]);
    s.ok(&["export", "-o", "single.mp4"]);
    let out = s.ok(&["export", "-o", "chunked.mp4", "--parallel", "2"]);
    assert!(out.contains("2-way"), "scheduler was not used: {out}");

    for file in ["single.mp4", "chunked.mp4"] {
        s.assert_dur(file, 4.0, "audio post keeps the programme length");
        assert_eq!(s.audio_streams(file), ["aac"], "{file}: one encode");
        assert_eq!(s.audio_sample_rate(file), 48_000, "{file}: 48 kHz");
    }
    // The fade/loudnorm must be shaped over the joined programme, not once
    // per chunk: both paths fade in at the head and out at the tail.
    for file in ["single.mp4", "chunked.mp4"] {
        let head = s.audio_mean_volume(file, 0.0, 0.1);
        let middle = s.audio_mean_volume(file, 2.0, 0.2);
        let tail = s.audio_mean_volume(file, 3.9, 0.1);
        assert!(
            head < middle - 3.0,
            "{file}: fade-in head {head:.1} dB vs middle {middle:.1} dB"
        );
        assert!(
            tail < middle - 3.0,
            "{file}: fade-out tail {tail:.1} dB vs middle {middle:.1} dB"
        );
    }
    let single = s.audio_mean_volume("single.mp4", 2.0, 0.2);
    let chunked = s.audio_mean_volume("chunked.mp4", 2.0, 0.2);
    assert!(
        (single - chunked).abs() < 1.5,
        "single {single:.1} dB vs chunked {chunked:.1} dB at programme level"
    );
}

#[test]
fn chunked_audio_post_on_a_video_only_programme_writes_silence() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "video_only.mp4"]);
    s.ok(&["add", "video_only.mp4", "--in", "0", "--out", "1"]);
    s.ok(&[
        "add",
        "video_only.mp4",
        "--at",
        "1",
        "--in",
        "0",
        "--out",
        "1",
    ]);
    s.ok(&[
        "sequence",
        "audio",
        "--normalize",
        "--fade-in",
        "0.5",
        "--fade-out",
        "0.5",
    ]);
    let out = s.ok(&["export", "-o", "silent.mp4", "--parallel", "2"]);
    assert!(out.contains("2-way"), "scheduler was not used: {out}");
    s.assert_dur("silent.mp4", 2.0, "the video-only length");
    assert_eq!(s.audio_streams("silent.mp4"), ["aac"]);
    assert_eq!(s.audio_sample_rate("silent.mp4"), 48_000);
    let level = s.audio_mean_volume("silent.mp4", 0.5, 0.5);
    assert!(
        level < -60.0,
        "a video-only programme has no audio material to shape: {level:.1} dB"
    );
}

#[test]
fn normalizing_a_video_only_programme_exports_silence() {
    // There is no audio stream to measure; emitting `loudnorm` over the
    // padded silence makes ffmpeg fail ("Conversion failed"), so the stage is
    // skipped and the AAC silence track still comes out.
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "video_only.mp4"]);
    s.ok(&["add", "video_only.mp4", "--in", "0", "--out", "1"]);
    s.ok(&["sequence", "audio", "--normalize", "--fade-in", "0.5"]);
    s.export("silent.mp4");
    s.assert_dur("silent.mp4", 1.0, "the video-only length");
    assert_eq!(s.audio_streams("silent.mp4"), ["aac"]);
}

#[test]
fn speech_fades_shape_the_head_and_tail() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "4"]);
    s.ok(&["sequence", "audio", "--fade-in", "1", "--fade-out", "1"]);
    s.export("fade.mp4");
    s.assert_dur("fade.mp4", 4.0, "fades do not change the length");
    assert_eq!(
        s.audio_sample_rate("fade.mp4"),
        48_000,
        "a fades-only chain stays at the graph rate"
    );
    let head = s.audio_mean_volume("fade.mp4", 0.0, 0.1);
    let middle = s.audio_mean_volume("fade.mp4", 2.0, 0.2);
    let tail = s.audio_mean_volume("fade.mp4", 3.9, 0.1);
    assert!(
        head < middle - 8.0,
        "fade-in: head {head:.1} dB vs middle {middle:.1} dB"
    );
    assert!(
        tail < middle - 8.0,
        "fade-out: tail {tail:.1} dB vs middle {middle:.1} dB"
    );
}

#[test]
fn normalize_and_fades_compose_in_one_chain() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["add", "green.mp4", "--in", "0", "--out", "2"]);
    s.ok(&[
        "sequence",
        "audio",
        "--normalize",
        "--fade-in",
        "0.5",
        "--fade-out",
        "0.5",
    ]);
    s.export("post-fade.mp4");
    s.assert_dur("post-fade.mp4", 4.0, "the full chain keeps the length");
    assert_eq!(s.audio_streams("post-fade.mp4"), ["aac"]);
    assert_eq!(s.audio_sample_rate("post-fade.mp4"), 48_000);
    let head = s.audio_mean_volume("post-fade.mp4", 0.0, 0.1);
    let middle = s.audio_mean_volume("post-fade.mp4", 2.0, 0.2);
    let tail = s.audio_mean_volume("post-fade.mp4", 3.9, 0.1);
    assert!(
        head < middle - 4.0,
        "normalize + fade-in: head {head:.1} dB vs middle {middle:.1} dB"
    );
    assert!(
        tail < middle - 4.0,
        "normalize + fade-out: tail {tail:.1} dB vs middle {middle:.1} dB"
    );
}

/// The project file as JSON.
fn project_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).expect("read the project")).expect("project JSON")
}

/// The model body without the monotonic revision, for byte-for-byte
/// before/after comparisons: revision is deliberately allowed to advance.
fn model_json(path: &std::path::Path) -> serde_json::Value {
    let mut value = project_json(path);
    value
        .as_object_mut()
        .expect("project object")
        .remove("revision");
    if let Some(sequences) = value["sequences"].as_array_mut() {
        for sequence in sequences {
            let sequence = sequence.as_object_mut().expect("sequence object");
            sequence.remove("manual_edits");
            // Undo/redo deliberately move the cursor to the position the edit
            // was made from, so it is session state, not model state.
            sequence.remove("playhead");
        }
    }
    value
}

/// The persisted model revision.
fn revision(path: &std::path::Path) -> u64 {
    project_json(path)["revision"]
        .as_u64()
        .expect("the project carries a revision")
}

#[test]
fn undo_redo_round_trip() {
    let s = Sandbox::new();
    s.base();
    let before = model_json(&s.file(P));
    s.ok(&["split", "--at", "1"]);
    s.ok(&["trim-out", "--at", "5"]);
    s.ok(&["gap", "--at", "3", "--frames", "15"]);
    s.ok(&["delete", "--clip", "3", "--ripple"]);
    s.ok(&["speed", "1", "200"]);
    let after = model_json(&s.file(P));
    let edited_revision = revision(&s.file(P));
    for _ in 0..5 {
        s.ok(&["undo"]);
    }
    assert_eq!(
        model_json(&s.file(P)),
        before,
        "5 undos restore the model (revision is monotonic)"
    );
    let undone_revision = revision(&s.file(P));
    assert!(
        undone_revision > edited_revision,
        "undo advances the revision ({edited_revision} → {undone_revision})"
    );

    for _ in 0..5 {
        s.ok(&["redo"]);
    }
    assert_eq!(
        model_json(&s.file(P)),
        after,
        "5 redos restore the model (revision is monotonic)"
    );
    let redone_revision = revision(&s.file(P));
    assert!(
        redone_revision > undone_revision,
        "redo advances the revision"
    );

    // A playhead-only save is not a model write.
    s.ok(&["seek", "2"]);
    assert_eq!(
        revision(&s.file(P)),
        redone_revision,
        "a playhead-only save leaves the revision alone"
    );
}

#[test]
fn a_forced_rebuild_keeps_the_revision_advancing() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["split", "--at", "1"]);
    let before = revision(&s.file(P));
    assert!(before > 0, "edits bump the revision");
    // `init --force` rebuilds the file from scratch. A rebuild must not
    // rewind the revision, or a stale compare-and-swap client would match
    // the replaced model and its write would be accepted.
    s.ok(&["init", "--force"]);
    let after = revision(&s.file(P));
    assert!(
        after > before,
        "the rebuild advanced from {before}, got {after}"
    );
}

#[test]
fn undoable_track_rm_media_rm_imports() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["track", "add", "video"]);
    s.ok(&[
        "add", "blue.mp4", "--track", "2", "--at", "0", "--in", "0", "--out", "1",
    ]);
    s.ok(&["track", "rm", "2"]);
    s.ok(&["undo"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 0.5, BLUE, "the removed track's clip came back");

    // A pool with an unused asset: red+green placed, blue only pooled.
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4", "blue.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["add", "green.mp4", "--at", "2", "--in", "0", "--out", "2"]);
    s.ok(&["media", "rm", "3"]);
    let assets = s.ok(&["media", "ls"]);
    assert!(!assets.contains("#3"), "blue left the pool:\n{assets}");
    s.ok(&["undo"]);
    let assets = s.ok(&["media", "ls"]);
    assert!(assets.contains("#3"), "undo brought it back:\n{assets}");
    s.ok(&["import", "red.mp4"]);
    let assets = s.ok(&["media", "ls"]);
    assert!(
        assets.contains("3 asset(s)"),
        "re-importing dedupes:\n{assets}"
    );
}

#[test]
fn ripple_delete_keeps_a_separate_audio_track_in_sync() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["add", "green.mp4", "--at", "2", "--in", "0", "--out", "2"]);
    s.ok(&["track", "add", "audio"]);
    s.ok(&[
        "add",
        "green.mp4",
        "--track",
        "2",
        "--in",
        "0",
        "--out",
        "2",
    ]);
    s.ok(&[
        "add", "red.mp4", "--track", "2", "--at", "2", "--in", "0", "--out", "2",
    ]);
    // Removing the base head must remove the same span from the audio track.
    s.ok(&["delete", "--clip", "1", "--ripple"]);

    let p = nle_core::Project::load(&s.file(P)).unwrap();
    let base = p.timeline.track(0).unwrap();
    assert_eq!(base.segments.len(), 1, "the base kept only green");
    assert_eq!(base.end(), nle_core::time::Seconds::from_integer(2));
    let audio = p.timeline.track(1).unwrap();
    assert_eq!(audio.segments.len(), 1, "{:?}", audio.segments);
    assert_eq!(audio.start_of(0), nle_core::time::Seconds::from_integer(0));
    assert_eq!(audio.end(), nle_core::time::Seconds::from_integer(2));
    let nle_core::timeline::Segment::Clip(c) = &audio.segments[0] else {
        panic!("expected the surviving audio clip")
    };
    assert!(
        p.asset(c.asset)
            .unwrap()
            .path
            .to_string_lossy()
            .ends_with("red.mp4"),
        "the red audio at 2 s slid to 0 s"
    );
}

#[test]
fn importing_a_directory_is_one_undoable_batch() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    let batch = s.path().join("batch");
    fs::create_dir(&batch).unwrap();
    fs::copy(s.file("red.mp4"), batch.join("red.mp4")).unwrap();
    fs::copy(s.file("green.mp4"), batch.join("green.mp4")).unwrap();
    fs::copy(s.file("notmedia.txt"), batch.join("ignore.txt")).unwrap();

    s.ok(&["import", "batch"]);
    let assets = s.ok(&["media", "ls"]);
    assert!(assets.contains("2 asset(s)"), "{assets}");
    assert!(!assets.contains("ignore.txt"), "{assets}");

    s.ok(&["undo"]);
    let assets = s.ok(&["media", "ls"]);
    assert!(
        assets.contains("media pool is empty") || assets.contains("0 asset(s)"),
        "one undo removes the batch:\n{assets}"
    );
}

#[test]
fn head_material_reaches_back_past_time_zero() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "1", "--out", "3"]);
    s.ok(&["extend", "--clip", "1", "--edge", "in", "--to", "-1"]);
    s.export("h.mp4");
    s.assert_px("h.mp4", 0.5, RED, "the prepended source plays from 0");
    s.assert_dur("h.mp4", 3.0, "3 s");
}

const EDL: &str = "\
TITLE: e
FCM: NON-DROP FRAME

001  AX  V  C  00:00:00:00 00:00:01:00 01:00:00:00 01:00:01:00
* FROM CLIP NAME: red.mp4
002  AX  V  C  00:00:00:00 00:00:01:00 01:00:02:00 01:00:03:00
* FROM CLIP NAME: blue.mp4
";

#[test]
fn edl_import_round_trip() {
    let s = Sandbox::new();
    s.write("e.edl", EDL);
    s.ok(&["import-edl", "e.edl", "--media", ".", "--force"]);
    s.export("edl.mp4");
    s.assert_px("edl.mp4", 0.5, RED, "EDL event 1");
    s.assert_px("edl.mp4", 1.5, BLACK, "the record gap");
    s.assert_px("edl.mp4", 2.5, BLUE, "EDL event 2");
}

/// An EDL whose source out-point runs past the file is clamped to the media
/// (as OTIO and detect-edits are), so the model and the export agree.
#[test]
fn edl_import_clamps_to_the_media() {
    let s = Sandbox::new();
    // red.mp4 is 5 s; this event claims a 9 s source span.
    s.write(
        "e.edl",
        "TITLE: t\nFCM: NON-DROP FRAME\n\n\
         001  AX  V  C  00:00:00:00 00:00:09:00 01:00:00:00 01:00:09:00\n\
         * FROM CLIP NAME: red.mp4\n",
    );
    s.ok(&["import-edl", "e.edl", "--media", ".", "--force"]);
    let show = s.show();
    assert!(
        show.contains("5 s"),
        "the model is clamped to the file: {show}"
    );
    s.export("edl-clamp.mp4");
    s.assert_dur("edl-clamp.mp4", 5.0, "the export matches the clamped model");
}

/// A conform EDL with a dissolve and a wipe imports as hard cuts (each
/// reported), rather than sinking the whole list.
#[test]
fn edl_import_degrades_transitions_to_cuts() {
    let s = Sandbox::new();
    s.write(
        "e.edl",
        "TITLE: t\nFCM: NON-DROP FRAME\n\n\
         001  AX  V  C       00:00:00:00 00:00:02:00 01:00:00:00 01:00:02:00\n\
         * FROM CLIP NAME: red.mp4\n\
         002  AX  V  D  030  00:00:00:00 00:00:02:00 01:00:02:00 01:00:04:00\n\
         * FROM CLIP NAME: blue.mp4\n\
         003  AX  V  W001 030  00:00:00:00 00:00:02:00 01:00:04:00 01:00:06:00\n\
         * FROM CLIP NAME: green.mp4\n",
    );
    let out = s.run(&["import-edl", "e.edl", "--media", ".", "--force"]);
    let text = merged(&out);
    assert!(out.status.success(), "the import should succeed:\n{text}");
    assert!(
        text.contains("imported as a cut"),
        "the non-cut events are reported:\n{text}"
    );
    // The record span is kept whole: three adjacent 2 s cuts.
    let show = s.show();
    assert!(show.contains("6 s"), "the timeline is 6 s: {show}");
    s.export("edl-degrade.mp4");
    s.assert_dur("edl-degrade.mp4", 6.0, "three 2 s cuts");
    s.assert_px("edl-degrade.mp4", 1.0, RED, "event 1");
    s.assert_px("edl-degrade.mp4", 3.0, BLUE, "the dissolve, now a cut");
    s.assert_px("edl-degrade.mp4", 5.0, GREEN, "the wipe, now a cut");
}

/// A black event (`BL`) is empty space, not a clip — the fuzzy reel matcher
/// used to latch it onto `blue.mp4`.
#[test]
fn edl_black_reel_becomes_a_gap() {
    let s = Sandbox::new();
    s.write(
        "e.edl",
        "TITLE: t\nFCM: NON-DROP FRAME\n\n\
         001  AX  V  C  00:00:00:00 00:00:02:00 01:00:00:00 01:00:02:00\n\
         * FROM CLIP NAME: red.mp4\n\
         002  BL  V  C  00:00:00:00 00:00:01:00 01:00:02:00 01:00:03:00\n",
    );
    s.ok(&["import-edl", "e.edl", "--media", ".", "--force"]);
    s.export("edl-black.mp4");
    s.assert_dur("edl-black.mp4", 3.0, "red + a 1 s black span");
    s.assert_px("edl-black.mp4", 1.0, RED, "event 1");
    s.assert_px(
        "edl-black.mp4",
        2.5,
        BLACK,
        "the BL event is black, not blue",
    );
}

const OTIO: &str = r#"{
  "OTIO_SCHEMA": "Timeline.1",
  "tracks": {
    "OTIO_SCHEMA": "Stack.1",
    "children": [
      {
        "OTIO_SCHEMA": "Track.1", "kind": "Video",
        "children": [
          { "OTIO_SCHEMA": "Clip.1",
            "media_reference": { "OTIO_SCHEMA": "ExternalReference.1", "target_url": "red.mp4" },
            "source_range": {
              "OTIO_SCHEMA": "TimeRange.1",
              "start_time": { "OTIO_SCHEMA": "RationalTime.1", "value": 0, "rate": 30 },
              "duration": { "OTIO_SCHEMA": "RationalTime.1", "value": 60, "rate": 30 } } },
          { "OTIO_SCHEMA": "Gap.1",
            "source_range": {
              "OTIO_SCHEMA": "TimeRange.1",
              "start_time": { "OTIO_SCHEMA": "RationalTime.1", "value": 0, "rate": 30 },
              "duration": { "OTIO_SCHEMA": "RationalTime.1", "value": 30, "rate": 30 } } },
          { "OTIO_SCHEMA": "Clip.1",
            "media_reference": { "OTIO_SCHEMA": "ExternalReference.1", "target_url": "green.mp4" },
            "source_range": {
              "OTIO_SCHEMA": "TimeRange.1",
              "start_time": { "OTIO_SCHEMA": "RationalTime.1", "value": 0, "rate": 30 },
              "duration": { "OTIO_SCHEMA": "RationalTime.1", "value": 60, "rate": 30 } } }
        ]
      }
    ]
  }
}"#;

#[test]
fn otio_import_round_trip() {
    let s = Sandbox::new();
    s.write("e.otio", OTIO);
    s.ok(&["import-otio", "e.otio", "--force"]);
    s.export("otio.mp4");
    s.assert_px("otio.mp4", 1.0, RED, "OTIO clip 1");
    s.assert_px("otio.mp4", 2.5, BLACK, "the OTIO gap");
    s.assert_px("otio.mp4", 4.0, GREEN, "OTIO clip 2");
    s.assert_dur("otio.mp4", 5.0, "5 s");
}

#[test]
fn transitions() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["transition", "--at", "2", "--frames", "15"]);
    s.export("t.mp4");
    // Each neighbour gives up 15 f (0.5 s) and the transition takes its own
    // 0.5 s.
    s.assert_dur(
        "t.mp4",
        5.5,
        "a clip|clip transition shortens the timeline by its length",
    );
    s.assert_px("t.mp4", 1.0, RED, "red before the seam");
    s.assert_px("t.mp4", 5.0, BLUE, "blue still follows");
    // The old cut at 2.0 is now a blend: neither side is pure there.
    s.assert_px_ne("t.mp4", 1.75, RED, "the seam is no longer a cut");
    s.assert_px_ne("t.mp4", 1.75, GREEN, "…and not the incoming side either");
    // `--dur` and the fade kind are the same code path.
    s.base();
    s.ok(&["transition", "fade", "--at", "2", "--dur", "0.5"]);
    s.export("t.mp4");
    s.assert_dur("t.mp4", 5.5, "--dur and the fade kind agree");
}

#[test]
fn transition_defaults_to_twelve_frames() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["transition", "--at", "2"]);
    let shown = s.ok(&["show"]);
    assert!(
        shown.contains("(12f)"),
        "default should be 12 frames:\n{shown}"
    );
    s.export("default-transition.mp4");
    s.assert_dur(
        "default-transition.mp4",
        5.6,
        "12 frames at 30 fps removes 0.4 s from the timeline",
    );
}

#[test]
fn parallel_export_splits_at_a_transition_midpoint() {
    let s = Sandbox::new();
    s.base();
    // Keep the scheduler's frame-alignment precondition: replace the 59.94
    // blue source with 30 fps red, then split green at 3. Red 0-2, green
    // 2-4 split at 3, red 4-6; a 15f dissolve at 3 makes the timeline 5.5 s
    // and puts its midpoint at 2.75 s, exactly the two-way target.
    s.ok(&["delete", "--at", "5", "--ripple"]);
    s.ok(&["add", "red.mp4", "--at", "4", "--in", "0", "--out", "2"]);
    s.ok(&["split", "--at", "3"]);
    s.ok(&["transition", "--at", "3", "--frames", "15"]);
    let out = s.ok(&["export", "-o", "parallel.mp4", "--parallel", "2"]);
    assert!(out.contains("2-way"), "scheduler was not used: {out}");
    s.assert_dur(
        "parallel.mp4",
        5.5,
        "parallel chunks concatenate to the timeline length",
    );
    s.assert_px("parallel.mp4", 1.0, RED, "before the split");
    s.assert_px("parallel.mp4", 4.5, RED, "after the split");
    assert!(
        s.has_audio("parallel.mp4"),
        "the final mux must keep the mix"
    );
    s.assert_px_ne(
        "parallel.mp4",
        2.6,
        RED,
        "inside the first half of the blend",
    );
    s.assert_px_ne(
        "parallel.mp4",
        2.9,
        BLUE,
        "inside the second half of the blend",
    );
}

#[test]
fn a_playhead_move_keeps_the_parallel_chunks_cached() {
    use std::os::unix::fs::MetadataExt;

    // The TUI persists the playhead on every seek; it is session state, not
    // render state, so it must not invalidate the chunk cache.
    let s = Sandbox::new();
    s.base();
    s.ok(&["delete", "--at", "5", "--ripple"]);
    s.ok(&["add", "red.mp4", "--at", "4", "--in", "0", "--out", "2"]);
    s.ok(&["split", "--at", "3"]);
    s.ok(&["transition", "--at", "3", "--frames", "15"]);
    s.ok(&["export", "-o", "one.mp4", "--parallel", "2"]);

    let dir = s.cache_dir().join("export");
    // The inode changes when a chunk is re-rendered (temp file + rename), so
    // comparing it is immune to filesystem timestamp granularity.
    let chunks = || {
        let mut found: Vec<(String, u64)> = fs::read_dir(&dir)
            .expect("chunk cache")
            .map(|entry| {
                let entry = entry.expect("cache entry");
                let ino = entry.metadata().expect("chunk metadata").ino();
                (entry.file_name().to_string_lossy().into_owned(), ino)
            })
            .collect();
        found.sort();
        found
    };
    let before = chunks();
    assert_eq!(before.len(), 2, "expected two chunks: {before:?}");

    s.ok(&["seek", "1"]);
    s.ok(&["export", "-o", "two.mp4", "--parallel", "2"]);
    assert_eq!(chunks(), before, "a seek rebuilt the chunks");
}

#[test]
fn a_chunked_hevc_export_is_tagged_hvc1() {
    // Chunks are Matroska, so their `-tag:v hvc1` cannot survive the stream
    // copy; the final mux has to set it or Apple players reject the file.
    let s = Sandbox::new();
    s.base();
    s.ok(&["delete", "--at", "5", "--ripple"]);
    s.ok(&["add", "red.mp4", "--at", "4", "--in", "0", "--out", "2"]);
    s.ok(&["split", "--at", "3"]);
    let out = s.ok(&[
        "export",
        "-o",
        "hevc.mp4",
        "--preset",
        "max",
        "--encoder-preset",
        "ultrafast",
        "--parallel",
        "2",
    ]);
    assert!(out.contains("2-way"), "scheduler was not used: {out}");
    assert_eq!(s.video_tag("hevc.mp4"), "hvc1");
}

#[test]
fn a_fade_at_a_clip_gap_seam_exports_through_black() {
    let s = Sandbox::new();
    s.base();
    // red 0–2, gap 2–3, green 3–5, blue 5–7: the blend's b side is black.
    s.ok(&["gap", "--at", "2", "--frames", "30"]);
    s.ok(&["transition", "fade", "--at", "2", "--frames", "15"]);
    s.export("t.mp4");
    // A black side has no material to give back, so the length holds.
    s.assert_dur("t.mp4", 7.0, "a black-side fade leaves the length");
    s.assert_px("t.mp4", 1.0, RED, "red before the fade");
    s.assert_px_ne("t.mp4", 1.9, RED, "the tail fades toward the black gap");
    s.assert_px("t.mp4", 2.5, BLACK, "the gap is still black");
    s.assert_px("t.mp4", 4.0, GREEN, "green follows");
    // Removing it gives red's tail back without touching the gap.
    s.ok(&["delete", "--at", "1.5"]);
    s.export("t2.mp4");
    s.assert_dur("t2.mp4", 7.0, "still 7 s");
    s.assert_px("t2.mp4", 1.9, RED, "red's whole tail is back");
}

#[test]
fn a_transition_can_be_retimed() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["transition", "--at", "2", "--frames", "15"]);
    s.export("t.mp4");
    s.assert_dur("t.mp4", 5.5, "0.5 s blend");

    // The original cut addresses the blend; 30 frames is one second.
    s.ok(&["transition", "--at", "2", "--frames", "30"]);
    let show = s.show();
    assert!(show.contains("(30f)"), "{show}");
    s.export("t.mp4");
    s.assert_dur("t.mp4", 5.0, "a longer blend consumes another 0.5 s");
    s.assert_frames("t.mp4", 150, "30 fps × 5 s");

    // A rate is relative to the current length: 200% halves it.
    s.ok(&["transition", "--at", "2", "--percent", "200"]);
    s.export("t.mp4");
    s.assert_dur("t.mp4", 5.5, "200% → 15 frames again");

    // One undo restores the pre-retime length.
    s.ok(&["undo"]);
    let show = s.show();
    assert!(show.contains("(30f)"), "undo restored 30 frames:\n{show}");

    // A rate with no blend at that position is refused.
    let out = s.err(&["transition", "--at", "5", "--percent", "50"]);
    assert!(out.contains("no transition"), "{out}");
}

#[test]
fn removing_a_transition_gives_the_frames_back() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["transition", "--at", "2", "--frames", "15"]);
    s.ok(&["delete", "--at", "1.7"]);
    s.export("t.mp4");
    s.assert_dur("t.mp4", 6.0, "the timeline is whole again");
    s.assert_px("t.mp4", 1.0, RED, "red's tail is restored");
    s.assert_px("t.mp4", 3.0, GREEN, "green's head is restored");
    s.assert_px("t.mp4", 5.0, BLUE, "blue is untouched");

    // The same by segment index (`nile show` prints [1] for the transition).
    s.base();
    s.ok(&["transition", "--at", "2", "--frames", "15"]);
    s.ok(&["delete", "--seg", "1"]);
    s.export("t.mp4");
    s.assert_dur("t.mp4", 6.0, "delete --seg restores the neighbours too");
    s.assert_px("t.mp4", 3.0, GREEN, "…green's head is back");
}

#[test]
fn effect_index_replaces_or_refuses() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["effect", "eq", "--clip", "1", "--brightness", "0.1"]);
    s.ok(&["effect", "crop", "--clip", "1", "--x", "10", "--w", "90"]);
    // [eq, crop]: row 0 becomes scale, in place.
    s.ok(&[
        "effect", "scale", "--clip", "1", "--index", "0", "--w", "50", "--h", "50",
    ]);
    let show = s.show();
    assert!(show.contains("fx: scale, crop"), "{show}");

    // A kind already held by another row is refused (one per kind).
    let out = s.err(&[
        "effect", "crop", "--clip", "1", "--index", "0", "--x", "10", "--w", "90",
    ]);
    assert!(out.contains("already has a crop"), "{out}");

    // An out-of-range replacement names the row.
    let out = s.err(&["effect", "eq", "--clip", "1", "--index", "9"]);
    assert!(out.contains("has no effect #9"), "{out}");
}

#[test]
fn effects_eq_neutral_values() {
    let s = Sandbox::new();
    // No eq parameter given: contrast/saturation must stay neutral (this used
    // to come back flat and grey).
    s.base();
    s.ok(&["effect", "eq", "--clip", "1"]);
    s.export("fx.mp4");
    s.assert_px("fx.mp4", 1.0, RED, "eq with no arguments is a no-op");

    // Asking only for brightness must not desaturate either (the defaults for
    // the other two used to be 0 = flat grey).
    s.base();
    s.ok(&["effect", "eq", "--clip", "1", "--brightness", "-0.2"]);
    s.export("fx.mp4");
    s.assert_look("fx.mp4", 1.0, Look::Hue, "brightness alone keeps the hue");

    // …and saturation 0 is the deliberate desaturation.
    s.base();
    s.ok(&["effect", "eq", "--clip", "1", "--saturation", "0"]);
    s.export("fx.mp4");
    s.assert_look("fx.mp4", 1.0, Look::Grey, "saturation 0 desaturates");

    // Effects and a retime compose: 2 s at 2x is 1 s, still grey.
    s.base();
    s.ok(&["effect", "eq", "--clip", "1", "--saturation", "0"]);
    s.ok(&["speed", "1", "200"]);
    s.export("fx.mp4");
    s.assert_dur("fx.mp4", 5.0, "an effect clip retimes (1 s + 2 s + 2 s)");
    s.assert_look("fx.mp4", 0.5, Look::Grey, "…and the effect still applies");

    // An effect clip can't be expressed by `edl://`: the preview plays its
    // proxy. The first call renders it — and must render it *quietly*, since
    // the monitor runs the same code with the TUI on stderr.
    let first = s.run(&["edl", "--track", "1"]);
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        !stderr.contains("ffmpeg"),
        "the proxy render is quiet:\n{stderr}"
    );
    let edl = s.edl();
    assert!(
        !edl.contains("red.mp4"),
        "an effect clip plays its proxy:\n{edl}"
    );
    assert!(edl.contains("/proxy/"), "…from the proxy cache:\n{edl}");
}

#[test]
fn an_effect_survives_the_edits_that_rebuild_a_clip() {
    // trim/split/gap/paste rebuilt the clip with `effects: Vec::new()`, so the
    // next edit silently dropped it. Each case exports and checks the pixel.
    let s = Sandbox::new();

    s.base();
    s.ok(&["effect", "eq", "--clip", "1", "--saturation", "0"]);
    s.ok(&["trim-out", "--at", "1"]);
    s.export("fx.mp4");
    s.assert_look("fx.mp4", 0.5, Look::Grey, "the effect survives trim-out");

    s.base();
    s.ok(&["effect", "eq", "--clip", "1", "--saturation", "0"]);
    s.ok(&["split", "--at", "1"]);
    s.export("fx.mp4");
    s.assert_look(
        "fx.mp4",
        0.5,
        Look::Grey,
        "the effect survives split (head)",
    );
    s.assert_look("fx.mp4", 1.5, Look::Grey, "…and the tail");

    s.base();
    s.ok(&["effect", "eq", "--clip", "1", "--saturation", "0"]);
    s.ok(&["trim-in", "--at", "1"]);
    s.export("fx.mp4");
    s.assert_look("fx.mp4", 1.5, Look::Grey, "the effect survives trim-in");

    s.base();
    s.ok(&["effect", "eq", "--clip", "1", "--saturation", "0"]);
    s.ok(&["gap", "--at", "1", "--frames", "15"]);
    s.export("fx.mp4");
    s.assert_look(
        "fx.mp4",
        0.5,
        Look::Grey,
        "the effect survives insert-gap (head)",
    );
    s.assert_look("fx.mp4", 2.0, Look::Grey, "…and the tail");

    // The paste sweep left the remainder past it without the effect.
    s.base();
    s.ok(&["effect", "eq", "--clip", "2", "--saturation", "0"]);
    s.ok(&["yank", "--clip", "2"]);
    s.ok(&["paste", "--at", "1"]);
    s.export("fx.mp4");
    s.assert_look(
        "fx.mp4",
        3.5,
        Look::Grey,
        "the remainder past a paste keeps its effect",
    );
}

#[test]
fn a_retimed_clip_previews_from_a_proxy() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["speed", "1", "200"]);
    let edl = s.edl();
    assert!(
        !edl.contains("red.mp4"),
        "a 2x clip plays its proxy (an EDL entry can't retime):\n{edl}"
    );
    assert!(edl.contains("/proxy/"), "…from the proxy cache:\n{edl}");
    assert!(
        edl.contains("green.mp4"),
        "an untouched clip still plays its source:\n{edl}"
    );
}

#[test]
fn retiming_a_neighbour_retimes_the_transition_side() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["transition", "--at", "2", "--frames", "15"]);
    s.ok(&["speed", "1", "200"]);
    // red 0..2 at 2x is 1 s, and the dissolve owns the last 0.5 s of it: the
    // clip keeps 0.5 s and the transition's side plays source 1..2, also at
    // 2x. Before, the clip kept 0.75 s and the transition played its span at
    // 1x — a speed step at the seam.
    let show = s.show();
    assert!(
        show.contains("4.5 s"),
        "the retimed contribution (1 s) plus green + blue:\n{show}"
    );
    s.export("ts.mp4");
    // 15 (red) + 15 (transition) + 45 (green) + 60 (blue).
    s.assert_frames("ts.mp4", 135, "the render is the model's 135 frames");
}

#[test]
fn a_rate_that_misses_the_frame_grid_snaps_the_timeline() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["speed", "1", "70"]);
    // 2 s at 70% = 2.857 s = 85.714 frames. The clip takes 86 frames and the
    // rate is 0.7, so every boundary stays on the grid and the source
    // span — the material the user picked — is untouched.
    let show = s.show();
    assert!(
        show.contains("2.866667"),
        "the clip lands on frame 86:\n{show}"
    );
    assert!(
        show.contains("@0.697674"),
        "…at the rate that fits:\n{show}"
    );
    s.export("fr.mp4");
    // The model's frames: 86 (0.7x) + 60 + 60. A source span nudged to fit 86
    // frames would read one extra source frame and render 207.
    s.assert_frames(
        "fr.mp4",
        206,
        "the render is the model's 206 frames (86+60+60)",
    );
}

#[test]
fn an_off_sequence_clip_previews_at_the_sequence_size() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "wide169.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]); // 4:3 sets the sequence
    s.ok(&["add", "wide169.mp4", "--at", "2", "--in", "0", "--out", "2"]);
    s.ok(&["transition", "--at", "2", "--frames", "30"]);
    let edl = s.edl();
    // mpv re-frames a raw file to its own aspect (bars) but plays the
    // transition's proxy already scaled to the sequence, so the wide clip must
    // be a proxy too — otherwise the framing jumps as the monitor leaves the
    // dissolve.
    assert!(
        !edl.contains("wide169.mp4"),
        "the 16:9 clip is proxied, not played raw:\n{edl}"
    );
    s.assert_edl_geometry(&edl, 320, 240, "every entry is the sequence's 320x240");
}

#[test]
fn a_multi_track_preview_renders_the_composite() {
    // Two video layers, both with sound: `edl://` can carry only one source's
    // audio per span, so the monitor plays the rendered composite (the export's
    // own overlay + amix) where they overlap.
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "4"]);
    s.ok(&["track", "add", "video"]);
    s.ok(&[
        "add",
        "green.mp4",
        "--track",
        "2",
        "--at",
        "1",
        "--in",
        "0",
        "--out",
        "2",
    ]);
    let edl = s.edl();
    assert!(
        edl.contains("red.mp4"),
        "the uncovered base plays straight from source:\n{edl}"
    );
    assert!(
        edl.contains("/proxy/"),
        "the overlap is a rendered composite:\n{edl}"
    );
    s.assert_edl_geometry(&edl, 320, 240, "every entry is the sequence's 320x240");

    // The composite is the export's: the opaque overlay wins, and the mix
    // carries both tracks' audio.
    let proxies: Vec<_> = edl_paths(&edl)
        .into_iter()
        .filter(|p| p.to_string_lossy().contains("/proxy/"))
        .collect();
    assert_eq!(proxies.len(), 1, "one span, one render: {proxies:?}");
    let proxy = proxies[0].display().to_string();
    s.assert_px(&proxy, 1.0, GREEN, "the overlay is on top");
    assert!(s.has_audio(&proxy), "the composite carries the mix");
    s.assert_dur(&proxy, 2.0, "the span is [1,3)");
}

#[test]
fn a_cropped_overlay_previews_the_composite() {
    // A crop is a mask, so the monitor must render the overlay to reveal the
    // layer below — the clip's own proxy would show black margins instead.
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "4"]);
    s.ok(&["track", "add", "video"]);
    s.ok(&[
        "add",
        "green.mp4",
        "--track",
        "2",
        "--in",
        "0",
        "--out",
        "2",
    ]);
    s.ok(&["effect", "crop", "--clip", "2", "--x", "50", "--w", "50"]);
    let edl = s.edl();
    assert!(
        edl.contains("red.mp4"),
        "the uncovered base still plays from source:\n{edl}"
    );
    let proxies: Vec<_> = edl_paths(&edl)
        .into_iter()
        .filter(|p| p.to_string_lossy().contains("/proxy/"))
        .collect();
    assert_eq!(proxies.len(), 1, "one composite span: {proxies:?}");
    let proxy = proxies[0].display().to_string();
    s.assert_px_at(&proxy, 0.5, 20, 120, RED, "the crop reveals the base");
    s.assert_px_at(
        &proxy,
        0.5,
        300,
        120,
        GREEN,
        "the overlay keeps its right half",
    );
}

#[test]
fn an_audio_track_is_mixed_into_the_preview() {
    // The monitor used to play the video layer's audio and ignore a separate
    // audio track entirely; its span is now rendered and mixed.
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["track", "add", "audio"]);
    s.ok(&[
        "add",
        "green.mp4",
        "--track",
        "2",
        "--in",
        "0",
        "--out",
        "1",
    ]);
    let edl = s.edl();
    let entries = edl_paths(&edl);
    assert_eq!(entries.len(), 2, "one span + the plain tail:\n{edl}");
    assert!(
        entries[0].to_string_lossy().contains("/proxy/"),
        "the audio track's span is rendered:\n{edl}"
    );
    assert!(
        entries[1].ends_with("red.mp4"),
        "the rest still plays from source:\n{edl}"
    );
    let mixed = entries[0].display().to_string();
    assert!(s.has_audio(&mixed), "the mix carries the audio track");
    s.assert_dur(&mixed, 1.0, "the span is [0,1)");
    s.assert_edl_geometry(&edl, 320, 240, "every entry is the sequence's 320x240");
}

#[test]
fn a_muted_layer_needs_no_composite() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "4"]);
    s.ok(&["track", "add", "video"]);
    s.ok(&[
        "add",
        "green.mp4",
        "--track",
        "2",
        "--at",
        "1",
        "--in",
        "0",
        "--out",
        "2",
    ]);
    s.ok(&["track", "mute", "2"]);
    let edl = s.edl();
    assert!(
        !edl.contains("/proxy/"),
        "a muted layer contributes neither picture nor sound:\n{edl}"
    );
    assert!(edl.contains("red.mp4"), "the base still shows:\n{edl}");
}

#[test]
fn cli_only_guards() {
    let s = Sandbox::new();
    let out = s.err(&["trim", "red.mp4", "--in", "0", "--out", "99", "-o", "t.mp4"]);
    assert!(
        out.contains("beyond the file's duration"),
        "trim refuses an --out past the end:\n{out}"
    );

    s.base();
    let out = s.err(&["effect", "crop", "--clip", "1", "--x", "101"]);
    assert!(
        out.contains("0–100"),
        "an out-of-range crop inset is refused:\n{out}"
    );
    let out = s.err(&["effect", "lut", "--clip", "1", "--path", ""]);
    assert!(
        out.contains("needs --path"),
        "an empty LUT path is refused:\n{out}"
    );
    let out = s.err(&["export", "-o", "."]);
    assert!(
        out.contains("Error opening output"),
        "a failed export names the reason:\n{out}"
    );
    let first = s.ok(&["media", "ls"]);
    assert!(
        first.lines().next().unwrap_or("").contains("asset(s)"),
        "piped output survives an early-closed consumer:\n{first}"
    );

    // Effects are video filters: on an audio track they would be stored but
    // never reach the graph, so the action refuses instead. `--clip` finds the
    // clip's own track (as delete/yank/speed do), so the guard is reached by
    // putting the clip on the audio track.
    s.ok(&["track", "add", "audio"]);
    s.ok(&["add", "red.mp4", "--track", "2", "--in", "0", "--out", "1"]);
    let out = s.err(&["effect", "eq", "--clip", "4"]);
    assert!(
        out.contains("video clips"),
        "an effect on an audio track is refused:\n{out}"
    );
}

// `nile cache prune` is the only thing that deletes proxy renders: age-based
// on the file's mtime, and never implicit.
#[test]
fn cache_prune_is_explicit_and_age_based() {
    use std::time::{Duration, SystemTime};

    let s = Sandbox::new();
    s.base();
    let dir = s.cache_dir().join("proxy");
    fs::create_dir_all(&dir).unwrap();
    let now = SystemTime::now();
    let write = |name: &str, len: usize, age: Duration| {
        let path = dir.join(name);
        fs::write(&path, vec![0u8; len]).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(now - age))
            .unwrap();
    };
    write("aaaa.mp4", 100, Duration::from_secs(31 * 24 * 3600));
    write("bbbb.mp4", 200, Duration::from_secs(3600));

    // An ordinary command sweeps nothing: pruning is explicit.
    s.ok(&["show"]);
    assert!(dir.join("aaaa.mp4").exists(), "no implicit sweep");

    let out = s.ok(&["cache", "prune", "--dry-run"]);
    assert!(out.contains("would remove 1"), "{out}");
    assert!(dir.join("aaaa.mp4").exists(), "a dry run deletes nothing");

    let out = s.ok(&["cache", "prune"]);
    assert!(out.contains("removed 1"), "{out}");
    assert!(
        out.contains("analysis:"),
        "the analysis sample sweep is reported too:\n{out}"
    );
    assert!(!dir.join("aaaa.mp4").exists(), "the old entry is gone");
    assert!(dir.join("bbbb.mp4").exists(), "the young entry is kept");

    let out = s.err(&["cache", "prune", "--days", "0"]);
    assert!(out.contains("at least 1"), "0 days is refused:\n{out}");
}

#[test]
fn the_default_project_is_nile_json() {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nile"))
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("run init");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.path().join("nile.json").exists());
    assert!(!dir.path().join("nle.json").exists());
}

#[test]
fn the_default_project_can_be_configured_per_directory() {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_nile"))
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("run nile")
    };

    let out = run(&["default", "trip.json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.path().join(".nile/config.json").exists());

    let out = run(&["init"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.path().join("trip.json").exists());

    let out = run(&["show"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("trip.json"));

    // --project still wins over the directory default.
    let out = run(&["-p", "other.json", "init"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run(&["show", "-p", "other.json"]);
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("other.json"));

    let out = run(&["default", "--unset"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!dir.path().join(".nile/config.json").exists());
    let out = run(&["default"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("nile.json (fallback)"));
}

#[test]
fn edit_refuses_a_missing_terminal() {
    // `Command::output()` gives nile pipes, not a TTY. ratatui would panic
    // while taking the terminal, breaking the CLI's 0/1/2 exit contract.
    let s = Sandbox::new();
    s.base();
    let out = s.run(&["edit"]);
    let text = merged(&out);
    assert!(!text.contains("panicked"), "{text}");
    assert!(
        out.status.code().is_some_and(|code| code <= 2),
        "edit exited {:?}: {text}",
        out.status.code()
    );
    #[cfg(feature = "tui")]
    assert!(text.contains("needs a terminal"), "{text}");
}

#[cfg(target_os = "macos")]
#[test]
fn the_preview_preset_encodes_with_videotoolbox() {
    // Linux has no VideoToolbox, so the macOS job is the only place this
    // argv runs end to end: the platform engine, the yuv420p tail and the
    // pixels. `hardware_encoder()` picking VAAPI here would fall back to CPU.
    let s = Sandbox::new();
    s.base();
    let out = s.ok(&["export", "-o", "pv.mp4", "--preset", "preview"]);
    assert!(
        !out.contains("CPU fallback"),
        "preview fell back to CPU: {out}"
    );
    assert_eq!(s.video_tag("pv.mp4"), "avc1", "not H.264: {out}");
    s.assert_dur("pv.mp4", 6.0, "the preview keeps the sequence length");
    s.assert_px("pv.mp4", 1.0, RED, "red under VideoToolbox");
    s.assert_px("pv.mp4", 3.0, GREEN, "green under VideoToolbox");
    s.assert_px("pv.mp4", 5.0, BLUE, "blue under VideoToolbox");
}

#[cfg(target_os = "macos")]
#[test]
fn a_missing_hardware_encoder_retries_on_cpu() {
    // macOS has no /dev/dri, so VAAPI cannot initialize: the whole graph must
    // be retried on libx264 and say so, not fail. No Linux CI can assert this
    // because VAAPI may actually be there.
    let s = Sandbox::new();
    s.base();
    let out = s.ok(&["export", "-o", "fallback.mp4", "--encoder", "vaapi"]);
    assert!(out.contains("CPU fallback"), "no fallback notice: {out}");
    s.assert_dur("fallback.mp4", 6.0, "the retry renders the same film");
    s.assert_px("fallback.mp4", 3.0, GREEN, "the retry's pixels");
}

// The stored playhead is what the next command and the TUI open on, so a
// length-changing CLI edit must leave it inside the timeline — `run_cli_action`
// used to persist no cursor at all, leaving it past the new end.
#[test]
fn a_cli_edit_keeps_the_stored_playhead_inside_the_timeline() {
    let s = Sandbox::new();
    s.base(); // red 0–2, green 2–4, blue 4–6
    s.ok(&["seek", "5"]);
    let out = s.ok(&["delete", "--at", "5", "--ripple"]);
    assert!(!out.contains("playhead f"), "a delete has no frame: {out}");
    let loaded = nle_core::Project::load(&s.file(P)).expect("the edit persisted");
    assert!(
        loaded.playhead <= loaded.last_frame(),
        "stored {} but the last frame is {}",
        nle_core::format_seconds(&loaded.playhead),
        nle_core::format_seconds(&loaded.last_frame())
    );

    // An edit that does carry a frame announces the one it stored, not the
    // frame it asked for (undo restores the position the delete was made from).
    let out = s.ok(&["undo"]);
    let loaded = nle_core::Project::load(&s.file(P)).expect("the undo persisted");
    let announced = out
        .rsplit("(playhead f")
        .next()
        .and_then(|tail| tail.split(')').next())
        .and_then(|frame| frame.parse::<i64>().ok())
        .unwrap_or_else(|| panic!("no playhead frame in: {out}"));
    let announced_at = nle_core::frame_to_secs(loaded.fps.as_ref(), announced);
    assert!(
        (announced_at - nle_core::secs_f64(&loaded.playhead)).abs() < 1e-9,
        "announced f{announced} ({announced_at} s) but stored {} s",
        nle_core::secs_f64(&loaded.playhead)
    );
}

// A LUT path crosses two parsers: `:` splits filter options, `,` separates
// option values and `'` closes the graph quote, so each needs its own escape.
// The unit test pins the emitted string; only a real ffmpeg run proves the
// string parses, and a comma needs a different escape from the others.
#[test]
fn a_lut_path_with_metacharacters_exports() {
    const CUBE: &str = "LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n";
    for name in ["a:b'c[;x].cube", "a,b:c'[;x].cube"] {
        let s = Sandbox::new();
        s.base();
        let lut = s.file(name);
        std::fs::write(&lut, CUBE).expect("write the LUT");
        s.ok(&[
            "effect",
            "lut",
            "--clip",
            "1",
            "--path",
            lut.to_str().expect("utf-8 path"),
        ]);
        s.ok(&["export", "-o", "lut.mp4"]);
        s.assert_dur(
            "lut.mp4",
            6.0,
            &format!("the export parses the escaped path {name:?}"),
        );
    }
}

// A directory symlink is a second spelling of the directory, not a canonical
// path: an edit addressed through `alias/nile.json` must still keep its state
// in the real tree. This says nothing about a symlinked *file* — whether that
// is canonicalized stays a separate finding.
#[cfg(unix)]
#[test]
fn a_symlinked_project_directory_keeps_state_beside_the_real_file() {
    let s = Sandbox::new();
    s.base(); // red 0–2, green 2–4, blue 4–6
    let real = s.path().join("real");
    fs::create_dir_all(&real).expect("real/");
    fs::copy(s.file(P), real.join("nile.json")).expect("copy the project");
    std::os::unix::fs::symlink("real", s.path().join("alias")).expect("alias -> real");

    // An edit addressed through the link …
    s.ok_on("alias/nile.json", &["split", "--at", "1"]);

    // … persisted in the file the link points at …
    let text = fs::read_to_string(real.join("nile.json")).expect("the real project file");
    let project: serde_json::Value = serde_json::from_str(&text).expect("the project JSON");
    let segments = &project["sequences"][0]["timeline"]["tracks"][0]["segments"];
    assert!(
        segments.as_array().is_some_and(|segs| segs.len() >= 4),
        "the split through the symlink did not land: {text}"
    );

    // `alias` is a symlink to `real`, so `alias/.nile` and `real/.nile` are
    // the same directory: what matters is that the history lands in the real
    // tree, and that no second `.nile` appears beside the sandbox's other
    // entries.
    assert!(
        real.join(".nile")
            .join("nile")
            .join("history.json")
            .is_file(),
        "history is not under real/.nile/nile: {:?}",
        fs::read_dir(&real)
            .map(|entries| entries.flatten().map(|e| e.path()).collect::<Vec<_>>())
            .unwrap_or_default()
    );
    assert!(
        !s.path().join(".nile").exists(),
        "state leaked into the sandbox root beside the alias"
    );
}

// A symlinked project *file* is the same project under another name: an edit
// made through `alias.json` must be visible when the real spelling undoes it.
// `state_dir` canonicalizes the project path, so both spellings key the same
// `.nile/p/` history instead of an `.nile/alias/` of their own.
#[cfg(unix)]
#[test]
fn a_symlinked_project_file_shares_one_history() {
    let s = Sandbox::new();
    s.base(); // red 0–2, green 2–4, blue 4–6
    std::os::unix::fs::symlink("p.json", s.path().join("alias.json"))
        .expect("alias.json -> p.json");

    let segments = |s: &Sandbox| -> usize {
        let project: serde_json::Value =
            serde_json::from_str(&s.read(P)).expect("the project JSON");
        project["sequences"][0]["timeline"]["tracks"][0]["segments"]
            .as_array()
            .expect("segments")
            .len()
    };
    assert_eq!(segments(&s), 3, "the base fixture's three clips");

    // An edit through the alias …
    s.ok_on("alias.json", &["split", "--at", "1"]);
    assert_eq!(segments(&s), 4, "the split through the alias persisted");

    // … is undone through the real spelling, so the two share one history.
    s.ok(&["undo"]);
    assert_eq!(segments(&s), 3, "the real spelling undid the alias's split");

    // State lives under the real file's stem, never the alias's. Check
    // existence physically: /var vs /private/var makes macOS paths differ
    // lexically while naming the same directory.
    assert!(
        s.path()
            .join(".nile")
            .join("p")
            .join("history.json")
            .is_file(),
        "history is not under .nile/p"
    );
    assert!(
        !s.path().join(".nile").join("alias").exists(),
        "the alias grew its own state directory: {:?}",
        fs::read_dir(s.path().join(".nile"))
            .map(|entries| entries.flatten().map(|e| e.path()).collect::<Vec<_>>())
            .unwrap_or_default()
    );
}
