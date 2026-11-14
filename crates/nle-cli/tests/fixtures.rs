//! The golden fixture show: a 2-cam EDL/OTIO/ATEM set with a generated
//! program reference. `cargo test` runs this, so `scripts/check.sh` gates it.

mod common;

use std::path::Path;
use std::process::Command;

use common::Sandbox;
use serde_json::{Value, json};

const SHOW_EDL: &str = include_str!("../../../fixtures/show.edl");
const SHOW_OTIO: &str = include_str!("../../../fixtures/show.otio");
const ATEM_LOG: &str = include_str!("../../../fixtures/atem.log");
const EXPECTED_PROJECT: &str = include_str!("../../../fixtures/expected/project.json");
const EXPECTED_MONITOR: &str = include_str!("../../../fixtures/expected/monitor.edl");
const EXPECTED_FRAMES: &str = include_str!("../../../fixtures/expected/frames.txt");

#[test]
fn the_fixture_show_imports_to_the_golden_project() {
    let sandbox = fixture_sandbox();
    let expected: Value = serde_json::from_str(EXPECTED_PROJECT).expect("golden project JSON");

    // The CMX EDL is the video cut list; the mic is a separate audio track.
    sandbox.ok(&["import-edl", "show.edl", "--media", ".", "--force"]);
    sandbox.ok(&["track", "add", "audio"]);
    sandbox.ok(&[
        "add", "mic.m4a", "--track", "2", "--at", "0", "--in", "0", "--out", "6",
    ]);
    let edl: Value =
        serde_json::from_str(&sandbox.ok(&["show", "--json"])).expect("EDL project JSON");

    // The Resolve-style OTIO carries the same cuts plus the mic track.
    sandbox.ok_on("show-otio.json", &["import-otio", "show.otio", "--force"]);
    let otio: Value = serde_json::from_str(&sandbox.ok_on("show-otio.json", &["show", "--json"]))
        .expect("OTIO project JSON");

    assert_eq!(normalized(&edl), expected, "CMX EDL import");
    assert_eq!(normalized(&otio), expected, "OTIO import");
    assert_atem_matches(&expected);
}

#[test]
fn the_fixture_show_exports_and_matches_the_program() {
    let sandbox = fixture_sandbox();
    sandbox.ok(&["import-edl", "show.edl", "--media", ".", "--force"]);
    sandbox.ok(&["track", "add", "audio"]);
    sandbox.ok(&[
        "add", "mic.m4a", "--track", "2", "--at", "0", "--in", "0", "--out", "6",
    ]);

    assert_eq!(
        monitor_segments(&sandbox.ok(&["edl", "--track", "1"])),
        expected_monitor(),
        "monitor EDL"
    );

    let frames: i64 = EXPECTED_FRAMES.trim().parse().expect("frame count");
    sandbox.export("show-export.mp4");
    assert_eq!(
        sandbox.frame_count("show-export.mp4"),
        frames,
        "export frame count"
    );

    program(&sandbox);
    for (frame, at) in [(15, "0.5"), (75, "2.5"), (135, "4.5")] {
        let psnr = frame_psnr(&sandbox, "show-export.mp4", "program.mp4", frame);
        assert!(
            psnr >= 30.0,
            "PSNR at {at} s (frame {frame}) is {psnr:.2} dB"
        );
    }
}

fn fixture_sandbox() -> Sandbox {
    let sandbox = Sandbox::fixtures();
    sandbox.write("show.edl", SHOW_EDL);
    sandbox.write("show.otio", SHOW_OTIO);
    sandbox.write("atem.log", ATEM_LOG);
    sandbox
}

/// The project shape the fixture is about: asset names and spans, not the
/// absolute temp paths or ffprobe reports.
fn normalized(project: &Value) -> Value {
    let media: Vec<String> = project["media"]
        .as_array()
        .expect("media")
        .iter()
        .map(|asset| {
            Path::new(asset["path"].as_str().expect("path"))
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    let active = project["active"].as_u64().expect("active sequence");
    let sequence = project["sequences"]
        .as_array()
        .expect("sequences")
        .iter()
        .find(|sequence| sequence["id"].as_u64() == Some(active))
        .expect("active sequence");
    let tracks: Vec<Value> = sequence["timeline"]["tracks"]
        .as_array()
        .expect("tracks")
        .iter()
        .map(|track| {
            let segments: Vec<Value> = track["segments"]
                .as_array()
                .expect("segments")
                .iter()
                .map(|segment| {
                    if let Some(clip) = segment.get("Clip") {
                        let asset = clip["asset"].as_u64().expect("asset id") as usize;
                        json!({
                            "kind": "clip",
                            "id": clip["id"],
                            "asset": media[asset - 1],
                            "in": clip["asset_in"],
                            "out": clip["asset_out"],
                            "speed": clip["speed"],
                        })
                    } else if let Some(gap) = segment.get("Gap") {
                        json!({"kind": "gap", "duration": gap["duration"]})
                    } else if let Some(transition) = segment.get("Transition") {
                        json!({
                            "kind": "transition",
                            "id": transition["id"],
                            "dur": transition["dur"],
                            "transition": transition["kind"],
                        })
                    } else {
                        panic!("unknown segment {segment}");
                    }
                })
                .collect();
            json!({
                "kind": track["kind"],
                "muted": track["muted"],
                "segments": segments,
            })
        })
        .collect();

    json!({
        "fps": sequence["fps"],
        "video": sequence["video"],
        "tracks": tracks,
    })
}

/// The ATEM program-bus cuts must agree with the golden video track.
fn assert_atem_matches(expected: &Value) {
    let cuts: Vec<(f64, u32)> = ATEM_LOG
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let mut parts = line.split_whitespace();
            let time = parts.next()?;
            let bus = parts.next()?;
            let camera = parts.next()?.parse::<u32>().ok()?;
            if bus != "PGM" {
                return None;
            }
            let t = nle_core::parse_time(time).ok()?;
            Some((nle_core::secs_f64(&t), camera))
        })
        .collect();

    let video = expected["tracks"][0]["segments"]
        .as_array()
        .expect("video segments");
    assert_eq!(cuts.len(), video.len(), "one ATEM cut per video clip");

    let mut start = 0.0;
    for (i, (at, camera)) in cuts.iter().enumerate() {
        let clip = &video[i];
        assert!(
            (start - at).abs() < 1e-9,
            "ATEM cut {i} at {at} s, clip starts at {start} s"
        );
        let wanted = match camera {
            1 => "cam_a.mp4",
            2 => "cam_b.mp4",
            other => panic!("unexpected camera {other}"),
        };
        assert_eq!(clip["asset"], wanted, "ATEM camera {camera} at {at} s");
        start += duration(clip);
    }
}

