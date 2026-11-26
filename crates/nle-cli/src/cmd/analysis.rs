//! Analysis products beside the project: transcript import and the sidecar
//! analyzer run.

use std::path::Path;

use anyhow::{Context, Result, bail};

use nle_core::analysis::{
    AnalysisProduct, ProductStore, Transcript, TranscriptInput, TranscriptStore,
};
use nle_core::time::checked_sub;
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

const BAD_WINDOW: &str = "--duration must be positive (the window ends at or before its start)";

pub(crate) fn cmd_analyze(project_path: &Path, args: AnalyzeArgs<'_>) -> Result<()> {
    let project = Project::load(project_path)?;
    let media = project
        .asset(args.asset)
        .with_context(|| format!("asset #{} is not in the media pool", args.asset))?;
    if !media.probe.has_video() {
        bail!("asset #{} has no video stream to sample", args.asset);
    }
    // `parse_time` already rejects a negative start; this value only has to
    // stay exact through the default-duration subtraction.
    let start = match args.start {
        Some(value) => parse_cli_time(value, "--start")?,
        None => Seconds::from_integer(0),
    };
    let duration = match args.duration {
        Some(value) => parse_cli_time(value, "--duration")?,
        None => {
            let asset_duration = media.duration().with_context(|| {
                format!(
                    "asset #{} has no probed duration; pass --duration",
                    args.asset
                )
            })?;
            default_duration(asset_duration, start)?
        }
    };
    if duration <= Seconds::from_integer(0) {
        bail!(BAD_WINDOW);
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
    if width == 0 || height == 0 {
        // Before the budget: a zero dimension is a bad grid, not a window
        // shorter than one frame.
        bail!("width and height must be non-zero");
    }
    let spec = FrameSpec {
        source: media.path.clone(),
        start,
        duration,
        fps,
        width,
        height,
    };
    analysis::check_sample_budget(&spec)?;
    let cache = SampleCache::new(analysis::sample_cache_dir(project_path));
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
