//! Packed frame/audio sampling with a content-addressed cache.
//!
//! Analysis sidecars want *arrays*, not a stream of JPEGs: one RGB24 buffer
//! per sampled frame, one interleaved s16le buffer per audio range. This module
//! turns a [`FrameSpec`] or [`AudioSpec`] into exactly that, sampling through
//! ffmpeg and caching the packed result under a caller-chosen root.
//!
//! The cache is content-addressed the same way proxies are (`proxy.rs`):
//!
//! ```text
//! key = H(version, kind, spec params, canonical source, source size + mtime)
//! ```
//!
//! so editing the spec or replacing the source file misses; an unchanged source
//! hits without touching ffmpeg. Entries are `<key:016x>.raw` (the packed
//! bytes) plus `<key:016x>.json` (the validation metadata). The raw file is
//! written first and the metadata last, so a crash between the two leaves an
//! entry that is ignored and rewritten, never a half-file that is served.
//!
//! When the source has been removed, the key can no longer be recomputed (its
//! mtime/size are part of it). The cache then falls back to scanning entry
//! metadata for the newest generation matching the source + spec and serves it
//! — which is what makes an offline analysis pass possible. Metadata spells
//! the source with `to_string_lossy()`, so a non-UTF-8 path can only be found
//! offline through the lossy form; the key itself hashes the exact `Path`.
//!
//! Sampling buffers the whole result in memory (`width × height × 3` bytes per
//! frame; `samples × channels × 2` bytes of audio). Callers pick a range and a
//! geometry small enough for the analysis they are doing.

use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use nle_core::Seconds;
use nle_core::time::format_seconds;
use thiserror::Error;

/// Bump when the *meaning* of a sample changes (a different pixel format, a
/// different resampler, a different truncation rule). Old entries are then
/// unreachable instead of being served as if they were the new thing.
const SAMPLE_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum SampleError {
    #[error("no such file: {0}")]
    MissingFile(String),
    #[error("invalid sampling spec: {0}")]
    InvalidSpec(String),
    #[error("could not run ffmpeg: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("ffmpeg failed ({status}): {message}")]
    Ffmpeg { status: ExitStatus, message: String },
    #[error("ffmpeg returned {bytes} bytes, not a whole number of {unit}-byte samples")]
    Ragged { bytes: usize, unit: usize },
    #[error("ffmpeg produced no {what} for {path} — is the range past the end of the media?")]
    Empty { what: &'static str, path: String },
    #[error("could not write the sample cache at {path}: {error}")]
    CacheIo {
        path: PathBuf,
        #[source]
        error: std::io::Error,
    },
    #[error("could not encode sample cache metadata: {0}")]
    CacheEncode(String),
}

/// A frame-sampling request: the `[start, start + duration)` source range,
/// sampled at `fps` and scaled to `width` × `height` RGB24.
///
/// `duration` is an exact rational and `start` is measured from the source's
/// zero. The frame count is `floor(duration × fps)`; a range past the end of
/// the media yields fewer frames, never invented ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSpec {
    pub source: PathBuf,
    pub start: Seconds,
    pub duration: Seconds,
    pub fps: Seconds,
    pub width: u32,
    pub height: u32,
}

impl FrameSpec {
    /// How many frames `[start, start + duration)` contains at `fps`.
    ///
    /// Exact rational floor — a float product would round at 29.97 fps.
    pub fn frame_count(&self) -> u64 {
        product_floor(self.duration, self.fps)
    }

    fn validate(&self) -> Result<(), SampleError> {
        if self.start < Seconds::from_integer(0) {
            return Err(SampleError::InvalidSpec(
                "start must not be negative".into(),
            ));
        }
        if self.duration <= Seconds::from_integer(0) {
            return Err(SampleError::InvalidSpec("duration must be positive".into()));
        }
        if self.fps <= Seconds::from_integer(0) {
            return Err(SampleError::InvalidSpec("fps must be positive".into()));
        }
        if self.width == 0 || self.height == 0 {
            return Err(SampleError::InvalidSpec(
                "width and height must be non-zero".into(),
            ));
        }
        if self.frame_count() == 0 {
            return Err(SampleError::InvalidSpec(
                "the range is shorter than one frame at this fps".into(),
            ));
        }
        Ok(())
    }
}

/// The packed frames: `frames` × `height` rows of `width` RGB triples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSample {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

