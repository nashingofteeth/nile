//! nile (alias nle) — a terminal-native NLE.
//!
//! M2 status: interactive editor — TUI dashboard, mpv monitor, vim keys,
//! instant EDL preview. Project file, media pool, timeline, and export
//! landed in M0/M1.

mod log;
#[cfg(feature = "tui")]
mod mpv;
// The extend-mode preview helpers are TUI-only; without that feature they're
// compiled but unused, so allow the dead code rather than gate each one.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
mod preview;
// The proxy cache renders what `edl://` can't (effects, transitions); the
// preview uses it, so it shares preview's fate outside the TUI.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
mod proxy;
// Keyframe timestamps back the pool's J/K keys; `prune` is used headless.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
mod keyframes;
#[cfg(feature = "tui")]
mod tui;
// Timeline filmstrips back the video rows; `prune` is used headless.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
mod filmstrip;
// Asset thumbnails back the pool preview; `prune` is used headless.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
mod thumbnails;
// Waveform peaks back the timeline's audio row; `prune` is used headless.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
mod waveform;
// The analysis sidecar host and the hidden mean-luma analyzer child.
mod analysis;

// Declared after the macros: a child module only sees a macro_rules! that
// precedes the `mod` declaration.
mod cmd;
mod jobs;
mod serve;
mod session;

/// Print a line, tolerating a closed consumer (`nile show | head`). Rust
/// ignores `SIGPIPE`, so `say!` *panics* on the broken pipe; a Unix
/// tool should just stop talking. Same syntax as `say!` — all CLI
/// output goes through here. (A genuine write failure, e.g. a full disk,
/// is swallowed too: the exit status stays the command's, not the
/// terminal's.)
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}
pub(crate) use say;

/// `say!` for warnings (stderr).
macro_rules! say_err {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}
pub(crate) use say_err;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use nle_core::edit;
use nle_core::{Seconds, parse_rate};
use nle_ffmpeg::Encoder;
#[cfg(test)]
use nle_ffmpeg::RawProbe;

