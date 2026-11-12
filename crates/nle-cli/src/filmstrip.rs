//! Cached timeline filmstrips.
//!
//! Each strip is a row-major RGB sprite sampled across one clip's *source*
//! span. The timeline maps a cell's source time back into the sprite and draws
//! it with truecolor half-blocks; no terminal image protocol is required.
//!
//! The cache is keyed by source path + mtime + source in/out, not by asset
//! alone: a two-hour asset used for a five-second clip should extract only
//! those five seconds.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use thiserror::Error;

use nle_core::time::Seconds;
use nle_core::{Asset, secs_f64};

/// Samples across the clip's source span.
pub const FILMSTRIP_COLS: u32 = 64;
/// Vertical pixels in one strip. The timeline draws two terminal rows, each
/// using two pixels (foreground/background half-blocks).
pub const FILMSTRIP_ROWS: u32 = 4;
const FILMSTRIP_VERSION: u32 = 1;

/// One clip's source span. Two clips with the same source span share a strip;
/// a trim or split gets a new key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FilmstripKey {
    pub asset: u32,
    pub src_in: (i64, i64),
    pub src_out: (i64, i64),
}

impl FilmstripKey {
    pub fn new(asset: u32, src_in: &Seconds, src_out: &Seconds) -> Self {
        Self {
            asset,
            src_in: (*src_in.numer(), *src_in.denom()),
            src_out: (*src_out.numer(), *src_out.denom()),
        }
    }
}

#[derive(Debug, Error)]
pub enum FilmstripError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("asset has no video stream")]
    NoVideo,
    #[error("clip has no positive source span")]
    BadSpan,
    #[error("ffmpeg could not extract a filmstrip: {0}")]
    Ffmpeg(String),
    #[error("ffmpeg returned {0} bytes for a filmstrip")]
    BadOutput(usize),
}

/// A source-span filmstrip in row-major RGB24 order.
#[derive(Debug, Clone, PartialEq)]
pub struct Filmstrip {
    pub cols: u32,
    pub rows: u32,
    pub rgb: Vec<u8>,
}

impl Filmstrip {
    pub fn new(rgb: Vec<u8>) -> Result<Self, FilmstripError> {
        let stride = (FILMSTRIP_ROWS * 3) as usize;
        if rgb.is_empty() || !rgb.len().is_multiple_of(stride) {
            return Err(FilmstripError::BadOutput(rgb.len()));
        }
        let cols = (rgb.len() / stride) as u32;
        if cols == 0 {
            return Err(FilmstripError::BadOutput(rgb.len()));
        }
        Ok(Self {
            cols,
            rows: FILMSTRIP_ROWS,
            rgb,
        })
    }

    /// RGB at a pixel; callers clamp the coordinates into the sprite.
    pub fn pixel(&self, col: u32, row: u32) -> (u8, u8, u8) {
        let c = col.min(self.cols - 1);
        let r = row.min(self.rows - 1);
        let i = ((r * self.cols + c) * 3) as usize;
        (self.rgb[i], self.rgb[i + 1], self.rgb[i + 2])
    }
}

pub fn cache_dir(project_path: &Path) -> PathBuf {
    nle_core::state::cache_dir(project_path).join("filmstrip")
}

