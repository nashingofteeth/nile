//! Analysis products beside the project: transcript import and the sidecar
//! analyzer run.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use nle_core::analysis::{
    AnalysisProduct, ProductStore, Transcript, TranscriptInput, TranscriptStore,
};
use nle_core::time::checked_sub;
use nle_core::{Project, Seconds, parse_time};
use nle_ffmpeg::sampling::{AudioSpec, FrameMode, FrameSpec, SampleCache};

use crate::{analysis, parse_fps_arg, say};

/// `nile analysis import --asset N FILE` — store a words-only transcript and
/// print its content hash, which `--transcript HASH` then names.
///
/// The file is `{ "words": [ { "text", "start", "end" } ] }`; times are exact
/// rational source seconds (`"1/2"`), not floats. The asset must be in the
/// pool, and every word must end inside its probed duration.
pub(crate) fn cmd_import(project_path: &Path, asset: u32, file: &Path) -> Result<()> {
    let project = Project::load(project_path)?;
    let media = project
        .asset(asset)
        .with_context(|| format!("asset #{asset} is not in the media pool"))?;
    let text = std::fs::read_to_string(file)
        .with_context(|| format!("reading transcript {}", file.display()))?;
    let input: TranscriptInput = serde_json::from_str(&text)
        .with_context(|| format!("parsing {} as a transcript", file.display()))?;
    let transcript = Transcript::new(asset, input.words);
    transcript.validate(media.probe.duration())?;
    let hash = TranscriptStore::at(project_path).import(&transcript)?;
    say!("{hash}");
    Ok(())
}

/// `nile analysis analyze --asset N …` — sample one source window, run the
/// bundled mean-luma analyzer out of process, and print the `a1-…` address of
/// the product it stored under `analysis/products/`.
pub(crate) struct AnalyzeArgs<'a> {
    pub(crate) asset: u32,
    pub(crate) start: Option<&'a str>,
    pub(crate) duration: Option<&'a str>,
    pub(crate) fps: Option<&'a str>,
    pub(crate) width: Option<u32>,
    pub(crate) height: Option<u32>,
}

const BAD_WINDOW: &str = "--duration must be positive (the window ends at or before its start)";

pub(crate) fn cmd_analyze(project_path: &Path, args: AnalyzeArgs<'_>) -> Result<()> {
    let project = Project::load(project_path)?;
    let spec = sample_spec(
        &project,
        args.asset,
        args.start,
        args.duration,
        args.fps,
        args.width,
        args.height,
    )?;
    analysis::check_sample_budget(&spec)?;
    let cache = SampleCache::new(analysis::sample_cache_dir(project_path));
    let (_sample, entry) = cache.frames_entry(&spec)?;
    let stage = analysis::StageDir::create(cache.root())?;
    let store = ProductStore::at(project_path);
    let hash = analysis::run_sidecar(
        analysis::SampleRequest::Frames(&spec),
        &entry,
        &stage,
        &store,
        &analysis::Task::MeanLuma,
        &analysis::Analyzer::Bundled,
    )?;
    // Parse once more: an `a1-` address with a broken payload would be a
    // corrupt store, not a successful analysis.
    let product: AnalysisProduct = store.load(&hash)?;
    product
        .validate()
        .map_err(|error| anyhow::anyhow!("stored product: {error}"))?;
    say!("{hash}");
    Ok(())
}

/// `nile analysis detect --asset N --label face …` — run a subject detector
/// over one source window and print the product's `a1-…` address.
///
/// The detector is the bundled deterministic colour-box child unless
/// `--analyzer` (or `NILE_ANALYZER`) names an absolute executable. There is no
/// PATH search and no project-file detector setting: a detector is a process
/// boundary, not a dependency.
pub(crate) struct DetectArgs<'a> {
    pub(crate) asset: u32,
    pub(crate) start: Option<&'a str>,
    pub(crate) duration: Option<&'a str>,
    pub(crate) fps: Option<&'a str>,
    pub(crate) width: Option<u32>,
    pub(crate) height: Option<u32>,
    pub(crate) labels: &'a [String],
    pub(crate) analyzer: Option<&'a Path>,
    pub(crate) min_confidence: Option<f64>,
}

