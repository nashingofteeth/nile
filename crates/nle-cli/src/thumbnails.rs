//! Cached asset thumbnails for the media pool.
//!
//! One RGB frame is extracted at a low resolution and cached beside the
//! project. The TUI renders it with truecolor half-blocks; no terminal image
//! protocol is required.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;

use nle_core::{Asset, secs_f64};

pub const THUMB_WIDTH: u32 = 96;
pub const THUMB_HEIGHT: u32 = 54;
const THUMB_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum ThumbError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("asset has no video stream")]
    NoVideo,
    #[error("ffmpeg could not extract a thumbnail: {0}")]
    Ffmpeg(String),
    #[error("ffmpeg returned {0} bytes for a {THUMB_WIDTH}×{THUMB_HEIGHT} thumbnail")]
    BadOutput(usize),
}

/// A thumbnail frame in row-major RGB24 order.
#[derive(Debug, Clone, PartialEq)]
pub struct Thumbnail {
    pub rgb: Vec<u8>,
}

impl Thumbnail {
    pub fn new(rgb: Vec<u8>) -> Result<Self, ThumbError> {
        if rgb.len() != (THUMB_WIDTH * THUMB_HEIGHT * 3) as usize {
            return Err(ThumbError::BadOutput(rgb.len()));
        }
        Ok(Self { rgb })
    }

    /// RGB at a pixel; callers clamp the coordinates into the frame.
    pub fn pixel(&self, x: u32, y: u32) -> (u8, u8, u8) {
        let i = ((y.min(THUMB_HEIGHT - 1) * THUMB_WIDTH + x.min(THUMB_WIDTH - 1)) * 3) as usize;
        (self.rgb[i], self.rgb[i + 1], self.rgb[i + 2])
    }
}

pub fn cache_dir(project_path: &Path) -> PathBuf {
    nle_core::state::cache_dir(project_path).join("thumbs")
}

/// Load this asset's thumbnail, extracting it if the cache is cold.
pub fn load(project_path: &Path, asset: &Asset) -> Result<Thumbnail, ThumbError> {
    if !asset.probe.has_video() {
        return Err(ThumbError::NoVideo);
    }
    let dir = cache_dir(project_path);
    std::fs::create_dir_all(&dir)?;
    let hash = asset_hash(asset);
    let path = dir.join(format!("{hash:016x}.rgb"));
    if let Ok(rgb) = std::fs::read(&path)
        && let Ok(thumb) = Thumbnail::new(rgb)
    {
        return Ok(thumb);
    }

    let mid = asset.duration().map(|d| secs_f64(&d) / 2.0).unwrap_or(0.0);
    let filter = format!(
        "scale={THUMB_WIDTH}:{THUMB_HEIGHT}:force_original_aspect_ratio=decrease,\
         pad={THUMB_WIDTH}:{THUMB_HEIGHT}:(ow-iw)/2:(oh-ih)/2:black"
    );
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-ss", &format!("{mid:.3}"), "-i"])
        .arg(&asset.path)
        .args([
            "-frames:v",
            "1",
            "-an",
            "-vf",
            &filter,
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-",
        ])
        .output()?;
    if !out.status.success() {
        return Err(ThumbError::Ffmpeg(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    let thumb = Thumbnail::new(out.stdout)?;
    let tmp = tempfile::Builder::new()
        .prefix(&format!("{hash:016x}.tmp-"))
        .suffix(".rgb")
        .tempfile_in(&dir)?;
    std::fs::write(tmp.path(), &thumb.rgb)?;
    tmp.persist(&path).map_err(|e| e.error)?;
    Ok(thumb)
}

fn asset_hash(asset: &Asset) -> u64 {
    let mut h = DefaultHasher::new();
    THUMB_VERSION.hash(&mut h);
    THUMB_WIDTH.hash(&mut h);
    THUMB_HEIGHT.hash(&mut h);
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

    #[test]
    fn thumbnails_are_extracted_and_cached() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("thumb.mp4");
        let out = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x120:rate=30:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&media)
            .status()
            .unwrap();
        assert!(out.success(), "generate video");
        let probe = nle_ffmpeg::probe(&media).unwrap();
        let asset = Asset {
            id: 1,
            path: media,
            probe,
        };
        let project_path = dir.path().join("nile.json");
        let first = load(&project_path, &asset).unwrap();
        assert_eq!(first.rgb.len(), (THUMB_WIDTH * THUMB_HEIGHT * 3) as usize);
        assert!(first.rgb.iter().any(|b| *b > 0), "not all black");
        let cached = cache_dir(&project_path).read_dir().unwrap().count();
        let second = load(&project_path, &asset).unwrap();
        assert_eq!(second, first);
        assert_eq!(cache_dir(&project_path).read_dir().unwrap().count(), cached);
    }

    #[test]
    fn a_stale_thumbnail_is_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let project_path = dir.path().join("nile.json");
        let cache = cache_dir(&project_path);
        std::fs::create_dir_all(&cache).unwrap();
        let path = cache.join("0123456789abcdef.rgb");
        std::fs::write(&path, vec![0u8; 16]).unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_times(
            std::fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(3600)),
        )
        .unwrap();

        let report = prune(&project_path, Duration::ZERO, false).unwrap();
        assert_eq!(report.removed, 1, "{report:?}");
        assert!(!path.exists(), "the stale thumbnail is gone");
    }

    #[test]
    fn audio_only_assets_have_no_thumbnail() {
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
            Err(ThumbError::NoVideo)
        ));
    }
}
