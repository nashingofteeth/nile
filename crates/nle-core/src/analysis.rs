//! Analysis products that live beside the project: transcripts and analyzer
//! products.
//!
//! Both are **content-addressed**, not named: the payload is serialised in a
//! canonical form, hashed with FNV-1a 64, and stored under
//! `.nile/<stem>/analysis/` — `transcripts/t1-<16 hex>` for words,
//! `products/a1-<16 hex>` for an analyzer's output. The `Expectation::WordsComplete`
//! payload names a transcript hash, so a transcript can be re-imported, edited
//! and re-imported without an expectation silently describing different words:
//! a changed transcript gets a new address, while the old address keeps naming
//! exactly the old words. The store never deletes an address; the guarantee is
//! "the hash always means the same words", not "the old words disappear".
//!
//! A product additionally records the sample and source signature it was
//! computed from. That signature is what freshness is judged against, not a
//! live cache key: a removed source or a pruned sample leaves the product
//! readable (an offline analysis pass), while a source whose size or mtime
//! moved makes it stale. A sample- or product-schema bump is *incompatible*
//! instead.
//!
//! The stores are deliberately outside `nile.json`: words and analysis change
//! on their own schedule and a project file is a hand-editable document.
//! [`TranscriptStore`] and [`ProductStore`] keep the root as
//! `Option<PathBuf>`, so a report-only caller (a unit test, a check with no
//! project file) says "no store" with [`TranscriptStore::empty`] instead of an
//! empty string path that would accidentally resolve to `.nile/...` in the
//! working directory.
//!
//! No dependency buys the hash: FNV-1a 64 is a dozen lines, and the stores'
//! contract (stable spelling, strict validation, atomic writes) is ours to
//! keep, not a crate's to change.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::state_dir;
use crate::time::{Seconds, rational_ser};

/// Version of the on-disk transcript shape. A build that does not know this
/// version rejects the file; there is no migration code.
pub const TRANSCRIPT_VERSION: u32 = 1;

/// One recognised word, in **asset source seconds** — the same time base as
/// `Clip::asset_in`/`asset_out`, so mapping to the timeline goes through the
/// clip's speed and a trim cuts the word exactly where the model says it does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    #[serde(with = "rational_ser")]
    pub start: Seconds,
    #[serde(with = "rational_ser")]
    pub end: Seconds,
}

/// A whole transcript for one asset, in canonical word order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub version: u32,
    /// The asset the source times belong to.
    pub asset: u32,
    pub words: Vec<Word>,
}

impl Transcript {
    /// Build a v1 transcript, sorting the words into canonical order.
    ///
    /// Sorting here (not only in [`Transcript::canonical_bytes`]) means two
    /// imports of the same words in a different order agree on one hash;
    /// duplicates are kept because a transcript may legitimately repeat a word
    /// at the same time (two speakers, a stutter).
    pub fn new(asset: u32, mut words: Vec<Word>) -> Self {
        words.sort_by(|a, b| (&a.start, &a.end, &a.text).cmp(&(&b.start, &b.end, &b.text)));
        Self {
            version: TRANSCRIPT_VERSION,
            asset,
            words,
        }
    }

    /// The words in canonical order. Cheap enough to do on every call: a
    /// transcript is a human-scale list, and a canonical order that only
    /// holds for values built by [`Transcript::new`] would let a parsed
    /// transcript hash differently from the same words imported locally.
    fn sorted(&self) -> Vec<Word> {
        let mut words = self.words.clone();
        words.sort_by(|a, b| (&a.start, &a.end, &a.text).cmp(&(&b.start, &b.end, &b.text)));
        words
    }

    /// Refuse words that cannot be true of an asset: a version we do not
    /// know, empty text, a negative or zero-width span, and (when the caller
    /// knows the probe) a word past the asset's end.
    pub fn validate(&self, asset_duration: Option<Seconds>) -> Result<(), AnalysisError> {
        if self.version != TRANSCRIPT_VERSION {
            return Err(AnalysisError::Invalid(format!(
                "unsupported transcript version {} (expected {TRANSCRIPT_VERSION})",
                self.version
            )));
        }
        let zero = Seconds::from_integer(0);
        for (index, word) in self.words.iter().enumerate() {
            if word.text.is_empty() {
                return Err(AnalysisError::Invalid(format!(
                    "word {index} has empty text"
                )));
            }
            if word.start < zero {
                return Err(AnalysisError::Invalid(format!(
                    "word {index} ({:?}) starts before zero",
                    word.text
                )));
            }
            if word.end <= word.start {
                return Err(AnalysisError::Invalid(format!(
                    "word {index} ({:?}) ends before it starts",
                    word.text
                )));
            }
            if let Some(duration) = asset_duration
                && word.end > duration
            {
                return Err(AnalysisError::Invalid(format!(
                    "word {index} ({:?}) ends past the asset ({} s)",
                    word.text,
                    crate::time::format_seconds(&duration)
                )));
            }
        }
        Ok(())
    }

    /// The exact bytes the hash is taken over: canonical JSON, no whitespace,
    /// rational times as `"n/d"` strings.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let canonical = Self {
            version: self.version,
            asset: self.asset,
            words: self.sorted(),
        };
        // A struct of strings and rationals cannot fail to serialise; the
        // `expect` documents that rather than hiding a broken store behind an
        // empty file.
        serde_json::to_vec(&canonical).expect("a transcript serialises")
    }

    /// Content address of these words: `t1-` plus 16 lowercase hex digits.
    pub fn hash(&self) -> String {
        format!("t1-{:016x}", fnv1a64(&self.canonical_bytes()))
    }
}

/// Words pasted in by a front-end before they are attached to an asset. A
/// separate type so the caller cannot build a `Transcript` with a version it
/// did not choose.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TranscriptInput {
    pub words: Vec<Word>,
}

/// A transcript-store failure. Every variant is a message for the check's
/// unverifiable detail: a missing or corrupt store must never become a panic
/// in the middle of a render review.
#[derive(Debug, thiserror::Error)]
pub enum AnalysisError {
    #[error("transcript io: {0}")]
    Io(#[from] std::io::Error),
    #[error("transcript json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid transcript: {0}")]
    Invalid(String),
    #[error("transcript {0} not found")]
    NotFound(String),
    #[error("transcript {0} is corrupt: {1}")]
    Corrupt(String, String),
    #[error("no transcript store")]
    NoStore,
}

/// Where transcripts live for one project.
///
/// `root: None` is the report-only store: reads answer "not found" and writes
/// are refused, so `evaluate` (which has no filesystem context) keeps its old
/// signature without inventing a path.
#[derive(Debug, Clone, Default)]
pub struct TranscriptStore {
    root: Option<PathBuf>,
}

impl TranscriptStore {
    /// A store that holds nothing (the report-only checks).
    pub fn empty() -> Self {
        Self { root: None }
    }

    /// The store beside `project_path`: `.nile/<stem>/analysis/transcripts`.
    pub fn at(project_path: &Path) -> Self {
        Self {
            root: Some(state_dir(project_path).join("analysis").join("transcripts")),
        }
    }