#[derive(Parser)]
#[command(
    name = "nile",
    version,
    about = "a terminal-native NLE",
    // `--to -1.5` is a real thing (an edge reaching back past time 0), and
    // for the rest a negative is our own clear error, not clap's
    // "unexpected argument '-1'".
    allow_negative_numbers = true
)]
struct Cli {
    /// Project file (default: .nile/config.json, else nile.json)
    #[arg(short, long, global = true, value_name = "FILE")]
    project: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Show or set the directory's default project
    Default {
        /// Project path to make default; omit to show the current default
        file: Option<PathBuf>,
        /// Remove the configured default and fall back to nile.json
        #[arg(long)]
        unset: bool,
    },
    /// Create an empty project
    Init {
        /// Overwrite an existing project file
        #[arg(long)]
        force: bool,
    },
    /// Probe files and add them to the media pool
    Import {
        /// Media files to import
        files: Vec<PathBuf>,
        /// Group the imported files as one logical recording (argument order)
        #[arg(long, value_name = "NAME")]
        group: Option<String>,
    },
    /// Place a clip on the video track
    Add {
        /// Media file (auto-imported if new)
        file: PathBuf,
        /// Track number to place on (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
        /// Timeline position (default: end of track)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: Option<String>,
        /// Source in-point (default: 0)
        #[arg(long = "in", value_name = "TIME", allow_negative_numbers = true)]
        in_point: Option<String>,
        /// Source out-point (default: end of file)
        #[arg(long = "out", value_name = "TIME", allow_negative_numbers = true)]
        out_point: Option<String>,
    },
    /// Remove a clip by id (see `nile show`); `--ripple` closes the gap
    Remove {
        /// Clip id
        id: u32,
        /// Ripple: followers slide left instead of leaving a gap
        #[arg(long)]
        ripple: bool,
    },
    /// Change a clip's playback speed (the track ripples to fit)
    Speed {
        /// Clip id (from `nile show`)
        id: u32,
        /// New speed in percent: 100 = normal, 50 = half, 200 = double
        #[arg(allow_negative_numbers = true)]
        rate: String,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
    },
    /// Move a segment's in-edge to a position (TUI `i`; `--ripple` = `I`)
    TrimIn {
        /// Where the new edge lands (seconds or timecode)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: String,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
        /// Ripple: followers slide left to close the trimmed span
        #[arg(long)]
        ripple: bool,
    },
    /// Move a segment's out-edge to a position (TUI `o` / `O`)
    TrimOut {
        /// Where the new edge lands (seconds or timecode)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: String,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
        /// Ripple: followers slide left to close the trimmed span
        #[arg(long)]
        ripple: bool,
    },
    /// Split the segment under a position (TUI `s`)
    Split {
        /// Where to cut (seconds or timecode)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: String,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
    },
    /// Insert a gap at a position (TUI `_`)
    Gap {
        /// Where to insert (seconds or timecode)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: String,
        /// Gap length in frames (default 1)
        #[arg(long, allow_negative_numbers = true)]
        frames: Option<i64>,
        /// Gap length as a time (alternative to --frames)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        dur: Option<String>,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
    },
    /// Delete a segment or a whole-segment range (TUI `dd`; `--ripple` = `D`)
    Delete {
        /// Delete the segment under this position
        #[arg(
            long,
            value_name = "TIME",
            required_unless_present_any = ["clip", "seg", "from_seg", "to_seg"]
        , allow_negative_numbers = true)]
        at: Option<String>,
        /// Delete this clip by id (finds its track)
        #[arg(long)]
        clip: Option<u32>,
        /// Delete this segment by index (from `nile show`)
        #[arg(long)]
        seg: Option<usize>,
        /// Delete a range: first segment index (0-based)
        #[arg(long, requires = "to_seg")]
        from_seg: Option<usize>,
        /// Delete a range: last segment index, inclusive
        #[arg(long, requires = "from_seg")]
        to_seg: Option<usize>,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
        /// Ripple: followers slide left instead of leaving a gap
        #[arg(long)]
        ripple: bool,
    },
    /// Yank a segment or a whole-segment range into the shared register (TUI `yy`/`y`)
    Yank {
        /// Yank the segment under this position
        #[arg(
            long,
            value_name = "TIME",
            required_unless_present_any = ["clip", "seg", "from_seg", "to_seg"]
        , allow_negative_numbers = true)]
        at: Option<String>,
        /// Yank this clip by id (finds its track)
        #[arg(long)]
        clip: Option<u32>,
        /// Yank this segment by index (from `nile show`)
        #[arg(long)]
        seg: Option<usize>,
        /// Yank a range: first segment index (0-based)
        #[arg(long, requires = "to_seg")]
        from_seg: Option<usize>,
        /// Yank a range: last segment index, inclusive
        #[arg(long, requires = "from_seg")]
        to_seg: Option<usize>,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
    },
    /// Paste the register at a position (TUI `p`; `--ripple` = `P`)
    Paste {
        /// Where to insert (seconds or timecode)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: String,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
        /// Ripple: insert and push followers right
        #[arg(long)]
        ripple: bool,
        /// Paste the whole range register (visual `p`); implies ripple
        #[arg(long)]
        range: bool,
    },
    /// Move one edge of a clip or gap to a position (the extend commit)
    Extend {
        /// The clip whose edge to move (finds its track)
        #[arg(long, required_unless_present_any = ["seg", "at"])]
        clip: Option<u32>,
        /// The segment whose edge to move (index from `nile show`)
        #[arg(long)]
        seg: Option<usize>,
        /// The segment under this position (alternative to --clip/--seg)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: Option<String>,
        /// Which edge: in (head/left) or out (tail/right)
        #[arg(long, value_name = "EDGE")]
        edge: String,
        /// Where the edge lands (seconds or timecode; may be negative)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        to: String,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
        /// Ripple: push followers aside instead of overwriting
        #[arg(long)]
        ripple: bool,
    },
    /// Add a crossfade at a boundary (TUI `X`; the default kind is a dissolve)
    Transition {
        /// Which shape: dissolve (default) or fade (through black)
        #[arg(value_name = "KIND")]
        kind: Option<String>,
        /// Where the cut is (seconds or timecode)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: String,
        /// Length in frames (default: 12)
        #[arg(long, allow_negative_numbers = true)]
        frames: Option<i64>,
        /// Length as a time (alternative to --frames)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        dur: Option<String>,
        /// Retime an existing transition: rate percent (50 = twice as long)
        #[arg(long, value_name = "P", allow_negative_numbers = true)]
        percent: Option<f64>,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
    },
    /// Add, remove, or clear a visual effect on a clip (TUI `x`)
    Effect {
        /// Which effect: crop | scale | eq | lut | rm | clear
        #[arg(value_name = "KIND")]
        kind: String,
        /// The clip to attach it to
        #[arg(long)]
        clip: u32,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
        /// `rm`: which effect to drop; add: which row to replace (0-based;
        /// default: rm's last row, add appends)
        #[arg(long, value_name = "N")]
        index: Option<usize>,
        /// crop: add/replace a keyframe at this timeline position
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: Option<String>,
        /// crop/scale: width as a percentage (crop window; scale 100 = unchanged)
        #[arg(long, value_name = "P", allow_negative_numbers = true)]
        w: Option<f64>,
        /// crop/scale: height as a percentage (crop window; scale 100 = unchanged)
        #[arg(long, value_name = "P", allow_negative_numbers = true)]
        h: Option<f64>,
        /// crop/offset: x percentage (crop top-left; offset shift)
        #[arg(long, value_name = "P", allow_negative_numbers = true)]
        x: Option<f64>,
        /// crop/offset: y percentage (crop top-left; offset shift)
        #[arg(long, value_name = "P", allow_negative_numbers = true)]
        y: Option<f64>,
        /// eq brightness (0 is neutral)
        #[arg(long, value_name = "F", allow_negative_numbers = true)]
        brightness: Option<f64>,
        /// eq contrast (1 is neutral)
        #[arg(long, value_name = "F", allow_negative_numbers = true)]
        contrast: Option<f64>,
        /// eq saturation (1 is neutral)
        #[arg(long, value_name = "F", allow_negative_numbers = true)]
        saturation: Option<f64>,
        /// lut3d file
        #[arg(long, value_name = "PATH")]
        path: Option<String>,
    },
    /// Undo the last edit (TUI `u`; works on TUI-made edits too)
    Undo {},
    /// Redo the last undone edit (TUI `Ctrl-r`)
    Redo {},
    /// Drop sub-frame stumps and snap a drifted timeline to the frame grid
    PurgeStumps {
        /// Only this track (1-based); default: every track
        #[arg(long, value_name = "N")]
        track: Option<usize>,
    },
    /// Show the timeline
    Show {
        /// Print the raw project JSON
        #[arg(long)]
        json: bool,
        /// Add the derived per-track/asset time maps (requires --json)
        #[arg(long, requires = "json")]
        maps: bool,
        /// Report this sequence instead of the active one
        #[arg(long, value_name = "ID")]
        sequence: Option<u32>,
    },
    /// Show or move the stored playhead (shared with the TUI)
    Seek {
        /// Where to move it (seconds or timecode); omit to print it
        #[arg(value_name = "TIME", allow_negative_numbers = true)]
        at: Option<String>,
    },
    /// Media pool: list assets and their usage
    Media {
        #[command(subcommand)]
        action: MediaCmd,
    },
    /// Logical media groups: list, remove
    Group {
        #[command(subcommand)]
        action: GroupCmd,
    },
    /// Notes, markers and render expectations
    Annotation {
        #[command(subcommand)]
        action: AnnotationCmd,
    },
    /// Analysis products stored beside the project: transcripts
    Analysis {
        #[command(subcommand)]
        action: AnalysisCmd,
    },
    /// Print the report written beside an exported file (output-relative)
    Report {
        /// The exported video (or its .report.json)
        file: PathBuf,
        /// Print the full report JSON
        #[arg(long)]
        json: bool,
    },
    /// Timeline tracks: list, add, mute, remove
    Track {
        #[command(subcommand)]
        action: TrackCmd,
    },
    /// Sequences: list, add, switch the active one
    Sequence {
        #[command(subcommand)]
        action: SequenceCmd,
    },
    /// On-disk caches: prune old proxy renders (explicit, never automatic)
    Cache {
        #[command(subcommand)]
        action: CacheCmd,
    },
    /// Import a CMX3600 EDL, building a project from the listed media
    ImportEdl {
        /// The .edl file
        file: PathBuf,
        /// Directory of media to match reels/clip names against
        #[arg(long, value_name = "DIR")]
        media: PathBuf,
        /// Force the frame rate (e.g. 30000/1001, 29.97, 24)
        #[arg(long, value_name = "RATE")]
        fps: Option<String>,
        /// Source timecode that maps to file time zero
        #[arg(long, value_name = "TC", default_value = "00:00:00:00")]
        src_offset: String,
        /// Override a reel: REEL=PATH (repeatable)
        #[arg(long = "map", value_name = "REEL=PATH")]
        map: Vec<String>,
        /// Group files as one recording: NAME=FILE,FILE,… (repeatable)
        #[arg(long = "group", value_name = "NAME=FILE,…")]
        group: Vec<GroupSpec>,
        /// Overwrite an existing project file
        #[arg(long)]
        force: bool,
    },
    /// Import an OpenTimelineIO (.otio) timeline — e.g. a Kdenlive export
    ImportOtio {
        /// The .otio file
        file: PathBuf,
        /// Extra directory of media to pool (for moved files)
        #[arg(long, value_name = "DIR")]
        media: Option<PathBuf>,
        /// Force the frame rate (e.g. 30000/1001, 29.97, 24)
        #[arg(long, value_name = "RATE")]
        fps: Option<String>,
        /// Remap a clip path: OLD=NEW (repeatable)
        #[arg(long = "remap", value_name = "OLD=NEW")]
        remap: Vec<String>,
        /// Group files as one recording: NAME=FILE,FILE,… (repeatable)
        #[arg(long = "group", value_name = "NAME=FILE,…")]
        group: Vec<GroupSpec>,
        /// Overwrite an existing project file
        #[arg(long)]
        force: bool,
    },
    /// Internal: the mean-luma analyzer child `nile analysis analyze` spawns
    #[command(hide = true)]
    AnalysisAnalyzer,
    /// Detect the cuts in a rendered file and rebuild an editable timeline
    DetectEdits {
        /// The rendered/edited video to analyze
        file: PathBuf,
        /// Scene-change score in 0..1 (lower = more cuts)
        #[arg(long, default_value_t = 0.4)]
        threshold: f32,
        /// Ignore shots shorter than this (suppresses flashes)
        #[arg(
            long,
            value_name = "TIME",
            default_value = "0.5",
            allow_negative_numbers = true
        )]
        min_shot: String,
        /// Shortest black interval to treat as a gap
        #[arg(
            long,
            value_name = "TIME",
            default_value = "0.1",
            allow_negative_numbers = true
        )]
        black_min: String,
        /// Do not recover black intervals as gaps
        #[arg(long)]
        no_gaps: bool,
        /// Force the frame rate (e.g. 30000/1001, 29.97, 24)
        #[arg(long, value_name = "RATE")]
        fps: Option<String>,
        /// Rebuild video only, dropping the file's audio
        #[arg(long)]
        video_only: bool,
        /// Print the detected cut points; write no project
        #[arg(long)]
        list: bool,
        /// Append the detected clips to an existing project (at the end)
        #[arg(long)]
        append: bool,
        /// Video track to append to (1-based; default: first video track)
        #[arg(long, value_name = "N")]
        track: Option<usize>,
        /// Overwrite an existing project file
        #[arg(long)]
        force: bool,
    },
    /// Benchmark export presets and report time/fps
    Bench {
        /// Number of runs per preset
        #[arg(long, default_value_t = 1)]
        runs: usize,
        /// Benchmark only this tier
        #[arg(long, value_enum)]
        preset: Option<QualityPreset>,
        /// Number of chunked-export workers (benchmark; default: single)
        #[arg(long)]
        parallel: Option<usize>,
    },
    /// Render the timeline to a file
    Export {
        /// Output file
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
        /// Quality tier: preview, balanced, max
        #[arg(long, value_enum, default_value = "balanced")]
        preset: QualityPreset,
        /// Override the tier's encode engine
        /// (cpu|vaapi|qsv|videotoolbox|x265|svt-av1)
        #[arg(long, value_enum)]
        encoder: Option<EncoderArg>,
        /// Override the tier's quality (0-51, lower = better)
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=51))]
        crf: Option<u8>,
        /// Override the encoder's own preset (x264 names or SVT-AV1 number)
        #[arg(long)]
        encoder_preset: Option<String>,
        /// Override the hardware bitrate (vaapi/qsv/videotoolbox)
        #[arg(long)]
        bitrate: Option<String>,
        /// Force N chunked-export workers; omit to stay single-worker
        #[arg(long)]
        parallel: Option<usize>,
        /// Export this sequence instead of the active one
        #[arg(long, value_name = "ID")]
        sequence: Option<u32>,
    },
    /// Durable render jobs on `nile serve` (submit, list, inspect, cancel)
    Job {
        #[command(subcommand)]
        action: JobCmd,
    },
    /// Interactive editor: TUI dashboard + mpv monitor, vim keys
    #[cfg(feature = "tui")]
    Edit {
        /// Render the TUI once, print the state and grid, and exit (diagnostics)
        #[arg(long)]
        dump: bool,
        /// Grid size for --dump
        #[arg(long, value_name = "WxH", default_value = "100x30")]
        dump_size: String,
        /// Keys to feed before dumping (`\n` = Enter, `\e` = Esc)
        #[arg(long, value_name = "KEYS", default_value = "")]
        dump_keys: String,
    },

    /// Print the mpv preview URL the monitor plays (the composite), or one track's
    /// with `--track N` (debugging)
    Edl {
        /// Print one track instead of the composite the monitor plays
        /// (1-based)
        #[arg(long)]
        track: Option<usize>,
        /// Print this sequence instead of the active one
        #[arg(long, value_name = "ID")]
        sequence: Option<u32>,
    },

    /// Serve the project over JSON-RPC 2.0 on its Unix socket
    Serve {
        /// Use stdin/stdout instead of the project socket (pipes, tests)
        #[arg(long)]
        stdio: bool,
    },

    /// Show what's inside a media file (streams, codecs, rates)
    Probe {
        /// Media file to inspect
        file: PathBuf,
        /// Print raw ffprobe JSON instead of a summary
        #[arg(long)]
        json: bool,
    },
    /// Cut [in, out) out of a file into a new file
    Trim {
        /// Source file
        input: PathBuf,
        /// Start: seconds ("12.5") or timecode ("0:01:23.500")
        #[arg(long = "in", value_name = "TIME", allow_negative_numbers = true)]
        in_point: String,
        /// End (same formats, exclusive)
        #[arg(long = "out", value_name = "TIME", allow_negative_numbers = true)]
        out_point: String,
        /// Where to write the result
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
        /// Encode engine: cpu (libx264), vaapi (Intel GPU), qsv (Quick Sync),
        /// videotoolbox (macOS)
        #[arg(long, value_enum, default_value = "cpu")]
        encoder: EncoderArg,
        /// Stream copy: instant + lossless; starts exactly at --in, end lands
        /// within a frame or two of --out (packet boundaries)
        #[arg(long)]
        copy: bool,
        /// x264 quality, 0-51, lower = better [cpu only]
        #[arg(long, default_value_t = 18)]
        crf: u8,
        /// x264 preset: ultrafast..veryslow [cpu only]
        #[arg(long, default_value = "fast")]
        preset: String,
        /// Target bitrate for hardware encoders [vaapi/qsv/videotoolbox]
        #[arg(long, default_value = "8M")]
        bitrate: String,
    },
}