/// Load this source span's filmstrip, extracting it if the cache is cold.
pub fn load(
    project_path: &Path,
    asset: &Asset,
    src_in: Seconds,
    src_out: Seconds,
) -> Result<Filmstrip, FilmstripError> {
    if !asset.probe.has_video() {
        return Err(FilmstripError::NoVideo);
    }
    let span = src_out - src_in;
    if span <= Seconds::from_integer(0) {
        return Err(FilmstripError::BadSpan);
    }
    let dir = cache_dir(project_path);
    std::fs::create_dir_all(&dir)?;
    let hash = filmstrip_hash(asset, &src_in, &src_out);
    let path = dir.join(format!("{hash:016x}.rgb"));
    if let Ok(rgb) = std::fs::read(&path)
        && let Ok(strip) = Filmstrip::new(rgb)
    {
        return Ok(strip);
    }

    // One frame per column, each squeezed to one pixel wide and four tall;
    // `tile` lays those columns out as a single sprite. `-ss`/`-t` are input
    // options so a long asset is only read across the clip's source span.
    let rate = f64::from(FILMSTRIP_COLS) / secs_f64(&span);
    let filter = format!("fps={rate:.6},scale=1:{FILMSTRIP_ROWS},tile={FILMSTRIP_COLS}x1");
    let out = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-ss",
            &format!("{:.6}", secs_f64(&src_in)),
            "-t",
            &format!("{:.6}", secs_f64(&span)),
            "-i",
        ])
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
        return Err(FilmstripError::Ffmpeg(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    let strip = Filmstrip::new(out.stdout)?;
    let tmp = tempfile::Builder::new()
        .prefix(&format!("{hash:016x}.tmp-"))
        .suffix(".rgb")
        .tempfile_in(&dir)?;
    std::fs::write(tmp.path(), &strip.rgb)?;
    tmp.persist(&path).map_err(|e| e.error)?;
    Ok(strip)
}

fn filmstrip_hash(asset: &Asset, src_in: &Seconds, src_out: &Seconds) -> u64 {
    let mut h = DefaultHasher::new();
    FILMSTRIP_VERSION.hash(&mut h);
    FILMSTRIP_COLS.hash(&mut h);
    FILMSTRIP_ROWS.hash(&mut h);
    asset.path.hash(&mut h);
    (*src_in.numer(), *src_in.denom()).hash(&mut h);
    (*src_out.numer(), *src_out.denom()).hash(&mut h);
    if let Ok(meta) = std::fs::metadata(&asset.path) {
        meta.len().hash(&mut h);
        if let Ok(modified) = meta.modified()
            && let Ok(since) = modified.duration_since(SystemTime::UNIX_EPOCH)
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

    fn generated_asset(dir: &Path) -> Asset {
        let media = dir.join("strip.mp4");
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
        Asset {
            id: 1,
            path: media,
            probe,
        }
    }

    #[test]
    fn filmstrips_are_extracted_cached_and_span_keyed() {
        let dir = tempfile::tempdir().unwrap();
        let asset = generated_asset(dir.path());
        let project_path = dir.path().join("nile.json");
        let first = load(
            &project_path,
            &asset,
            Seconds::from_integer(0),
            Seconds::from_integer(1),
        )
        .unwrap();
        assert_eq!(first.cols, FILMSTRIP_COLS);
        assert_eq!(first.rows, FILMSTRIP_ROWS);
        assert!(first.rgb.iter().any(|b| *b > 0), "not all black");
        let cached = cache_dir(&project_path).read_dir().unwrap().count();
        let second = load(
            &project_path,
            &asset,
            Seconds::from_integer(0),
            Seconds::from_integer(1),
        )
        .unwrap();
        assert_eq!(second, first);
        assert_eq!(cache_dir(&project_path).read_dir().unwrap().count(), cached);

        let half = load(
            &project_path,
            &asset,
            Seconds::from_integer(0),
            Seconds::new(1, 2),
        )
        .unwrap();
        assert_ne!(half, first, "a different source span is a new strip");
        assert_eq!(
            cache_dir(&project_path).read_dir().unwrap().count(),
            cached + 1
        );
    }

    #[test]
    fn a_stale_filmstrip_is_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let project_path = dir.path().join("nile.json");
        let cache = cache_dir(&project_path);
        std::fs::create_dir_all(&cache).unwrap();
        let path = cache.join("0123456789abcdef.rgb");
        std::fs::write(
            &path,
            vec![0u8; (FILMSTRIP_COLS * FILMSTRIP_ROWS * 3) as usize],
        )
        .unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_times(
            std::fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(3600)),
        )
        .unwrap();
        let report = prune(&project_path, Duration::ZERO, false).unwrap();
        assert_eq!(report.removed, 1, "{report:?}");
        assert!(!path.exists(), "the stale strip is gone");
    }

    #[test]
    fn audio_only_assets_have_no_filmstrip() {
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
            load(
                &dir.path().join("nile.json"),
                &asset,
                Seconds::from_integer(0),
                Seconds::from_integer(1)
            ),
            Err(FilmstripError::NoVideo)
        ));
    }
}