    /// The directory this store reads and writes, if it has one.
    pub fn dir(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Store `transcript` under its own hash and return that hash.
    ///
    /// Idempotent: importing the same words twice returns the same address.
    /// The write is atomic (a temp file in the same directory, then a rename),
    /// so an interrupted import cannot leave a half-written transcript that a
    /// later check would read as corrupt.
    pub fn import(&self, transcript: &Transcript) -> Result<String, AnalysisError> {
        let root = self.root.as_deref().ok_or(AnalysisError::NoStore)?;
        transcript.validate(None)?;
        let bytes = transcript.canonical_bytes();
        let hash = format!("t1-{:016x}", fnv1a64(&bytes));
        std::fs::create_dir_all(root)?;
        let path = root.join(&hash);
        // The address is the identity: accept any encoding that parses to the
        // same transcript (the writer here is canonical, a hand-edited file
        // may be pretty-printed). `load` also re-checks version/hash.
        match self.load(&hash) {
            Ok(existing) if existing.canonical_bytes() == bytes => return Ok(hash),
            Ok(_) => {
                return Err(AnalysisError::Corrupt(
                    hash,
                    "a different transcript already has this address".to_string(),
                ));
            }
            Err(AnalysisError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        let mut tmp = tempfile::NamedTempFile::new_in(root)?;
        tmp.write_all(&bytes)?;
        tmp.write_all(b"\n")?;
        tmp.persist(&path).map_err(|error| error.error)?;
        Ok(hash)
    }

    /// Read the transcript at `hash`, verifying the address still describes
    /// the content it names.
    ///
    /// The hash is checked **before** it is joined to a path: only
    /// `t1-` + 16 lowercase hex digits is accepted, so `../nile.json` is an
    /// invalid address, not a file read.
    pub fn load(&self, hash: &str) -> Result<Transcript, AnalysisError> {
        if !is_transcript_hash(hash) {
            return Err(AnalysisError::Invalid(format!(
                "not a transcript hash: {hash:?}"
            )));
        }
        let Some(root) = self.root.as_deref() else {
            return Err(AnalysisError::NotFound(hash.to_string()));
        };
        let path = root.join(hash);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(AnalysisError::NotFound(hash.to_string()));
            }
            Err(error) => return Err(error.into()),
        };
        let transcript: Transcript = serde_json::from_slice(&bytes)?;
        // Version (and word shape) first: a future-version file should say so,
        // not "its content hashes to …" because fields we do not know were
        // dropped by the parser.
        transcript.validate(None)?;
        let actual = transcript.hash();
        if actual != hash {
            return Err(AnalysisError::Corrupt(
                hash.to_string(),
                format!("its content hashes to {actual}"),
            ));
        }
        Ok(transcript)
    }
}

/// `t1-` + 16 lowercase hex digits. Lowercase only: that is the one spelling
/// [`Transcript::hash`] writes, so any other case is a typo, not an address.
pub fn is_transcript_hash(hash: &str) -> bool {
    is_hash("t1-", hash)
}

/// `a1-` + 16 lowercase hex digits, the spelling [`AnalysisProduct::hash`]
/// writes.
pub fn is_product_hash(hash: &str) -> bool {
    is_hash("a1-", hash)
}

fn is_hash(prefix: &str, hash: &str) -> bool {
    let Some(digits) = hash.strip_prefix(prefix) else {
        return false;
    };
    digits.len() == 16
        && digits
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ---------------------------------------------------------------- products

/// Version of the sampler's packed-frame/audio contract. It lives beside the
/// product schema because a product records it: a bump makes old products
/// incompatible, not stale-by-mtime. `nle-ffmpeg` re-exports this constant so
/// the sampler and the consumers cannot drift.
pub const SAMPLE_VERSION: u32 = 2;

/// Version of the on-disk analyzer-product shape. A product from another
/// schema is refused with a clear error; there is no migration code.
pub const PRODUCT_VERSION: u32 = 1;

/// A product's analyzer identity: `mean-luma` v1 today, anything a future
/// out-of-process tool reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzerInfo {
    pub name: String,
    pub version: u32,
}

/// The source file a sample was taken from, as recorded at sampling time.
///
/// This is a **snapshot**, not a live key: when the source is gone the sample
/// key can no longer be recomputed, so a product stays readable and a later
/// re-sample compares its new signature against this one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSignature {
    pub source: String,
    pub source_arg: String,
    pub source_size: u64,
    pub source_mtime_secs: u64,
    pub source_mtime_nanos: u32,
}

/// The sample a product was computed from. `key`/`params` are the sampler's
/// opaque 16-hex fingerprints; `version` is the sampler's `SAMPLE_VERSION`, so
/// a bump can be told apart from a moved source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductSample {
    pub version: u32,
    pub key: String,
    pub params: String,
    #[serde(flatten)]
    pub source: SourceSignature,
}

/// The sampled frame grid a per-frame product is indexed on.
///
/// Sample frame `i` covers source `[start + i/fps, start + (i+1)/fps)`. The
/// geometry is the sampled picture's, not the source's: normalized box
/// coordinates are invariant to the sampler's `scale`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleGrid {
    #[serde(with = "rational_ser")]
    pub start: Seconds,
    #[serde(with = "rational_ser")]
    pub fps: Seconds,
    pub width: u32,
    pub height: u32,
}

impl SampleGrid {
    /// The sample frame covering source time `source`, or `None` before the
    /// grid. Exact rational floor: no float can round the lookup onto a
    /// neighbouring frame.
    pub fn index_at(&self, source: Seconds) -> Option<u64> {
        if self.fps <= Seconds::from_integer(0) {
            return None;
        }
        let offset = crate::time::checked_sub(&source, &self.start)?;
        if offset < Seconds::from_integer(0) {
            return None;
        }
        let numerator = i128::from(*offset.numer()) * i128::from(*self.fps.numer());
        let denominator = i128::from(*offset.denom()) * i128::from(*self.fps.denom());
        if denominator <= 0 {
            return None;
        }
        u64::try_from(numerator / denominator).ok()
    }
}

/// One detector class/track. `label` is the class the detector was asked for
/// (`face`, `person`); `id` is its optional stable instance id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectKey {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// A box in millionths of the sampled picture (`1_000_000` = one side).
/// Integers keep the canonical bytes float-free, the same reason `luma_milli`
/// is an integer: a float's last bit must not decide an `a1-` address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// One detected subject in one sample frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedBox {
    pub subject: SubjectKey,
    pub rect: BoxRect,
    pub confidence_milli: u32,
}

/// The boxes detected in sample frame `index` (one entry per packed frame,
/// in presentation order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameBoxes {
    pub index: u64,
    pub boxes: Vec<DetectedBox>,
}

/// The one canonical box order: label, id, confidence descending, then
/// geometry. The analyzer sorts before writing; [`AnalysisProduct::validate`]
/// refuses bytes that are not in this order, so equal detections hash alike.
pub fn sort_detected_boxes(boxes: &mut [DetectedBox]) {
    boxes.sort_by(|a, b| {
        a.subject
            .label
            .cmp(&b.subject.label)
            .then_with(|| a.subject.id.cmp(&b.subject.id))
            .then_with(|| b.confidence_milli.cmp(&a.confidence_milli))
            .then_with(|| {
                (a.rect.x, a.rect.y, a.rect.w, a.rect.h)
                    .cmp(&(b.rect.x, b.rect.y, b.rect.w, b.rect.h))
            })
    });
}

/// The smallest accepted box side, in millionths. A zero-width/height box is
/// not a detection; the host refuses it rather than dividing by zero later.
pub const BOX_MIN_SIDE: u32 = 1;

