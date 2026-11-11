//! Cached audio peaks for the timeline's waveform row.
//!
//! The file is decoded once at a low mono rate; the peak bins are keyed by
//! source path + mtime + format version, so replacing media invalidates them.
//! Rendering uses the cached array and never spawns ffmpeg.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use nle_core::Asset;

#[derive(Debug, Error)]
pub enum WaveError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("ffmpeg could not decode audio: {0}")]
    Ffmpeg(String),
    #[error("asset has no audio stream")]
    NoAudio,
}

/// One cached waveform: normalized peaks over the source duration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Waveform {
    pub peaks: Vec<f32>,
    pub duration: f64,
}

const WAVE_VERSION: u32 = 2;
const WAVE_BINS: usize = 2048;
const SAMPLE_RATE: u32 = 8_000;

/// Cached peak files for one project.
pub fn cache_dir(project_path: &Path) -> PathBuf {
    nle_core::state::cache_dir(project_path).join("wave")
}

/// Load this asset's waveform, decoding it if the cache is cold.
pub fn load(project_path: &Path, asset: &Asset) -> Result<Waveform, WaveError> {
    if !asset.probe.has_audio() {
        return Err(WaveError::NoAudio);
    }
    let dir = cache_dir(project_path);
    std::fs::create_dir_all(&dir)?;
    let hash = asset_hash(asset);
    let path = dir.join(format!("{hash:016x}.wave"));
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(cached) = serde_json::from_str::<Waveform>(&text)
        && cached.peaks.len() == WAVE_BINS
    {
        return Ok(cached);
    }

    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(&asset.path)
        .args([
            "-map",
            "0:a:0",
            "-vn",
            "-ac",
            "1",
            "-ar",
            &SAMPLE_RATE.to_string(),
            "-f",
            "s16le",
            "-",
        ])
        .output()?;
    if !out.status.success() {
        return Err(WaveError::Ffmpeg(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    let wave = peaks_from_pcm(&out.stdout, WAVE_BINS, SAMPLE_RATE, asset);
    let tmp = tempfile::Builder::new()
        .prefix(&format!("{hash:016x}.tmp-"))
        .suffix(".wave")
        .tempfile_in(&dir)?;
    serde_json::to_writer(tmp.as_file(), &wave)?;
    tmp.persist(&path).map_err(|e| e.error)?;
    Ok(wave)
}

fn peaks_from_pcm(pcm: &[u8], bins: usize, rate: u32, asset: &Asset) -> Waveform {
    let samples = pcm.as_chunks::<2>().0.len();
    let mut peaks = vec![0.0f32; bins];
    if samples == 0 || bins == 0 {
        return Waveform {
            peaks,
            duration: asset
                .duration()
                .map(|d| *d.numer() as f64 / *d.denom() as f64)
                .unwrap_or(0.0),
        };
    }
    for (i, pair) in pcm.as_chunks::<2>().0.iter().enumerate() {
        let v = i16::from_le_bytes([pair[0], pair[1]]) as i32;
        // Distribute all samples across all bins; integer ceil-division
        // leaves the tail bins empty for non-multiples (visible as a gap at
        // the end of short clips).
        let bin = i.saturating_mul(bins) / samples;
        let amp = v.unsigned_abs() as f32 / 32768.0;
        if amp > peaks[bin] {
            peaks[bin] = amp;
        }
    }
    let max = peaks.iter().copied().fold(0.0f32, f32::max);
    if max > 0.0 {
        for p in &mut peaks {
            *p /= max;
        }
    }
    Waveform {
        peaks,
        // The peaks cover the decoded audio stream, so the drawing must map
        // source time by the PCM's own length — the container duration can
        // differ (audio embedded in a longer video), which stretched the
        // waveform by that factor and drifted it near the clip end.
        duration: samples as f64 / f64::from(rate),
    }
}

fn asset_hash(asset: &Asset) -> u64 {
    let mut h = DefaultHasher::new();
    WAVE_VERSION.hash(&mut h);
    WAVE_BINS.hash(&mut h);
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

/// Prune this project's cached waveforms.
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

    fn asset() -> Asset {
        Asset {
            id: 1,
            path: "/media/a.wav".into(),
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
        }
    }

    #[test]
    fn pcm_peaks_are_binned_and_normalized() {
        let pcm: Vec<u8> = [0i16, 0, 1000, 0, -2000, 0, 500, 0]
            .into_iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let w = peaks_from_pcm(&pcm, 2, 8000, &asset());
        assert_eq!(w.peaks.len(), 2);
        // The peaks cover the decoded stream, not the container: a video whose
        // audio stream is shorter than its container must not stretch the
        // waveform. 8 samples @ 8 kHz = 1 ms, despite the 1 s container.
        assert!((w.duration - 8.0 / 8000.0).abs() < 1e-9, "{}", w.duration);
        assert!((w.peaks[0] - 0.5).abs() < 1e-6);
        assert!((w.peaks[1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn decodes_and_caches_real_audio() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("tone.wav");
        let out = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.2",
                "-c:a",
                "pcm_s16le",
            ])
            .arg(&media)
            .status()
            .unwrap();
        assert!(out.success(), "generate tone");
        let probe = nle_ffmpeg::probe(&media).unwrap();
        let asset = Asset {
            id: 1,
            path: media,
            probe,
        };
        let project_path = dir.path().join("nile.json");
        let first = load(&project_path, &asset).unwrap();
        assert_eq!(first.peaks.len(), WAVE_BINS);
        assert!(first.peaks.iter().any(|p| *p > 0.5));
        let cached = cache_dir(&project_path).read_dir().unwrap().count();
        let second = load(&project_path, &asset).unwrap();
        assert_eq!(second.peaks, first.peaks);
        assert_eq!(cache_dir(&project_path).read_dir().unwrap().count(), cached);
    }

    #[test]
    fn the_tail_samples_fill_the_last_bin() {
        let pcm: Vec<u8> = [100i16, 200, 300, 400]
            .into_iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let w = peaks_from_pcm(&pcm, 3, 8000, &asset());
        assert!(
            w.peaks[2] > 0.0,
            "the last bin must not be empty: {:?}",
            w.peaks
        );
    }

    #[test]
    fn silence_stays_flat() {
        let pcm = vec![0u8; 32];
        let w = peaks_from_pcm(&pcm, 4, 8000, &asset());
        assert!(w.peaks.iter().all(|p| *p == 0.0));
    }
}