fn duration(clip: &Value) -> f64 {
    let rate = |key: &str| -> f64 {
        let text = clip[key].as_str().expect("rational");
        let (num, den) = text.split_once('/').expect("numerator/denominator");
        num.parse::<f64>().expect("numerator") / den.parse::<f64>().expect("denominator")
    };
    (rate("out") - rate("in")) / rate("speed")
}

/// Parse the `nile edl` URL (or the expected line format) into
/// `(file, source offset, timeline length)`.
fn monitor_segments(text: &str) -> Vec<(String, f64, f64)> {
    if let Some(rest) = text.trim().strip_prefix("edl://") {
        return rest
            .split(';')
            .map(|entry| {
                let mut fields = entry.rsplitn(3, ',');
                let length = fields.next().expect("length").parse().expect("length");
                let offset = fields.next().expect("offset").parse().expect("offset");
                let path = fields.next().expect("path");
                // mpv EDL prefixes a path with its byte length: `%36%/…`.
                let path = path.rsplit_once('%').map_or(path, |(_, path)| path);
                let name = Path::new(path)
                    .file_name()
                    .expect("file name")
                    .to_string_lossy()
                    .into_owned();
                (name, offset, length)
            })
            .collect();
    }
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (name, rest) = line.split_once(',')?;
            let (offset, length) = rest.split_once(',')?;
            Some((name.to_string(), offset.parse().ok()?, length.parse().ok()?))
        })
        .collect()
}

fn expected_monitor() -> Vec<(String, f64, f64)> {
    monitor_segments(EXPECTED_MONITOR)
}

/// A standalone program render from the fixture's cuts. The export is
/// compared against this, not against a flat colour, so a wrong cut or source
/// span shows up as a PSNR drop.
fn program(sandbox: &Sandbox) {
    let filter = "[0:v]trim=start=0:end=2,setpts=PTS-STARTPTS[v0];\
                  [1:v]trim=start=0:end=2,setpts=PTS-STARTPTS[v1];\
                  [0:v]trim=start=2:end=4,setpts=PTS-STARTPTS[v2];\
                  [v0][v1][v2]concat=n=3:v=1:a=0[v]";
    let out = Command::new("ffmpeg")
        .current_dir(sandbox.path())
        .args([
            "-v",
            "error",
            "-y",
            "-i",
            "cam_a.mp4",
            "-i",
            "cam_b.mp4",
            "-filter_complex",
            filter,
            "-map",
            "[v]",
            "-c:v",
            "libx264",
            "-preset",
            "fast",
            "-crf",
            "18",
            "-pix_fmt",
            "yuv420p",
            "-an",
            "program.mp4",
        ])
        .output()
        .expect("spawn ffmpeg");
    assert!(
        out.status.success(),
        "program render failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// PSNR between one frame of the export and the same frame of the program.
fn frame_psnr(sandbox: &Sandbox, export: &str, program: &str, frame: i64) -> f64 {
    let select = format!("select=eq(n\\,{frame})");
    for (input, output) in [(export, "export.png"), (program, "program.png")] {
        let out = Command::new("ffmpeg")
            .current_dir(sandbox.path())
            .args([
                "-v",
                "error",
                "-y",
                "-i",
                input,
                "-vf",
                &select,
                "-frames:v",
                "1",
                output,
            ])
            .output()
            .expect("spawn ffmpeg");
        assert!(
            out.status.success(),
            "frame extraction failed ({input}):\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = Command::new("ffmpeg")
        .current_dir(sandbox.path())
        .args([
            "-v",
            "info",
            "-i",
            "export.png",
            "-i",
            "program.png",
            "-lavfi",
            "psnr",
            "-f",
            "null",
            "-",
        ])
        .output()
        .expect("spawn ffmpeg");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let psnr = stderr
        .split("average:")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| {
            value
                .parse::<f64>()
                .ok()
                .or_else(|| (value == "inf").then_some(f64::INFINITY))
        })
        .unwrap_or_else(|| panic!("no PSNR in ffmpeg output:\n{stderr}"));
    let _ = std::fs::remove_file(sandbox.file("export.png"));
    let _ = std::fs::remove_file(sandbox.file("program.png"));
    psnr
}