impl FrameSample {
    fn new(width: u32, height: u32, rgb: Vec<u8>) -> Self {
        Self { width, height, rgb }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Number of frames actually packed (≤ the spec's count when the range
    /// runs past the media's end).
    pub fn frame_count(&self) -> usize {
        self.rgb.len() / self.stride()
    }

    fn stride(&self) -> usize {
        self.width as usize * self.height as usize * 3
    }

    /// The whole packed buffer, row-major RGB24.
    pub fn rgb(&self) -> &[u8] {
        &self.rgb
    }

    /// One frame's RGB24 bytes; `None` past the last frame.
    pub fn frame(&self, index: usize) -> Option<&[u8]> {
        let stride = self.stride();
        let start = index.checked_mul(stride)?;
        let end = start.checked_add(stride)?;
        self.rgb.get(start..end)
    }

    /// One pixel's RGB triple; `None` outside the sample (including an index
    /// too large to address).
    pub fn pixel(&self, frame: usize, x: u32, y: u32) -> Option<[u8; 3]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let row = (y as usize)
            .checked_mul(self.width as usize)?
            .checked_add(x as usize)?
            .checked_mul(3)?;
        let offset = frame.checked_mul(self.stride())?.checked_add(row)?;
        let end = offset.checked_add(3)?;
        let px = self.rgb.get(offset..end)?;
        Some([px[0], px[1], px[2]])
    }
}

/// An audio-sampling request: the `[start, start + duration)` source range,
/// resampled to `sample_rate` and `channels`, packed as interleaved s16le.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSpec {
    pub source: PathBuf,
    pub start: Seconds,
    pub duration: Seconds,
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioSpec {
    /// Samples per channel in the requested range.
    pub fn sample_count(&self) -> u64 {
        product_floor(
            self.duration,
            Seconds::from_integer(self.sample_rate as i64),
        )
    }

    fn validate(&self) -> Result<(), SampleError> {
        if self.start < Seconds::from_integer(0) {
            return Err(SampleError::InvalidSpec(
                "start must not be negative".into(),
            ));
        }
        if self.duration <= Seconds::from_integer(0) {
            return Err(SampleError::InvalidSpec("duration must be positive".into()));
        }
        if self.sample_rate == 0 {
            return Err(SampleError::InvalidSpec(
                "sample rate must be non-zero".into(),
            ));
        }
        if self.channels == 0 {
            return Err(SampleError::InvalidSpec(
                "channel count must be non-zero".into(),
            ));
        }
        if self.sample_count() == 0 {
            return Err(SampleError::InvalidSpec(
                "the range is shorter than one audio sample".into(),
            ));
        }
        Ok(())
    }

    fn bytes_per_sample(&self) -> usize {
        self.channels as usize * 2
    }
}

/// Interleaved s16le PCM for one audio range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSample {
    sample_rate: u32,
    channels: u16,
    pcm: Vec<u8>,
}

