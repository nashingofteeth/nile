//! Cutting segments: translate a TrimSpec into ffmpeg arguments and run it.
//!
//! Strategy: `-ss` BEFORE `-i` = ffmpeg seeks at the demuxer (fast) then
//! decodes precisely up to the target frame — fast AND frame-accurate.
//! `-t` after input limits how long the output runs.
//!
//! The helpers here (device selection, codec args, the process runner)
//! are shared with the export graph compiler in `graph.rs`.

use std::path::PathBuf;
use std::process::Command;

use nle_core::{Seconds, format_seconds};
use thiserror::Error;

use crate::paths::local_path_arg;

/// The default VAAPI render node when `/dev/dri` cannot be read.
const DEFAULT_VAAPI_DEVICE: &str = "/dev/dri/renderD128";

/// The VAAPI render node. Picks the first `/dev/dri/renderD*` at runtime —
/// the node number can change after a driver/kernel update — overridable
/// with `NLE_VAAPI_DEVICE`. Falls back to `renderD128` when the directory
/// can't be read.
pub fn vaapi_device() -> String {
    if let Ok(p) = std::env::var("NLE_VAAPI_DEVICE") {
        return p;
    }
    pick_render_node(read_render_nodes()).unwrap_or_else(|| DEFAULT_VAAPI_DEVICE.to_string())
}

/// Lowest-numbered render node, as an absolute path.
fn pick_render_node(mut nodes: Vec<String>) -> Option<String> {
    nodes.sort();
    nodes.into_iter().next().map(|n| format!("/dev/dri/{n}"))
}

fn read_render_nodes() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/dev/dri") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("renderD"))
        .collect()
}

/// The encode engines: Intel UHD 630 for VAAPI + QSV on Linux, VideoToolbox
/// on macOS, libx264/x265/SVT-AV1 in software.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    /// libx264 — software, best quality per bit, many CPU cores.
    Cpu,
    /// Intel UHD 630 via VAAPI — near-zero CPU cost (previews/proxies).
    Vaapi,
    /// Intel Quick Sync (same GPU, different API).
    Qsv,
    /// macOS VideoToolbox H.264 — the media engine, near-zero CPU cost.
    VideoToolbox,
    /// libx265 — software HEVC, archival.
    X265,
    /// libsvtav1 — software AV1, archival.
    SvtAv1,
}

impl Encoder {
    /// Encoders that need hardware and can therefore fail at runtime; the
    /// export paths retry those on [`Encoder::Cpu`].
    pub fn is_hardware(self) -> bool {
        matches!(self, Encoder::Vaapi | Encoder::Qsv | Encoder::VideoToolbox)
    }

    /// Human name for notices ("VAAPI export failed, retrying on CPU").
    pub fn label(self) -> &'static str {
        match self {
            Encoder::Cpu => "CPU",
            Encoder::Vaapi => "VAAPI",
            Encoder::Qsv => "QSV",
            Encoder::VideoToolbox => "VideoToolbox",
            Encoder::X265 => "x265",
            Encoder::SvtAv1 => "SVT-AV1",
        }
    }
}

/// The hardware engine this platform should try first: VideoToolbox on
/// macOS, VAAPI elsewhere (QSV stays an explicit choice). One branch so the
/// preview preset and its fallback cannot drift apart.
pub fn hardware_encoder() -> Encoder {
    if cfg!(target_os = "macos") {
        Encoder::VideoToolbox
    } else {
        Encoder::Vaapi
    }
}

#[derive(Debug, Error)]
pub enum FfmpegError {
    #[error("could not run ffmpeg: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("ffmpeg failed ({status}): {tail}")]
    Failed {
        status: std::process::ExitStatus,
        /// The tail of ffmpeg's stderr — where the actual reason lives
        /// ("Error opening output file …", "Invalid data found …"). The
        /// exit status alone tells the user nothing.
        tail: String,
    },
    /// The caller asked to stop; the child was killed. Not a render failure:
    /// a job queue turns this into `cancelled`, not `failed`.
    #[error("ffmpeg was cancelled")]
    Cancelled,
}