pub(crate) fn cmd_detect(project_path: &Path, args: DetectArgs<'_>) -> Result<()> {
    let project = Project::load(project_path)?;
    let spec = sample_spec(
        &project,
        args.asset,
        args.start,
        args.duration,
        args.fps,
        args.width,
        args.height,
    )?;
    let mut labels: Vec<String> = args
        .labels
        .iter()
        .map(|label| label.trim().to_string())
        .filter(|label| !label.is_empty())
        .collect();
    if labels.is_empty() {
        bail!("pass at least one --label NAME");
    }
    labels.sort();
    labels.dedup();
    let min_confidence = args.min_confidence.unwrap_or(0.0);
    if !min_confidence.is_finite() || !(0.0..=1.0).contains(&min_confidence) {
        bail!("--min-confidence must be between 0 and 1");
    }
    let floor_milli = (min_confidence * 1000.0).round() as u32;
    let analyzer = resolve_analyzer(args.analyzer)?;
    analysis::check_sample_budget(&spec)?;
    let cache = SampleCache::new(analysis::sample_cache_dir(project_path));
    let (_sample, entry) = cache.frames_entry(&spec)?;
    let stage = analysis::StageDir::create(cache.root())?;
    let store = ProductStore::at(project_path);
    let hash = analysis::run_sidecar(
        analysis::SampleRequest::Frames(&spec),
        &entry,
        &stage,
        &store,
        &analysis::Task::SubjectBoxes {
            labels,
            floor_milli,
        },
        &analyzer,
    )?;
    let product: AnalysisProduct = store.load(&hash)?;
    product
        .validate()
        .map_err(|error| anyhow::anyhow!("stored product: {error}"))?;
    say!("{hash}");
    Ok(())
}

/// `nile analysis analyze-audio --asset N …` — sample an audio window and run
/// the bundled level analyzer out of process.
pub(crate) struct AnalyzeAudioArgs<'a> {
    pub(crate) asset: u32,
    pub(crate) start: Option<&'a str>,
    pub(crate) duration: Option<&'a str>,
    pub(crate) window: Option<&'a str>,
    pub(crate) sample_rate: Option<u32>,
    pub(crate) channels: Option<u16>,
}

pub(crate) fn cmd_analyze_audio(project_path: &Path, args: AnalyzeAudioArgs<'_>) -> Result<()> {
    let project = Project::load(project_path)?;
    let (spec, window_samples) = audio_spec(
        &project,
        args.asset,
        args.start,
        args.duration,
        args.window,
        args.sample_rate,
        args.channels,
    )?;
    let cache = SampleCache::new(analysis::sample_cache_dir(project_path));
    let (_sample, entry) = cache.audio_entry(&spec)?;
    let stage = analysis::StageDir::create(cache.root())?;
    let store = ProductStore::at(project_path);
    let hash = analysis::run_sidecar(
        analysis::SampleRequest::Audio(&spec),
        &entry,
        &stage,
        &store,
        &analysis::Task::AudioLevels { window_samples },
        &analysis::Analyzer::Bundled,
    )?;
    let product: AnalysisProduct = store.load(&hash)?;
    product
        .validate()
        .map_err(|error| anyhow::anyhow!("stored product: {error}"))?;
    say!("{hash}");
    Ok(())
}

fn resolve_analyzer(path: Option<&Path>) -> Result<analysis::Analyzer> {
    let selected = match path {
        Some(path) => path.to_path_buf(),
        None => match std::env::var_os("NILE_ANALYZER") {
            Some(value) => PathBuf::from(value),
            None => return Ok(analysis::Analyzer::Bundled),
        },
    };
    if !selected.is_absolute() {
        bail!("analyzer path must be absolute: {}", selected.display());
    }
    Ok(analysis::Analyzer::External(selected))
}

/// Resolve the audio window and its window length in samples per channel.
///
/// Rate and channels default to the asset's own audio stream (48 kHz mono when
/// the probe does not say), so a caller gets a sample at the media's rate
/// without repeating the probe; `--sample-rate`/`--channels` override.
fn audio_spec(
    project: &Project,
    asset: u32,
    start: Option<&str>,
    duration: Option<&str>,
    window: Option<&str>,
    sample_rate: Option<u32>,
    channels: Option<u16>,
) -> Result<(AudioSpec, u64)> {
    let media = project
        .asset(asset)
        .with_context(|| format!("asset #{asset} is not in the media pool"))?;
    if !media.probe.has_audio() {
        bail!("asset #{asset} has no audio stream to sample");
    }
    let start = match start {
        Some(value) => parse_cli_time(value, "--start")?,
        None => Seconds::from_integer(0),
    };
    let duration = match duration {
        Some(value) => parse_cli_time(value, "--duration")?,
        None => {
            let asset_duration = media.duration().with_context(|| {
                format!("asset #{asset} has no probed duration; pass --duration")
            })?;
            default_duration(asset_duration, start)?
        }
    };
    if duration <= Seconds::from_integer(0) {
        bail!(BAD_WINDOW);
    }
    let stream = media.probe.audio();
    let sample_rate = match sample_rate {
        Some(rate) => rate,
        None => stream
            .and_then(|audio| audio.sample_rate.as_deref())
            .and_then(|rate| rate.parse::<u32>().ok())
            .filter(|rate| *rate > 0)
            .unwrap_or(48_000),
    };
    if sample_rate == 0 {
        bail!("--sample-rate must be non-zero");
    }
    let channels = match channels {
        Some(channels) => channels,
        None => stream
            .and_then(|audio| audio.channels)
            .and_then(|channels| u16::try_from(channels).ok())
            .filter(|channels| *channels > 0)
            .unwrap_or(1),
    };
    if channels == 0 {
        bail!("--channels must be non-zero");
    }
    let window = match window {
        Some(value) => parse_cli_time(value, "--window")?,
        None => Seconds::new(1, 10),
    };
    if window <= Seconds::from_integer(0) {
        bail!("--window must be positive");
    }
    let window_samples = floor_samples(window, sample_rate);
    if window_samples == 0 {
        bail!("--window is shorter than one sample at {sample_rate} Hz");
    }
    Ok((
        AudioSpec {
            source: media.path.clone(),
            start,
            duration,
            sample_rate,
            channels,
        },
        window_samples,
    ))
}