impl AudioSample {
    fn new(sample_rate: u32, channels: u16, pcm: Vec<u8>) -> Self {
        Self {
            sample_rate,
            channels,
            pcm,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Samples per channel actually packed.
    pub fn samples(&self) -> usize {
        self.pcm.len() / (self.channels as usize * 2)
    }

    /// Interleaved little-endian i16 samples.
    pub fn pcm(&self) -> &[u8] {
        &self.pcm
    }
}

/// A content-addressed store for packed samples. The caller picks the root;
/// `nile-cli` will point it at `.nile/<stem>/cache/analysis` (disposable
/// recomputable data), not `.nile/<stem>/analysis` (analysis state), once the
/// sidecar contract lands.
#[derive(Debug, Clone)]
pub struct SampleCache {
    root: PathBuf,
}

impl SampleCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Sample `spec`, or load the cached result when the source and spec are
    /// unchanged. A missing source can still be answered from the cache.
    pub fn frames(&self, spec: &FrameSpec) -> Result<FrameSample, SampleError> {
        spec.validate()?;
        let params = frame_params(spec);
        let key = source_key(&spec.source);
        if let Some(id) = source_identity(&spec.source) {
            let entry = entry_key(params, &key.component, &id);
            if let Some(sample) = self.load_frames(entry, params, &key, spec) {
                return Ok(sample);
            }
            let sample = sample_frames(spec)?;
            self.store_frames(entry, params, &key, &id, spec, &sample)?;
            Ok(sample)
        } else if let Some(sample) = self.offline_frames(params, &key, spec) {
            Ok(sample)
        } else {
            Err(SampleError::MissingFile(spec.source.display().to_string()))
        }
    }

    /// Sample `spec`, or load the cached result. Same invalidation rules as
    /// [`SampleCache::frames`].
    pub fn audio(&self, spec: &AudioSpec) -> Result<AudioSample, SampleError> {
        spec.validate()?;
        let params = audio_params(spec);
        let key = source_key(&spec.source);
        if let Some(id) = source_identity(&spec.source) {
            let entry = entry_key(params, &key.component, &id);
            if let Some(sample) = self.load_audio(entry, params, &key, spec) {
                return Ok(sample);
            }
            let sample = sample_audio(spec)?;
            self.store_audio(entry, params, &key, &id, spec, &sample)?;
            Ok(sample)
        } else if let Some(sample) = self.offline_audio(params, &key, spec) {
            Ok(sample)
        } else {
            Err(SampleError::MissingFile(spec.source.display().to_string()))
        }
    }

    fn load_frames(
        &self,
        entry: u64,
        params: u64,
        key: &SourceKey,
        spec: &FrameSpec,
    ) -> Option<FrameSample> {
        let (meta, raw) = self.load_entry(entry, params, key)?;
        let EntryData::Frames {
            width,
            height,
            frames,
        } = meta.data
        else {
            return None;
        };
        if width != spec.width || height != spec.height || frames == 0 {
            return None;
        }
        // A corrupt/huge metadata count must be a miss, not an overflow.
        let stride = (width as u64).checked_mul(height as u64)?.checked_mul(3)?;
        let expected = frames.checked_mul(stride)?;
        if expected != raw.len() as u64 || meta.bytes != expected {
            return None;
        }
        Some(FrameSample::new(width, height, raw))
    }

    fn load_audio(
        &self,
        entry: u64,
        params: u64,
        key: &SourceKey,
        spec: &AudioSpec,
    ) -> Option<AudioSample> {
        let (meta, raw) = self.load_entry(entry, params, key)?;
        let EntryData::Audio {
            sample_rate,
            channels,
            samples,
        } = meta.data
        else {
            return None;
        };
        if sample_rate != spec.sample_rate || channels != spec.channels || samples == 0 {
            return None;
        }
        let unit = (channels as u64).checked_mul(2)?;
        let expected = samples.checked_mul(unit)?;
        if expected != raw.len() as u64 || meta.bytes != expected {
            return None;
        }
        Some(AudioSample::new(sample_rate, channels, raw))
    }

    /// Read one entry, validating that its metadata still describes the key it
    /// is filed under and that the packed file has the length the metadata
    /// claims. Any mismatch is a miss: re-sampling is cheaper than debugging a
    /// truncated buffer.
    fn load_entry(&self, entry: u64, params: u64, key: &SourceKey) -> Option<(EntryMeta, Vec<u8>)> {
        let (raw_path, meta_path) = entry_paths(&self.root, entry);
        let bytes = std::fs::read(&meta_path).ok()?;
        let meta: EntryMeta = serde_json::from_slice(&bytes).ok()?;
        if meta.version != SAMPLE_VERSION
            || meta.params != hex(params)
            || meta.key != hex(entry)
            || !key.matches(&meta)
            || meta.bytes != std::fs::metadata(&raw_path).ok()?.len()
        {
            return None;
        }
        let raw = std::fs::read(&raw_path).ok()?;
        Some((meta, raw))
    }

    /// The cached generations for a source that no longer exists, newest
    /// first. A corrupt newest entry falls through to the one before it.
    fn offline_frames(
        &self,
        params: u64,
        key: &SourceKey,
        spec: &FrameSpec,
    ) -> Option<FrameSample> {
        self.find_offline(params, key, |data| matches!(data, EntryData::Frames { .. }))
            .into_iter()
            .find_map(|entry| self.load_frames(entry, params, key, spec))
    }

    fn offline_audio(&self, params: u64, key: &SourceKey, spec: &AudioSpec) -> Option<AudioSample> {
        self.find_offline(params, key, |data| matches!(data, EntryData::Audio { .. }))
            .into_iter()
            .find_map(|entry| self.load_audio(entry, params, key, spec))
    }

    /// Scan entry metadata for generations matching the spec and the source's
    /// spelling, newest first. This is only used when the source is gone,
    /// because its mtime/size are part of the normal key.
    fn find_offline(
        &self,
        params: u64,
        key: &SourceKey,
        kind: impl Fn(&EntryData) -> bool,
    ) -> Vec<u64> {
        let wanted = hex(params);
        let mut found: Vec<(u64, u64, u32)> = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        for dir_entry in entries.flatten() {
            let name = dir_entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            if stem.len() != 16 {
                continue;
            }
            let Ok(entry) = u64::from_str_radix(stem, 16) else {
                continue;
            };
            let Ok(bytes) = std::fs::read(dir_entry.path()) else {
                continue;
            };
            let Ok(meta) = serde_json::from_slice::<EntryMeta>(&bytes) else {
                continue;
            };
            if meta.version != SAMPLE_VERSION
                || meta.params != wanted
                || meta.key != stem
                || !key.matches(&meta)
                || !kind(&meta.data)
            {
                continue;
            }
            found.push((entry, meta.source_mtime_secs, meta.source_mtime_nanos));
        }
        // Newest source mtime first. Entries with the same mtime have no
        // defined winner (the hash order is arbitrary): an offline lookup
        // cannot reconstruct which write came last when the source is gone.
        found.sort_by_key(|(entry, secs, nanos)| std::cmp::Reverse((*secs, *nanos, *entry)));
        found.into_iter().map(|(entry, _, _)| entry).collect()
    }

    fn store_frames(
        &self,
        entry: u64,
        params: u64,
        key: &SourceKey,
        id: &SourceId,
        spec: &FrameSpec,
        sample: &FrameSample,
    ) -> Result<(), SampleError> {
        let meta = EntryMeta::new(
            params,
            key,
            id,
            entry,
            sample.rgb().len(),
            EntryData::Frames {
                width: spec.width,
                height: spec.height,
                frames: sample.frame_count() as u64,
            },
        );
        self.store_entry(entry, &meta, sample.rgb())
    }

    fn store_audio(
        &self,
        entry: u64,
        params: u64,
        key: &SourceKey,
        id: &SourceId,
        spec: &AudioSpec,
        sample: &AudioSample,
    ) -> Result<(), SampleError> {
        let meta = EntryMeta::new(
            params,
            key,
            id,
            entry,
            sample.pcm().len(),
            EntryData::Audio {
                sample_rate: spec.sample_rate,
                channels: spec.channels,
                samples: sample.samples() as u64,
            },
        );
        self.store_entry(entry, &meta, sample.pcm())
    }

    /// Raw first, metadata last: metadata is the commit marker, so a crash
    /// between the writes leaves an entry that simply misses.
    fn store_entry(&self, entry: u64, meta: &EntryMeta, raw: &[u8]) -> Result<(), SampleError> {
        let stem = hex(entry);
        write_atomic(&self.root, &format!("{stem}.raw"), raw)?;
        let json = serde_json::to_vec(meta).map_err(|e| SampleError::CacheEncode(e.to_string()))?;
        write_atomic(&self.root, &format!("{stem}.json"), &json)
    }
}

/// The cache key components of a source path. `component` is the canonical
/// spelling when the file exists (stable across symlinks) and `arg` is the
/// lexical absolute path as handed in; an offline lookup matches either.
///
/// The key hashes these `Path`s directly (`Path: Hash`), so a non-UTF-8 name
/// keys exactly. Metadata must be JSON, so it stores the `to_string_lossy()`
/// spelling; only the offline scan loses information for such paths.
struct SourceKey {
    component: PathBuf,
    arg: PathBuf,
}

impl SourceKey {
    fn matches(&self, meta: &EntryMeta) -> bool {
        meta.source == self.component.to_string_lossy().as_ref()
            || meta.source_arg == self.arg.to_string_lossy().as_ref()
    }
}

fn source_key(path: &Path) -> SourceKey {
    let arg = absolute_lexical(path);
    let component = std::fs::canonicalize(path).unwrap_or_else(|_| arg.clone());
    SourceKey { component, arg }
}

fn absolute_lexical(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|dir| dir.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

#[derive(Debug, Clone, Copy)]
struct SourceId {
    size: u64,
    mtime_secs: u64,
    mtime_nanos: u32,
}

fn source_identity(path: &Path) -> Option<SourceId> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let (mtime_secs, mtime_nanos) = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| (d.as_secs(), d.subsec_nanos()))
        .unwrap_or((0, 0));
    Some(SourceId {
        size: meta.len(),
        mtime_secs,
        mtime_nanos,
    })
}