#[derive(Debug, Error)]
pub enum TrimError {
    #[error("invalid range: end ({end}) must be after start ({start})")]
    Range { start: String, end: String },
    #[error(transparent)]
    Ffmpeg(#[from] FfmpegError),
}

/// Everything needed to perform one cut.
#[derive(Debug, Clone)]
pub struct TrimSpec {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Inclusive start (seconds, exact).
    pub start: Seconds,
    /// Exclusive end (seconds, exact).
    pub end: Seconds,
    pub encoder: Encoder,
    /// Stream copy instead of re-encode: instant, starts exactly at
    /// `start` (mp4 edit list), end lands within a frame or two.
    pub copy: bool,
    pub crf: u8,
    pub preset: String,
    pub bitrate: String,
}

/// Global args that select and initialize a hardware device.
///
/// `hw_decode` also decodes the input on the GPU (QSV trim does; the
/// export graph decodes in software so filters can process frames).
pub fn device_args(encoder: Encoder, hw_decode: bool) -> Vec<String> {
    let mut a = Vec::new();
    match encoder {
        Encoder::Cpu => {}
        Encoder::Vaapi => {
            a.push("-init_hw_device".into());
            a.push(format!("vaapi=va:{}", vaapi_device()));
            a.push("-filter_hw_device".into());
            a.push("va".into());
        }
        Encoder::Qsv => {
            a.push("-init_hw_device".into());
            a.push("qsv=qsv".into());
            if hw_decode {
                a.push("-hwaccel".into());
                a.push("qsv".into());
            }
        }
        // VideoToolbox takes software frames and uploads them itself; it has
        // no `-init_hw_device` equivalent.
        Encoder::VideoToolbox | Encoder::X265 | Encoder::SvtAv1 => {}
    }
    a
}

/// Output-side codec args (`-c:v ...`). Video only; audio is always AAC.
pub fn video_encode_args(encoder: Encoder, crf: u8, preset: &str, bitrate: &str) -> Vec<String> {
    let mut a = Vec::new();
    match encoder {
        Encoder::Cpu => {
            a.push("-c:v".into());
            a.push("libx264".into());
            a.push("-preset".into());
            a.push(preset.into());
            a.push("-crf".into());
            a.push(crf.to_string());
        }
        Encoder::Vaapi => {
            a.push("-c:v".into());
            a.push("h264_vaapi".into());
            a.push("-b:v".into());
            a.push(bitrate.into());
        }
        Encoder::Qsv => {
            a.push("-c:v".into());
            a.push("h264_qsv".into());
            a.push("-b:v".into());
            a.push(bitrate.into());
        }
        Encoder::VideoToolbox => {
            a.push("-c:v".into());
            a.push("h264_videotoolbox".into());
            // Hardware is preferred; this only permits Apple's software
            // encoder when the media engine is busy or absent (a VM), where
            // the alternative is discarding the render and retrying on CPU.
            a.push("-allow_sw".into());
            a.push("1".into());
            a.push("-b:v".into());
            a.push(bitrate.into());
        }
        Encoder::X265 => {
            a.push("-c:v".into());
            a.push("libx265".into());
            a.push("-preset".into());
            a.push(preset.into());
            a.push("-crf".into());
            a.push(crf.to_string());
            a.push("-tag:v".into());
            a.push("hvc1".into());
        }
        Encoder::SvtAv1 => {
            a.push("-c:v".into());
            a.push("libsvtav1".into());
            a.push("-preset".into());
            a.push(preset.into());
            a.push("-crf".into());
            a.push(crf.to_string());
        }
    }
    a
}

/// Run ffmpeg, streaming its stderr live (progress belongs on our stderr)
/// while keeping the last few lines: when it fails, the error must name
/// the cause — a bare exit status tells the user nothing.
pub fn run_ffmpeg(args: &[String]) -> Result<(), FfmpegError> {
    run_ffmpeg_opt(args, true)
}

/// [`run_ffmpeg`] with the live echo optional. A caller whose stderr is a
/// terminal UI (the monitor's proxy render) must not have ffmpeg paint over
/// the screen — the tail is still captured, so a failure still names the
/// cause.
pub fn run_ffmpeg_opt(args: &[String], echo: bool) -> Result<(), FfmpegError> {
    run_ffmpeg_inner(args, echo, None, None)
}

/// Run ffmpeg and report its output position (seconds) as it goes.
///
/// Uses `-progress pipe:1`, ffmpeg's machine-readable key/value stream, so
/// the caller can turn it into a percentage (the monitor's proxy renders can
/// take seconds). The normal path adds no flags and keeps stdout null, so a
/// plain export's command line is unchanged.
pub fn run_ffmpeg_progress(
    args: &[String],
    echo: bool,
    progress: &mut dyn FnMut(f64),
) -> Result<(), FfmpegError> {
    run_ffmpeg_inner(args, echo, Some(progress), None)
}

/// [`run_ffmpeg_progress`] that stops the child when `cancel` turns true.
/// The kill happens on the caller's polling thread; the stderr reader is
/// joined, so no thread is left behind and the caller can safely rename or
/// delete the partial output.
pub fn run_ffmpeg_progress_cancel(
    args: &[String],
    echo: bool,
    progress: &mut dyn FnMut(f64),
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(), FfmpegError> {
    run_ffmpeg_inner(args, echo, Some(progress), Some(cancel))
}

fn run_ffmpeg_inner(
    args: &[String],
    echo: bool,
    mut progress: Option<&mut dyn FnMut(f64)>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<(), FfmpegError> {
    use std::io::{BufRead, BufReader, Write as _};
    use std::process::Stdio;
    use std::sync::atomic::Ordering;

    // A job can be cancelled while it waits in the queue; don't spawn ffmpeg
    // just to kill it.
    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Err(FfmpegError::Cancelled);
    }

    const TAIL: usize = 8;
    let mut cmd = Command::new("ffmpeg");
    if progress.is_some() {
        cmd.args(["-progress", "pipe:1", "-nostats"]);
    }
    let mut child = cmd
        .args(args)
        .stdin(Stdio::null())
        .stdout(if progress.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = child.stderr.take().expect("stderr was piped");
    let reader = std::thread::spawn(move || {
        let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            // Live, like the old inherited stderr; a closed pipe is not a
            // reason to panic.
            if echo {
                let _ = writeln!(std::io::stderr(), "{line}");
            }
            if tail.len() == TAIL {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        error_lines(&tail.into_iter().collect::<Vec<_>>())
    });
    // The progress stream is read on its own thread; we poll the child so the
    // callback runs on the caller's thread (no shared mutable state).
    let progress_rx = child.stdout.take().map(|stdout| {
        let (tx, rx) = std::sync::mpsc::channel::<f64>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(seconds) = parse_progress_seconds(&line)
                    && tx.send(seconds).is_err()
                {
                    break;
                }
            }
        });
        rx
    });
    let status = loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(FfmpegError::Cancelled);
        }
        if let (Some(rx), Some(cb)) = (&progress_rx, progress.as_mut()) {
            while let Ok(t) = rx.try_recv() {
                (**cb)(t);
            }
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    };
    // Drain what landed between the last poll and exit.
    if let (Some(rx), Some(cb)) = (&progress_rx, progress.as_mut()) {
        while let Ok(t) = rx.try_recv() {
            (**cb)(t);
        }
    }
    let tail = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(FfmpegError::Failed { status, tail });
    }
    Ok(())
}

