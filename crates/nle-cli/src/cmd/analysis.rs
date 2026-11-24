//! Analysis products beside the project: transcript import today.

use std::path::Path;

use anyhow::{Context, Result};

use nle_core::Project;
use nle_core::analysis::{Transcript, TranscriptInput, TranscriptStore};

use crate::say;

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