/// One RMS/peak window over packed s16le PCM, in **millionths of full scale**
/// (`1_000_000` = the largest representable s16 amplitude). Integers keep the
/// canonical bytes float-free, the same reason `luma_milli` and `BoxRect` are
/// integers: a float's last bit must not decide an `a1-` address.
///
/// `start` is the first sample (per channel) of this window relative to the
/// beginning of the packed sample, and `samples` is how many per-channel
/// samples it covers. Windows tile the packed payload: the first starts at 0,
/// each next starts where the previous ended, and only the last may be shorter
/// than `window_samples`.
///
/// Both amplitudes aggregate **every channel** in the window into one scalar;
/// there is no per-channel breakdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioWindowLevel {
    pub start: u64,
    pub samples: u64,
    pub rms_millionths: u32,
    pub peak_millionths: u32,
}

/// What an analyzer produced. Tagged so a second analyzer cannot be mistaken
/// for the first; `luma_milli` is per-frame mean Rec. 709 luma × 1000, in
/// presentation order (integer units keep the canonical bytes float-free).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProductResult {
    MeanLuma {
        luma_milli: Vec<u32>,
    },
    /// Per-frame subject boxes on one sampled grid. `labels` is the sorted,
    /// deduplicated set the detector was asked for and `floor_milli` is the
    /// confidence floor it retained; a consumer asking for a lower threshold
    /// knows the answer is incomplete.
    SubjectBoxes {
        grid: SampleGrid,
        labels: Vec<String>,
        floor_milli: u32,
        frames: Vec<FrameBoxes>,
    },
    /// RMS/peak levels over one packed audio window. `start` is the source
    /// time of sample 0 of the packed payload; `windows[i].start` is an offset
    /// from that point, so source time maps back through `sample_rate`.
    AudioLevels {
        #[serde(with = "rational_ser")]
        start: Seconds,
        sample_rate: u32,
        channels: u16,
        window_samples: u64,
        windows: Vec<AudioWindowLevel>,
    },
}

/// A content-addressed analyzer product.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisProduct {
    pub version: u32,
    pub analyzer: AnalyzerInfo,
    pub sample: ProductSample,
    pub result: ProductResult,
}

/// How a stored product relates to the source it recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The live source's size and mtime still match the recorded signature.
    Fresh,
    /// The live source exists but its size or mtime moved.
    Stale,
    /// The source is gone (or is not a file): the product stays readable.
    Offline,
}

impl AnalysisProduct {
    /// Whether this product can be compared with a sampler version at all.
    ///
    /// A schema or sampler bump is incompatible, not stale-by-mtime: the
    /// result means something else even if the source file is unchanged.
    pub fn is_compatible(&self, sample_version: u32) -> bool {
        self.version == PRODUCT_VERSION && self.sample.version == sample_version
    }

    /// Judge freshness against the recorded signature. A missing source is
    /// [`Freshness::Offline`] — readable, not an error.
    pub fn freshness(&self) -> Freshness {
        let primary = Path::new(&self.sample.source.source);
        let path = if primary.is_file() {
            primary
        } else {
            Path::new(&self.sample.source.source_arg)
        };
        let Ok(meta) = std::fs::metadata(path) else {
            return Freshness::Offline;
        };
        if !meta.is_file() {
            return Freshness::Offline;
        }
        let (secs, nanos) = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| (d.as_secs(), d.subsec_nanos()))
            .unwrap_or((0, 0));
        let recorded = &self.sample.source;
        if meta.len() == recorded.source_size
            && secs == recorded.source_mtime_secs
            && nanos == recorded.source_mtime_nanos
        {
            Freshness::Fresh
        } else {
            Freshness::Stale
        }
    }

    /// Refuse a product that cannot be true of the schema: an unknown version,
    /// empty analyzer name, a malformed sample fingerprint, a non-absolute
    /// source, or a per-frame luma outside 0..=255 000.
    pub fn validate(&self) -> Result<(), ProductError> {
        if self.version != PRODUCT_VERSION {
            return Err(ProductError::Invalid(format!(
                "unsupported product version {} (expected {PRODUCT_VERSION})",
                self.version
            )));
        }
        if self.analyzer.name.is_empty() {
            return Err(ProductError::Invalid("empty analyzer name".into()));
        }
        if self.analyzer.version == 0 {
            return Err(ProductError::Invalid(
                "analyzer version must be positive".into(),
            ));
        }
        if self.sample.version == 0 {
            return Err(ProductError::Invalid(
                "sample version must be positive".into(),
            ));
        }
        if !is_hex16(&self.sample.key) || !is_hex16(&self.sample.params) {
            return Err(ProductError::Invalid(
                "sample key and params must be 16 hex digits".into(),
            ));
        }
        if self.sample.source.source_arg.is_empty()
            || !Path::new(&self.sample.source.source_arg).is_absolute()
        {
            return Err(ProductError::Invalid(
                "the recorded source must be an absolute path".into(),
            ));
        }
        match &self.result {
            ProductResult::MeanLuma { luma_milli } => {
                if luma_milli.is_empty() {
                    return Err(ProductError::Invalid("the product has no frames".into()));
                }
                if let Some(value) = luma_milli.iter().find(|v| **v > 255_000) {
                    return Err(ProductError::Invalid(format!(
                        "luma {value} is outside 0..=255000"
                    )));
                }
            }
            ProductResult::SubjectBoxes {
                grid,
                labels,
                floor_milli,
                frames,
            } => {
                validate_subject_boxes(grid, labels, *floor_milli, frames)?;
            }
            ProductResult::AudioLevels {
                start,
                sample_rate,
                channels,
                window_samples,
                windows,
            } => {
                validate_audio_levels(*start, *sample_rate, *channels, *window_samples, windows)?;
            }
        }
        Ok(())
    }

    /// The exact bytes the hash is taken over: canonical JSON, no whitespace.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        // A struct of strings, integers and a tagged enum cannot fail to
        // serialise; the `expect` documents that rather than hiding a broken
        // store behind an empty file.
        serde_json::to_vec(self).expect("a product serialises")
    }

    /// Content address of this product: `a1-` plus 16 lowercase hex digits.
    pub fn hash(&self) -> String {
        format!("a1-{:016x}", fnv1a64(&self.canonical_bytes()))
    }
}