/// One `-progress` key/value line. `out_time_us` is microseconds;
/// `out_time_ms` is a historical misnomer for the same unit. `N/A` (before
/// the first frame) parses to nothing.
fn parse_progress_seconds(line: &str) -> Option<f64> {
    let (key, value) = line.split_once('=')?;
    if key != "out_time_us" && key != "out_time_ms" {
        return None;
    }
    let micros: f64 = value.trim().parse().ok()?;
    Some(micros / 1_000_000.0)
}

/// The reason a failed run failed, out of the last lines of its stderr:
/// ffmpeg buries the cause among progress and stream metadata, so prefer
/// the lines that actually name a problem.
pub(crate) fn error_lines(lines: &[String]) -> String {
    const KEYS: &[&str] = &["error", "invalid", "no such", "not found", "failed"];
    let hits: Vec<&String> = lines
        .iter()
        .filter(|l| {
            let lower = l.to_ascii_lowercase();
            KEYS.iter().any(|k| lower.contains(k))
        })
        .collect();
    let picked = if hits.is_empty() {
        lines.iter().collect()
    } else {
        hits
    };
    picked
        .iter()
        .rev()
        .take(3)
        .rev()
        .map(|l| l.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the argument list for a trim. Exposed so tests (and later the
/// graph compiler) can inspect the exact command without running it.
pub fn build_args(spec: &TrimSpec) -> Result<Vec<String>, TrimError> {
    if spec.end <= spec.start {
        return Err(TrimError::Range {
            start: format_seconds(&spec.start),
            end: format_seconds(&spec.end),
        });
    }
    let duration = spec.end - spec.start;

    let mut a: Vec<String> = Vec::new();
    a.push("-y".to_string()); // overwrite output without asking
    a.extend(device_args(spec.encoder, true));

    a.push("-ss".into());
    a.push(format_seconds(&spec.start));
    a.push("-i".into());
    a.push(local_path_arg(&spec.input));
    a.push("-t".into());
    a.push(format_seconds(&duration));

    if spec.copy {
        a.push("-c".into());
        a.push("copy".into());
        // NOTE: deliberately NO `-avoid_negative_ts make_zero` — it shifts
        // the pre-start packets back into the output (measured: 6.09s
        // instead of 4s). The mp4 muxer's edit list hides the pre-start
        // material, giving the exact requested window with clean decode.
    } else {
        // Video only: route frames onto the GPU where the encoder wants them.
        if spec.encoder == Encoder::Vaapi {
            a.push("-vf".into());
            a.push("format=nv12,hwupload".into());
        }
        a.extend(video_encode_args(
            spec.encoder,
            spec.crf,
            &spec.preset,
            &spec.bitrate,
        ));
        a.push("-c:a".into());
        a.push("aac".into());
        a.push("-b:a".into());
        a.push("192k".into());
        a.push("-movflags".into());
        a.push("+faststart".into());
    }

    // `--` ends option parsing: a file literally named "-version.mp4" can't
    // be misread as an option. `-i` consumes its value literally but the URL
    // parser still runs, so the input needs the same guard as the trailing
    // (positional) OUTPUT.
    a.push("--".into());
    a.push(local_path_arg(&spec.output));
    Ok(a)
}

/// Execute a trim.
pub fn trim(spec: &TrimSpec) -> Result<(), TrimError> {
    let args = build_args(spec)?;
    eprintln!("$ ffmpeg {}", args.join(" "));
    run_ffmpeg(&args)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cancel must kill the child, not wait for a 30 s encode to finish.
    /// The flag is set from another thread, the same way the job worker does
    /// it; the runner returns `Cancelled` and no ffmpeg is left behind.
    #[test]
    fn a_cancel_kills_the_running_child() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("cancel.mp4");
        let args = vec![
            "-y".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc2=size=3840x2160:rate=60".to_string(),
            "-t".to_string(),
            "120".to_string(),
            "-c:v".to_string(),
            "libx264".to_string(),
            "-preset".to_string(),
            "veryslow".to_string(),
            "--".to_string(),
            out.display().to_string(),
        ];
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let started = std::time::Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            let mut progress = |_seconds: f64| {};
            let error = run_ffmpeg_progress_cancel(&args, false, &mut progress, &cancel)
                .expect_err("cancelled");
            assert!(matches!(error, FfmpegError::Cancelled));
        });
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the child was killed, not awaited"
        );
    }

    /// A cancel that arrives before the spawn must not start ffmpeg at all:
    /// a queued job's cancel races the dispatcher.
    #[test]
    fn an_already_cancelled_run_never_spawns() {
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let mut progress = |_seconds: f64| {};
        let error = run_ffmpeg_progress_cancel(
            &["--definitely-not-a-real-argument".to_string()],
            false,
            &mut progress,
            &cancel,
        )
        .expect_err("cancelled before spawn");
        assert!(matches!(error, FfmpegError::Cancelled));
    }

    fn spec() -> TrimSpec {
        TrimSpec {
            input: "in.mp4".into(),
            output: "out.mp4".into(),
            start: Seconds::new(25, 2), // 12.5
            end: Seconds::from_integer(20),
            encoder: Encoder::Cpu,
            copy: false,
            crf: 18,
            preset: "fast".into(),
            bitrate: "8M".into(),
        }
    }

    #[test]
    fn cpu_args_accurate_cut() {
        let args = build_args(&spec()).unwrap();
        let line = args.join(" ");
        // seek before input, duration after → fast + accurate
        assert!(line.contains("-ss 12.5 -i in.mp4 -t 7.5"), "{line}");
        assert!(line.contains("-c:v libx264"), "{line}");
        // `--` guards the positional output against option-injection
        assert_eq!(args.last().map(String::as_str), Some("out.mp4"));
        assert_eq!(args[args.len() - 2], "--");
    }

    #[test]
    fn a_colon_output_stays_behind_the_option_terminator() {
        let mut s = spec();
        s.output = "a:b.mp4".into();
        let args = build_args(&s).unwrap();
        assert_eq!(args[args.len() - 2], "--");
        assert_eq!(args.last().map(String::as_str), Some("./a:b.mp4"));
    }

    #[test]
    fn a_colon_input_is_read_as_a_local_file() {
        // `-i` consumes its value literally, but ffmpeg still parses it as a
        // URL: `a:b.mp4` would name protocol `a`, so the input is guarded
        // exactly like the output.
        let mut s = spec();
        s.input = "a:b.mp4".into();
        let args = build_args(&s).unwrap();
        let i = args.iter().position(|a| a == "-i").expect("an -i argument");
        assert_eq!(args[i + 1], "./a:b.mp4");
    }

    #[test]
    fn vaapi_device_picks_the_lowest_render_node() {
        assert_eq!(
            pick_render_node(vec!["renderD129".into(), "renderD128".into()]),
            Some("/dev/dri/renderD128".into())
        );
        assert_eq!(pick_render_node(vec![]), None);
        assert!(vaapi_device().starts_with("/dev/dri/"));
    }

    #[test]
    fn vaapi_args_name_a_device() {
        let line = device_args(Encoder::Vaapi, false).join(" ");
        assert!(line.contains("vaapi=va:/dev/dri/"), "{line}");
    }

    #[test]
    fn copy_args_skip_encoding() {
        let mut s = spec();
        s.copy = true;
        let line = build_args(&s).unwrap().join(" ");
        assert!(line.contains("-c copy"), "{line}");
        assert!(!line.contains("libx264"), "{line}");
        // make_zero would re-include pre-start packets (bloats the output);
        // see the note in build_args.
        assert!(!line.contains("avoid_negative_ts"), "{line}");
    }

    #[test]
    fn vaapi_args_init_device_and_upload() {
        let mut s = spec();
        s.encoder = Encoder::Vaapi;
        let line = build_args(&s).unwrap().join(" ");
        assert!(line.contains("-init_hw_device vaapi=va:"), "{line}");
        assert!(line.contains("format=nv12,hwupload"), "{line}");
        assert!(line.contains("-c:v h264_vaapi"), "{line}");
        // device init must appear before -i
        assert!(
            line.find("-init_hw_device").unwrap() < line.find(" -i ").unwrap(),
            "{line}"
        );
    }

    #[test]
    fn qsv_trim_decodes_on_gpu() {
        let mut s = spec();
        s.encoder = Encoder::Qsv;
        let line = build_args(&s).unwrap().join(" ");
        assert!(line.contains("-hwaccel qsv"), "{line}");
        assert!(line.contains("-c:v h264_qsv"), "{line}");
        // trim decodes on the GPU; the export graph does NOT (see graph.rs)
    }

    #[test]
    fn videotoolbox_takes_software_frames() {
        let mut s = spec();
        s.encoder = Encoder::VideoToolbox;
        let args = build_args(&s).unwrap();
        let line = args.join(" ");
        assert!(line.contains("-c:v h264_videotoolbox"), "{line}");
        assert!(line.contains("-b:v 8M"), "{line}");
        // VT's own software encoder is the last resort before libx264.
        assert!(line.contains("-allow_sw 1"), "{line}");
        // No device init: VT accepts software nv12/yuv420p and uploads itself.
        assert!(!line.contains("-init_hw_device"), "{line}");
        assert!(!line.contains("hwupload"), "{line}");
    }

    #[test]
    fn hardware_encoder_tracks_the_platform() {
        assert_eq!(
            hardware_encoder(),
            if cfg!(target_os = "macos") {
                Encoder::VideoToolbox
            } else {
                Encoder::Vaapi
            }
        );
        assert!(hardware_encoder().is_hardware());
    }

    #[test]
    fn rejects_inverted_range() {
        let mut s = spec();
        s.start = Seconds::from_integer(10);
        s.end = Seconds::from_integer(10);
        assert!(matches!(build_args(&s), Err(TrimError::Range { .. })));
    }

    #[test]
    fn progress_lines_parse_to_seconds() {
        // `out_time_us` is the modern key; `out_time_ms` is the same unit.
        assert_eq!(parse_progress_seconds("out_time_us=1500000"), Some(1.5));
        assert_eq!(parse_progress_seconds("out_time_ms=250000"), Some(0.25));
        // ffmpeg prints `N/A` before the first frame; other keys are ignored.
        assert_eq!(parse_progress_seconds("out_time_us=N/A"), None);
        assert_eq!(parse_progress_seconds("frame=12"), None);
        assert_eq!(parse_progress_seconds("progress=continue"), None);
    }

    #[test]
    fn a_failed_run_reports_the_reason_not_the_noise() {
        let lines: Vec<String> = [
            "  Duration: 00:00:06.00, start: 0.000000",
            "      handler_name    : SoundHandler",
            "[out#0/mp4 @ 0x1] Error opening output /nope/x.mp4: No such file or directory",
            "Error opening output file /nope/x.mp4.",
            "Error opening output files: No such file or directory",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let msg = error_lines(&lines);
        assert!(msg.contains("No such file or directory"), "{msg}");
        assert!(!msg.contains("SoundHandler"), "noise filtered: {msg}");
        // Nothing error-ish in the tail: report it rather than nothing.
        let quiet: Vec<String> = ["progress", "more"].iter().map(|s| s.to_string()).collect();
        assert_eq!(error_lines(&quiet), "progress\nmore");
    }
}
