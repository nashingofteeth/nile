//! Analysis products that live beside the project: transcripts today.
//!
//! A transcript is **content-addressed**, not named: the words are serialised
//! in a canonical order, hashed with FNV-1a 64, and stored under
//! `.nile/<stem>/analysis/transcripts/t1-<16 hex>`. The `Expectation::WordsComplete`
//! payload names that hash, so a transcript can be re-imported, edited and
//! re-imported without an expectation silently describing different words: a
//! changed transcript gets a new address, while the old address keeps naming
//! exactly the old words. The store never deletes an address; the guarantee is
//! "the hash always means the same words", not "the old words disappear".
//!
//! The store is deliberately outside `nile.json`: words change on their own
//! schedule and a project file is a hand-editable document. [`TranscriptStore`]
//! keeps the root as `Option<PathBuf>`, so a report-only caller (a unit test,
//! a check with no project file) says "no store" with [`TranscriptStore::empty`]
//! instead of an empty string path that would accidentally resolve to
//! `.nile/...` in the working directory.
//!
//! No dependency buys the hash: FNV-1a 64 is a dozen lines, and the store's
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
    let Some(digits) = hash.strip_prefix("t1-") else {
        return false;
    };
    digits.len() == 16
        && digits
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
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
}