fn validate_subject_boxes(
    grid: &SampleGrid,
    labels: &[String],
    floor_milli: u32,
    frames: &[FrameBoxes],
) -> Result<(), ProductError> {
    if grid.start < Seconds::from_integer(0) {
        return Err(ProductError::Invalid(
            "the sample grid starts before zero".into(),
        ));
    }
    if grid.fps <= Seconds::from_integer(0) {
        return Err(ProductError::Invalid(
            "the sample grid needs a positive fps".into(),
        ));
    }
    if grid.width == 0 || grid.height == 0 {
        return Err(ProductError::Invalid(
            "the sample grid has a zero dimension".into(),
        ));
    }
    if labels.is_empty() {
        return Err(ProductError::Invalid("the product names no labels".into()));
    }
    if labels.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(ProductError::Invalid(
            "labels must be sorted and unique".into(),
        ));
    }
    if floor_milli > 1000 {
        return Err(ProductError::Invalid(format!(
            "the confidence floor {floor_milli} is outside 0..=1000"
        )));
    }
    if frames.is_empty() {
        return Err(ProductError::Invalid("the product has no frames".into()));
    }
    for (position, frame) in frames.iter().enumerate() {
        if frame.index != position as u64 {
            return Err(ProductError::Invalid(format!(
                "frame {position} records index {}",
                frame.index
            )));
        }
        if !boxes_in_order(&frame.boxes) {
            return Err(ProductError::Invalid(format!(
                "frame {position} boxes are not in canonical order"
            )));
        }
        for detected in &frame.boxes {
            if detected.subject.label.is_empty() {
                return Err(ProductError::Invalid(format!(
                    "frame {position} has an empty subject label"
                )));
            }
            if !labels.contains(&detected.subject.label) {
                return Err(ProductError::Invalid(format!(
                    "frame {position} uses label {:?}, not one it declared",
                    detected.subject.label
                )));
            }
            if detected.subject.id.as_deref() == Some("") {
                return Err(ProductError::Invalid(format!(
                    "frame {position} has an empty subject id"
                )));
            }
            let rect = detected.rect;
            if rect.w < BOX_MIN_SIDE || rect.h < BOX_MIN_SIDE {
                return Err(ProductError::Invalid(format!(
                    "frame {position} has a zero-side box"
                )));
            }
            if u64::from(rect.x) + u64::from(rect.w) > 1_000_000
                || u64::from(rect.y) + u64::from(rect.h) > 1_000_000
            {
                return Err(ProductError::Invalid(format!(
                    "frame {position} box leaves the sampled picture"
                )));
            }
            if detected.confidence_milli < floor_milli || detected.confidence_milli > 1000 {
                return Err(ProductError::Invalid(format!(
                    "frame {position} confidence {} is outside {floor_milli}..=1000",
                    detected.confidence_milli
                )));
            }
        }
    }
    Ok(())
}

fn validate_audio_levels(
    start: Seconds,
    sample_rate: u32,
    channels: u16,
    window_samples: u64,
    windows: &[AudioWindowLevel],
) -> Result<(), ProductError> {
    if start < Seconds::from_integer(0) {
        return Err(ProductError::Invalid(
            "the audio window starts before zero".into(),
        ));
    }
    if sample_rate == 0 {
        return Err(ProductError::Invalid(
            "the audio window has a zero sample rate".into(),
        ));
    }
    if channels == 0 {
        return Err(ProductError::Invalid(
            "the audio window has zero channels".into(),
        ));
    }
    if window_samples == 0 {
        return Err(ProductError::Invalid(
            "the audio window length is zero samples".into(),
        ));
    }
    if windows.is_empty() {
        return Err(ProductError::Invalid(
            "the product has no audio windows".into(),
        ));
    }
    let mut expected = 0_u64;
    for (position, window) in windows.iter().enumerate() {
        if window.start != expected {
            return Err(ProductError::Invalid(format!(
                "audio window {position} starts at sample {} but the previous one ended at {expected}",
                window.start
            )));
        }
        if window.samples == 0 {
            return Err(ProductError::Invalid(format!(
                "audio window {position} is empty"
            )));
        }
        if window.samples > window_samples {
            return Err(ProductError::Invalid(format!(
                "audio window {position} has {} samples (window length {window_samples})",
                window.samples
            )));
        }
        // Every window but the last is full: a short middle window would leave
        // a hole the host's coverage check could not tell from a shorter sample.
        let last = position + 1 == windows.len();
        if !last && window.samples != window_samples {
            return Err(ProductError::Invalid(format!(
                "audio window {position} has {} of {window_samples} samples but is not last",
                window.samples
            )));
        }
        if window.rms_millionths > window.peak_millionths {
            return Err(ProductError::Invalid(format!(
                "audio window {position} has RMS {} above peak {}",
                window.rms_millionths, window.peak_millionths
            )));
        }
        if window.peak_millionths > 1_000_000 {
            return Err(ProductError::Invalid(format!(
                "audio window {position} peak {} is outside 0..=1000000",
                window.peak_millionths
            )));
        }
        expected = expected
            .checked_add(window.samples)
            .ok_or_else(|| ProductError::Invalid("the audio window coverage overflows".into()))?;
    }
    Ok(())
}

fn boxes_in_order(boxes: &[DetectedBox]) -> bool {
    boxes.windows(2).all(|pair| {
        let (a, b) = (&pair[0], &pair[1]);
        (
            a.subject.label.as_str(),
            a.subject.id.as_deref(),
            std::cmp::Reverse(a.confidence_milli),
            a.rect.x,
            a.rect.y,
            a.rect.w,
            a.rect.h,
        ) <= (
            b.subject.label.as_str(),
            b.subject.id.as_deref(),
            std::cmp::Reverse(b.confidence_milli),
            b.rect.x,
            b.rect.y,
            b.rect.w,
            b.rect.h,
        )
    })
}

