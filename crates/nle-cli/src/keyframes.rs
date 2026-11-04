//! Cached video keyframe timestamps for the pool preview.
//!
//! Probing scans the whole file, so the result is kept beside the project
//! (keyed by source path + mtime + format version) and reused across
//! sessions. The TUI probes on a worker; navigation never spawns ffprobe.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use nle_core::Asset;

#[derive(Debug, Error)]
pub enum KeyframeError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Probe(#[from] nle_ffmpeg::ProbeError),
    #[error("asset has no video stream")]
    NoVideo,
}

/// One asset's keyframe timestamps, in seconds and sorted ascending.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Keyframes {
    pub times: Vec<f64>,
}

const KEYFRAME_VERSION: u32 = 1;

/// Cached keyframe files for one project.
pub fn cache_dir(project_path: &Path) -> PathBuf {
    nle_core::state::cache_dir(project_path).join("keyframes")
}

/// Load this asset's keyframes, probing the file if the cache is cold.
pub fn load(project_path: &Path, asset: &Asset) -> Result<Keyframes, KeyframeError> {
    if !asset.probe.has_video() {
        return Err(KeyframeError::NoVideo);
    }
    let dir = cache_dir(project_path);
    std::fs::create_dir_all(&dir)?;
    let hash = asset_hash(asset);
    let path = dir.join(format!("{hash:016x}.json"));
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(cached) = serde_json::from_str::<Keyframes>(&text)
        && cached.times.windows(2).all(|w| w[0] <= w[1])
    {
        return Ok(cached);
    }

    let times = nle_ffmpeg::keyframes(&asset.path)?;
    let keyframes = Keyframes { times };
    let tmp = tempfile::Builder::new()
        .prefix(&format!("{hash:016x}.tmp-"))
        .suffix(".json")
        .tempfile_in(&dir)?;
    serde_json::to_writer(tmp.as_file(), &keyframes)?;
    tmp.persist(&path).map_err(|e| e.error)?;
    Ok(keyframes)
}

fn asset_hash(asset: &Asset) -> u64 {
    let mut h = DefaultHasher::new();
    KEYFRAME_VERSION.hash(&mut h);
    asset.path.hash(&mut h);
    if let Ok(meta) = std::fs::metadata(&asset.path) {
        meta.len().hash(&mut h);
        if let Ok(modified) = meta.modified()
            && let Ok(since) = modified.duration_since(UNIX_EPOCH)
        {
            since.as_secs().hash(&mut h);
            since.subsec_nanos().hash(&mut h);
        }
    }
    h.finish()
}

/// Prune this project's cached keyframe lists.
pub fn prune(
    project_path: &Path,
    max_age: Duration,
    dry_run: bool,
) -> Result<crate::proxy::PruneReport, crate::proxy::ProxyError> {
    crate::proxy::prune_at(
        &cache_dir(project_path),
        max_age,
        SystemTime::now(),
        dry_run,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_core::media::{RawFormat, RawProbe, RawStream};
    use std::process::Command;

    fn video_asset(path: PathBuf) -> Asset {
        let probe = nle_ffmpeg::probe(&path).unwrap();
        Asset { id: 1, path, probe }
    }

    #[test]
    fn probes_and_caches_real_keyframes() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("kf.mp4");
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
        let asset = video_asset(media);
        let project_path = dir.path().join("nile.json");

        let first = load(&project_path, &asset).unwrap();
        assert!(first.times.len() >= 3, "{:?}", first.times);
        assert!(first.times.windows(2).all(|w| w[0] <= w[1]));
        let cached = cache_dir(&project_path).read_dir().unwrap().count();

        let second = load(&project_path, &asset).unwrap();
        assert_eq!(second, first, "the cache round-trips");
        assert_eq!(
            cache_dir(&project_path).read_dir().unwrap().count(),
            cached,
            "the second load does not add a cache entry"
        );
    }

    #[test]
    fn audio_only_assets_have_no_keyframes() {
        let asset = Asset {
            id: 1,
            path: "/media/tone.wav".into(),
            probe: RawProbe {
                streams: vec![RawStream {
                    index: 0,
                    codec_type: Some("audio".into()),
                    codec_name: Some("pcm".into()),
                    ..Default::default()
                }],
                format: RawFormat {
                    format_name: "wav".into(),
                    duration: Some("1".into()),
                    ..Default::default()
                },
            },
        };
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            load(&dir.path().join("nile.json"), &asset),
            Err(KeyframeError::NoVideo)
        ));
    }
}