/// `floor(window × sample_rate)` in exact integers: a window is a whole number
/// of samples, never a rounded-up one that would overrun the payload.
fn floor_samples(window: Seconds, sample_rate: u32) -> u64 {
    let numerator = i128::from(*window.numer()) * i128::from(sample_rate);
    let denominator = i128::from(*window.denom());
    if numerator <= 0 || denominator <= 0 {
        return 0;
    }
    (numerator / denominator).min(u64::MAX as i128) as u64
}

/// Resolve the shared source window into the grid the sampler will pack.
fn sample_spec(
    project: &Project,
    asset: u32,
    start: Option<&str>,
    duration: Option<&str>,
    fps: Option<&str>,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<FrameSpec> {
    let media = project
        .asset(asset)
        .with_context(|| format!("asset #{asset} is not in the media pool"))?;
    if !media.probe.has_video() {
        bail!("asset #{asset} has no video stream to sample");
    }
    // `parse_time` already rejects a negative start; this value only has to
    // stay exact through the default-duration subtraction.
    let start = match start {
        Some(value) => parse_cli_time(value, "--start")?,
        None => Seconds::from_integer(0),
    };
    let duration = match duration {
        Some(value) => parse_cli_time(value, "--duration")?,
        None => {
            let asset_duration = media.duration().with_context(|| {
                format!("asset #{asset} has no probed duration; pass --duration")
            })?;
            default_duration(asset_duration, start)?
        }
    };
    if duration <= Seconds::from_integer(0) {
        bail!(BAD_WINDOW);
    }
    let fps = match fps {
        Some(value) => parse_fps_arg(value)?,
        None => media
            .fps()
            .or(project.fps)
            .with_context(|| format!("asset #{asset} has no frame rate; pass --fps"))?,
    };
    let (width, height) = match (width, height) {
        (Some(width), Some(height)) => (width, height),
        (None, None) => (64, 36),
        _ => bail!("pass both --width and --height (or neither for the 64×36 default)"),
    };
    if width == 0 || height == 0 {
        // Before the budget: a zero dimension is a bad grid, not a window
        // shorter than one frame.
        bail!("width and height must be non-zero");
    }
    Ok(FrameSpec {
        source: media.path.clone(),
        mode: FrameMode::Grid {
            start,
            duration,
            fps,
        },
        width,
        height,
    })
}

fn parse_cli_time(value: &str, flag: &str) -> Result<Seconds> {
    parse_time(value).with_context(|| format!("invalid {flag} {value:?} (try 1.5 or 0:00:01.500)"))
}

/// `asset_duration - start`, refusing overflow instead of panicking on a
/// hand-typed extreme start. A non-integer probe duration and a huge start
/// can exceed `i64` while the exact difference is still meaningful.
fn default_duration(asset_duration: Seconds, start: Seconds) -> Result<Seconds> {
    checked_sub(&asset_duration, &start).ok_or_else(|| anyhow::anyhow!(BAD_WINDOW))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_huge_start_is_refused_not_panicked() {
        // A non-integer probe duration: the unchecked subtraction
        // `i64::MAX/3 - i64::MAX` overflows `i64` in debug before it can be
        // compared with zero.
        let asset_duration = Seconds::new(i64::MAX, 3);
        let start = Seconds::from_integer(i64::MAX);
        let error = default_duration(asset_duration, start).unwrap_err();
        assert!(error.to_string().contains("window ends"), "{error}");

        // The ordinary non-integer case still subtracts exactly.
        let duration = default_duration(Seconds::new(10, 3), Seconds::new(1, 3)).unwrap();
        assert_eq!(duration, Seconds::from_integer(3));
    }
}