fn is_hex16(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A product-store failure. Like [`AnalysisError`], every variant is a message
/// for a consumer's "unverifiable" detail: a missing or corrupt product must
/// never become a panic in the middle of a render review.
#[derive(Debug, thiserror::Error)]
pub enum ProductError {
    #[error("product io: {0}")]
    Io(#[from] std::io::Error),
    #[error("product json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid product: {0}")]
    Invalid(String),
    #[error("product {0} not found")]
    NotFound(String),
    #[error("product {0} is corrupt: {1}")]
    Corrupt(String, String),
    #[error("no product store")]
    NoStore,
}

/// Where analyzer products live for one project: `.nile/<stem>/analysis/products`.
///
/// `root: None` is the report-only store: reads answer "not found" and writes
/// are refused, mirroring [`TranscriptStore`].
#[derive(Debug, Clone, Default)]
pub struct ProductStore {
    root: Option<PathBuf>,
}

impl ProductStore {
    /// A store that holds nothing (report-only consumers).
    pub fn empty() -> Self {
        Self { root: None }
    }

    /// The store beside `project_path`.
    pub fn at(project_path: &Path) -> Self {
        Self {
            root: Some(state_dir(project_path).join("analysis").join("products")),
        }
    }

    /// The directory this store reads and writes, if it has one.
    pub fn dir(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Store `product` under its own address and return that address.
    ///
    /// Idempotent: importing identical content twice returns the same
    /// address. The write is atomic (a temp file in the same directory, then a
    /// rename), so an interrupted import cannot leave a half-written product.
    pub fn import(&self, product: &AnalysisProduct) -> Result<String, ProductError> {
        let root = self.root.as_deref().ok_or(ProductError::NoStore)?;
        product.validate()?;
        let bytes = product.canonical_bytes();
        let hash = product.hash();
        std::fs::create_dir_all(root)?;
        let path = root.join(&hash);
        match self.load(&hash) {
            Ok(existing) if existing.canonical_bytes() == bytes => return Ok(hash),
            Ok(_) => {
                return Err(ProductError::Corrupt(
                    hash,
                    "a different product already has this address".to_string(),
                ));
            }
            Err(ProductError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        let mut tmp = tempfile::NamedTempFile::new_in(root)?;
        tmp.write_all(&bytes)?;
        tmp.write_all(b"\n")?;
        tmp.persist(&path).map_err(|error| error.error)?;
        Ok(hash)
    }

    /// Adopt a staged, already-canonical product file: verify `address` names
    /// its content, then rename it into the store.
    ///
    /// This is the host's path for an out-of-process analyzer: the child can
    /// only write inside its staging directory, and nothing lands in
    /// `analysis/products/` until this validates the bytes. Identical bytes at
    /// an existing address are idempotent; a different file under that address
    /// is a refusal, never an overwrite.
    pub fn adopt(
        &self,
        product: &AnalysisProduct,
        staged: &Path,
        address: &str,
    ) -> Result<String, ProductError> {
        let root = self.root.as_deref().ok_or(ProductError::NoStore)?;
        product.validate()?;
        if !is_product_hash(address) {
            return Err(ProductError::Invalid(format!(
                "not a product hash: {address:?}"
            )));
        }
        let canonical = product.canonical_bytes();
        let expected = product.hash();
        if address != expected {
            return Err(ProductError::Corrupt(
                address.to_string(),
                format!("its content hashes to {expected}"),
            ));
        }
        let staged_meta = std::fs::symlink_metadata(staged)?;
        if !staged_meta.file_type().is_file() {
            return Err(ProductError::Invalid(
                "the staged product is not a regular file".to_string(),
            ));
        }
        let raw = std::fs::read(staged)?;
        let content = raw.strip_suffix(b"\n").unwrap_or(&raw);
        if content != canonical.as_slice() {
            return Err(ProductError::Corrupt(
                address.to_string(),
                "the staged bytes are not the canonical product".to_string(),
            ));
        }
        std::fs::create_dir_all(root)?;
        let target = root.join(&expected);
        match self.load(&expected) {
            Ok(existing) => {
                if existing.canonical_bytes() == canonical {
                    // The content is already there; the staged copy is done.
                    let _ = std::fs::remove_file(staged);
                    return Ok(expected);
                }
                return Err(ProductError::Corrupt(
                    expected,
                    "a different product already has this address".to_string(),
                ));
            }
            Err(ProductError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        std::fs::rename(staged, &target)?;
        Ok(expected)
    }

    /// Read the product at `address`, verifying that the address still
    /// describes its content.
    pub fn load(&self, address: &str) -> Result<AnalysisProduct, ProductError> {
        if !is_product_hash(address) {
            return Err(ProductError::Invalid(format!(
                "not a product hash: {address:?}"
            )));
        }
        let Some(root) = self.root.as_deref() else {
            return Err(ProductError::NotFound(address.to_string()));
        };
        let bytes = match std::fs::read(root.join(address)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ProductError::NotFound(address.to_string()));
            }
            Err(error) => return Err(error.into()),
        };
        let product: AnalysisProduct = serde_json::from_slice(&bytes)?;
        product.validate()?;
        let actual = product.hash();
        if actual != address {
            return Err(ProductError::Corrupt(
                address.to_string(),
                format!("its content hashes to {actual}"),
            ));
        }
        Ok(product)
    }
}

/// The project's analysis stores, bundled so a check cannot accidentally
/// consult a transcript store from one project and a product store from
/// another. [`AnalysisStores::empty`] is the report-only case.
#[derive(Debug, Clone)]
pub struct AnalysisStores {
    pub transcripts: TranscriptStore,
    pub products: ProductStore,
}

impl AnalysisStores {
    /// No stores: every transcript/product lookup is a clean miss.
    pub fn empty() -> Self {
        Self {
            transcripts: TranscriptStore::empty(),
            products: ProductStore::empty(),
        }
    }

    /// The stores beside `project_path`.
    pub fn at(project_path: &Path) -> Self {
        Self {
            transcripts: TranscriptStore::at(project_path),
            products: ProductStore::at(project_path),
        }
    }

    /// A bundle with a transcript store and no products (the older callers).
    pub fn with_transcripts(transcripts: TranscriptStore) -> Self {
        Self {
            transcripts,
            products: ProductStore::empty(),
        }
    }

    /// A bundle with a product store and no transcripts.
    pub fn with_products(products: ProductStore) -> Self {
        Self {
            transcripts: TranscriptStore::empty(),
            products,
        }
    }
}

/// FNV-1a 64. Stable spelling matters more than collision resistance here:
/// the address is a name for local content, not a security boundary.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n: i64) -> Seconds {
        Seconds::from_integer(n)
    }

    fn word(text: &str, start: i64, end: i64) -> Word {
        Word {
            text: text.to_string(),
            start: s(start),
            end: s(end),
        }
    }

    fn sample() -> Transcript {
        Transcript::new(1, vec![word("two", 1, 2), word("take", 0, 1)])
    }

    fn product(source: &Path, size: u64, mtime_secs: u64, mtime_nanos: u32) -> AnalysisProduct {
        AnalysisProduct {
            version: PRODUCT_VERSION,
            analyzer: AnalyzerInfo {
                name: "mean-luma".to_string(),
                version: 1,
            },
            sample: ProductSample {
                version: 1,
                key: "0123456789abcdef".to_string(),
                params: "fedcba9876543210".to_string(),
                source: SourceSignature {
                    source: source.display().to_string(),
                    source_arg: source.display().to_string(),
                    source_size: size,
                    source_mtime_secs: mtime_secs,
                    source_mtime_nanos: mtime_nanos,
                },
            },
            result: ProductResult::MeanLuma {
                luma_milli: vec![53_000, 52_750],
            },
        }
    }

    fn product_in(dir: &Path) -> AnalysisProduct {
        product(&dir.join("red.mp4"), 1234, 1_700_000_000, 42)
    }

    #[test]
    fn new_sorts_the_words_and_keeps_duplicates() {
        let transcript =
            Transcript::new(7, vec![word("b", 2, 3), word("a", 0, 1), word("a", 0, 1)]);
        let texts: Vec<&str> = transcript
            .words
            .iter()
            .map(|word| word.text.as_str())
            .collect();
        assert_eq!(texts, ["a", "a", "b"]);
        assert_eq!(transcript.version, TRANSCRIPT_VERSION);
        assert_eq!(transcript.asset, 7);
    }

    #[test]
    fn a_transcript_input_reads_a_words_array() {
        let input: TranscriptInput =
            serde_json::from_str(r#"{"words":[{"text":"hi","start":"0/1","end":"1/2"}]}"#).unwrap();
        assert_eq!(input.words.len(), 1);
        assert_eq!(input.words[0].text, "hi");
        assert_eq!(input.words[0].start, Seconds::new(0, 1));
        assert_eq!(input.words[0].end, Seconds::new(1, 2));
    }

    #[test]
    fn the_hash_spelling_is_stable() {
        // Hard-coded so a field rename, a serialiser change or a hash tweak
        // cannot silently re-address every stored transcript.
        let transcript = Transcript::new(1, vec![word("take", 0, 1), word("two", 1, 2)]);
        assert_eq!(transcript.hash(), "t1-d2d70167b35e1c23");
        // Same words in another order are the same address.
        let reordered = Transcript::new(1, vec![word("two", 1, 2), word("take", 0, 1)]);
        assert_eq!(reordered.hash(), transcript.hash());
    }

    #[test]
    fn canonical_bytes_ignore_the_input_order() {
        let mut transcript = sample();
        transcript.words.reverse();
        let canonical = Transcript {
            version: transcript.version,
            asset: transcript.asset,
            words: transcript.sorted(),
        };
        assert_eq!(
            transcript.canonical_bytes(),
            serde_json::to_vec(&canonical).unwrap()
        );
        assert!(
            std::str::from_utf8(&transcript.canonical_bytes())
                .unwrap()
                .contains("\"start\":\"0/1\"")
        );
    }

    #[test]
    fn a_store_round_trips_and_import_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = TranscriptStore::at(&dir.path().join("nile.json"));
        let transcript = sample();
        let hash = store.import(&transcript).unwrap();
        assert_eq!(hash, transcript.hash());
        assert_eq!(store.load(&hash).unwrap(), transcript);
        assert_eq!(store.import(&transcript).unwrap(), hash, "same content");
        assert!(
            store
                .dir()
                .unwrap()
                .ends_with(".nile/nile/analysis/transcripts"),
            "{:?}",
            store.dir()
        );
        // The file is human-readable and newline-terminated.
        let text = std::fs::read_to_string(store.dir().unwrap().join(&hash)).unwrap();
        assert!(text.ends_with('\n'));
        assert!(text.contains("\"take\""));
    }

    #[test]
    fn a_pretty_printed_file_at_the_address_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = TranscriptStore::at(&dir.path().join("nile.json"));
        let transcript = sample();
        let hash = store.import(&transcript).unwrap();
        let path = store.dir().unwrap().join(&hash);
        std::fs::write(&path, serde_json::to_string_pretty(&transcript).unwrap()).unwrap();

        // `load` re-serialises before hashing; `import` must agree instead of
        // calling equivalent JSON a collision.
        assert_eq!(store.load(&hash).unwrap(), transcript);
        assert_eq!(store.import(&transcript).unwrap(), hash);
    }

    #[test]
    fn a_reordered_file_at_the_address_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = TranscriptStore::at(&dir.path().join("nile.json"));
        let transcript = sample();
        let hash = store.import(&transcript).unwrap();
        let path = store.dir().unwrap().join(&hash);

        let mut reordered = transcript.clone();
        reordered.words.reverse();
        std::fs::write(&path, serde_json::to_vec(&reordered).unwrap()).unwrap();

        // The address is canonical content, not field order: `load` accepts
        // the reordered file and `import` must see the same transcript.
        assert_eq!(store.load(&hash).unwrap(), reordered);
        assert_eq!(store.import(&transcript).unwrap(), hash);
    }

    #[test]
    fn a_future_version_is_named_before_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = TranscriptStore::at(&dir.path().join("nile.json"));
        let mut transcript = sample();
        transcript.version = TRANSCRIPT_VERSION + 1;
        let hash = transcript.hash();
        std::fs::create_dir_all(store.dir().unwrap()).unwrap();
        std::fs::write(
            store.dir().unwrap().join(&hash),
            transcript.canonical_bytes(),
        )
        .unwrap();

        let error = store.load(&hash).unwrap_err();
        assert!(
            error.to_string().contains("unsupported transcript version"),
            "{error}"
        );
    }

    #[test]
    fn a_bad_address_is_refused_before_it_touches_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = TranscriptStore::at(&dir.path().join("nile.json"));
        for bad in [
            "",
            "take",
            "t1-",
            "t1-xyz",
            "t1-000000000000000",
            "t1-00000000000000000",
            "t1-000000000000000G",
            "t1-0000000000000000/../nile.json",
            "../t1-0000000000000000",
            "t1-000000000000000A",
        ] {
            let error = store.load(bad).unwrap_err();
            assert!(
                matches!(error, AnalysisError::Invalid(_)),
                "{bad:?}: {error}"
            );
        }
        assert!(
            store.load("t1-0000000000000000").is_err(),
            "a valid but absent address"
        );
    }

    #[test]
    fn corrupt_content_and_a_mismatched_address_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = TranscriptStore::at(&dir.path().join("nile.json"));
        let hash = store.import(&sample()).unwrap();
        let path = store.dir().unwrap().join(&hash);

        std::fs::write(&path, b"not json\n").unwrap();
        assert!(matches!(store.load(&hash), Err(AnalysisError::Json(_))));

        // Valid JSON under the wrong address is a corrupt store, not a
        // silently different transcript.
        let moved = store.dir().unwrap().join("t1-0000000000000000");
        std::fs::write(&moved, serde_json::to_vec(&sample()).unwrap()).unwrap();
        assert!(matches!(
            store.load("t1-0000000000000000"),
            Err(AnalysisError::Corrupt(..))
        ));
    }

    #[test]
    fn two_different_transcripts_cannot_share_an_address() {
        let dir = tempfile::tempdir().unwrap();
        let store = TranscriptStore::at(&dir.path().join("nile.json"));
        let hash = store.import(&sample()).unwrap();
        let path = store.dir().unwrap().join(&hash);
        let mut other = sample();
        other.asset = 2;
        std::fs::write(&path, other.canonical_bytes()).unwrap();
        assert!(matches!(
            store.import(&sample()),
            Err(AnalysisError::Corrupt(..))
        ));
    }

    #[test]
    fn an_empty_store_loads_nothing_and_refuses_writes() {
        let store = TranscriptStore::empty();
        assert_eq!(store.dir(), None);
        assert!(matches!(
            store.load("t1-0000000000000000"),
            Err(AnalysisError::NotFound(_))
        ));
        assert!(matches!(
            store.import(&sample()),
            Err(AnalysisError::NoStore)
        ));
    }

    #[test]
    fn validation_refuses_impossible_words() {
        let cases = [
            (word("", 0, 1), "empty text"),
            (word("a", -1, 1), "before zero"),
            (word("a", 1, 1), "ends before"),
            (word("a", 2, 1), "ends before"),
        ];
        for (bad, needle) in cases {
            let transcript = Transcript {
                version: TRANSCRIPT_VERSION,
                asset: 1,
                words: vec![bad],
            };
            let error = transcript.validate(None).unwrap_err().to_string();
            assert!(error.contains(needle), "{error}");
        }
        let mut wrong_version = sample();
        wrong_version.version = 2;
        assert!(
            wrong_version
                .validate(None)
                .unwrap_err()
                .to_string()
                .contains("version")
        );
        assert!(sample().validate(None).is_ok());
        assert!(sample().validate(Some(s(2))).is_ok(), "end == duration");
        let error = sample().validate(Some(s(1))).unwrap_err().to_string();
        assert!(error.contains("past the asset"), "{error}");
    }

    #[test]
    fn a_product_store_round_trips_and_import_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProductStore::at(&dir.path().join("nile.json"));
        let product = product_in(dir.path());
        let hash = store.import(&product).unwrap();
        assert_eq!(hash, product.hash());
        assert!(is_product_hash(&hash), "{hash}");
        assert_eq!(store.load(&hash).unwrap(), product);
        assert_eq!(store.import(&product).unwrap(), hash, "same content");
        assert!(
            store
                .dir()
                .unwrap()
                .ends_with(".nile/nile/analysis/products"),
            "{:?}",
            store.dir()
        );
        // The file is human-readable and newline-terminated.
        let text = std::fs::read_to_string(store.dir().unwrap().join(&hash)).unwrap();
        assert!(text.ends_with('\n'));
        assert!(text.contains("\"mean_luma\""));
    }

    #[test]
    fn the_product_hash_spelling_is_stable() {
        // Hard-coded so a field rename, a serialiser change or a hash tweak
        // cannot silently re-address every stored product.
        let product = product(Path::new("/media/red.mp4"), 1234, 1_700_000_000, 42);
        assert_eq!(product.hash(), "a1-f51a8f2781c8363e");
        assert_eq!(
            product.canonical_bytes(),
            serde_json::to_vec(&product).unwrap()
        );
        assert!(
            std::str::from_utf8(&product.canonical_bytes())
                .unwrap()
                .contains("\"kind\":\"mean_luma\"")
        );
    }

    #[test]
    fn adopting_a_staged_product_validates_its_address_and_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProductStore::at(&dir.path().join("nile.json"));
        let product = product_in(dir.path());
        let hash = product.hash();
        let stage = dir.path().join("stage");
        std::fs::create_dir(&stage).unwrap();
        let staged = stage.join("product.json");
        let mut bytes = product.canonical_bytes();
        bytes.push(b'\n');
        std::fs::write(&staged, &bytes).unwrap();

        assert_eq!(store.adopt(&product, &staged, &hash).unwrap(), hash);
        assert!(!staged.exists(), "the staged copy moved into the store");
        assert_eq!(store.load(&hash).unwrap(), product);

        // Same bytes at the same address are idempotent, including from a
        // second staging directory.
        std::fs::write(&staged, &bytes).unwrap();
        assert_eq!(store.adopt(&product, &staged, &hash).unwrap(), hash);
        assert!(!staged.exists());

        // A wrong address is refused even when the bytes are fine.
        std::fs::write(&staged, &bytes).unwrap();
        let error = store
            .adopt(&product, &staged, "a1-0000000000000000")
            .unwrap_err();
        assert!(matches!(error, ProductError::Corrupt(..)), "{error}");
        assert!(staged.exists(), "a refusal leaves the staged file alone");

        // A valid address with non-canonical bytes is refused too.
        std::fs::write(&staged, serde_json::to_vec_pretty(&product).unwrap()).unwrap();
        let error = store.adopt(&product, &staged, &hash).unwrap_err();
        assert!(matches!(error, ProductError::Corrupt(..)), "{error}");
    }

    #[test]
    fn product_staleness_follows_the_recorded_source_signature() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("clip.mp4");
        std::fs::write(&source, b"first").unwrap();
        let meta = std::fs::metadata(&source).unwrap();
        let (secs, nanos) = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| (d.as_secs(), d.subsec_nanos()))
            .unwrap();
        let mut product = product_in(dir.path());
        product.sample.source = SourceSignature {
            source: source.display().to_string(),
            source_arg: source.display().to_string(),
            source_size: meta.len(),
            source_mtime_secs: secs,
            source_mtime_nanos: nanos,
        };
        assert_eq!(product.freshness(), Freshness::Fresh);

        // Same size, moved mtime: the signature no longer matches.
        let file = std::fs::File::options().write(true).open(&source).unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        drop(file);
        assert_eq!(product.freshness(), Freshness::Stale);

        // Gone: still readable, judged offline rather than stale.
        std::fs::remove_file(&source).unwrap();
        assert_eq!(product.freshness(), Freshness::Offline);
    }

    #[test]
    fn a_product_schema_or_sampler_bump_is_incompatible_not_stale() {
        let product = product(Path::new("/media/red.mp4"), 1, 2, 3);
        assert!(product.is_compatible(1));
        assert!(!product.is_compatible(2), "SAMPLE_VERSION moved");

        let mut future = product.clone();
        future.version = PRODUCT_VERSION + 1;
        assert!(!future.is_compatible(1));
        let error = future.validate().unwrap_err().to_string();
        assert!(error.contains("unsupported product version"), "{error}");
    }

    #[test]
    fn a_bad_product_address_is_refused_before_it_touches_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProductStore::at(&dir.path().join("nile.json"));
        for bad in [
            "",
            "product",
            "a1-",
            "a1-xyz",
            "a1-000000000000000",
            "a1-00000000000000000",
            "a1-000000000000000G",
            "a1-0000000000000000/../nile.json",
            "../a1-0000000000000000",
            "A1-0000000000000000",
        ] {
            let error = store.load(bad).unwrap_err();
            assert!(
                matches!(error, ProductError::Invalid(_)),
                "{bad:?}: {error}"
            );
        }
        assert!(
            store.load("a1-0000000000000000").is_err(),
            "a valid but absent address"
        );
    }

    fn subject_product() -> AnalysisProduct {
        AnalysisProduct {
            version: PRODUCT_VERSION,
            analyzer: AnalyzerInfo {
                name: "test-detector".to_string(),
                version: 1,
            },
            sample: ProductSample {
                version: SAMPLE_VERSION,
                key: "0123456789abcdef".to_string(),
                params: "fedcba9876543210".to_string(),
                source: SourceSignature {
                    source: "/media/a.mp4".to_string(),
                    source_arg: "/media/a.mp4".to_string(),
                    source_size: 1,
                    source_mtime_secs: 2,
                    source_mtime_nanos: 3,
                },
            },
            result: ProductResult::SubjectBoxes {
                grid: SampleGrid {
                    start: s(0),
                    fps: s(30),
                    width: 64,
                    height: 36,
                },
                labels: vec!["face".to_string()],
                floor_milli: 0,
                frames: vec![FrameBoxes {
                    index: 0,
                    boxes: vec![DetectedBox {
                        subject: SubjectKey {
                            label: "face".to_string(),
                            id: Some("spk-2".to_string()),
                        },
                        rect: BoxRect {
                            x: 312_500,
                            y: 333_333,
                            w: 125_000,
                            h: 166_667,
                        },
                        confidence_milli: 940,
                    }],
                }],
            },
        }
    }

    #[test]
    fn subject_boxes_validate_their_grid_order_and_bounds() {
        let product = subject_product();
        product.validate().expect("the fixture is valid");
        assert!(product.is_compatible(SAMPLE_VERSION));
        assert!(!product.is_compatible(SAMPLE_VERSION - 1));

        let invalid = |mutate: &dyn Fn(&mut AnalysisProduct), what: &str| {
            let mut product = subject_product();
            mutate(&mut product);
            assert!(product.validate().is_err(), "{what}");
        };
        invalid(
            &|product| {
                let ProductResult::SubjectBoxes { grid, .. } = &mut product.result else {
                    panic!("subject fixture")
                };
                grid.fps = s(0);
            },
            "zero fps",
        );
        invalid(
            &|product| {
                let ProductResult::SubjectBoxes { labels, .. } = &mut product.result else {
                    panic!("subject fixture")
                };
                labels.clear();
            },
            "no labels",
        );
        invalid(
            &|product| {
                let ProductResult::SubjectBoxes { labels, .. } = &mut product.result else {
                    panic!("subject fixture")
                };
                labels.insert(0, "zebra".to_string());
            },
            "labels not sorted",
        );
        invalid(
            &|product| {
                let ProductResult::SubjectBoxes { frames, .. } = &mut product.result else {
                    panic!("subject fixture")
                };
                frames[0].index = 2;
            },
            "frame index mismatch",
        );
        invalid(
            &|product| {
                let ProductResult::SubjectBoxes { frames, .. } = &mut product.result else {
                    panic!("subject fixture")
                };
                frames[0].boxes[0].rect.w = 0;
            },
            "zero-side box",
        );
        invalid(
            &|product| {
                let ProductResult::SubjectBoxes { frames, .. } = &mut product.result else {
                    panic!("subject fixture")
                };
                frames[0].boxes[0].rect.x = 900_000;
                frames[0].boxes[0].rect.w = 200_000;
            },
            "box leaves the picture",
        );
        invalid(
            &|product| {
                let ProductResult::SubjectBoxes { frames, .. } = &mut product.result else {
                    panic!("subject fixture")
                };
                frames[0].boxes[0].confidence_milli = 1001;
            },
            "confidence above 1000",
        );
        invalid(
            &|product| {
                let ProductResult::SubjectBoxes { frames, .. } = &mut product.result else {
                    panic!("subject fixture")
                };
                frames[0].boxes.push(DetectedBox {
                    subject: SubjectKey {
                        label: "face".to_string(),
                        id: None,
                    },
                    rect: BoxRect {
                        x: 0,
                        y: 0,
                        w: 1,
                        h: 1,
                    },
                    confidence_milli: 1000,
                });
            },
            "boxes not in canonical order",
        );
    }

    #[test]
    fn the_sample_grid_index_is_an_exact_rational_floor() {
        let grid = SampleGrid {
            start: s(0),
            fps: Seconds::new(30_000, 1001),
            width: 64,
            height: 36,
        };
        assert_eq!(grid.index_at(s(0)), Some(0));
        let frame_one = Seconds::new(1001, 30_000);
        assert_eq!(grid.index_at(frame_one), Some(1));
        // Just after the boundary is the next frame; just before it is not.
        let before = frame_one - Seconds::new(1, 1_000_000);
        assert_eq!(grid.index_at(before), Some(0));
        let after = frame_one + Seconds::new(1, 1_000_000);
        assert_eq!(grid.index_at(after), Some(1));
        assert_eq!(grid.index_at(-Seconds::from_integer(1)), None);
        let late = SampleGrid {
            start: s(10),
            ..grid
        };
        assert_eq!(late.index_at(s(9)), None);
        assert_eq!(late.index_at(s(10)), Some(0));
    }

    #[test]
    fn sorting_detected_boxes_matches_the_validation_order() {
        let mut boxes = vec![
            DetectedBox {
                subject: SubjectKey {
                    label: "person".to_string(),
                    id: None,
                },
                rect: BoxRect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 10,
                },
                confidence_milli: 900,
            },
            DetectedBox {
                subject: SubjectKey {
                    label: "face".to_string(),
                    id: Some("b".to_string()),
                },
                rect: BoxRect {
                    x: 20,
                    y: 20,
                    w: 10,
                    h: 10,
                },
                confidence_milli: 800,
            },
            DetectedBox {
                subject: SubjectKey {
                    label: "face".to_string(),
                    id: Some("a".to_string()),
                },
                rect: BoxRect {
                    x: 30,
                    y: 30,
                    w: 10,
                    h: 10,
                },
                confidence_milli: 1000,
            },
        ];
        sort_detected_boxes(&mut boxes);
        let keys: Vec<_> = boxes
            .iter()
            .map(|b| (b.subject.label.as_str(), b.subject.id.as_deref()))
            .collect();
        assert_eq!(
            keys,
            [("face", Some("a")), ("face", Some("b")), ("person", None)]
        );
    }

    #[test]
    fn product_validation_refuses_impossible_results() {
        let dir = tempfile::tempdir().unwrap();
        let mut bad = product_in(dir.path());
        bad.analyzer.name.clear();
        assert!(bad.validate().is_err());

        let mut bad = product_in(dir.path());
        bad.sample.key = "not-hex".to_string();
        assert!(bad.validate().is_err());

        let mut bad = product_in(dir.path());
        bad.sample.source.source_arg = "relative.mp4".to_string();
        assert!(bad.validate().is_err());

        let mut bad = product_in(dir.path());
        let ProductResult::MeanLuma { luma_milli } = &mut bad.result else {
            panic!("the fixture is mean luma")
        };
        luma_milli.clear();
        assert!(bad.validate().is_err());

        let mut bad = product_in(dir.path());
        let ProductResult::MeanLuma { luma_milli } = &mut bad.result else {
            panic!("the fixture is mean luma")
        };
        luma_milli[0] = 255_001;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn an_empty_product_store_loads_nothing_and_refuses_writes() {
        let store = ProductStore::empty();
        assert_eq!(store.dir(), None);
        assert!(matches!(
            store.load("a1-0000000000000000"),
            Err(ProductError::NotFound(_))
        ));
        let product = product(Path::new("/media/red.mp4"), 1, 2, 3);
        assert!(matches!(store.import(&product), Err(ProductError::NoStore)));
    }

    fn audio_product(dir: &Path) -> AnalysisProduct {
        let mut product = product_in(dir);
        product.analyzer = AnalyzerInfo {
            name: "audio-levels".to_string(),
            version: 1,
        };
        product.result = ProductResult::AudioLevels {
            start: s(0),
            sample_rate: 48_000,
            channels: 1,
            window_samples: 2,
            windows: vec![
                AudioWindowLevel {
                    start: 0,
                    samples: 2,
                    rms_millionths: 500_000,
                    peak_millionths: 500_000,
                },
                AudioWindowLevel {
                    start: 2,
                    samples: 1,
                    rms_millionths: 0,
                    peak_millionths: 0,
                },
            ],
        };
        product
    }

    #[test]
    fn audio_levels_round_trip_with_integer_amplitudes() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProductStore::at(&dir.path().join("nile.json"));
        let product = audio_product(dir.path());
        let hash = store.import(&product).unwrap();
        assert!(hash.starts_with("a1-"), "{hash}");
        assert_eq!(store.load(&hash).unwrap(), product);
        let text = String::from_utf8(product.canonical_bytes()).unwrap();
        assert!(text.contains("\"kind\":\"audio_levels\""), "{text}");
        assert!(text.contains("\"rms_millionths\":500000"), "{text}");
        assert!(text.contains("\"peak_millionths\":500000"), "{text}");
        assert!(!text.contains(".0,"), "amplitudes stay integers: {text}");
    }

    #[test]
    fn audio_levels_validation_refuses_non_tiling_windows_and_bad_levels() {
        let dir = tempfile::tempdir().unwrap();
        let product = audio_product(dir.path());
        product.validate().expect("the fixture is valid");

        let mutate = |change: &dyn Fn(&mut ProductResult)| {
            let mut bad = product.clone();
            change(&mut bad.result);
            bad.validate().unwrap_err().to_string()
        };

        let error = mutate(&|result| {
            let ProductResult::AudioLevels { windows, .. } = result else {
                panic!("audio")
            };
            windows[0].start = 1;
        });
        assert!(error.contains("starts at sample"), "{error}");

        let error = mutate(&|result| {
            let ProductResult::AudioLevels { windows, .. } = result else {
                panic!("audio")
            };
            windows[1].start = 3;
        });
        assert!(error.contains("previous one ended"), "{error}");

        let error = mutate(&|result| {
            let ProductResult::AudioLevels { window_samples, .. } = result else {
                panic!("audio")
            };
            *window_samples = 0;
        });
        assert!(error.contains("zero samples"), "{error}");

        let error = mutate(&|result| {
            let ProductResult::AudioLevels { windows, .. } = result else {
                panic!("audio")
            };
            windows[0].samples = 1;
        });
        assert!(error.contains("not last"), "{error}");

        let error = mutate(&|result| {
            let ProductResult::AudioLevels { windows, .. } = result else {
                panic!("audio")
            };
            windows[0].rms_millionths = 600_000;
        });
        assert!(error.contains("above peak"), "{error}");

        let error = mutate(&|result| {
            let ProductResult::AudioLevels { windows, .. } = result else {
                panic!("audio")
            };
            windows[0].peak_millionths = 1_000_001;
        });
        assert!(error.contains("outside"), "{error}");

        let error = mutate(&|result| {
            let ProductResult::AudioLevels { windows, .. } = result else {
                panic!("audio")
            };
            windows.clear();
        });
        assert!(error.contains("no audio windows"), "{error}");

        let error = mutate(&|result| {
            let ProductResult::AudioLevels { start, .. } = result else {
                panic!("audio")
            };
            *start = s(-1);
        });
        assert!(error.contains("before zero"), "{error}");
    }
}