/// Durable job subcommands. Every one talks to the running server: the queue
/// and its single writer live there, not in a one-shot process.
#[derive(Debug, Subcommand)]
enum JobCmd {
    /// List jobs, oldest first
    Ls,
    /// Print one job as JSON
    Show {
        /// Job id (from `nile job ls`)
        id: u64,
    },
    /// Drop a queued job, or kill the running export
    Cancel {
        /// Job id (from `nile job ls`)
        id: u64,
    },
    /// Delete a terminal job's record and snapshot
    Rm {
        /// Job id (from `nile job ls`)
        id: u64,
    },
    /// Delete old terminal job records (queued and running jobs are kept)
    Prune {
        /// Keep this many of the newest terminal jobs
        #[arg(long, value_name = "N")]
        keep: u64,
        /// Also require a job to be at least this many days old
        #[arg(long, value_name = "DAYS")]
        older_than: Option<u64>,
    },
    /// Snapshot the project and queue an export
    Submit {
        /// Output file (relative paths resolve against the current directory)
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
        /// Sequence to render (default: the server's active sequence)
        #[arg(long, value_name = "ID")]
        sequence: Option<u32>,
        /// Quality tier: preview, balanced, max
        #[arg(long, value_enum, default_value = "balanced")]
        preset: QualityPreset,
        /// Override the tier's quality (0-51, lower = better)
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=51))]
        crf: Option<u8>,
        /// Override the encoder's own preset (x264 names or SVT-AV1 number)
        #[arg(long)]
        encoder_preset: Option<String>,
    },
}

/// Media-pool subcommands.
#[derive(Debug, Subcommand)]
enum MediaCmd {
    /// List pooled assets and how many clips use each
    Ls,
    /// Remove an unused asset from the pool
    Rm {
        /// Asset id (from `nile media ls`)
        id: u32,
    },
}

/// Annotation subcommands.
#[derive(Debug, Subcommand)]
enum AnnotationCmd {
    /// Add a note anchored to a source time or timeline position, or an
    /// expectation a finished render must satisfy
    Add {
        /// Note text (omit with --expect)
        #[arg(required_unless_present = "expect")]
        text: Option<String>,
        /// Source asset id (from `nile media ls`), or the cam/absent/no-sliver
        /// subject
        #[arg(long, group = "source")]
        asset: Option<u32>,
        /// Media group id; --time is then on the group clock, or the
        /// cam/absent subject
        #[arg(long, group = "source")]
        group: Option<u32>,
        /// Source time (seconds or timecode)
        #[arg(long, value_name = "TIME", required_unless_present = "timeline")]
        time: Option<String>,
        /// Anchor to the timeline clip under --at instead of a source time;
        /// required for --expect
        #[arg(long, conflicts_with = "time")]
        timeline: bool,
        /// Timeline position to pin (with --timeline)
        #[arg(
            long,
            value_name = "TIME",
            conflicts_with = "time",
            requires = "timeline"
        )]
        at: Option<String>,
        /// End of a timeline range (with --timeline; default: a point at --at)
        #[arg(
            long,
            value_name = "TIME",
            conflicts_with = "time",
            requires = "timeline"
        )]
        to: Option<String>,
        /// Track for --timeline (1-based; default 1)
        #[arg(long, value_name = "N", default_value_t = 1)]
        track: usize,
        /// Write a check a finished render must satisfy (needs --timeline)
        #[arg(
            long,
            value_enum,
            value_name = "CHECK",
            conflicts_with = "text",
            requires = "timeline"
        )]
        expect: Option<ExpectCheck>,
        /// cam/absent: frames trimmed from each end before checking (default 0)
        #[arg(long, value_name = "N")]
        tolerance_frames: Option<i64>,
        /// crop-stable: maximum crop movement, percent per second (default 25)
        #[arg(long, value_name = "P")]
        max_move: Option<f64>,
        /// crop-stable: a panning crop must not change size (default: hold)
        #[arg(long)]
        no_hold_size: bool,
        /// work-in-frame: percent of the picture kept on every side (default 5)
        #[arg(long, value_name = "P")]
        margin: Option<f64>,
        /// words-complete: with a transcript, the fraction of relevant words
        /// that must be whole; without one, the fraction of checked frames
        /// that must carry audio (default 1.0)
        #[arg(long, value_name = "F")]
        min_coverage: Option<f64>,
        /// words-complete: transcript hash (`t1-…`) from the project's
        /// transcript store
        #[arg(long, value_name = "HASH")]
        transcript: Option<String>,
        /// absent: also refuse black frames (and a blend's black side)
        #[arg(long)]
        black: bool,
        /// no-sliver: shortest visible run, in frames (default 2)
        #[arg(long, value_name = "N")]
        min_frames: Option<i64>,
    },
    /// List annotations (id, status, anchor, text)
    Ls {
        /// Sequence id (default: the active one)
        #[arg(long)]
        sequence: Option<u32>,
    },
    /// Check a render's report against the sequence's expectations
    Check {
        /// The exported video (or its .report.json)
        #[arg(long, value_name = "FILE")]
        report: PathBuf,
        /// Sequence id (default: the report's own)
        #[arg(long)]
        sequence: Option<u32>,
        /// Only these annotation ids (repeatable)
        #[arg(long = "id", value_name = "N")]
        id: Vec<u32>,
        /// Print the check report as JSON (also when a check fails)
        #[arg(long)]
        json: bool,
    },
    /// Remove an annotation
    Rm {
        /// Annotation id (from `nile annotation ls`)
        id: u32,
    },
    /// Mark an annotation resolved
    Resolve {
        /// Annotation id
        id: u32,
    },
}

/// The `--expect` checks, spelled like the check's wire name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ExpectCheck {
    /// Every checked frame shows this asset (or a member of this group)
    Cam,
    /// The crop window never moves faster than --max-move and keeps its size
    CropStable,
    /// Every crop keeps --margin percent of the picture on every side
    WorkInFrame,
    /// The checked span carries audio for --min-coverage of its frames
    WordsComplete,
    /// Nothing named appears (and, with --black, no black frame either)
    Absent,
    /// No visible run is shorter than --min-frames
    NoSliver,
}

impl ExpectCheck {
    /// The model spelling (`crop_stable`), not the clap spelling.
    fn name(self) -> &'static str {
        match self {
            ExpectCheck::Cam => "cam",
            ExpectCheck::CropStable => "crop_stable",
            ExpectCheck::WorkInFrame => "work_in_frame",
            ExpectCheck::WordsComplete => "words_complete",
            ExpectCheck::Absent => "absent",
            ExpectCheck::NoSliver => "no_sliver",
        }
    }
}