/// The full cache key: spec + version + canonical source + source identity.
fn entry_key(params: u64, source: &Path, id: &SourceId) -> u64 {
    let mut hasher = DefaultHasher::new();
    params.hash(&mut hasher);
    source.hash(&mut hasher);
    id.size.hash(&mut hasher);
    id.mtime_secs.hash(&mut hasher);
    id.mtime_nanos.hash(&mut hasher);
    hasher.finish()
}

fn frame_params(spec: &FrameSpec) -> u64 {
    let mut hasher = DefaultHasher::new();
    SAMPLE_VERSION.hash(&mut hasher);
    "frames".hash(&mut hasher);
    rational(&mut hasher, spec.start);
    rational(&mut hasher, spec.duration);
    rational(&mut hasher, spec.fps);
    spec.width.hash(&mut hasher);
    spec.height.hash(&mut hasher);
    hasher.finish()
}

fn audio_params(spec: &AudioSpec) -> u64 {
    let mut hasher = DefaultHasher::new();
    SAMPLE_VERSION.hash(&mut hasher);
    "audio".hash(&mut hasher);
    rational(&mut hasher, spec.start);
    rational(&mut hasher, spec.duration);
    spec.sample_rate.hash(&mut hasher);
    spec.channels.hash(&mut hasher);
    hasher.finish()
}

fn rational(hasher: &mut DefaultHasher, value: Seconds) {
    value.numer().hash(hasher);
    value.denom().hash(hasher);
}

/// `floor(a × b)` in exact integers. A `Ratio` multiply would already be
/// exact but can overflow `i64` for an absurd spec; `i128` holds the product
/// of two `i64` numerators, so a bad spec becomes a huge (clamped) count
/// rather than a panic in debug builds.
fn product_floor(a: Seconds, b: Seconds) -> u64 {
    let numerator = *a.numer() as i128 * *b.numer() as i128;
    let denominator = *a.denom() as i128 * *b.denom() as i128;
    if numerator <= 0 || denominator <= 0 {
        return 0;
    }
    (numerator / denominator).min(u64::MAX as i128) as u64
}

/// The stable, file-name-safe spelling of a key: 16 lowercase hex digits.
fn hex(key: u64) -> String {
    format!("{key:016x}")
}

fn entry_paths(root: &Path, entry: u64) -> (PathBuf, PathBuf) {
    let stem = hex(entry);
    (
        root.join(format!("{stem}.raw")),
        root.join(format!("{stem}.json")),
    )
}

/// A process-local uniquifier for temp names. The pid separates processes;
/// the sequence separates threads and successive writes in this one.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// `<key>.tmp-<pid>-<seq>.<ext>` — the spelling `proxy::is_entry_name` knows,
/// so a crashed write is swept with the other cache entries.
fn tmp_name(name: &str) -> String {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, "tmp"));
    format!("{stem}.tmp-{}-{seq}.{ext}", std::process::id())
}

/// Write `name` under `dir` through a sibling temp file, fsync it, rename it
/// into place, then fsync the directory (best effort on Unix).
///
/// The rename is atomic, so a reader sees the old entry or the complete new
/// one — never a short file under the final name. A crash before the rename
/// leaves a `<key>.tmp-…` file that prune can identify and remove; the
/// metadata file is written after the raw one and acts as the commit marker.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), SampleError> {
    std::fs::create_dir_all(dir).map_err(|e| SampleError::CacheIo {
        path: dir.to_path_buf(),
        error: e,
    })?;
    let target = dir.join(name);
    let mut attempts = 0;
    let (tmp, mut file) = loop {
        let path = dir.join(tmp_name(name));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => break (path, file),
            // A stale temp from an earlier process with the same pid: next seq.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < 16 => {
                attempts += 1;
            }
            Err(error) => {
                return Err(SampleError::CacheIo { path, error });
            }
        }
    };
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(SampleError::CacheIo { path: tmp, error });
    }
    drop(file);
    if let Err(error) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(SampleError::CacheIo {
            path: target,
            error,
        });
    }
    // The rename is only durable once the directory is synced; on platforms
    // where a directory can't be opened this is skipped (the file is synced).
    #[cfg(unix)]
    if let Ok(dir_file) = std::fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

