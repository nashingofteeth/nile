//! ffprobe wrapper: ask ffmpeg's probe tool what's inside a file,
//! parse the JSON answer into typed structs.
//!
//! The types (RawProbe etc.) live in nle-core::media — pure data shared
//! with the project file. This module only owns the *process* side.

use std::path::Path;
use std::process::Command;

use nle_core::media::RawProbe;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("no such file: {0}")]
    MissingFile(String),
    #[error("could not run ffprobe: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("ffprobe failed ({status}): {message}")]
    Failed {
        status: std::process::ExitStatus,
        message: String,
    },
    #[error("could not parse ffprobe output: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Count the actual video frames in `path`, without decoding unless the
/// container has no `nb_frames` metadata. `None` means ffprobe could not answer;
/// callers can fall back to the plan's frame count.
pub fn frame_count(path: &Path) -> Option<i64> {
    if !path.is_file() {
        return None;
    }
    let probe_int = |args: &[&str]| {
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0"])
            .args(args)
            .arg("--")
            .arg(path)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|line| line.trim().parse::<i64>().ok())
            .filter(|n| *n >= 0)
    };
    probe_int(&[
        "-show_entries",
        "stream=nb_frames",
        "-of",
        "default=nw=1:nk=1",
    ])
    .or_else(|| {
        probe_int(&[
            "-count_frames",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "default=nw=1:nk=1",
        ])
    })
}

/// Video keyframe timestamps in the first video stream, in seconds and
/// sorted. Packets are read first (no decode); a container that doesn't flag
/// keyframe packets falls back to a keyframe-only frame scan. Audio-only
/// files return an empty list.
pub fn keyframes(path: &Path) -> Result<Vec<f64>, ProbeError> {
    if !path.is_file() {
        return Err(ProbeError::MissingFile(path.display().to_string()));
    }
    let packet_scan = run_probe(
        path,
        &[
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time,flags",
            "-of",
            "csv=p=0",
        ],
    )?;
    let mut times = parse_packet_keyframes(&packet_scan);
    if times.is_empty() {
        let frame_scan = run_probe(
            path,
            &[
                "-select_streams",
                "v:0",
                "-skip_frame",
                "nokey",
                "-show_entries",
                "frame=pts_time",
                "-of",
                "csv=p=0",
            ],
        )?;
        times = parse_frame_keyframes(&frame_scan);
    }
    times.sort_by(f64::total_cmp);
    times.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    Ok(times)
}

fn run_probe(path: &Path, args: &[&str]) -> Result<Vec<u8>, ProbeError> {
    let out = Command::new("ffprobe")
        .args(["-v", "error"])
        .args(args)
        .arg("--")
        .arg(path)
        .output()?;
    if !out.status.success() {
        return Err(ProbeError::Failed {
            status: out.status,
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(out.stdout)
}

/// Parse ffprobe's `pts_time,flags` CSV. Only packets whose flags contain
/// `K` are keyframes.
fn parse_packet_keyframes(bytes: &[u8]) -> Vec<f64> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| {
            let (time, flags) = line.split_once(',')?;
            if !flags.trim().contains('K') {
                return None;
            }
            finite_time(time)
        })
        .collect()
}

/// Parse ffprobe's keyframe-only frame timestamps (one per line).
fn parse_frame_keyframes(bytes: &[u8]) -> Vec<f64> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(finite_time)
        .collect()
}

fn finite_time(text: &str) -> Option<f64> {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|t| t.is_finite() && *t >= 0.0)
}

/// Run ffprobe on `path` and return the parsed report.
pub fn probe(path: &Path) -> Result<RawProbe, ProbeError> {
    if !path.is_file() {
        return Err(ProbeError::MissingFile(path.display().to_string()));
    }
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg("--")
        .arg(path)
        .output()?;
    if !out.status.success() {
        return Err(ProbeError::Failed {
            status: out.status,
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_flagged_keyframe_packets() {
        let csv = b"0.000000,K__\n0.033333,___\n1.500000,K__\n2.000000,__K\nnonsense\n";
        let times = parse_packet_keyframes(csv);
        assert_eq!(times, vec![0.0, 1.5, 2.0]);
    }

    #[test]
    fn parses_keyframe_frame_times_and_rejects_junk() {
        let csv = b"0.000000\n1.250000\nN/A\n-1.0\n3.5\n";
        let times = parse_frame_keyframes(csv);
        assert_eq!(times, vec![0.0, 1.25, 3.5]);
    }

    #[test]
    fn real_video_keyframes_are_sorted_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("kf.mp4");
        // A short GOP so there are several keyframes to find.
        let out = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x120:rate=30:duration=2",
                "-g",
                "15",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&media)
            .status()
            .unwrap();
        assert!(out.success(), "generate video");
        let times = keyframes(&media).unwrap();
        assert!(times.len() >= 3, "expected several keyframes: {times:?}");
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
        assert!(times[0] >= 0.0);
        assert!(times.last().is_some_and(|t| *t < 2.1), "{times:?}");
    }
}