/// Analysis-product subcommands.
#[derive(Debug, Subcommand)]
enum AnalysisCmd {
    /// Store a transcript for an asset and print its content hash
    Import {
        /// Asset id (from `nile media ls`)
        #[arg(long)]
        asset: u32,
        /// JSON file: { "words": [ { "text", "start", "end" } ] }
        file: PathBuf,
    },
    /// Sample an asset and run the bundled mean-luma analyzer
    Analyze {
        /// Asset id (from `nile media ls`)
        #[arg(long)]
        asset: u32,
        /// Source start (seconds or timecode; default 0)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        start: Option<String>,
        /// Source window length (default: from --start to the asset's end)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        duration: Option<String>,
        /// Sampling rate (default: the asset's frame rate)
        #[arg(long, value_name = "RATE")]
        fps: Option<String>,
        /// Sample width in pixels (default 64; pass with --height)
        #[arg(long)]
        width: Option<u32>,
        /// Sample height in pixels (default 36; pass with --width)
        #[arg(long)]
        height: Option<u32>,
    },
}

/// Media-group subcommands.
#[derive(Debug, Subcommand)]
enum GroupCmd {
    /// List groups, their parts and group end
    Ls,
    /// Remove a media group by id (undoable)
    Rm {
        /// Group id (from `nile group ls`)
        id: u32,
    },
}

#[derive(Debug, Subcommand)]
enum CacheCmd {
    /// Delete proxy renders older than N days
    Prune {
        /// Maximum age in days (default 30)
        #[arg(long, default_value_t = 30)]
        days: u64,
        /// List what would go; remove nothing
        #[arg(long)]
        dry_run: bool,
    },
}

