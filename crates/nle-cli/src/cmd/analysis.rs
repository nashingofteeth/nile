//! Analysis products beside the project: transcript import and the sidecar
//! analyzer run.

use std::path::Path;

use anyhow::{Context, Result, bail};

use nle_core::analysis::{
    AnalysisProduct, ProductStore, Transcript, TranscriptInput, TranscriptStore,
};
use nle_core::{Project, Seconds, parse_time};
use nle_ffmpeg::sampling::{FrameSpec, SampleCache};

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

pub(crate) fn cmd_analyze(project_path: &Path, args: AnalyzeArgs<'_>) -> Result<()> {
    let project = Project::load(project_path)?;
    let media = project
        .asset(args.asset)
        .with_context(|| format!("asset #{} is not in the media pool", args.asset))?;
    if !media.probe.has_video() {
        bail!("asset #{} has no video stream to sample", args.asset);
    }
    let start = match args.start {
        Some(value) => parse_cli_time(value, "--start")?,
        None => Seconds::from_integer(0),
    };
    let duration = match args.duration {
        Some(value) => parse_cli_time(value, "--duration")?,
        None => {
            media.duration().with_context(|| {
                format!(
                    "asset #{} has no probed duration; pass --duration",
                    args.asset
                )
            })? - start
        }
    };
    if start < Seconds::from_integer(0) {
        bail!("--start must not be negative");
    }
    if duration <= Seconds::from_integer(0) {
        bail!("--duration must be positive (the window ends at or before its start)");
    }
    let fps = match args.fps {
        Some(value) => parse_fps_arg(value)?,
        None => media
            .fps()
            .or(project.fps)
            .with_context(|| format!("asset #{} has no frame rate; pass --fps", args.asset))?,
    };
    let (width, height) = match (args.width, args.height) {
        (Some(width), Some(height)) => (width, height),
        (None, None) => (64, 36),
        _ => bail!("pass both --width and --height (or neither for the 64×36 default)"),
    };
    let spec = FrameSpec {
        source: media.path.clone(),
        start,
        duration,
        fps,
        width,
        height,
    };
    analysis::check_sample_budget(&spec)?;
    let cache = SampleCache::new(nle_core::state::cache_dir(project_path).join("analysis"));
    let (_sample, entry) = cache.frames_entry(&spec)?;
    let stage = analysis::StageDir::create(cache.root())?;
    let store = ProductStore::at(project_path);
    let hash = analysis::run_sidecar(&spec, &entry, &stage, &store)?;
    // Parse once more: an `a1-` address with a broken payload would be a
    // corrupt store, not a successful analysis.
    let product: AnalysisProduct = store.load(&hash)?;
    product
        .validate()
        .map_err(|error| anyhow::anyhow!("stored product: {error}"))?;
    say!("{hash}");
    Ok(())
}

fn parse_cli_time(value: &str, flag: &str) -> Result<Seconds> {
    parse_time(value).with_context(|| format!("invalid {flag} {value:?} (try 1.5 or 0:00:01.500)"))
}