/// The metadata sidecar. It carries everything `load_entry` needs to reject a
/// stale or truncated entry, plus the spec fingerprint (`params`) and both
/// source spellings for the offline lookup.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct EntryMeta {
    version: u32,
    params: String,
    source: String,
    source_arg: String,
    source_size: u64,
    source_mtime_secs: u64,
    source_mtime_nanos: u32,
    key: String,
    bytes: u64,
    #[serde(flatten)]
    data: EntryData,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum EntryData {
    Frames {
        width: u32,
        height: u32,
        frames: u64,
    },
    Audio {
        sample_rate: u32,
        channels: u16,
        samples: u64,
    },
}

impl EntryMeta {
    fn new(
        params: u64,
        key: &SourceKey,
        id: &SourceId,
        entry: u64,
        bytes: usize,
        data: EntryData,
    ) -> Self {
        Self {
            version: SAMPLE_VERSION,
            params: hex(params),
            source: key.component.to_string_lossy().into_owned(),
            source_arg: key.arg.to_string_lossy().into_owned(),
            source_size: id.size,
            source_mtime_secs: id.mtime_secs,
            source_mtime_nanos: id.mtime_nanos,
            key: hex(entry),
            bytes: bytes as u64,
            data,
        }
    }
}

/// ffmpeg's output argument is positional (`-` for the pipe), so it follows
/// `--`: a source path can never be mistaken for an option.
fn run_ffmpeg(args: &[OsString]) -> Result<Vec<u8>, SampleError> {
    let out = Command::new("ffmpeg")
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        let lines: Vec<String> = String::from_utf8_lossy(&out.stderr)
            .lines()
            .map(str::to_string)
            .collect();
        return Err(SampleError::Ffmpeg {
            status: out.status,
            message: crate::trim::error_lines(&lines),
        });
    }
    Ok(out.stdout)
}

fn sample_frames(spec: &FrameSpec) -> Result<FrameSample, SampleError> {
    let stride = (spec.width as usize)
        .checked_mul(spec.height as usize)
        .and_then(|n| n.checked_mul(3))
        .filter(|n| *n > 0)
        .ok_or_else(|| SampleError::InvalidSpec("frame size overflows".into()))?;
    let mut raw = run_ffmpeg(&frame_args(spec))?;
    if raw.len() % stride != 0 {
        return Err(SampleError::Ragged {
            bytes: raw.len(),
            unit: stride,
        });
    }
    if raw.is_empty() {
        return Err(SampleError::Empty {
            what: "frames",
            path: spec.source.display().to_string(),
        });
    }
    // `-frames:v` caps the count, but never trust the process for the shape:
    // a filter rounding up must not hand the caller more than it asked for.
    let wanted = spec.frame_count() as usize;
    let frames = (raw.len() / stride).min(wanted);
    // `truncate` is the no-copy path when the count is already exact.
    raw.truncate(frames * stride);
    Ok(FrameSample::new(spec.width, spec.height, raw))
}

fn sample_audio(spec: &AudioSpec) -> Result<AudioSample, SampleError> {
    let unit = spec.bytes_per_sample();
    let mut raw = run_ffmpeg(&audio_args(spec))?;
    if raw.len() % unit != 0 {
        return Err(SampleError::Ragged {
            bytes: raw.len(),
            unit,
        });
    }
    if raw.is_empty() {
        return Err(SampleError::Empty {
            what: "audio samples",
            path: spec.source.display().to_string(),
        });
    }
    let wanted = spec.sample_count() as usize;
    let samples = (raw.len() / unit).min(wanted);
    raw.truncate(samples * unit);
    Ok(AudioSample::new(spec.sample_rate, spec.channels, raw))
}

/// ffmpeg's rational spelling: `30` for a whole rate, `30000/1001` otherwise.
fn rate_arg(fps: &Seconds) -> String {
    if *fps.denom() == 1 {
        fps.numer().to_string()
    } else {
        format!("{}/{}", fps.numer(), fps.denom())
    }
}

/// The exact argv for a frame sample, so the command can be inspected without
/// running it.
fn frame_args(spec: &FrameSpec) -> Vec<OsString> {
    let filter = format!(
        "fps={},scale={}:{}:flags=area,setsar=1",
        rate_arg(&spec.fps),
        spec.width,
        spec.height
    );
    let mut args = common_args(spec.source.as_path(), spec.start, spec.duration);
    args.extend([
        "-map".into(),
        "0:v:0".into(),
        "-an".into(),
        "-sn".into(),
        "-dn".into(),
        "-vf".into(),
        filter.into(),
        "-frames:v".into(),
        spec.frame_count().to_string().into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "rgb24".into(),
        "--".into(),
        "-".into(),
    ]);
    args
}

/// The exact argv for an audio sample. `atrim=end_sample` makes the sample
/// count exact even when the source's rate differs from the requested one.
fn audio_args(spec: &AudioSpec) -> Vec<OsString> {
    let mut args = common_args(spec.source.as_path(), spec.start, spec.duration);
    args.extend([
        "-map".into(),
        "0:a:0".into(),
        "-vn".into(),
        "-sn".into(),
        "-dn".into(),
        "-af".into(),
        format!(
            "aresample={},atrim=end_sample={}",
            spec.sample_rate,
            spec.sample_count()
        )
        .into(),
        "-ac".into(),
        spec.channels.to_string().into(),
        "-ar".into(),
        spec.sample_rate.to_string().into(),
        "-f".into(),
        "s16le".into(),
        "-acodec".into(),
        "pcm_s16le".into(),
        "--".into(),
        "-".into(),
    ]);
    args
}