/// Track subcommands.
#[derive(Debug, Subcommand)]
enum TrackCmd {
    /// List tracks (index, kind, mute, segment count)
    Ls,
    /// Append a new track
    Add {
        /// Track kind
        #[arg(value_enum)]
        kind: TrackKindArg,
    },
    /// Toggle a track's mute flag (default: track 1)
    Mute {
        /// Track index (1-based, from `nile track ls`)
        index: Option<usize>,
    },
    /// Remove a track by index (1-based), deleting its clips
    Rm {
        /// Track index (1-based)
        index: usize,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TrackKindArg {
    Video,
    Audio,
}

/// Sequence subcommands.
#[derive(Debug, Subcommand)]
enum SequenceCmd {
    /// List sequences (id, name, settings, active marker)
    Ls,
    /// Add an empty sequence (not active)
    Add {
        /// Display name
        name: String,
        /// Output width in pixels (must accompany --height)
        #[arg(long)]
        width: Option<u32>,
        /// Output height in pixels (must accompany --width)
        #[arg(long)]
        height: Option<u32>,
        /// Output frame rate (e.g. 30000/1001, 29.97, 24)
        #[arg(long, value_name = "RATE")]
        fps: Option<String>,
    },
    /// Make a sequence active (edits and undo follow it)
    Use {
        /// Sequence id (from `nile sequence ls`)
        id: u32,
    },
    /// Set the active sequence's audio post (normalization and fades)
    Audio {
        /// Normalize the mix to -16 LUFS / -1.5 dBTP (on, or off)
        #[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
        normalize: Option<bool>,
        /// Fade-in at the programme head, in seconds
        #[arg(long, value_name = "SECONDS", allow_hyphen_values = true)]
        fade_in: Option<String>,
        /// Fade-out at the programme tail, in seconds
        #[arg(long, value_name = "SECONDS", allow_hyphen_values = true)]
        fade_out: Option<String>,
    },
}

/// CLI spelling of the encoder choices (clap needs ValueEnum to build
/// an argument parser from it).
#[derive(Debug, Clone, Copy, ValueEnum)]
enum EncoderArg {
    Cpu,
    Vaapi,
    Qsv,
    #[value(name = "videotoolbox", alias = "vt")]
    VideoToolbox,
    X265,
    #[value(alias = "svtav1")]
    SvtAv1,
}

impl From<EncoderArg> for Encoder {
    fn from(v: EncoderArg) -> Self {
        match v {
            EncoderArg::Cpu => Encoder::Cpu,
            EncoderArg::Vaapi => Encoder::Vaapi,
            EncoderArg::Qsv => Encoder::Qsv,
            EncoderArg::VideoToolbox => Encoder::VideoToolbox,
            EncoderArg::X265 => Encoder::X265,
            EncoderArg::SvtAv1 => Encoder::SvtAv1,
        }
    }
}

use cmd::check::cmd_annotation_check;
use cmd::detect::{DetectArgs, cmd_detect_edits};
use cmd::export::{ExportOptions, QualityPreset, cmd_export};
use cmd::import::{EdlImport, GroupSpec, OtioImport, cmd_import, cmd_import_edl, cmd_import_otio};
use cmd::media::{cmd_probe, cmd_trim};
#[cfg(feature = "tui")]
use cmd::project::cmd_edit;
use cmd::project::{
    cmd_add, cmd_annotation_ls, cmd_cache, cmd_default, cmd_edl, cmd_group, cmd_init, cmd_media_ls,
    cmd_report, cmd_seek, cmd_sequence, cmd_show, cmd_track_ls,
};
use cmd::{run_cli_action, to_action};

// ------------------------------------------------------------ action mapping

/// The user-facing text for a model error. Track numbers are 1-based in
/// the CLI and the TUI (`v1`, `a1`), so the model's 0-based index is
/// translated here — one place, so the two front-ends agree.
pub(crate) fn edit_message(e: &edit::EditError) -> String {
    match e {
        edit::EditError::TrackNotFound(n) => format!("no track #{}", n + 1),
        other => other.to_string(),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    if let Cmd::Default { file, unset } = &cli.command {
        if cli.project.is_some() {
            anyhow::bail!("`nile default` takes the project as its FILE argument, not --project");
        }
        return cmd_default(&cwd, file.as_deref(), *unset);
    }
    // A report belongs to the output file, not the project: read it without
    // requiring a project in the current directory.
    if let Cmd::Report { file, json } = &cli.command {
        if cli.project.is_some() {
            anyhow::bail!("`nile report` reads the sidecar beside FILE; --project is not used");
        }
        return cmd_report(file, *json);
    }
    // The analyzer child has no project: it reads the packed sample paths the
    // parent put on its stdin, so dispatch before project resolution.
    if let Cmd::AnalysisAnalyzer = &cli.command {
        if cli.project.is_some() {
            anyhow::bail!("analysis-analyzer is an internal child command; --project is not used");
        }
        return analysis::run_analyzer().map_err(Into::into);
    }
    let project = match cli.project.clone() {
        Some(project) => project,
        None => cmd::project::resolve_default_project(&cwd)?,
    };
    // Process-bound commands and queries dispatch here; every edit-shaped
    // command falls through to the action catalog below — the same
    // executor (and undo history) the TUI uses.
    match &cli.command {
        #[cfg(feature = "tui")]
        Cmd::Edit {
            dump,
            dump_size,
            dump_keys,
        } => return cmd_edit(&project, *dump, dump_size, dump_keys),
        Cmd::Edl { track, sequence } => return cmd_edl(&project, *track, *sequence),
        Cmd::Serve { stdio } => return serve::cmd_serve(&project, *stdio),
        Cmd::Init { force } => return cmd_init(&project, *force),
        Cmd::Import { files, group } => return cmd_import(&project, files, group.as_deref()),
        Cmd::Add {
            file,
            track,
            at,
            in_point,
            out_point,
        } => {
            return cmd_add(
                &project,
                file,
                *track,
                at.as_deref(),
                in_point.as_deref(),
                out_point.as_deref(),
            );
        }
        Cmd::Show {
            json,
            maps,
            sequence,
        } => return cmd_show(&project, *json, *maps, *sequence),
        Cmd::Seek { at } => return cmd_seek(&project, at.as_deref()),
        Cmd::Track {
            action: TrackCmd::Ls,
        } => return cmd_track_ls(&project),
        Cmd::Sequence { action } => return cmd_sequence(&project, action),
        Cmd::Cache { action } => return cmd_cache(&project, action),
        Cmd::Media {
            action: MediaCmd::Ls,
        } => return cmd_media_ls(&project),
        Cmd::Group { action } => return cmd_group(&project, action),
        Cmd::Annotation {
            action: AnnotationCmd::Ls { sequence },
        } => return cmd_annotation_ls(&project, *sequence),
        Cmd::Annotation {
            action:
                AnnotationCmd::Check {
                    report,
                    sequence,
                    id,
                    json,
                },
        } => return cmd_annotation_check(&project, report, *sequence, id, *json),
        Cmd::Analysis {
            action: AnalysisCmd::Import { asset, file },
        } => return cmd::analysis::cmd_import(&project, *asset, file),
        Cmd::Analysis {
            action:
                AnalysisCmd::Analyze {
                    asset,
                    start,
                    duration,
                    fps,
                    width,
                    height,
                },
        } => {
            return cmd::analysis::cmd_analyze(
                &project,
                cmd::analysis::AnalyzeArgs {
                    asset: *asset,
                    start: start.as_deref(),
                    duration: duration.as_deref(),
                    fps: fps.as_deref(),
                    width: *width,
                    height: *height,
                },
            );
        }
        Cmd::Report { .. } => unreachable!("handled before project resolution"),
        Cmd::AnalysisAnalyzer => unreachable!("handled before project resolution"),
        Cmd::Job { action } => return cmd::job::cmd_job(&project, action),
        Cmd::ImportEdl {
            file,
            media,
            fps,
            src_offset,
            map,
            group,
            force,
        } => {
            return cmd_import_edl(EdlImport {
                project: &project,
                file,
                media,
                fps: fps.as_deref(),
                src_offset,
                maps: map,
                groups: group,
                force: *force,
            });
        }
        Cmd::ImportOtio {
            file,
            media,
            fps,
            remap,
            group,
            force,
        } => {
            return cmd_import_otio(OtioImport {
                project: &project,
                file,
                media: media.as_deref(),
                fps: fps.as_deref(),
                remaps: remap,
                groups: group,
                force: *force,
            });
        }
        Cmd::DetectEdits {
            file,
            threshold,
            min_shot,
            black_min,
            no_gaps,
            fps,
            video_only,
            list,
            append,
            track,
            force,
        } => {
            return cmd_detect_edits(DetectArgs {
                project: &project,
                file: file.as_path(),
                threshold: *threshold,
                min_shot: min_shot.as_str(),
                black_min: black_min.as_str(),
                no_gaps: *no_gaps,
                fps: fps.as_deref(),
                video_only: *video_only,
                list: *list,
                append: *append,
                track: *track,
                force: *force,
            });
        }
        Cmd::Bench {
            runs,
            preset,
            parallel,
        } => return cmd::export::cmd_bench(&project, *runs, *preset, *parallel),
        Cmd::Export {
            output,
            preset,
            encoder,
            crf,
            encoder_preset,
            bitrate,
            parallel,
            sequence,
        } => {
            let options = ExportOptions {
                preset: *preset,
                encoder: encoder.map(Into::into),
                crf: *crf,
                encoder_preset: encoder_preset.clone(),
                bitrate: bitrate.clone(),
                parallel: *parallel,
                use_cache: true,
            };
            return cmd_export(&project, output, *sequence, &options);
        }
        Cmd::Probe { file, json } => return cmd_probe(file, *json),
        Cmd::Trim {
            input,
            in_point,
            out_point,
            output,
            encoder,
            copy,
            crf,
            preset,
            bitrate,
        } => {
            return cmd_trim(
                input,
                in_point,
                out_point,
                output,
                (*encoder).into(),
                *copy,
                *crf,
                preset,
                bitrate,
            );
        }
        _ => {}
    }
    // Everything else is an edit: run it through the catalog.
    let action =
        to_action(&cli.command).ok_or_else(|| anyhow::anyhow!("internal: not an action"))?;
    run_cli_action(&project, &action.map_err(anyhow::Error::msg)?)
}

// ------------------------------------------------------------ project cmds

/// `--fps` accepts `n/d`, a decimal NTSC rate, or a whole number.
pub(crate) fn parse_fps_arg(s: &str) -> Result<Seconds> {
    let rate = match parse_rate(s) {
        Some(rate) => rate,
        None => match s {
            "23.976" => Seconds::new(24000, 1001),
            "29.97" => Seconds::new(30000, 1001),
            "59.94" => Seconds::new(60000, 1001),
            _ => {
                let n: i64 = s.parse().with_context(|| {
                    format!("invalid --fps {s:?} (try 30000/1001, 29.97, or 24)")
                })?;
                Seconds::from_integer(n)
            }
        },
    };
    if rate <= Seconds::from_integer(0) {
        bail!("--fps must be positive (got {s:?})");
    }
    Ok(rate)
}

/// A speed accepts `n/d` or a positive decimal (`1`, `0.5`, `0.8`, `2`).
fn parse_speed_arg(s: &str) -> Result<Seconds> {
    let bad = || format!("invalid speed {s:?} (try 100, 200, or 50)");
    let percent: f64 = s.parse().with_context(bad)?;
    if !percent.is_finite() || percent <= 0.0 {
        bail!("speed must be positive (got {s:?})");
    }
    Seconds::approximate_float(percent / 100.0)
        .filter(|r| *r > Seconds::from_integer(0))
        .with_context(bad)
}

// ------------------------------------------------------------ media cmds (M0)

// ------------------------------------------------------------ trim (M0)

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cmd::detect::{append_spans, snap_fps_for};
    use crate::cmd::import::match_asset;
    use crate::cmd::project::show_report;
    use std::path::Path;

    use nle_core::Edge;
    use nle_core::timeline::{Clip, Gap, Segment, TrackKind};
    use nle_core::{Action, GapLen, Project, Target};

    /// Every catalog action is reachable from the CLI — through the same
    /// [`to_action`] mapping `main` dispatches with. A new `Action`
    /// variant without a CLI spelling fails this.
    #[test]
    fn every_action_is_reachable_from_the_cli() {
        let cmds = [
            Cmd::TrimIn {
                at: "1".into(),
                track: 1,
                ripple: false,
            },
            Cmd::TrimOut {
                at: "1".into(),
                track: 1,
                ripple: false,
            },
            Cmd::Split {
                at: "1".into(),
                track: 1,
            },
            Cmd::Gap {
                at: "1".into(),
                frames: Some(3),
                dur: None,
                track: 1,
            },
            Cmd::Delete {
                at: Some("1".into()),
                clip: None,
                seg: None,
                from_seg: None,
                to_seg: None,
                track: 1,
                ripple: false,
            },
            Cmd::Delete {
                at: None,
                clip: None,
                seg: None,
                from_seg: Some(1),
                to_seg: Some(2),
                track: 1,
                ripple: false,
            },
            Cmd::Yank {
                at: Some("1".into()),
                clip: None,
                seg: None,
                from_seg: None,
                to_seg: None,
                track: 1,
            },
            Cmd::Yank {
                at: None,
                clip: None,
                seg: None,
                from_seg: Some(1),
                to_seg: Some(2),
                track: 1,
            },
            Cmd::Paste {
                at: "1".into(),
                track: 1,
                ripple: false,
                range: false,
            },
            Cmd::Paste {
                at: "1".into(),
                track: 1,
                ripple: false,
                range: true,
            },
            Cmd::Speed {
                id: 1,
                rate: "2".into(),
                track: 1,
            },
            Cmd::Extend {
                clip: Some(1),
                seg: None,
                at: None,
                edge: "in".into(),
                to: "2".into(),
                track: 1,
                ripple: false,
            },
            Cmd::PurgeStumps { track: None },
            Cmd::Track {
                action: TrackCmd::Add {
                    kind: TrackKindArg::Video,
                },
            },
            Cmd::Track {
                action: TrackCmd::Mute { index: None },
            },
            Cmd::Track {
                action: TrackCmd::Rm { index: 1 },
            },
            Cmd::Media {
                action: MediaCmd::Rm { id: 1 },
            },
            Cmd::Undo {},
            Cmd::Redo {},
            Cmd::Transition {
                kind: None,
                at: "1".into(),
                frames: Some(3),
                dur: None,
                percent: None,
                track: 1,
            },
            Cmd::Effect {
                kind: "crop".into(),
                clip: 1,
                track: 1,
                index: None,
                w: None,
                h: None,
                x: None,
                y: None,
                brightness: None,
                contrast: None,
                saturation: None,
                at: None,
                path: None,
            },
            Cmd::Effect {
                kind: "crop".into(),
                clip: 1,
                track: 1,
                index: None,
                w: Some(90.0),
                h: None,
                x: Some(10.0),
                y: None,
                brightness: None,
                contrast: None,
                saturation: None,
                at: Some("1".into()),
                path: None,
            },
            Cmd::Effect {
                kind: "rm".into(),
                clip: 1,
                track: 1,
                index: Some(0),
                w: None,
                h: None,
                x: None,
                y: None,
                brightness: None,
                contrast: None,
                saturation: None,
                at: None,
                path: None,
            },
            Cmd::Effect {
                kind: "clear".into(),
                clip: 1,
                track: 1,
                index: None,
                w: None,
                h: None,
                x: None,
                y: None,
                brightness: None,
                contrast: None,
                saturation: None,
                at: None,
                path: None,
            },
            Cmd::Annotation {
                action: AnnotationCmd::Add {
                    text: Some("hi".into()),
                    asset: Some(1),
                    group: None,
                    time: Some("0".into()),
                    timeline: false,
                    at: None,
                    to: None,
                    track: 1,
                    expect: None,
                    tolerance_frames: None,
                    max_move: None,
                    no_hold_size: false,
                    margin: None,
                    min_coverage: None,
                    transcript: None,
                    black: false,
                    min_frames: None,
                },
            },
            Cmd::Annotation {
                action: AnnotationCmd::Add {
                    text: Some("pin".into()),
                    asset: None,
                    group: None,
                    time: None,
                    timeline: true,
                    at: Some("1".into()),
                    to: None,
                    track: 1,
                    expect: None,
                    tolerance_frames: None,
                    max_move: None,
                    no_hold_size: false,
                    margin: None,
                    min_coverage: None,
                    transcript: None,
                    black: false,
                    min_frames: None,
                },
            },
            Cmd::Annotation {
                action: AnnotationCmd::Rm { id: 1 },
            },
            Cmd::Annotation {
                action: AnnotationCmd::Resolve { id: 1 },
            },
        ];
        let mut seen = vec![false; nle_core::action::SLOTS];
        for cmd in &cmds {
            let a = to_action(cmd)
                .unwrap_or_else(|| panic!("{cmd:?} is an edit command"))
                .unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
            let s = a.slot();
            assert!(!seen[s], "{cmd:?} duplicates slot {s}");
            seen[s] = true;
        }
        assert!(seen.iter().all(|&s| s), "CLI-unreachable actions: {seen:?}");
    }

    /// Parse one argv through clap into the catalog, exactly as `main` does.
    fn clap_action(argv: &[&str]) -> Action {
        let mut full = vec!["nile"];
        full.extend_from_slice(argv);
        let cli = Cli::try_parse_from(&full).unwrap_or_else(|e| panic!("clap {argv:?}: {e}"));
        to_action(&cli.command)
            .unwrap_or_else(|| panic!("{argv:?} is not an edit command"))
            .unwrap_or_else(|e| panic!("{argv:?}: {e}"))
    }

    /// The three layers a front-end shares: clap's spelling, `to_action`'s
    /// mapping, and the `Action` wire form `nle-protocol` sends. The
    /// slot-reachability test builds `Cmd` values directly, so a mis-wired
    /// long flag (two arguments naming one field) would slip past it.
    #[test]
    fn clap_spellings_survive_the_action_wire_form() {
        let argv: &[&[&str]] = &[
            // `--at`, `--track` and `--ripple` in one command.
            &["trim-in", "--at", "2", "--track", "2", "--ripple"],
            &["split", "--at", "1.5"],
            // The three ways to address a segment.
            &["delete", "--at", "2"],
            &["delete", "--clip", "7", "--ripple"],
            &["delete", "--seg", "3"],
            &["delete", "--from-seg", "1", "--to-seg", "2"],
            // `--edge out`, and `--edge in` on a clip id.
            &[
                "extend", "--seg", "1", "--edge", "out", "--to", "3", "--track", "2",
            ],
            &["extend", "--clip", "1", "--edge", "in", "--to", "2"],
            // The two gap lengths.
            &["gap", "--at", "1", "--frames", "3"],
            &["gap", "--at", "1", "--dur", "2.5"],
            // Speed is a percentage on the CLI, a rate in the model.
            &["speed", "1", "200"],
            // One effect's argument set.
            &[
                "effect",
                "eq",
                "--clip",
                "1",
                "--brightness",
                "0.5",
                "--saturation",
                "0.8",
            ],
            // One named transition kind.
            &["transition", "fade", "--at", "3", "--frames", "15"],
        ];
        for argv in argv {
            let action = clap_action(argv);
            let json = serde_json::to_string(&action).expect("serialize the action");
            let back: Action = serde_json::from_str(&json).expect("deserialize the action");
            assert_eq!(action, back, "{argv:?} does not survive {json}");
        }
        // A defaulted `--track` is track 1, not a distinct spelling.
        assert_eq!(
            clap_action(&["split", "--at", "1"]),
            clap_action(&["split", "--at", "1", "--track", "1"]),
            "an omitted --track must mean track 1"
        );
    }

    /// Parsing and wire round-tripping are not enough: a flag can survive both
    /// while mapping to the wrong unit or a default. Pin the ambiguous
    /// spellings by value.
    #[test]
    fn clap_spellings_map_to_their_values() {
        match clap_action(&["speed", "1", "200"]) {
            Action::SetSpeed { track, clip, rate } => {
                assert_eq!(track, 0, "track is 0-based in the model");
                assert_eq!(clip, 1);
                assert_eq!(rate, Seconds::from_integer(2), "200% is 2×");
            }
            other => panic!("speed 200 parsed as {other:?}"),
        }
        match clap_action(&["gap", "--at", "1", "--frames", "3"]) {
            Action::InsertGap { len, .. } => assert_eq!(len, GapLen::Frames(3)),
            other => panic!("gap --frames parsed as {other:?}"),
        }
        // `--dur` is seconds (5/2), never frames.
        match clap_action(&["gap", "--at", "1", "--dur", "2.5"]) {
            Action::InsertGap { len, .. } => assert_eq!(len, GapLen::Seconds(2.5)),
            other => panic!("gap --dur parsed as {other:?}"),
        }
        match clap_action(&["transition", "fade", "--at", "3", "--frames", "15"]) {
            Action::Transition { kind, .. } => {
                assert_eq!(
                    kind,
                    nle_core::TransitionKind::FadeBlack,
                    "fade is FadeBlack"
                );
            }
            other => panic!("transition fade parsed as {other:?}"),
        }
        // The effect's arguments reach the effect, with `contrast` left at its
        // neutral default rather than dropped.
        let Action::AddEffect { effect, .. } = clap_action(&[
            "effect",
            "eq",
            "--clip",
            "1",
            "--brightness",
            "0.5",
            "--saturation",
            "0.8",
        ]) else {
            panic!("effect eq is not an AddEffect");
        };
        assert_eq!(
            effect,
            nle_core::timeline::Effect::Eq {
                brightness: 0.5,
                contrast: 1.0,
                saturation: 0.8,
            }
        );

        // The three addressing flags land on three different target shapes.
        let target = |argv: &[&str]| match clap_action(argv) {
            Action::Delete { target, .. } => target,
            other => panic!("{argv:?} parsed as {other:?}"),
        };
        assert_eq!(target(&["delete", "--at", "2"]), Target::At(2.0));
        assert_eq!(target(&["delete", "--clip", "7"]), Target::Clip(7));
        assert_eq!(target(&["delete", "--seg", "3"]), Target::Segment(3));
    }

    // `nile show` names a clip's effects, in the order the export applies
    // them, so a shell-only session can see what is on a clip.
    #[test]
    fn show_report_names_a_clips_effects() {
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (a, _) = p.add_asset(
            "/media/red.mp4".into(),
            nle_core::media::RawProbe::default(),
        );
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
            id: 1,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: vec![
                nle_core::timeline::Effect::Eq {
                    brightness: 0.0,
                    contrast: 1.0,
                    saturation: 1.0,
                },
                nle_core::timeline::Effect::Lut {
                    path: "/luts/warm.cube".into(),
                },
            ],
        })];
        let report = show_report(&p);
        assert!(report.contains("· fx: eq, lut"), "{report}");
    }

    #[test]
    fn cli_flags_map_to_the_expected_action() {
        let cmd = Cmd::Delete {
            at: Some("1.5".into()),
            clip: None,
            seg: None,
            from_seg: None,
            to_seg: None,
            track: 2,
            ripple: true,
        };
        assert_eq!(
            to_action(&cmd).unwrap().unwrap(),
            Action::Delete {
                track: 1,
                target: Target::At(1.5),
                ripple: true,
            }
        );
        // --dur resolves against the project's frame rate at run time.
        let cmd = Cmd::Gap {
            at: "0".into(),
            frames: None,
            dur: Some("2".into()),
            track: 1,
        };
        assert_eq!(
            to_action(&cmd).unwrap().unwrap(),
            Action::InsertGap {
                track: 0,
                at: 0.0,
                len: GapLen::Seconds(2.0),
            }
        );
        let cmd = Cmd::Annotation {
            action: AnnotationCmd::Add {
                text: Some("range".into()),
                asset: None,
                group: None,
                time: None,
                timeline: true,
                at: Some("1".into()),
                to: Some("2".into()),
                track: 1,
                expect: None,
                tolerance_frames: None,
                max_move: None,
                no_hold_size: false,
                margin: None,
                min_coverage: None,
                transcript: None,
                black: false,
                min_frames: None,
            },
        };
        assert_eq!(
            to_action(&cmd).unwrap().unwrap(),
            Action::AnnotationAddAt {
                track: 0,
                at: 1.0,
                to: Some(2.0),
                kind: nle_core::AnnotationKind::Note {
                    text: "range".into(),
                },
            }
        );
        // Conflicting addressing is refused, not silently picked.
        let cmd = Cmd::Yank {
            at: Some("1".into()),
            clip: Some(3),
            seg: None,
            from_seg: None,
            to_seg: None,
            track: 1,
        };
        assert!(to_action(&cmd).unwrap().is_err());

        let cmd = Cmd::Delete {
            at: None,
            clip: None,
            seg: None,
            from_seg: Some(1),
            to_seg: Some(2),
            track: 2,
            ripple: false,
        };
        assert_eq!(
            to_action(&cmd).unwrap().unwrap(),
            Action::DeleteRange {
                track: 1,
                start: 1,
                end: 2,
            }
        );
        let cmd = Cmd::Paste {
            at: "1.5".into(),
            track: 2,
            ripple: false,
            range: true,
        };
        assert_eq!(
            to_action(&cmd).unwrap().unwrap(),
            Action::PasteRange { track: 1, at: 1.5 }
        );
        // Range forms reject mixed addressing and redundant ripple.
        let cmd = Cmd::Delete {
            at: Some("1".into()),
            clip: None,
            seg: None,
            from_seg: Some(0),
            to_seg: Some(1),
            track: 1,
            ripple: false,
        };
        assert!(to_action(&cmd).unwrap().is_err());
        let cmd = Cmd::Paste {
            at: "1".into(),
            track: 1,
            ripple: true,
            range: true,
        };
        assert!(to_action(&cmd).unwrap().is_err());
    }

    /// A CLI edit records history — `nile undo` undoes it, and the
    /// playhead attribution comes back with the entry.
    /// Gap lengths are validated with our own message (a negative or zero
    /// length used to fall through to "shorter than one frame").
    #[test]
    fn gap_lengths_are_validated_with_a_clear_message() {
        let gap = |frames: Option<i64>, dur: Option<&str>| {
            to_action(&Cmd::Gap {
                at: "1".into(),
                frames,
                dur: dur.map(String::from),
                track: 1,
            })
            .unwrap()
        };
        assert!(
            gap(Some(0), None)
                .unwrap_err()
                .contains("at least one frame")
        );
        assert!(
            gap(Some(-5), None)
                .unwrap_err()
                .contains("at least one frame")
        );
        assert!(
            gap(None, Some("-2"))
                .unwrap_err()
                .contains("positive length")
        );
        assert!(gap(Some(3), None).is_ok());
    }

    /// A negative `--to` is real (an edge reaching back past time 0) and
    /// must survive clap into the action; other negatives get our own
    /// error, not clap's "unexpected argument '-1'".
    #[test]
    fn negative_times_reach_the_action() {
        let cmd = Cmd::Extend {
            clip: None,
            seg: Some(1),
            at: None,
            edge: "in".into(),
            to: "-2".into(),
            track: 1,
            ripple: false,
        };
        assert_eq!(
            to_action(&cmd).unwrap().unwrap(),
            Action::Extend {
                track: 0,
                target: Target::Segment(1),
                edge: Edge::In,
                to: -2.0,
                ripple: false,
            }
        );
    }

    /// The model counts tracks from 0; the message must speak the number
    /// you typed (1-based, like `v1`/`a1`).
    #[test]
    fn track_errors_report_the_number_you_typed() {
        let dir = std::env::temp_dir().join(format!("nle-tracknum-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.json");
        Project::new().save(&path).unwrap();

        let err = run_cli_action(&path, &Action::TrackMute { track: 4 })
            .unwrap_err()
            .to_string();
        assert!(err.contains("#5"), "mute track 5 → {err}");
        let err = run_cli_action(&path, &Action::TrackRm { track: 9 })
            .unwrap_err()
            .to_string();
        assert!(err.contains("#10"), "rm track 10 → {err}");
    }

    /// Track numbers are 1-based everywhere; `0` is not a track (it used
    /// to silently mean "track 1" in `add` and `edl`).
    #[test]
    fn track_numbers_are_one_based() {
        let dir = std::env::temp_dir().join(format!("nle-track0-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.json");
        Project::new().save(&path).unwrap();

        let err = cmd_edl(&path, Some(0), None).unwrap_err().to_string();
        assert!(err.contains("1-based"), "{err}");
        let err = cmd_add(&path, Path::new("nope.mp4"), 0, None, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("1-based"), "{err}");
        // …and the valid paths still work (the composite has nothing to play
        // on an empty timeline either).
        assert!(
            cmd_edl(&path, Some(1), None).is_err(),
            "an empty timeline has no EDL"
        );
        assert!(
            cmd_edl(&path, None, None).is_err(),
            "an empty timeline has no EDL"
        );
    }

    /// `nile seek` stores a snapped, clamped playhead — the TUI opens there.
    #[test]
    fn seek_stores_a_clamped_playhead() {
        let dir = std::env::temp_dir().join(format!("nle-seek-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.json");
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (a, _) = p.add_asset(PathBuf::from("/media/red.mp4"), RawProbe::default());
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip::new(
            1,
            a,
            Seconds::from_integer(0),
            Seconds::from_integer(4),
        ))];
        p.save(&path).unwrap();

        cmd_seek(&path, Some("2")).unwrap();
        assert_eq!(
            Project::load(&path).unwrap().playhead,
            Seconds::from_integer(2)
        );
        // Past the end clamps onto the last frame: 4 s spans [0, 4), so the
        // final frame is 119/30, not the 4 s boundary.
        cmd_seek(&path, Some("99")).unwrap();
        assert_eq!(
            Project::load(&path).unwrap().playhead,
            Seconds::new(119, 30)
        );
        // …and an off-grid request snaps to a frame (1.02 s @ 30 = f30.6).
        cmd_seek(&path, Some("1.02")).unwrap();
        assert_eq!(Project::load(&path).unwrap().playhead, Seconds::new(31, 30));
    }

    #[test]
    fn cli_edits_go_through_the_undo_history() {
        let dir = std::env::temp_dir().join(format!("nle-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.json");
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (a, _) = p.add_asset(PathBuf::from("/media/red.mp4"), RawProbe::default());
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip::new(
            1,
            a,
            Seconds::from_integer(0),
            Seconds::from_integer(4),
        ))];
        p.save(&path).unwrap();

        run_cli_action(&path, &Action::Split { track: 0, at: 2.0 }).unwrap();
        assert_eq!(
            Project::load(&path)
                .unwrap()
                .timeline
                .video_track()
                .unwrap()
                .segments
                .len(),
            2
        );
        run_cli_action(&path, &Action::Undo {}).unwrap();
        let after = Project::load(&path).unwrap();
        assert_eq!(
            after.timeline.video_track().unwrap().segments.len(),
            1,
            "undo restores the pre-split timeline"
        );
        run_cli_action(&path, &Action::Redo {}).unwrap();
        assert_eq!(
            Project::load(&path)
                .unwrap()
                .timeline
                .video_track()
                .unwrap()
                .segments
                .len(),
            2,
            "redo reapplies it"
        );
    }

    fn gap(secs: i64) -> Segment {
        Segment::Gap(Gap {
            duration: Seconds::from_integer(secs),
        })
    }

    #[test]
    fn append_spans_coalesces_a_padding_gap_into_a_leading_gap() {
        let mut p = Project::new();
        let ai = p.timeline.add_track(TrackKind::Audio);
        // Appending at 5s pads 0..5, and the run starts with a 3s gap: the two
        // must merge into one 8s gap, not sit adjacent.
        append_spans(&mut p, ai, Seconds::from_integer(5), vec![gap(3), gap(2)]);
        let segs = &p.timeline.track(ai).unwrap().segments;
        assert_eq!(segs, &vec![gap(8), gap(2)], "{segs:?}");
    }

    #[test]
    fn append_spans_keeps_the_padding_when_the_run_leads_with_media() {
        let mut p = Project::new();
        let ai = p.timeline.add_track(TrackKind::Audio);
        let clip = Segment::Clip(Clip::new(
            1,
            1,
            Seconds::from_integer(0),
            Seconds::from_integer(2),
        ));
        append_spans(&mut p, ai, Seconds::from_integer(5), vec![clip.clone()]);
        let segs = &p.timeline.track(ai).unwrap().segments;
        assert_eq!(segs, &vec![gap(5), clip], "{segs:?}");
    }

    #[test]
    fn append_snaps_to_the_project_grid_not_the_source() {
        let file = Seconds::new(60000, 1001); // 59.94 source
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(24)); // 24 fps timeline
        assert_eq!(snap_fps_for(Some(&p), file), Seconds::from_integer(24));
        // A fresh project (no target) uses the source's fps.
        assert_eq!(snap_fps_for(None, file), file);
        // An append target with no fps falls back to the source's.
        assert_eq!(snap_fps_for(Some(&Project::new()), file), file);
    }

    fn pool(names: &[&str]) -> Project {
        let mut p = Project::new();
        for n in names {
            let (_, _) = p.add_asset(PathBuf::from(n), RawProbe::default());
        }
        p
    }

    // Only exact tiers bind when several candidates contain the needle —
    // a first-match would pick by pool order, not by evidence.
    #[test]
    fn fuzzy_matches_must_be_unique() {
        let p = pool(&["/media/red_a.mp4", "/media/red_b.mp4"]);
        assert_eq!(match_asset(&p, "red_a"), Some(1));
        assert_eq!(match_asset(&p, "red"), None, "two contain 'red'");
    }

    // A filename match is stronger evidence than a stem match.
    #[test]
    fn a_filename_beats_a_stem_match() {
        let p = pool(&["/media/red.mkv", "/media/red.mp4"]);
        assert_eq!(
            match_asset(&p, "red.mp4"),
            Some(2),
            "the .mp4, not the .mkv"
        );
    }

    // ... but an ambiguous exact match is refused by the same rule the
    // fuzzy tier follows.
    #[test]
    fn an_ambiguous_exact_name_stays_unresolved() {
        let p = pool(&["/a/red.mp4", "/b/red.mp4"]);
        assert_eq!(match_asset(&p, "red.mp4"), None, "two files named red.mp4");
        let p = pool(&["/a/red.mp4", "/b/red.mkv"]);
        assert_eq!(match_asset(&p, "red"), None, "two stems are 'red'");
    }

    #[test]
    fn a_generic_reel_stays_unresolved() {
        // AX is a substring of unrelated stems; without hints it must not
        // silently bind any of them (This was the "taxi" trap: reel AX +
        // taxi.mp4 pooled resolves... nothing, and import-edl says so.)
        let p = pool(&["/media/taxi.mp4"]);
        assert_eq!(match_asset(&p, "AX"), None);
        assert_eq!(match_asset(&p, "ax"), None);
    }

    #[test]
    fn unique_fuzzy_and_exact_hint_still_bind() {
        let p = pool(&["/media/red_only.mp4"]);
        assert_eq!(match_asset(&p, "red"), Some(1), "unique substring");
        // A whole-path SOURCE FILE hint matches its basename stem.
        let p = pool(&["/media/blue.mp4"]);
        assert_eq!(match_asset(&p, "/media/blue.mp4"), Some(1));
    }

    #[test]
    fn an_empty_needle_never_fuzzy_matches() {
        let p = pool(&["/media/a.mp4", "/media/b.mp4"]);
        assert_eq!(match_asset(&p, ""), None);
    }

    /// Parse one argv through clap, without requiring it to be an action.
    fn try_clap(argv: &[&str]) -> Result<Cli, clap::Error> {
        let mut full = vec!["nile"];
        full.extend_from_slice(argv);
        Cli::try_parse_from(&full)
    }

    /// A clap-valid command whose `to_action` mapping refuses it.
    fn action_error(argv: &[&str]) -> String {
        let cli = try_clap(argv).unwrap_or_else(|e| panic!("clap {argv:?}: {e}"));
        to_action(&cli.command)
            .unwrap_or_else(|| panic!("{argv:?} is not an edit command"))
            .unwrap_err()
    }

    /// `--expect` is one grammar for six checks: the clap spelling and the
    /// shared `ExpectArgs` mapping must land on the model's payload, taking
    /// the core defaults for an omitted flag.
    #[test]
    fn expect_checks_parse_and_take_the_core_defaults() {
        let expectation = |argv: &[&str]| match clap_action(argv) {
            Action::AnnotationAddAt {
                kind: nle_core::AnnotationKind::Expectation { expect },
                ..
            } => expect,
            other => panic!("{argv:?} parsed as {other:?}"),
        };

        assert_eq!(
            expectation(&[
                "annotation",
                "add",
                "--expect",
                "cam",
                "--timeline",
                "--at",
                "1",
                "--asset",
                "2",
            ]),
            nle_core::Expectation::Cam {
                asset: Some(2),
                group: None,
                tolerance_frames: 0,
            }
        );
        assert_eq!(
            expectation(&[
                "annotation",
                "add",
                "--expect",
                "cam",
                "--timeline",
                "--at",
                "1",
                "--group",
                "3",
                "--tolerance-frames",
                "4",
            ]),
            nle_core::Expectation::Cam {
                asset: None,
                group: Some(3),
                tolerance_frames: 4,
            }
        );
        assert_eq!(
            expectation(&[
                "annotation",
                "add",
                "--expect",
                "crop-stable",
                "--timeline",
                "--at",
                "1",
            ]),
            nle_core::Expectation::CropStable {
                max_move_pct_per_sec: 25.0,
                hold_size: true,
            }
        );
        assert_eq!(
            expectation(&[
                "annotation",
                "add",
                "--expect",
                "crop-stable",
                "--timeline",
                "--at",
                "1",
                "--max-move",
                "10",
                "--no-hold-size",
            ]),
            nle_core::Expectation::CropStable {
                max_move_pct_per_sec: 10.0,
                hold_size: false,
            }
        );
        assert_eq!(
            expectation(&[
                "annotation",
                "add",
                "--expect",
                "work-in-frame",
                "--timeline",
                "--at",
                "1",
            ]),
            nle_core::Expectation::WorkInFrame { margin_pct: 5.0 }
        );
        assert_eq!(
            expectation(&[
                "annotation",
                "add",
                "--expect",
                "words-complete",
                "--timeline",
                "--at",
                "1",
            ]),
            nle_core::Expectation::WordsComplete {
                min_coverage: 1.0,
                transcript: None,
            }
        );
        assert_eq!(
            expectation(&[
                "annotation",
                "add",
                "--expect",
                "absent",
                "--timeline",
                "--at",
                "1",
                "--black",
            ]),
            nle_core::Expectation::Absent {
                asset: None,
                group: None,
                black: true,
                tolerance_frames: 0,
            }
        );
        assert_eq!(
            expectation(&[
                "annotation",
                "add",
                "--expect",
                "no-sliver",
                "--timeline",
                "--at",
                "1",
                "--asset",
                "1",
            ]),
            nle_core::Expectation::NoSliver {
                min_frames: 2,
                asset: Some(1),
            }
        );
        // `--to` makes the expectation a range, exactly like a note.
        match clap_action(&[
            "annotation",
            "add",
            "--expect",
            "cam",
            "--timeline",
            "--at",
            "1",
            "--to",
            "2",
            "--asset",
            "1",
        ]) {
            Action::AnnotationAddAt { at, to, .. } => {
                assert_eq!(at, 1.0);
                assert_eq!(to, Some(2.0));
            }
            other => panic!("expect range parsed as {other:?}"),
        }
    }

    /// A check flag belongs to one check; passing it to another (or to a
    /// note) is refused instead of silently dropped.
    #[test]
    fn expect_flags_reject_the_wrong_check_and_the_wrong_anchor() {
        let note_with_black = Cmd::Annotation {
            action: AnnotationCmd::Add {
                text: Some("hi".into()),
                asset: Some(1),
                group: None,
                time: Some("0".into()),
                timeline: false,
                at: None,
                to: None,
                track: 1,
                expect: None,
                tolerance_frames: None,
                max_move: None,
                no_hold_size: false,
                margin: None,
                min_coverage: None,
                transcript: None,
                black: true,
                min_frames: None,
            },
        };
        let err = to_action(&note_with_black).unwrap().unwrap_err();
        assert!(err.contains("--black needs --expect"), "{err}");

        let err = action_error(&[
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
        ]);
        assert!(
            err.contains("--black does not apply to the cam check"),
            "{err}"
        );

        let err = action_error(&[
            "annotation",
            "add",
            "pin",
            "--timeline",
            "--at",
            "1",
            "--asset",
            "1",
        ]);
        assert!(err.contains("cannot carry --asset"), "{err}");
    }

    /// clap owns the usage-level refusals (exit 2), the mapping owns the
    /// rest: neither may accept a half-specified expectation.
    #[test]
    fn clap_refuses_text_with_expect_and_expect_without_timeline() {
        assert!(
            try_clap(&[
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
            ])
            .is_err(),
            "text and --expect conflict"
        );
        assert!(
            try_clap(&[
                "annotation",
                "add",
                "--expect",
                "cam",
                "--at",
                "1",
                "--asset",
                "1",
            ])
            .is_err(),
            "--expect needs --timeline"
        );
        assert!(
            try_clap(&["annotation", "add", "--timeline", "--at", "1"]).is_err(),
            "a note still needs TEXT"
        );

        // `to_action` is also called with hand-built values; it must refuse
        // an expectation with no timeline rather than invent one.
        let cmd = Cmd::Annotation {
            action: AnnotationCmd::Add {
                text: None,
                asset: Some(1),
                group: None,
                time: None,
                timeline: false,
                at: Some("1".into()),
                to: None,
                track: 1,
                expect: Some(ExpectCheck::Cam),
                tolerance_frames: None,
                max_move: None,
                no_hold_size: false,
                margin: None,
                min_coverage: None,
                transcript: None,
                black: false,
                min_frames: None,
            },
        };
        let err = to_action(&cmd).unwrap().unwrap_err();
        assert!(err.contains("--expect needs --timeline"), "{err}");
    }

    /// `check` names its report with `--report`, matching the `:cmd` grammar
    /// the TUI will use; a positional is not accepted.
    #[test]
    fn annotation_check_takes_the_report_as_a_flag() {
        assert!(
            try_clap(&[
                "annotation",
                "check",
                "--report",
                "out.mp4",
                "--sequence",
                "1",
                "--id",
                "3",
                "--json",
            ])
            .is_ok()
        );
        assert!(
            try_clap(&["annotation", "check", "out.mp4"]).is_err(),
            "the report is a --report flag, not a positional"
        );
        assert!(
            try_clap(&["annotation", "check"]).is_err(),
            "--report is required"
        );
    }
}