fn common_args(source: &Path, start: Seconds, duration: Seconds) -> Vec<OsString> {
    vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-v".into(),
        "error".into(),
        // Seek before `-i`: fast demuxer seek, then decode forward to the
        // exact frame. Duration after `-i` bounds what is decoded.
        "-ss".into(),
        format_seconds(&start).into(),
        "-i".into(),
        // `-i` consumes its value literally, so a path starting with `-` is
        // safe here; the `--` below guards the positional output.
        source.as_os_str().to_os_string(),
        "-t".into(),
        format_seconds(&duration).into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn ffmpeg(args: &[&str]) {
        let out = Command::new("ffmpeg").args(args).output().unwrap();
        assert!(
            out.status.success(),
            "ffmpeg {args:?}:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A 1-second solid-color clip at 30 fps.
    fn video(dir: &Path, color: &str) -> PathBuf {
        let path = dir.join("source.mp4");
        ffmpeg(&[
            "-nostdin",
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("color=c={color}:s=160x120:r=30:d=1"),
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-an",
            path.to_str().unwrap(),
        ]);
        path
    }

    /// A 1-second 300 Hz mono tone as PCM (exact sample count, unlike AAC).
    fn tone(dir: &Path) -> PathBuf {
        let path = dir.join("tone.wav");
        ffmpeg(&[
            "-nostdin",
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=300:duration=1",
            "-c:a",
            "pcm_s16le",
            path.to_str().unwrap(),
        ]);
        path
    }

    fn frame_spec(source: &Path) -> FrameSpec {
        FrameSpec {
            source: source.to_path_buf(),
            start: Seconds::from_integer(0),
            duration: Seconds::from_integer(1),
            fps: Seconds::from_integer(5),
            width: 32,
            height: 24,
        }
    }

    fn audio_spec(source: &Path) -> AudioSpec {
        AudioSpec {
            source: source.to_path_buf(),
            start: Seconds::from_integer(0),
            duration: Seconds::from_integer(1),
            sample_rate: 48_000,
            channels: 2,
        }
    }

    fn args_line(args: &[OsString]) -> String {
        args.iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Force a visible source change: same second, new mtime, so a filesystem
    /// with coarse timestamps can't hide the edit.
    fn touch_forward(path: &Path) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
    }

    #[test]
    fn frames_are_rgb24_at_the_requested_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let source = video(dir.path(), "red");
        let cache = SampleCache::new(dir.path().join("cache"));
        let spec = frame_spec(&source);
        let sample = cache.frames(&spec).unwrap();

        assert_eq!((sample.width(), sample.height()), (32, 24));
        assert_eq!(sample.frame_count(), 5);
        assert_eq!(sample.rgb().len(), 5 * 32 * 24 * 3);
        assert_eq!(sample.frame(5), None, "no sixth frame");
        assert_eq!(sample.frame(usize::MAX), None, "an absurd index is None");
        assert_eq!(sample.pixel(0, 32, 0), None, "x is out of bounds");
        assert_eq!(sample.pixel(usize::MAX, 0, 0), None, "frame overflow");
        assert_eq!(sample.pixel(0, u32::MAX, u32::MAX), None, "pixel overflow");
        // A solid-color clip decodes to that color (yuv420p round-trip is
        // lossy by a couple of levels, hence the tolerance).
        let [r, g, b] = sample.pixel(0, 16, 12).unwrap();
        assert!(r > 200 && g < 60 && b < 60, "not red: {r},{g},{b}");
    }

    #[test]
    fn a_cached_frame_sample_survives_a_removed_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = video(dir.path(), "red");
        let cache = SampleCache::new(dir.path().join("cache"));
        let spec = frame_spec(&source);
        let first = cache.frames(&spec).unwrap();
        assert_eq!(entries(&cache), 2, "one .raw plus one .json");

        std::fs::remove_file(&source).unwrap();
        let second = cache.frames(&spec).unwrap();
        assert_eq!(second, first);
        assert_eq!(entries(&cache), 2, "the hit does not grow the cache");
    }

    #[test]
    fn a_changed_source_misses_the_frame_cache() {
        let dir = tempfile::tempdir().unwrap();
        let source = video(dir.path(), "red");
        let cache = SampleCache::new(dir.path().join("cache"));
        let spec = frame_spec(&source);
        let red = cache.frames(&spec).unwrap();
        let [r, g, b] = red.pixel(0, 16, 12).unwrap();
        assert!(r > 200 && g < 60 && b < 60, "not red: {r},{g},{b}");

        // Rewrite the same path as blue, with a later mtime.
        video(dir.path(), "blue");
        touch_forward(&source);
        let blue = cache.frames(&spec).unwrap();
        let [r, g, b] = blue.pixel(0, 16, 12).unwrap();
        assert!(b > 200 && r < 60 && g < 60, "not blue: {r},{g},{b}");
        assert_eq!(entries(&cache), 4, "the new generation is a second entry");
    }

    /// Two generations exist after a source edit; with the source gone, the
    /// cache must serve the newest one (the blue render), not the first.
    #[test]
    fn an_offline_lookup_picks_the_newest_generation() {
        let dir = tempfile::tempdir().unwrap();
        let source = video(dir.path(), "red");
        let cache = SampleCache::new(dir.path().join("cache"));
        let spec = frame_spec(&source);
        cache.frames(&spec).unwrap();

        video(dir.path(), "blue");
        touch_forward(&source);
        let blue = cache.frames(&spec).unwrap();
        assert!(blue.pixel(0, 16, 12).unwrap()[2] > 200);

        std::fs::remove_file(&source).unwrap();
        assert_eq!(cache.frames(&spec).unwrap(), blue);
    }

    #[test]
    fn a_spec_change_misses_the_frame_cache() {
        let dir = tempfile::tempdir().unwrap();
        let source = video(dir.path(), "red");
        let cache = SampleCache::new(dir.path().join("cache"));
        let mut spec = frame_spec(&source);
        let first = cache.frames(&spec).unwrap();
        assert_eq!(first.rgb().len(), 5 * 32 * 24 * 3);

        spec.width = 16;
        spec.height = 12;
        let second = cache.frames(&spec).unwrap();
        assert_eq!(second.rgb().len(), 5 * 16 * 12 * 3);
        assert_eq!(entries(&cache), 4, "geometry is part of the key");
    }

    #[test]
    fn audio_is_sampled_and_cached_without_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = tone(dir.path());
        let cache = SampleCache::new(dir.path().join("cache"));
        let spec = audio_spec(&source);
        let first = cache.audio(&spec).unwrap();

        assert_eq!((first.sample_rate(), first.channels()), (48_000, 2));
        assert_eq!(first.samples(), 48_000);
        assert_eq!(first.pcm().len(), 48_000 * 2 * 2);
        assert!(first.pcm().iter().any(|b| *b != 0), "silent tone");

        std::fs::remove_file(&source).unwrap();
        assert_eq!(cache.audio(&spec).unwrap(), first);
        assert_eq!(entries(&cache), 2);
    }

    #[test]
    fn an_audio_spec_change_misses_the_audio_cache() {
        let dir = tempfile::tempdir().unwrap();
        let source = tone(dir.path());
        let cache = SampleCache::new(dir.path().join("cache"));
        let mut spec = audio_spec(&source);
        let first = cache.audio(&spec).unwrap();
        assert_eq!(first.samples(), 48_000);

        spec.sample_rate = 24_000;
        let second = cache.audio(&spec).unwrap();
        assert_eq!(second.sample_rate(), 24_000);
        assert_eq!(second.samples(), 24_000);
        assert_eq!(entries(&cache), 4, "sample rate is part of the key");
    }

    /// A cache entry whose raw file was truncated is a miss, not a short
    /// buffer handed to an analyzer.
    #[test]
    fn a_truncated_cache_entry_is_re_sampled() {
        let dir = tempfile::tempdir().unwrap();
        let source = video(dir.path(), "red");
        let cache = SampleCache::new(dir.path().join("cache"));
        let spec = frame_spec(&source);
        let first = cache.frames(&spec).unwrap();
        let raw = cache_file(&cache, "raw");
        let file = std::fs::File::options().write(true).open(&raw).unwrap();
        file.set_len(first.rgb().len() as u64 / 2).unwrap();
        drop(file);

        let second = cache.frames(&spec).unwrap();
        assert_eq!(second, first, "the truncated entry was re-sampled");
    }

    /// A metadata count that would overflow the length arithmetic is a miss
    /// (and is re-sampled), never a debug panic or a release wrap.
    #[test]
    fn a_huge_metadata_count_is_a_miss_not_an_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let source = video(dir.path(), "red");
        let cache = SampleCache::new(dir.path().join("cache"));
        let spec = frame_spec(&source);
        let first = cache.frames(&spec).unwrap();

        let meta_path = cache_file(&cache, "json");
        let mut meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        meta["frames"] = serde_json::Value::from(u64::MAX);
        std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();

        let again = cache.frames(&spec).unwrap();
        assert_eq!(again, first, "the corrupt count was re-sampled");
    }

    /// With the source gone, a corrupt newest metadata file is skipped and the
    /// older generation is served.
    #[test]
    fn an_offline_lookup_skips_corrupt_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let source = video(dir.path(), "red");
        let cache = SampleCache::new(dir.path().join("cache"));
        let spec = frame_spec(&source);
        let red = cache.frames(&spec).unwrap();

        video(dir.path(), "blue");
        touch_forward(&source);
        let blue = cache.frames(&spec).unwrap();
        assert_ne!(red, blue, "two generations exist");
        let blue_meta = std::fs::read_dir(cache.root())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .find(|p| std::fs::read(p.with_extension("raw")).is_ok_and(|raw| raw == blue.rgb()))
            .expect("the blue generation's metadata");
        std::fs::write(&blue_meta, b"{ not json").unwrap();

        std::fs::remove_file(&source).unwrap();
        assert_eq!(
            cache.frames(&spec).unwrap(),
            red,
            "fell back to the older intact generation"
        );
    }

    #[test]
    fn temp_names_are_prune_candidates() {
        let raw = tmp_name("0123456789abcdef.raw");
        let meta = tmp_name("0123456789abcdef.json");
        assert!(raw.starts_with("0123456789abcdef.tmp-"), "{raw}");
        assert!(raw.ends_with(".raw"), "{raw}");
        assert!(meta.ends_with(".json"), "{meta}");
        assert!(
            raw.contains(&format!(".tmp-{}-", std::process::id())),
            "{raw}"
        );
        assert_ne!(raw, tmp_name("0123456789abcdef.raw"), "seq uniquifies");
    }

    #[test]
    fn a_missing_source_without_a_cache_entry_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SampleCache::new(dir.path().join("cache"));
        let err = cache
            .frames(&frame_spec(&dir.path().join("nope.mp4")))
            .unwrap_err();
        assert!(matches!(err, SampleError::MissingFile(_)), "{err:?}");
    }

    #[test]
    fn a_corrupt_source_reports_the_ffmpeg_failure() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("broken.mp4");
        std::fs::write(&source, b"not media at all").unwrap();
        let cache = SampleCache::new(dir.path().join("cache"));
        let err = cache.frames(&frame_spec(&source)).unwrap_err();
        assert!(matches!(err, SampleError::Ffmpeg { .. }), "{err:?}");
        // The message must name a reason, not just an exit status.
        let SampleError::Ffmpeg { message, .. } = err else {
            unreachable!()
        };
        assert!(!message.is_empty(), "ffmpeg failure without a reason");
    }

    #[test]
    fn invalid_specs_are_refused_before_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let source = video(dir.path(), "red");
        let cache = SampleCache::new(dir.path());
        let mut spec = frame_spec(&source);
        spec.duration = Seconds::from_integer(0);
        assert!(matches!(
            cache.frames(&spec),
            Err(SampleError::InvalidSpec(_))
        ));
        let mut spec = frame_spec(&source);
        spec.fps = Seconds::from_integer(0);
        assert!(matches!(
            cache.frames(&spec),
            Err(SampleError::InvalidSpec(_))
        ));
        let mut spec = frame_spec(&source);
        spec.width = 0;
        assert!(matches!(
            cache.frames(&spec),
            Err(SampleError::InvalidSpec(_))
        ));
        // A range shorter than one frame would ask ffmpeg for 0 frames, which
        // ffmpeg reads as "unlimited" — refuse it instead.
        let mut spec = frame_spec(&source);
        spec.duration = Seconds::new(1, 100);
        assert!(matches!(
            cache.frames(&spec),
            Err(SampleError::InvalidSpec(_))
        ));
    }

    #[test]
    fn frame_args_are_shell_free_and_guard_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a b.mp4");
        let spec = frame_spec(&source);
        let args = frame_args(&spec);
        let line = args_line(&args);
        assert!(line.contains("-ss 0 -i "), "{line}");
        assert!(line.contains("fps=5"), "{line}");
        assert!(line.contains("scale=32:24"), "{line}");
        assert!(line.contains("rawvideo"), "{line}");
        assert!(line.contains("rgb24"), "{line}");
        assert!(
            args.iter().any(|a| a.as_os_str() == source.as_os_str()),
            "a path with a space stays one argument: {args:?}"
        );
        assert_eq!(args[args.len() - 2], "--");
        assert_eq!(args.last().map(|a| a.to_str()), Some(Some("-")));
        assert!(
            args.iter().all(|a| {
                let s = a.to_string_lossy();
                !s.contains(';') && !s.contains("&&")
            }),
            "never a shell string: {args:?}"
        );
    }

    #[test]
    fn a_rational_fps_is_spelled_as_a_ratio() {
        assert_eq!(rate_arg(&Seconds::from_integer(30)), "30");
        assert_eq!(rate_arg(&Seconds::new(30_000, 1001)), "30000/1001");
        let mut spec = frame_spec(Path::new("in.mp4"));
        spec.fps = Seconds::new(30_000, 1001);
        assert!(args_line(&frame_args(&spec)).contains("fps=30000/1001"));
        assert_eq!(spec.frame_count(), 29);
    }

    #[test]
    fn audio_args_name_the_resampler_and_guard_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let spec = audio_spec(&dir.path().join("tone.wav"));
        let args = audio_args(&spec);
        let line = args_line(&args);
        assert!(
            line.contains("aresample=48000,atrim=end_sample=48000"),
            "{line}"
        );
        assert!(line.contains("-ac 2 -ar 48000"), "{line}");
        assert!(line.contains("s16le"), "{line}");
        assert_eq!(args[args.len() - 2], "--");
        assert_eq!(args.last().map(|a| a.to_str()), Some(Some("-")));
    }

    #[test]
    fn counts_are_exact_rationals_not_floats() {
        let spec = FrameSpec {
            source: PathBuf::from("in.mp4"),
            start: Seconds::from_integer(0),
            duration: Seconds::new(10, 1),
            fps: Seconds::new(30_000, 1001),
            width: 4,
            height: 4,
        };
        // 10 s at 29.97 fps is 299.7 frames, floored — never 300 by rounding.
        assert_eq!(spec.frame_count(), 299);
        // A hostile spec clamps instead of overflowing the rational multiply.
        assert_eq!(
            product_floor(
                Seconds::from_integer(i64::MAX),
                Seconds::from_integer(i64::MAX)
            ),
            u64::MAX
        );
        assert_eq!(
            product_floor(Seconds::from_integer(-1), Seconds::from_integer(5)),
            0
        );
    }

    fn entries(cache: &SampleCache) -> usize {
        std::fs::read_dir(cache.root())
            .map(|it| it.flatten().count())
            .unwrap_or(0)
    }

    fn cache_file(cache: &SampleCache, ext: &str) -> PathBuf {
        std::fs::read_dir(cache.root())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == ext))
            .unwrap_or_else(|| panic!("no .{ext} entry"))
    }
}
