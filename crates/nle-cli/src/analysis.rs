//! The analysis sidecar: an NDJSON host and the hidden analyzer.
//!
//! `nile analysis analyze` samples a window through
//! [`nle_ffmpeg::sampling::SampleCache`], creates a
//! staging directory under `cache/analysis/staging/`, and spawns this same
//! binary's hidden `analysis-analyzer` subcommand. The two speak one UTF-8
//! JSON object per line on the child's stdin/stdout — **not** a JSON-RPC
//! envelope — and packed bytes never travel on the wire, only paths.
//!
//! The analyzer reads the sample's metadata first (it is authoritative for
//! shape), cross-checks the request's window, and writes exactly one canonical
//! product into the staging directory. The host caps the control line and the
//! product, re-parses and re-hashes the product, checks its sample identity
//! against the entry it requested, and only then renames it into
//! `analysis/products/a1-…`. A failure leaves the sample cache and the project
//! untouched.
//!
//! The line reader is bounded: [`MAX_CONTROL_LINE`] is a hard cap, not a
//! truncation point. That is why it is hand-rolled over [`BufRead::fill_buf`]
//! instead of `read_until`: `read_until` would grow a `Vec` without a limit
//! and a hostile child could exhaust the host's memory before the cap is
//! checked.

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use nle_core::Seconds;
use nle_core::analysis::{
    AnalysisProduct, AnalyzerInfo, BoxRect, DetectedBox, FrameBoxes, ProductResult, ProductSample,
    ProductStore, SampleGrid, SourceSignature, SubjectKey, sort_detected_boxes,
};
use nle_ffmpeg::sampling::{
    FrameMode, FrameSpec, MAX_SAMPLE_BYTES, SAMPLE_VERSION, SampleData, SampleEntry, SampleMeta,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

/// The protocol version both sides must agree on.
const PROTOCOL: u32 = 2;

/// One control line may not exceed this many bytes. A longer line is fatal —
/// never truncated, because half a JSON object is not a smaller request.
pub(crate) const MAX_CONTROL_LINE: usize = 1024 * 1024;

/// The host's default cap for one staged product.
pub(crate) const DEFAULT_MAX_PRODUCT_BYTES: u64 = 256 * 1024 * 1024;

/// The host's sample budget. The sampler now has its own byte/frame caps;
/// this keeps the CLI's refusal message before the cache and ffmpeg are
/// touched, and is the same number so the two cannot drift.
pub(crate) const MAX_SAMPLE_BUDGET: u64 = MAX_SAMPLE_BYTES;

/// The one file an analyzer may report and the host may adopt.
const PRODUCT_FILE: &str = "product.json";

/// What the bundled child calls itself in `hello`. The product identity is
/// per task (mean luma keeps its stable name so old addresses do not move).
const BUILTIN_NAME: &str = "nile-builtin";
const BUILTIN_VERSION: u32 = 2;

/// The mean-luma product identity. Stable: changing it would move every
/// existing `a1-…` address.
const MEAN_LUMA_NAME: &str = "mean-luma";
const MEAN_LUMA_VERSION: u32 = 1;

/// The bundled deterministic subject-box identity.
const SUBJECT_NAME: &str = "color-boxes";
const SUBJECT_VERSION: u32 = 1;

/// What one analyze call asks the child to compute. The wire tag is the same
/// name the hello advertises in `tasks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Task {
    MeanLuma,
    SubjectBoxes {
        labels: Vec<String>,
        floor_milli: u32,
    },
}

impl Task {
    pub(crate) fn wire_name(&self) -> &'static str {
        match self {
            Task::MeanLuma => "mean_luma",
            Task::SubjectBoxes { .. } => "subject_boxes",
        }
    }

    /// The product identity the bundled analyzer writes for this task.
    fn builtin_analyzer(&self) -> AnalyzerInfo {
        match self {
            Task::MeanLuma => AnalyzerInfo {
                name: MEAN_LUMA_NAME.to_string(),
                version: MEAN_LUMA_VERSION,
            },
            Task::SubjectBoxes { .. } => AnalyzerInfo {
                name: SUBJECT_NAME.to_string(),
                version: SUBJECT_VERSION,
            },
        }
    }
}

/// Where the analyzer process comes from. The external path is explicit and
/// absolute: no PATH search, no `nile.json` entry can name an executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Analyzer {
    Bundled,
    External(PathBuf),
}

/// The wire `error.code` values. Unknown codes are a protocol error, not a
/// message to guess at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorCode {
    BadRequest,
    Unsupported,
    Io,
    CorruptInput,
    Internal,
}

impl ErrorCode {
    fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::Io => "io",
            ErrorCode::CorruptInput => "corrupt_input",
            ErrorCode::Internal => "internal",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "bad_request" => Some(ErrorCode::BadRequest),
            "unsupported" => Some(ErrorCode::Unsupported),
            "io" => Some(ErrorCode::Io),
            "corrupt_input" => Some(ErrorCode::CorruptInput),
            "internal" => Some(ErrorCode::Internal),
            _ => None,
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum SidecarError {
    #[error("analysis io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid analyzer request: {0}")]
    BadRequest(String),
    #[error("unsupported analyzer request: {0}")]
    Unsupported(String),
    #[error("corrupt analyzer input: {0}")]
    CorruptInput(String),
    #[error("invalid analyzer product: {0}")]
    Product(String),
    #[error("analyzer protocol: {0}")]
    Protocol(String),
    #[error("analyzer is too large: {0}")]
    RequestTooLarge(String),
    #[error("analyzer line is {bytes} bytes (limit {limit})")]
    OversizedLine { bytes: usize, limit: usize },
    #[error("analyzer failed ({code:?}): {message}")]
    Analyzer { code: ErrorCode, message: String },
}

impl SidecarError {
    /// The wire code an analyzer-side failure maps to. Host-only failures are
    /// `internal`: they are our bug (or a broken child), not the request's.
    fn code(&self) -> ErrorCode {
        match self {
            SidecarError::BadRequest(_) | SidecarError::RequestTooLarge(_) => ErrorCode::BadRequest,
            SidecarError::Unsupported(_) => ErrorCode::Unsupported,
            SidecarError::CorruptInput(_) => ErrorCode::CorruptInput,
            SidecarError::Io(_) => ErrorCode::Io,
            SidecarError::Analyzer { code, .. } => *code,
            SidecarError::Product(_)
            | SidecarError::Protocol(_)
            | SidecarError::OversizedLine { .. } => ErrorCode::Internal,
        }
    }
}

// ------------------------------------------------------------ host: the CLI

/// The analysis sample cache inside a project's state folder, mirroring
/// `proxy::proxy_cache_dir`: `.nile/<stem>/cache/analysis`.
pub(crate) fn sample_cache_dir(project_path: &Path) -> PathBuf {
    nle_core::state::cache_dir(project_path).join("analysis")
}

/// Prune this project's packed samples and stale per-run staging directories.
///
/// A staging directory is named `<pid>-<seq>` and only ever holds a child's
/// throwaway output. [`StageDir`] removes its own run, but a killed host leaves
/// one behind; this sweep is the only other remover. `analysis/products/` is
/// state, not cache (`cache/analysis` and `analysis/` are siblings), so it is
/// never reached from here.
pub(crate) fn prune(
    project_path: &Path,
    max_age: Duration,
    dry_run: bool,
) -> Result<crate::proxy::PruneReport, crate::proxy::ProxyError> {
    let dir = sample_cache_dir(project_path);
    let now = SystemTime::now();
    let mut report = crate::proxy::prune_at(&dir, max_age, now, dry_run)?;
    prune_staging(&dir.join("staging"), max_age, now, dry_run, &mut report)?;
    Ok(report)
}

/// Sweep `staging/<pid>-<seq>/` directories older than `max_age`. Only that
/// exact shape is a candidate: a name a future caller puts there is not ours.
fn prune_staging(
    staging: &Path,
    max_age: Duration,
    now: SystemTime,
    dry_run: bool,
    report: &mut crate::proxy::PruneReport,
) -> Result<(), crate::proxy::ProxyError> {
    let entries = match std::fs::read_dir(staging) {
        Ok(entries) => entries,
        // No staging directory yet is not an error: nothing to prune.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_stage_name(name) {
            continue;
        }
        // `symlink_metadata`: a link named like a stage must not be followed.
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.file_type().is_dir() {
            continue;
        }
        let age = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .unwrap_or_default();
        let bytes = dir_size(&entry.path());
        if age <= max_age {
            report.kept += 1;
            report.kept_bytes += bytes;
            continue;
        }
        if !dry_run && std::fs::remove_dir_all(entry.path()).is_err() {
            report.failed += 1;
            continue;
        }
        report.removed += 1;
        report.bytes += bytes;
    }
    Ok(())
}

/// `<pid>-<seq>`, exactly what [`StageDir::create`] makes: two non-empty
/// decimal numbers and one hyphen.
fn is_stage_name(name: &str) -> bool {
    let Some((pid, seq)) = name.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && !seq.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
        && seq.bytes().all(|b| b.is_ascii_digit())
}

/// Bytes a staging directory occupies. Plain files only, and never through a
/// symlink: a link is not this run's output.
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let meta = std::fs::symlink_metadata(entry.path()).ok()?;
            meta.file_type().is_file().then_some(meta.len())
        })
        .sum()
}

/// Refuse a sample whose packed payload cannot fit the host's memory budget.
///
/// This is deliberately checked before
/// [`nle_ffmpeg::sampling::SampleCache::frames`]: the sampler
/// should not spawn ffmpeg for a request we are about to refuse.
pub(crate) fn check_sample_budget(spec: &FrameSpec) -> Result<(), SidecarError> {
    let stride = (spec.width as u64)
        .checked_mul(spec.height as u64)
        .and_then(|n| n.checked_mul(3))
        .ok_or_else(|| SidecarError::RequestTooLarge("geometry overflows".to_string()))?;
    let frames = spec.frame_count();
    let bytes = frames
        .checked_mul(stride)
        .ok_or_else(|| SidecarError::RequestTooLarge("the sample length overflows".to_string()))?;
    if frames == 0 || bytes == 0 {
        return Err(SidecarError::BadRequest(
            "the window is shorter than one frame at this fps".to_string(),
        ));
    }
    if bytes > MAX_SAMPLE_BUDGET {
        return Err(SidecarError::RequestTooLarge(format!(
            "{frames} frames at {}×{} need {bytes} bytes (limit {MAX_SAMPLE_BUDGET})",
            spec.width, spec.height
        )));
    }
    Ok(())
}

/// A staging directory for one run, removed when it goes out of scope.
///
/// The `Drop` guard is what makes failure paths leave nothing behind: the
/// child only ever writes here and the host moves the one validated product
/// out before the directory disappears.
pub(crate) struct StageDir {
    path: PathBuf,
}

/// Separates staging directories created by this process.
static STAGE_SEQ: AtomicU64 = AtomicU64::new(0);

impl StageDir {
    /// Create `cache/analysis/staging/<pid>-<seq>/`.
    pub(crate) fn create(cache_root: &Path) -> Result<Self, SidecarError> {
        let cache_root = std::path::absolute(cache_root)?;
        let staging = cache_root.join("staging");
        std::fs::create_dir_all(&staging)?;
        let pid = std::process::id();
        for _ in 0..16 {
            let seq = STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = staging.join(format!("{pid}-{seq}"));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(SidecarError::Protocol(
            "could not create a fresh staging directory".to_string(),
        ))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StageDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Sample the window, run the analyzer on the packed sample, and adopt the
/// resulting product. Returns the `a1-…` address.
pub(crate) fn run_sidecar(
    spec: &FrameSpec,
    entry: &SampleEntry,
    stage: &StageDir,
    store: &ProductStore,
    task: &Task,
    analyzer: &Analyzer,
) -> Result<String, SidecarError> {
    let mut command = match analyzer {
        Analyzer::Bundled => {
            let exe = std::path::absolute(std::env::current_exe()?)?;
            let mut command = Command::new(exe);
            command.arg("analysis-analyzer");
            command
        }
        Analyzer::External(path) => {
            if !path.is_absolute() {
                return Err(SidecarError::BadRequest(format!(
                    "analyzer path is relative: {}",
                    path.display()
                )));
            }
            Command::new(path)
        }
    };
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let outcome = converse(&mut child, spec, entry, stage.path(), store, task);
    if outcome.is_err() {
        // A protocol failure can leave the child waiting for another line.
        let _ = child.kill();
    }
    let status = child.wait()?;
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    match outcome {
        Ok(hash) if status.success() => Ok(hash),
        Ok(_) => Err(SidecarError::Protocol(format!(
            "analyzer exited with {status}{}",
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {}", stderr.trim())
            }
        ))),
        Err(error) => Err(error),
    }
}

fn converse(
    child: &mut Child,
    spec: &FrameSpec,
    entry: &SampleEntry,
    stage: &Path,
    store: &ProductStore,
    task: &Task,
) -> Result<String, SidecarError> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| SidecarError::Protocol("analyzer has no stdin".to_string()))?;
    let mut stdout = BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| SidecarError::Protocol("analyzer has no stdout".to_string()))?,
    );

    write_control(
        &mut stdin,
        &Hello {
            id: 1,
            method: "hello",
            protocol: PROTOCOL,
        },
    )?;
    let hello: HelloOk = expect_ok(read_response(&mut stdout)?, 1)?;
    if hello.protocol != PROTOCOL {
        return Err(SidecarError::Protocol(format!(
            "analyzer speaks protocol {} (host speaks {PROTOCOL})",
            hello.protocol
        )));
    }
    if hello.analyzer.name.is_empty() || hello.analyzer.version == 0 {
        return Err(SidecarError::Protocol(
            "analyzer hello has no identity".to_string(),
        ));
    }
    if !hello.tasks.iter().any(|name| name == task.wire_name()) {
        return Err(SidecarError::Unsupported(format!(
            "analyzer does not advertise {} (tasks: {})",
            task.wire_name(),
            hello.tasks.join(", ")
        )));
    }

    let data = wire_path(entry.data_path())?;
    let meta = wire_path(entry.meta_path())?;
    let stage_wire = wire_path(stage)?;
    let request = wire_request(spec);
    write_control(
        &mut stdin,
        &Analyze {
            id: 2,
            method: "analyze",
            input: InputRef {
                kind: "frames",
                sample: entry.key(),
                data: &data,
                meta: &meta,
            },
            request,
            task: wire_task(task),
            stage: &stage_wire,
            limits: Limits {
                max_product_bytes: DEFAULT_MAX_PRODUCT_BYTES,
            },
        },
    )?;
    let ok: AnalyzeOk = expect_ok(read_response(&mut stdout)?, 2)?;
    let hash = adopt_product(store, entry, stage, &ok, spec, task)?;

    write_control(
        &mut stdin,
        &Shutdown {
            id: 3,
            method: "shutdown",
        },
    )?;
    let bye: ShutdownOk = expect_ok(read_response(&mut stdout)?, 3)?;
    if !bye.shutdown {
        return Err(SidecarError::Protocol(
            "analyzer did not acknowledge shutdown".to_string(),
        ));
    }
    Ok(hash)
}

/// Validate the one staged product, then move it into the store.
fn adopt_product(
    store: &ProductStore,
    entry: &SampleEntry,
    stage: &Path,
    ok: &AnalyzeOk,
    spec: &FrameSpec,
    task: &Task,
) -> Result<String, SidecarError> {
    if !is_product_hash(&ok.hash) {
        return Err(SidecarError::Product(format!(
            "not a product hash: {:?}",
            ok.hash
        )));
    }
    if !plain_file_name(&ok.stage) {
        return Err(SidecarError::Product(format!(
            "the analyzer named more than one plain file: {:?}",
            ok.stage
        )));
    }
    let staged = stage.join(&ok.stage);
    if staged.parent() != Some(stage) {
        return Err(SidecarError::Product(
            "the staged product escapes its staging directory".to_string(),
        ));
    }
    let meta = std::fs::symlink_metadata(&staged)?;
    if !meta.file_type().is_file() {
        return Err(SidecarError::Product(
            "the staged product is not a regular file".to_string(),
        ));
    }
    if meta.len() > DEFAULT_MAX_PRODUCT_BYTES {
        return Err(SidecarError::Product(format!(
            "the staged product is {} bytes (limit {DEFAULT_MAX_PRODUCT_BYTES})",
            meta.len()
        )));
    }
    let raw = std::fs::read(&staged)?;
    let product: AnalysisProduct = serde_json::from_slice(&raw)
        .map_err(|error| SidecarError::Product(format!("product json: {error}")))?;
    product
        .validate()
        .map_err(|error| SidecarError::Product(error.to_string()))?;

    // The address is content, not a name: check the analyzer's report against
    // the bytes it actually wrote.
    let actual = product.hash();
    if actual != ok.hash {
        return Err(SidecarError::Product(format!(
            "the analyzer reported {} but its bytes hash to {actual}",
            ok.hash
        )));
    }
    let mut canonical = product.canonical_bytes();
    canonical.push(b'\n');
    if raw != canonical && raw != product.canonical_bytes() {
        return Err(SidecarError::Product(
            "the staged product is not canonical JSON".to_string(),
        ));
    }

    // The product must describe the sample the host asked for, not another
    // cache entry the child happened to find.
    if product.sample.version != entry.version()
        || product.sample.key != entry.key()
        || product.sample.params != entry.params()
    {
        return Err(SidecarError::Product(
            "the product names a different sample".to_string(),
        ));
    }
    let meta_bytes = std::fs::read(entry.meta_path())?;
    let sample_meta: SampleMeta = serde_json::from_slice(&meta_bytes)
        .map_err(|error| SidecarError::Product(format!("sample metadata: {error}")))?;
    require_result_shape(&product, &sample_meta.data, spec, task)?;
    let recorded = &product.sample.source;
    if recorded.source != sample_meta.source
        || recorded.source_arg != sample_meta.source_arg
        || recorded.source_size != sample_meta.source_size
        || recorded.source_mtime_secs != sample_meta.source_mtime_secs
        || recorded.source_mtime_nanos != sample_meta.source_mtime_nanos
    {
        return Err(SidecarError::Product(
            "the product records a different source signature".to_string(),
        ));
    }

    store
        .adopt(&product, &staged, &ok.hash)
        .map_err(|error| SidecarError::Product(error.to_string()))
}

/// Metadata, not the product, says how many frames were packed. A result with
/// a different length or a grid that disagrees with the request is a
/// truncated/padded analysis, never a "shorter sample": refuse it before the
/// store sees it.
fn require_result_shape(
    product: &AnalysisProduct,
    data: &SampleData,
    spec: &FrameSpec,
    task: &Task,
) -> Result<(), SidecarError> {
    let SampleData::Frames {
        width,
        height,
        frames,
        ..
    } = data
    else {
        return Err(SidecarError::Product(
            "the analyzer needs a frame sample".to_string(),
        ));
    };
    match (task, &product.result) {
        (Task::MeanLuma, ProductResult::MeanLuma { luma_milli }) => {
            if luma_milli.len() as u64 != *frames {
                return Err(SidecarError::Product(format!(
                    "the product has {} frame results but the sample packed {frames}",
                    luma_milli.len()
                )));
            }
        }
        (
            Task::SubjectBoxes { labels, .. },
            ProductResult::SubjectBoxes {
                grid,
                labels: declared,
                frames: boxes,
                ..
            },
        ) => {
            let FrameMode::Grid { start, fps, .. } = &spec.mode else {
                return Err(SidecarError::Unsupported(
                    "subject boxes need a grid sample".to_string(),
                ));
            };
            if grid.start != *start
                || grid.fps != *fps
                || grid.width != *width
                || grid.height != *height
            {
                return Err(SidecarError::Product(
                    "the product's grid disagrees with the sample".to_string(),
                ));
            }
            if declared != labels {
                return Err(SidecarError::Product(format!(
                    "the product declares labels {declared:?}, request names {labels:?}"
                )));
            }
            if boxes.len() as u64 != *frames {
                return Err(SidecarError::Product(format!(
                    "the product has {} frame boxes but the sample packed {frames}",
                    boxes.len()
                )));
            }
            for (position, frame) in boxes.iter().enumerate() {
                if frame.index != position as u64 {
                    return Err(SidecarError::Product(format!(
                        "frame {position} records index {}",
                        frame.index
                    )));
                }
            }
        }
        (Task::MeanLuma, other) => {
            return Err(SidecarError::Product(format!(
                "the analyzer returned {} for mean luma",
                product_kind(other)
            )));
        }
        (Task::SubjectBoxes { .. }, other) => {
            return Err(SidecarError::Product(format!(
                "the analyzer returned {} for subject boxes",
                product_kind(other)
            )));
        }
    }
    Ok(())
}

fn product_kind(result: &ProductResult) -> &'static str {
    match result {
        ProductResult::MeanLuma { .. } => "mean_luma",
        ProductResult::SubjectBoxes { .. } => "subject_boxes",
    }
}

fn is_product_hash(hash: &str) -> bool {
    nle_core::analysis::is_product_hash(hash)
}

/// One plain path component: no separator, no absolute path, no `.`/`..`.
fn plain_file_name(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none()
}

/// Make a cache path absolute before it goes on the wire: the child's cwd is
/// not necessarily the client's, and the analyzer refuses relative paths.
fn wire_path(path: &Path) -> Result<String, SidecarError> {
    let absolute = std::path::absolute(path)?;
    absolute
        .into_os_string()
        .into_string()
        .map_err(|_| SidecarError::BadRequest(format!("path is not UTF-8: {}", path.display())))
}

fn rational(value: &Seconds) -> String {
    format!("{}/{}", value.numer(), value.denom())
}

// ------------------------------------------------------- analyzer (child)

/// Run the hidden `analysis-analyzer` subcommand: read NDJSON requests from
/// stdin, answer one line each on stdout, and write a product into the host's
/// staging directory.
pub(crate) fn run_analyzer() -> Result<(), SidecarError> {
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut output = BufWriter::new(stdout.lock());

    let mut hello_seen = false;
    let mut last_id = 0_u64;
    while let Some(line) = read_line_limited(&mut input, MAX_CONTROL_LINE)? {
        let control = match parse_control(&line) {
            Ok(control) => control,
            Err(error) => {
                write_error(&mut output, error.id, &error.error)?;
                // Before the handshake there is no protocol to continue. After
                // it, one bad line is answered and the next may be fine.
                if !hello_seen {
                    return Ok(());
                }
                continue;
            }
        };
        let id = control.id();
        if id <= last_id {
            write_error(
                &mut output,
                id,
                &SidecarError::BadRequest(format!("id {id} is not increasing after {last_id}")),
            )?;
            continue;
        }
        last_id = id;

        match control {
            Control::Hello { id, protocol } => {
                if hello_seen {
                    write_error(
                        &mut output,
                        id,
                        &SidecarError::BadRequest("hello was already exchanged".to_string()),
                    )?;
                    continue;
                }
                if protocol != PROTOCOL {
                    write_error(
                        &mut output,
                        id,
                        &SidecarError::Unsupported(format!(
                            "protocol {protocol} (this analyzer speaks {PROTOCOL})"
                        )),
                    )?;
                    return Ok(());
                }
                write_ok(
                    &mut output,
                    id,
                    &serde_json::json!({
                        "protocol": PROTOCOL,
                        "analyzer": { "name": BUILTIN_NAME, "version": BUILTIN_VERSION },
                        "tasks": ["mean_luma", "subject_boxes"],
                    }),
                )?;
                hello_seen = true;
            }
            Control::Analyze { id, request } => {
                if !hello_seen {
                    write_error(
                        &mut output,
                        id,
                        &SidecarError::BadRequest("analyze before hello".to_string()),
                    )?;
                    continue;
                }
                match analyze_request(&request) {
                    Ok(product) => {
                        write_ok(
                            &mut output,
                            id,
                            &serde_json::json!({
                                "hash": product.hash(),
                                "stage": PRODUCT_FILE,
                            }),
                        )?;
                    }
                    Err(error) => write_error(&mut output, id, &error)?,
                }
            }
            Control::Shutdown { id } => {
                if !hello_seen {
                    write_error(
                        &mut output,
                        id,
                        &SidecarError::BadRequest("shutdown before hello".to_string()),
                    )?;
                    continue;
                }
                write_ok(&mut output, id, &serde_json::json!({ "shutdown": true }))?;
                return Ok(());
            }
        }
    }
    Ok(())
}

/// A control message, already parsed and method-checked.
#[derive(Debug)]
enum Control {
    Hello {
        id: u64,
        protocol: u32,
    },
    Analyze {
        id: u64,
        request: Box<AnalyzeRequest>,
    },
    Shutdown {
        id: u64,
    },
}

impl Control {
    fn id(&self) -> u64 {
        match self {
            Control::Hello { id, .. } | Control::Analyze { id, .. } | Control::Shutdown { id } => {
                *id
            }
        }
    }
}

#[derive(Debug)]
struct RequestError {
    id: u64,
    error: SidecarError,
}

fn parse_control(line: &[u8]) -> Result<Control, RequestError> {
    let value: serde_json::Value = serde_json::from_slice(line).map_err(|error| RequestError {
        id: 0,
        error: SidecarError::BadRequest(format!("line is not JSON: {error}")),
    })?;
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let method = value
        .get("method")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| RequestError {
            id,
            error: SidecarError::BadRequest("missing method".to_string()),
        })?;
    if id == 0 {
        return Err(RequestError {
            id,
            error: SidecarError::BadRequest("id must be positive".to_string()),
        });
    }
    match method {
        "hello" => serde_json::from_value::<HelloRequest>(value)
            .map(|request| Control::Hello {
                id,
                protocol: request.protocol,
            })
            .map_err(|error| RequestError {
                id,
                error: SidecarError::BadRequest(format!("hello: {error}")),
            }),
        "analyze" => serde_json::from_value::<AnalyzeRequest>(value)
            .map(|request| Control::Analyze {
                id,
                request: Box::new(request),
            })
            .map_err(|error| RequestError {
                id,
                error: SidecarError::BadRequest(format!("analyze: {error}")),
            }),
        "shutdown" => serde_json::from_value::<ShutdownRequest>(value)
            .map(|_| Control::Shutdown { id })
            .map_err(|error| RequestError {
                id,
                error: SidecarError::BadRequest(format!("shutdown: {error}")),
            }),
        other => Err(RequestError {
            id,
            error: SidecarError::BadRequest(format!("unknown method {other:?}")),
        }),
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HelloRequest {
    #[allow(dead_code)]
    id: u64,
    #[allow(dead_code)]
    method: String,
    protocol: u32,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ShutdownRequest {
    #[allow(dead_code)]
    id: u64,
    #[allow(dead_code)]
    method: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AnalyzeRequest {
    #[allow(dead_code)]
    id: u64,
    #[allow(dead_code)]
    method: String,
    input: InputRefOwned,
    request: FrameRequestIn,
    task: TaskIn,
    stage: String,
    limits: LimitsIn,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskIn {
    kind: String,
    #[serde(default)]
    labels: Option<Vec<String>>,
    #[serde(default)]
    floor_milli: Option<u32>,
}

impl TaskIn {
    fn parse(&self) -> Result<Task, SidecarError> {
        match self.kind.as_str() {
            "mean_luma" => {
                if self.labels.is_some() || self.floor_milli.is_some() {
                    return Err(SidecarError::BadRequest(
                        "mean_luma does not take labels/floor_milli".to_string(),
                    ));
                }
                Ok(Task::MeanLuma)
            }
            "subject_boxes" => {
                let mut labels = self.labels.clone().ok_or_else(|| {
                    SidecarError::BadRequest("subject_boxes needs labels".to_string())
                })?;
                labels.retain(|label| !label.trim().is_empty());
                labels.sort();
                labels.dedup();
                if labels.is_empty() {
                    return Err(SidecarError::BadRequest(
                        "subject_boxes names no labels".to_string(),
                    ));
                }
                let floor_milli = self.floor_milli.unwrap_or(0);
                if floor_milli > 1000 {
                    return Err(SidecarError::BadRequest(format!(
                        "floor_milli {floor_milli} is outside 0..=1000"
                    )));
                }
                Ok(Task::SubjectBoxes {
                    labels,
                    floor_milli,
                })
            }
            other => Err(SidecarError::Unsupported(format!(
                "task {other:?} (this analyzer knows mean_luma and subject_boxes)"
            ))),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InputRefOwned {
    kind: String,
    sample: String,
    data: String,
    meta: String,
}

/// The wire DTO: stringly and permissive so the tag can be read first. The
/// typed [`FrameRequest`] is what the rest of the analyzer sees, so a mode
/// cannot carry fields it does not use.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FrameRequestIn {
    mode: String,
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    fps: Option<String>,
    #[serde(default)]
    rate: Option<String>,
    #[serde(default)]
    indices: Option<Vec<u64>>,
    width: u32,
    height: u32,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsIn {
    max_product_bytes: u64,
}

#[derive(Debug)]
struct FrameRequest {
    mode: FrameMode,
    width: u32,
    height: u32,
}

impl FrameRequest {
    fn parse(request: &FrameRequestIn) -> Result<Self, SidecarError> {
        let mode = match request.mode.as_str() {
            "grid" => {
                if request.rate.is_some() || request.indices.is_some() {
                    return Err(SidecarError::BadRequest(
                        "a grid request must not carry rate/indices".to_string(),
                    ));
                }
                let mode = FrameMode::Grid {
                    start: parse_rational(required(&request.start, "start")?, "start")?,
                    duration: parse_rational(required(&request.duration, "duration")?, "duration")?,
                    fps: parse_rational(required(&request.fps, "fps")?, "fps")?,
                };
                mode.validate()
                    .map_err(|error| SidecarError::BadRequest(error.to_string()))?;
                mode
            }
            "indices" => {
                if request.start.is_some() || request.duration.is_some() || request.fps.is_some() {
                    return Err(SidecarError::BadRequest(
                        "an index request derives its window from rate/indices".to_string(),
                    ));
                }
                let indices = request.indices.clone().ok_or_else(|| {
                    SidecarError::BadRequest("indices is required for an index request".to_string())
                })?;
                let mode = FrameMode::Indices {
                    rate: parse_rational(required(&request.rate, "rate")?, "rate")?,
                    indices,
                };
                mode.validate()
                    .map_err(|error| SidecarError::BadRequest(error.to_string()))?;
                mode
            }
            other => {
                return Err(SidecarError::Unsupported(format!(
                    "mode {other:?} (this analyzer samples a grid or exact source indices)"
                )));
            }
        };
        if request.width == 0 || request.height == 0 {
            return Err(SidecarError::BadRequest(
                "width and height must be non-zero".to_string(),
            ));
        }
        Ok(Self {
            mode,
            width: request.width,
            height: request.height,
        })
    }
}

fn required<'a>(value: &'a Option<String>, what: &str) -> Result<&'a str, SidecarError> {
    value
        .as_deref()
        .ok_or_else(|| SidecarError::BadRequest(format!("{what} is required for this mode")))
}

fn parse_rational(value: &str, what: &str) -> Result<Seconds, SidecarError> {
    let Some((numerator, denominator)) = value.split_once('/') else {
        return Err(SidecarError::BadRequest(format!(
            "{what} must be a rational like \"30000/1001\", got {value:?}"
        )));
    };
    let numerator: i64 = numerator
        .parse()
        .map_err(|_| SidecarError::BadRequest(format!("{what} numerator: {value:?}")))?;
    let denominator: i64 = denominator
        .parse()
        .map_err(|_| SidecarError::BadRequest(format!("{what} denominator: {value:?}")))?;
    if denominator == 0 {
        return Err(SidecarError::BadRequest(format!(
            "{what} denominator is zero"
        )));
    }
    Ok(Seconds::new(numerator, denominator))
}

fn analyze_request(request: &AnalyzeRequest) -> Result<AnalysisProduct, SidecarError> {
    if request.input.kind != "frames" {
        return Err(SidecarError::Unsupported(format!(
            "sample kind {:?} (mean luma reads frames)",
            request.input.kind
        )));
    }
    if !is_hex16(&request.input.sample) {
        return Err(SidecarError::BadRequest(
            "sample is not a 16-hex key".to_string(),
        ));
    }
    let meta_path = require_absolute(&request.input.meta, "meta")?;
    let data_path = require_absolute(&request.input.data, "data")?;
    let stage = require_absolute(&request.stage, "stage")?;
    let requested = FrameRequest::parse(&request.request)?;
    let task = request.task.parse()?;

    // Metadata first: it is authoritative for shape, the request is the
    // window. A disagreement is corrupt input, not a shorter sample.
    let meta_bytes = std::fs::read(&meta_path)?;
    let meta: SampleMeta = serde_json::from_slice(&meta_bytes)
        .map_err(|error| SidecarError::CorruptInput(format!("metadata: {error}")))?;
    if meta.version != SAMPLE_VERSION {
        return Err(SidecarError::CorruptInput(format!(
            "sample version {} (this analyzer reads {SAMPLE_VERSION})",
            meta.version
        )));
    }
    if meta.key != request.input.sample {
        return Err(SidecarError::CorruptInput(format!(
            "metadata names {} but the request names {}",
            meta.key, request.input.sample
        )));
    }
    let SampleData::Frames {
        width,
        height,
        frames,
        times,
        indices,
    } = &meta.data
    else {
        return Err(SidecarError::Unsupported(
            "audio samples are not mean-luma input".to_string(),
        ));
    };
    if *width != requested.width || *height != requested.height {
        return Err(SidecarError::CorruptInput(format!(
            "metadata is {width}×{height}, request is {}×{}",
            requested.width, requested.height
        )));
    }
    let wanted = requested.mode.frame_count();
    if *frames == 0 || *frames > wanted {
        return Err(SidecarError::CorruptInput(format!(
            "metadata has {frames} frames, request allows at most {wanted}"
        )));
    }
    // Times and source indices must be the ones the request names; a sample
    // from another clock is not the one this product would describe.
    let expected_times = requested
        .mode
        .times(*frames as usize)
        .ok_or_else(|| SidecarError::CorruptInput("frame times overflow".to_string()))?;
    if *times != expected_times {
        return Err(SidecarError::CorruptInput(
            "metadata frame times disagree with the request".to_string(),
        ));
    }
    match &requested.mode {
        FrameMode::Grid { .. } => {
            if indices.is_some() {
                return Err(SidecarError::CorruptInput(
                    "a grid sample records source frame indices".to_string(),
                ));
            }
        }
        FrameMode::Indices {
            indices: requested_indices,
            ..
        } => {
            if *frames as usize != requested_indices.len() {
                return Err(SidecarError::CorruptInput(format!(
                    "metadata packs {frames} of {} requested frames",
                    requested_indices.len()
                )));
            }
            if indices.as_deref() != Some(requested_indices.as_slice()) {
                return Err(SidecarError::CorruptInput(
                    "metadata source indices disagree with the request".to_string(),
                ));
            }
        }
    }
    if meta.bytes > MAX_SAMPLE_BYTES {
        return Err(SidecarError::CorruptInput(format!(
            "the sample is {} bytes (limit {MAX_SAMPLE_BYTES})",
            meta.bytes
        )));
    }
    let stride = (*width as u64)
        .checked_mul(*height as u64)
        .and_then(|n| n.checked_mul(3))
        .ok_or_else(|| SidecarError::CorruptInput("frame size overflows".to_string()))?;
    let expected = frames
        .checked_mul(stride)
        .ok_or_else(|| SidecarError::CorruptInput("payload length overflows".to_string()))?;
    if meta.bytes != expected {
        return Err(SidecarError::CorruptInput(format!(
            "metadata claims {} bytes, shape needs {expected}",
            meta.bytes
        )));
    }
    let data = std::fs::read(&data_path)?;
    if data.len() as u64 != expected {
        return Err(SidecarError::CorruptInput(format!(
            "packed payload is {} bytes, expected {expected}",
            data.len()
        )));
    }

    let result = match &task {
        Task::MeanLuma => ProductResult::MeanLuma {
            luma_milli: mean_luma(&data, *width, *height, *frames)?,
        },
        Task::SubjectBoxes {
            labels,
            floor_milli,
        } => {
            let FrameMode::Grid { start, fps, .. } = &requested.mode else {
                return Err(SidecarError::Unsupported(
                    "subject boxes need a grid sample; sample a window instead of exact indices"
                        .to_string(),
                ));
            };
            ProductResult::SubjectBoxes {
                grid: SampleGrid {
                    start: *start,
                    fps: *fps,
                    width: *width,
                    height: *height,
                },
                labels: labels.clone(),
                floor_milli: *floor_milli,
                frames: detect_subject_boxes(
                    &data,
                    *width,
                    *height,
                    *frames,
                    labels,
                    *floor_milli,
                )?,
            }
        }
    };
    let product = AnalysisProduct {
        version: nle_core::analysis::PRODUCT_VERSION,
        analyzer: task.builtin_analyzer(),
        sample: ProductSample {
            version: meta.version,
            key: meta.key.clone(),
            params: meta.params.clone(),
            source: SourceSignature {
                source: meta.source.clone(),
                source_arg: meta.source_arg.clone(),
                source_size: meta.source_size,
                source_mtime_secs: meta.source_mtime_secs,
                source_mtime_nanos: meta.source_mtime_nanos,
            },
        },
        result,
    };
    product
        .validate()
        .map_err(|error| SidecarError::Product(error.to_string()))?;
    let mut bytes = product.canonical_bytes();
    bytes.push(b'\n');
    if bytes.len() as u64 > request.limits.max_product_bytes {
        return Err(SidecarError::Product(format!(
            "the product is {} bytes (limit {})",
            bytes.len(),
            request.limits.max_product_bytes
        )));
    }
    if !stage.is_dir() {
        return Err(SidecarError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("staging directory {}", stage.display()),
        )));
    }
    std::fs::write(stage.join(PRODUCT_FILE), bytes)?;
    Ok(product)
}

fn require_absolute(value: &str, what: &str) -> Result<PathBuf, SidecarError> {
    let path = Path::new(value);
    if !path.is_absolute() {
        return Err(SidecarError::BadRequest(format!(
            "{what} path is relative: {value:?}"
        )));
    }
    Ok(path.to_path_buf())
}

fn is_hex16(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Per-frame mean Rec. 709 luma, ×1000, computed in integer arithmetic.
///
/// The integer path is deliberate: the product's canonical bytes are hashed,
/// and a float's last-bit formatting must never decide an address. `u128`
/// holds the per-frame weighted sums even for a pathological 256 MiB sample.
fn mean_luma(rgb: &[u8], width: u32, height: u32, frames: u64) -> Result<Vec<u32>, SidecarError> {
    let stride = (width as u64)
        .checked_mul(height as u64)
        .and_then(|n| n.checked_mul(3))
        .ok_or_else(|| SidecarError::CorruptInput("frame size overflows".to_string()))?;
    let stride = usize::try_from(stride)
        .map_err(|_| SidecarError::CorruptInput("frame size exceeds memory".to_string()))?;
    let count = usize::try_from(frames)
        .map_err(|_| SidecarError::CorruptInput("frame count exceeds memory".to_string()))?;
    let expected = count
        .checked_mul(stride)
        .ok_or_else(|| SidecarError::CorruptInput("payload length overflows".to_string()))?;
    if rgb.len() != expected {
        return Err(SidecarError::CorruptInput(format!(
            "packed payload is {} bytes, expected {expected}",
            rgb.len()
        )));
    }
    let pixels = width as u128 * height as u128;
    let mut result = Vec::with_capacity(count);
    for index in 0..count {
        let start = index * stride;
        let frame = &rgb[start..start + stride];
        let (mut red, mut green, mut blue) = (0_u128, 0_u128, 0_u128);
        for pixel in frame.as_chunks::<3>().0 {
            red += u128::from(pixel[0]);
            green += u128::from(pixel[1]);
            blue += u128::from(pixel[2]);
        }
        // Rec. 709 weights scaled by 10_000, so this is 10_000 × Σ luma.
        let scaled = 2126 * red + 7152 * green + 722 * blue;
        let numerator = scaled
            .checked_mul(1000)
            .ok_or_else(|| SidecarError::CorruptInput("luma sum overflows".to_string()))?;
        let denominator = 10_000 * pixels;
        let luma = (numerator + denominator / 2) / denominator;
        result.push(
            u32::try_from(luma)
                .map_err(|_| SidecarError::CorruptInput("luma exceeds 255000".to_string()))?,
        );
    }
    Ok(result)
}

/// The bundled deterministic subject task: a bounding box around the pixels
/// matching a colour label (`red`, `green`, `blue`, `white`, `black`). It is
/// the test double for the process boundary, not a face detector; an external
/// `--analyzer` is how real CV enters, with no ML in this binary.
fn detect_subject_boxes(
    rgb: &[u8],
    width: u32,
    height: u32,
    frames: u64,
    labels: &[String],
    floor_milli: u32,
) -> Result<Vec<FrameBoxes>, SidecarError> {
    for label in labels {
        if label_matches(label, [0, 0, 0]).is_none() {
            return Err(SidecarError::Unsupported(format!(
                "the bundled analyzer only detects red/green/blue/white/black, not {label:?}; pass --analyzer for a real detector"
            )));
        }
    }
    if floor_milli > 1000 {
        return Err(SidecarError::BadRequest(format!(
            "floor_milli {floor_milli} is outside 0..=1000"
        )));
    }
    let stride = (width as u64)
        .checked_mul(height as u64)
        .and_then(|n| n.checked_mul(3))
        .ok_or_else(|| SidecarError::CorruptInput("frame size overflows".to_string()))?;
    let stride = usize::try_from(stride)
        .map_err(|_| SidecarError::CorruptInput("frame size exceeds memory".to_string()))?;
    let count = usize::try_from(frames)
        .map_err(|_| SidecarError::CorruptInput("frame count exceeds memory".to_string()))?;
    let expected = count
        .checked_mul(stride)
        .ok_or_else(|| SidecarError::CorruptInput("payload length overflows".to_string()))?;
    if rgb.len() != expected {
        return Err(SidecarError::CorruptInput(format!(
            "packed payload is {} bytes, expected {expected}",
            rgb.len()
        )));
    }
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let frame = &rgb[index * stride..(index + 1) * stride];
        let mut boxes = Vec::new();
        for label in labels {
            let Some(rect) = color_box(frame, width, height, label) else {
                continue;
            };
            boxes.push(DetectedBox {
                subject: SubjectKey {
                    label: label.clone(),
                    id: None,
                },
                rect,
                confidence_milli: 1000,
            });
        }
        sort_detected_boxes(&mut boxes);
        boxes.retain(|detected| detected.confidence_milli >= floor_milli);
        out.push(FrameBoxes {
            index: index as u64,
            boxes,
        });
    }
    Ok(out)
}

fn label_matches(label: &str, [red, green, blue]: [u8; 3]) -> Option<bool> {
    match label.to_ascii_lowercase().as_str() {
        "red" => Some(red >= 128 && green <= 96 && blue <= 96),
        "green" => Some(green >= 128 && red <= 96 && blue <= 96),
        "blue" => Some(blue >= 128 && red <= 96 && green <= 96),
        "white" => Some(red >= 200 && green >= 200 && blue >= 200),
        "black" => Some(red <= 60 && green <= 60 && blue <= 60),
        _ => None,
    }
}

fn color_box(frame: &[u8], width: u32, height: u32, label: &str) -> Option<BoxRect> {
    let mut bounds: Option<(u32, u32, u32, u32)> = None;
    for (pixel, rgb) in frame.as_chunks::<3>().0.iter().enumerate() {
        if label_matches(label, *rgb) != Some(true) {
            continue;
        }
        let x = (pixel as u32) % width;
        let y = (pixel as u32) / width;
        bounds = Some(match bounds {
            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
            None => (x, y, x, y),
        });
    }
    let (x0, y0, x1, y1) = bounds?;
    let left = normalize_pixel(x0, width);
    let top = normalize_pixel(y0, height);
    let right = normalize_pixel(x1 + 1, width);
    let bottom = normalize_pixel(y1 + 1, height);
    Some(BoxRect {
        x: left,
        y: top,
        w: (right - left).max(1),
        h: (bottom - top).max(1),
    })
}

/// Pixel edge → millionths of the picture side, rounding to nearest.
fn normalize_pixel(value: u32, size: u32) -> u32 {
    ((u64::from(value) * 1_000_000 + u64::from(size) / 2) / u64::from(size)) as u32
}

// --------------------------------------------------------------- NDJSON

#[derive(Debug, Serialize)]
struct Hello<'a> {
    id: u64,
    method: &'a str,
    protocol: u32,
}

#[derive(Debug, Serialize)]
struct Analyze<'a> {
    id: u64,
    method: &'a str,
    input: InputRef<'a>,
    request: FrameRequestRef<'a>,
    task: TaskRef<'a>,
    stage: &'a str,
    limits: Limits,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TaskRef<'a> {
    MeanLuma,
    SubjectBoxes {
        labels: &'a [String],
        floor_milli: u32,
    },
}

fn wire_task(task: &Task) -> TaskRef<'_> {
    match task {
        Task::MeanLuma => TaskRef::MeanLuma,
        Task::SubjectBoxes {
            labels,
            floor_milli,
        } => TaskRef::SubjectBoxes {
            labels,
            floor_milli: *floor_milli,
        },
    }
}

/// The window on the wire, tagged by mode. A grid carries its own clock; an
/// index request carries the source rate and frame numbers, so the analyzer
/// can derive the same times the sampler recorded.
#[derive(Debug, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum FrameRequestRef<'a> {
    Grid {
        start: String,
        duration: String,
        fps: String,
        width: u32,
        height: u32,
    },
    Indices {
        rate: String,
        indices: &'a [u64],
        width: u32,
        height: u32,
    },
}

fn wire_request(spec: &FrameSpec) -> FrameRequestRef<'_> {
    match &spec.mode {
        FrameMode::Grid {
            start,
            duration,
            fps,
        } => FrameRequestRef::Grid {
            start: rational(start),
            duration: rational(duration),
            fps: rational(fps),
            width: spec.width,
            height: spec.height,
        },
        FrameMode::Indices { rate, indices } => FrameRequestRef::Indices {
            rate: rational(rate),
            indices,
            width: spec.width,
            height: spec.height,
        },
    }
}

#[derive(Debug, Serialize)]
struct InputRef<'a> {
    kind: &'a str,
    sample: &'a str,
    data: &'a str,
    meta: &'a str,
}

#[derive(Debug, Serialize)]
struct Limits {
    max_product_bytes: u64,
}

#[derive(Debug, Serialize)]
struct Shutdown<'a> {
    id: u64,
    method: &'a str,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    id: u64,
    #[serde(default)]
    ok: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<ErrorBody>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorBody {
    code: String,
    message: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HelloOk {
    protocol: u32,
    analyzer: AnalyzerInfo,
    tasks: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AnalyzeOk {
    hash: String,
    stage: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ShutdownOk {
    shutdown: bool,
}

fn read_response<R: BufRead>(reader: &mut R) -> Result<Response, SidecarError> {
    let Some(line) = read_line_limited(reader, MAX_CONTROL_LINE)? else {
        return Err(SidecarError::Protocol(
            "analyzer closed its stdout before replying".to_string(),
        ));
    };
    serde_json::from_slice(&line).map_err(|error| {
        SidecarError::Protocol(format!(
            "reply is not one JSON object: {error}; line: {}",
            String::from_utf8_lossy(&line)
        ))
    })
}

fn expect_ok<T: DeserializeOwned>(response: Response, expected_id: u64) -> Result<T, SidecarError> {
    if response.id != expected_id {
        return Err(SidecarError::Protocol(format!(
            "reply id {} (expected {expected_id})",
            response.id
        )));
    }
    match (response.ok, response.error) {
        (Some(value), None) => serde_json::from_value(value)
            .map_err(|error| SidecarError::Protocol(format!("reply body: {error}"))),
        (None, Some(error)) => {
            let code = ErrorCode::parse(&error.code).ok_or_else(|| {
                SidecarError::Protocol(format!("unknown error code {:?}", error.code))
            })?;
            Err(SidecarError::Analyzer {
                code,
                message: error.message,
            })
        }
        _ => Err(SidecarError::Protocol(
            "reply must carry exactly one of ok/error".to_string(),
        )),
    }
}

fn write_control<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<(), SidecarError> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| SidecarError::Protocol(format!("encode a control line: {error}")))?;
    if bytes.len() > MAX_CONTROL_LINE {
        return Err(SidecarError::OversizedLine {
            bytes: bytes.len(),
            limit: MAX_CONTROL_LINE,
        });
    }
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

fn write_ok<W: Write, T: Serialize>(writer: &mut W, id: u64, body: &T) -> Result<(), SidecarError> {
    #[derive(Serialize)]
    struct OkLine<'a, T> {
        id: u64,
        ok: &'a T,
    }
    write_control(writer, &OkLine { id, ok: body })
}

fn write_error<W: Write>(
    writer: &mut W,
    id: u64,
    error: &SidecarError,
) -> Result<(), SidecarError> {
    #[derive(Serialize)]
    struct ErrorLine<'a> {
        id: u64,
        error: ErrorOut<'a>,
    }
    #[derive(Serialize)]
    struct ErrorOut<'a> {
        code: &'a str,
        message: String,
    }
    write_control(
        writer,
        &ErrorLine {
            id,
            error: ErrorOut {
                code: error.code().as_str(),
                message: error.to_string(),
            },
        },
    )
}

/// Read one newline-terminated line without ever holding more than `limit`
/// bytes of it. Returns `None` at EOF with no bytes pending.
fn read_line_limited<R: BufRead>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<Vec<u8>>, SidecarError> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            break;
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            line.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            if line.len() > limit {
                return Err(SidecarError::OversizedLine {
                    bytes: line.len(),
                    limit,
                });
            }
            break;
        }
        let len = available.len();
        line.extend_from_slice(available);
        reader.consume(len);
        if line.len() > limit {
            return Err(SidecarError::OversizedLine {
                bytes: line.len(),
                limit,
            });
        }
    }
    while line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(Some(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn a_malformed_line_is_a_typed_bad_request() {
        let error = parse_control(b"{ not json").unwrap_err();
        assert_eq!(error.id, 0);
        assert!(matches!(error.error, SidecarError::BadRequest(_)));

        let error = parse_control(br#"{"id":2,"method":"analyze"}"#).unwrap_err();
        assert_eq!(error.id, 2);
        assert!(
            matches!(error.error, SidecarError::BadRequest(_)),
            "{error:?}"
        );
        assert!(error.error.to_string().contains("analyze"));
    }

    #[test]
    fn an_oversized_line_is_refused_without_truncation() {
        let mut reader = Cursor::new(vec![b'a'; 64]);
        let error = read_line_limited(&mut reader, 16).unwrap_err();
        assert!(
            matches!(
                error,
                SidecarError::OversizedLine {
                    bytes: 64,
                    limit: 16
                }
            ),
            "{error:?}"
        );

        // A line exactly at the cap is allowed.
        let mut reader = Cursor::new(vec![b'a'; 16]);
        let line = read_line_limited(&mut reader, 16).unwrap().unwrap();
        assert_eq!(line.len(), 16);
    }

    #[test]
    fn a_relative_input_or_stage_path_is_refused() {
        let error = require_absolute("relative/sample.raw", "data").unwrap_err();
        assert!(matches!(error, SidecarError::BadRequest(_)), "{error:?}");
        assert!(error.to_string().contains("relative"), "{error}");
        assert!(require_absolute("/abs/sample.raw", "data").is_ok());
    }

    #[test]
    fn the_stage_file_name_is_one_plain_component() {
        for good in ["product.json", "p"] {
            assert!(plain_file_name(good), "{good}");
        }
        for bad in ["", ".", "..", "a/b", "/abs", "../escape", "./p"] {
            assert!(!plain_file_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_analyzer_parses_grid_and_index_requests() {
        let grid = FrameRequest::parse(&FrameRequestIn {
            mode: "grid".to_string(),
            start: Some("0/1".to_string()),
            duration: Some("1/2".to_string()),
            fps: Some("2/1".to_string()),
            rate: None,
            indices: None,
            width: 32,
            height: 24,
        })
        .unwrap();
        assert_eq!(grid.mode.frame_count(), 1);
        assert_eq!(grid.mode.time(0), Some(Seconds::from_integer(0)));

        let index = FrameRequest::parse(&FrameRequestIn {
            mode: "indices".to_string(),
            start: None,
            duration: None,
            fps: None,
            rate: Some("30000/1001".to_string()),
            indices: Some(vec![0, 7]),
            width: 16,
            height: 12,
        })
        .unwrap();
        assert_eq!(index.mode.frame_count(), 2);
        assert_eq!(index.mode.time(1), Some(Seconds::new(7 * 1001, 30_000)));
        assert_eq!(index.mode.indices(), Some([0, 7].as_slice()));

        // A mode cannot carry the other mode's fields.
        let error = FrameRequest::parse(&FrameRequestIn {
            mode: "grid".to_string(),
            start: Some("0/1".to_string()),
            duration: Some("1/1".to_string()),
            fps: Some("2/1".to_string()),
            rate: Some("30/1".to_string()),
            indices: None,
            width: 4,
            height: 4,
        })
        .unwrap_err();
        assert!(matches!(error, SidecarError::BadRequest(_)), "{error:?}");

        let error = FrameRequest::parse(&FrameRequestIn {
            mode: "grid".to_string(),
            start: Some("-1/1".to_string()),
            duration: Some("1/2".to_string()),
            fps: Some("2/1".to_string()),
            rate: None,
            indices: None,
            width: 32,
            height: 24,
        })
        .unwrap_err();
        assert!(matches!(error, SidecarError::BadRequest(_)), "{error:?}");
    }

    #[test]
    fn mean_luma_is_integer_and_order_preserving() {
        // One black frame and one white frame at 1×1.
        let rgb = [0, 0, 0, 255, 255, 255];
        let luma = mean_luma(&rgb, 1, 1, 2).unwrap();
        assert_eq!(luma, [0, 255_000]);

        // A pure red pixel: Rec. 709 luma is 0.2126 × 255 ≈ 54.2.
        let luma = mean_luma(&[255, 0, 0], 1, 1, 1).unwrap();
        assert_eq!(luma, [54_213]);

        let error = mean_luma(&[0, 0, 0], 2, 1, 1).unwrap_err();
        assert!(matches!(error, SidecarError::CorruptInput(_)));
    }

    #[test]
    fn the_bundled_subject_task_finds_a_coloured_box() {
        // 4×2 frame: a red 2×1 run at (1,0).
        let rgb = [
            0, 0, 0, 255, 0, 0, 255, 0, 0, 0, 0, 0, // row 0
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // row 1
        ];
        let frames = detect_subject_boxes(&rgb, 4, 2, 1, &["red".to_string()], 0).expect("detect");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].index, 0);
        assert_eq!(frames[0].boxes.len(), 1);
        assert_eq!(
            frames[0].boxes[0].rect,
            BoxRect {
                x: 250_000,
                y: 0,
                w: 500_000,
                h: 500_000,
            }
        );
        assert_eq!(frames[0].boxes[0].confidence_milli, 1000);
        assert_eq!(frames[0].boxes[0].subject.label, "red");
        assert!(frames[0].boxes[0].subject.id.is_none());

        // A label with no matching pixels is an empty frame, not an error.
        let frames = detect_subject_boxes(&rgb, 4, 2, 1, &["blue".to_string()], 0).expect("detect");
        assert!(frames[0].boxes.is_empty());

        // The bundled detector is a test double, not an arbitrary classifier.
        let error = detect_subject_boxes(&rgb, 4, 2, 1, &["face".to_string()], 0).unwrap_err();
        assert!(matches!(error, SidecarError::Unsupported(_)), "{error:?}");
    }

    #[test]
    fn a_task_is_typed_on_the_wire() {
        let value = serde_json::to_value(wire_task(&Task::MeanLuma)).unwrap();
        assert_eq!(value, serde_json::json!({"kind": "mean_luma"}));

        let task = Task::SubjectBoxes {
            labels: vec!["face".to_string()],
            floor_milli: 200,
        };
        let value = serde_json::to_value(wire_task(&task)).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "subject_boxes",
                "labels": ["face"],
                "floor_milli": 200
            })
        );

        let parsed = TaskIn {
            kind: "subject_boxes".to_string(),
            labels: Some(vec!["face".to_string(), "face".to_string()]),
            floor_milli: Some(500),
        }
        .parse()
        .expect("parse");
        assert_eq!(
            parsed,
            Task::SubjectBoxes {
                labels: vec!["face".to_string()],
                floor_milli: 500,
            }
        );
        let error = TaskIn {
            kind: "segmenter".to_string(),
            labels: None,
            floor_milli: None,
        }
        .parse()
        .unwrap_err();
        assert!(matches!(error, SidecarError::Unsupported(_)), "{error:?}");
    }

    #[test]
    fn a_subject_product_must_match_the_sample_grid() {
        let spec = FrameSpec {
            source: PathBuf::from("/media/a.mp4"),
            mode: FrameMode::Grid {
                start: Seconds::from_integer(0),
                duration: Seconds::from_integer(1),
                fps: Seconds::from_integer(5),
            },
            width: 64,
            height: 36,
        };
        let product = AnalysisProduct {
            version: nle_core::analysis::PRODUCT_VERSION,
            analyzer: AnalyzerInfo {
                name: "color-boxes".to_string(),
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
                    start: Seconds::from_integer(0),
                    fps: Seconds::from_integer(5),
                    width: 64,
                    height: 36,
                },
                labels: vec!["red".to_string()],
                floor_milli: 0,
                frames: vec![FrameBoxes {
                    index: 0,
                    boxes: vec![DetectedBox {
                        subject: SubjectKey {
                            label: "red".to_string(),
                            id: None,
                        },
                        rect: BoxRect {
                            x: 0,
                            y: 0,
                            w: 500_000,
                            h: 500_000,
                        },
                        confidence_milli: 1000,
                    }],
                }],
            },
        };
        let data = SampleData::Frames {
            width: 64,
            height: 36,
            frames: 1,
            times: vec![Seconds::from_integer(0)],
            indices: None,
        };
        let task = Task::SubjectBoxes {
            labels: vec!["red".to_string()],
            floor_milli: 0,
        };
        require_result_shape(&product, &data, &spec, &task).expect("matching grid");

        let mut wrong = product.clone();
        if let ProductResult::SubjectBoxes { grid, .. } = &mut wrong.result {
            grid.fps = Seconds::from_integer(6);
        }
        let error = require_result_shape(&wrong, &data, &spec, &task).unwrap_err();
        assert!(matches!(error, SidecarError::Product(_)), "{error:?}");

        let mut extra = product.clone();
        if let ProductResult::SubjectBoxes { frames, .. } = &mut extra.result {
            frames.push(FrameBoxes {
                index: 1,
                boxes: Vec::new(),
            });
        }
        let error = require_result_shape(&extra, &data, &spec, &task).unwrap_err();
        assert!(matches!(error, SidecarError::Product(_)), "{error:?}");
    }

    #[test]
    fn a_product_must_have_one_result_per_packed_frame() {
        let product = AnalysisProduct {
            version: nle_core::analysis::PRODUCT_VERSION,
            analyzer: AnalyzerInfo {
                name: MEAN_LUMA_NAME.to_string(),
                version: MEAN_LUMA_VERSION,
            },
            sample: ProductSample {
                version: SAMPLE_VERSION,
                key: "0123456789abcdef".to_string(),
                params: "fedcba9876543210".to_string(),
                source: SourceSignature {
                    source: "/media/red.mp4".to_string(),
                    source_arg: "/media/red.mp4".to_string(),
                    source_size: 1,
                    source_mtime_secs: 2,
                    source_mtime_nanos: 3,
                },
            },
            result: ProductResult::MeanLuma {
                luma_milli: vec![1, 2],
            },
        };
        let frames = |count: u64| SampleData::Frames {
            width: 32,
            height: 24,
            frames: count,
            times: (0..count)
                .map(|index| Seconds::new(index as i64, 5))
                .collect(),
            indices: None,
        };
        let spec = FrameSpec {
            source: PathBuf::from("/media/red.mp4"),
            mode: FrameMode::Grid {
                start: Seconds::from_integer(0),
                duration: Seconds::from_integer(1),
                fps: Seconds::from_integer(5),
            },
            width: 32,
            height: 24,
        };
        let error = require_result_shape(&product, &frames(3), &spec, &Task::MeanLuma).unwrap_err();
        assert!(matches!(error, SidecarError::Product(_)), "{error:?}");
        assert!(error.to_string().contains("2 frame results"), "{error}");

        assert!(require_result_shape(&product, &frames(2), &spec, &Task::MeanLuma).is_ok());

        let error = require_result_shape(
            &product,
            &SampleData::Audio {
                sample_rate: 48_000,
                channels: 2,
                samples: 2,
            },
            &spec,
            &Task::MeanLuma,
        )
        .unwrap_err();
        assert!(matches!(error, SidecarError::Product(_)), "{error:?}");
    }

    #[test]
    fn the_wire_request_names_the_mode_and_its_fields() {
        let grid_spec = FrameSpec {
            source: PathBuf::from("/media/x.mp4"),
            mode: FrameMode::Grid {
                start: Seconds::from_integer(0),
                duration: Seconds::from_integer(1),
                fps: Seconds::from_integer(2),
            },
            width: 32,
            height: 24,
        };
        let grid = wire_request(&grid_spec);
        let json = serde_json::to_value(&grid).unwrap();
        assert_eq!(json["mode"], "grid");
        assert_eq!(json["fps"], "2/1");
        assert_eq!(json["width"], 32);
        assert!(json.get("rate").is_none(), "{json}");

        let index_spec = FrameSpec {
            source: PathBuf::from("/media/x.mp4"),
            mode: FrameMode::Indices {
                rate: Seconds::new(30_000, 1001),
                indices: vec![0, 7],
            },
            width: 16,
            height: 12,
        };
        let index = wire_request(&index_spec);
        let json = serde_json::to_value(&index).unwrap();
        assert_eq!(json["mode"], "indices");
        assert_eq!(json["rate"], "30000/1001");
        assert_eq!(json["indices"], serde_json::json!([0, 7]));
        assert!(json.get("start").is_none(), "{json}");
    }

    /// The analyzer trusts `meta` for shape but re-derives the times and
    /// indices from the request: a sample from another clock is refused.
    #[test]
    fn the_analyzer_cross_checks_times_and_indices() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        std::fs::create_dir(&stage).unwrap();
        let data = dir.path().join("sample.raw");
        // Two 1×1 frames: black, white.
        std::fs::write(&data, [0, 0, 0, 255, 255, 255]).unwrap();
        let meta_path = dir.path().join("sample.json");
        let write_meta = |times: Vec<Seconds>, indices: Option<Vec<u64>>| {
            let meta = SampleMeta {
                version: SAMPLE_VERSION,
                params: "fedcba9876543210".to_string(),
                source: "/media/x.mp4".to_string(),
                source_arg: "/media/x.mp4".to_string(),
                source_size: 1,
                source_mtime_secs: 2,
                source_mtime_nanos: 3,
                key: "0123456789abcdef".to_string(),
                bytes: 6,
                data: SampleData::Frames {
                    width: 1,
                    height: 1,
                    frames: 2,
                    times,
                    indices,
                },
            };
            std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        };
        let request = |indices: Option<Vec<u64>>| AnalyzeRequest {
            id: 2,
            method: "analyze".to_string(),
            input: InputRefOwned {
                kind: "frames".to_string(),
                sample: "0123456789abcdef".to_string(),
                data: data.display().to_string(),
                meta: meta_path.display().to_string(),
            },
            request: FrameRequestIn {
                mode: if indices.is_some() {
                    "indices".to_string()
                } else {
                    "grid".to_string()
                },
                start: indices.is_none().then(|| "0/1".to_string()),
                duration: indices.is_none().then(|| "2/1".to_string()),
                fps: indices.is_none().then(|| "1/1".to_string()),
                rate: indices.as_ref().map(|_| "1/1".to_string()),
                indices,
                width: 1,
                height: 1,
            },
            task: TaskIn {
                kind: "mean_luma".to_string(),
                labels: None,
                floor_milli: None,
            },
            stage: stage.display().to_string(),
            limits: LimitsIn {
                max_product_bytes: DEFAULT_MAX_PRODUCT_BYTES,
            },
        };

        write_meta(
            vec![Seconds::from_integer(0), Seconds::from_integer(1)],
            None,
        );
        let product = analyze_request(&request(None)).unwrap();
        assert_eq!(
            product.result,
            ProductResult::MeanLuma {
                luma_milli: vec![0, 255_000]
            }
        );

        // A recorded time that the request does not derive is corrupt input.
        write_meta(
            vec![Seconds::from_integer(0), Seconds::from_integer(2)],
            None,
        );
        let error = analyze_request(&request(None)).unwrap_err();
        assert!(matches!(error, SidecarError::CorruptInput(_)), "{error:?}");
        assert!(error.to_string().contains("times"), "{error}");

        // An index request with matching metadata works, and one missing an
        // index is refused rather than read as a shorter sample.
        write_meta(
            vec![Seconds::from_integer(0), Seconds::from_integer(1)],
            Some(vec![0, 1]),
        );
        assert!(analyze_request(&request(Some(vec![0, 1]))).is_ok());
        assert!(analyze_request(&request(Some(vec![0, 2]))).is_err());
        write_meta(
            vec![Seconds::from_integer(0), Seconds::from_integer(1)],
            None,
        );
        let error = analyze_request(&request(Some(vec![0, 1]))).unwrap_err();
        assert!(matches!(error, SidecarError::CorruptInput(_)), "{error:?}");
    }

    #[test]
    fn a_budget_refuses_an_oversized_window_before_sampling() {
        let spec = FrameSpec {
            source: PathBuf::from("/media/clip.mp4"),
            mode: FrameMode::Grid {
                start: Seconds::from_integer(0),
                duration: Seconds::from_integer(600),
                fps: Seconds::from_integer(30),
            },
            width: 4096,
            height: 4096,
        };
        let error = check_sample_budget(&spec).unwrap_err();
        assert!(
            matches!(error, SidecarError::RequestTooLarge(_)),
            "{error:?}"
        );
        // The error names the cap, not a raw allocation failure.
        assert!(error.to_string().contains("limit"), "{error}");
    }

    #[test]
    fn stage_names_are_pid_hyphen_sequence() {
        for good in ["1-0", "1234-17", "9-99999999999999999999"] {
            assert!(is_stage_name(good), "{good}");
        }
        for bad in ["", "1", "1-", "-1", "a-1", "1-b", "1-2-3", "1_2", "notes"] {
            assert!(!is_stage_name(bad), "{bad:?}");
        }
    }

    /// `cache prune` reports and removes old samples and stale staging
    /// directories, keeps young ones, and never reaches `analysis/products/`.
    #[test]
    fn prune_removes_old_samples_and_staging_but_never_products() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("nile.json");
        std::fs::write(&project, b"{}").unwrap();
        let cache = sample_cache_dir(&project);
        std::fs::create_dir_all(&cache).unwrap();
        let now = SystemTime::now();
        let old = Duration::from_secs(31 * 24 * 3600);
        let young = Duration::from_secs(3600);
        let write = |name: &str, len: usize, age: Duration| {
            let path = cache.join(name);
            std::fs::write(&path, vec![0u8; len]).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(now - age))
                .unwrap();
        };
        write("0123456789abcdef.raw", 10, old);
        write("0123456789abcdef.json", 5, old);
        write("0123456789abcdef.tmp-7-3.raw", 4, old);
        write("fedcba9876543210.raw", 6, young);

        let staging = cache.join("staging");
        std::fs::create_dir(&staging).unwrap();
        let stage = |name: &str, payload: usize, age: Duration| {
            let path = staging.join(name);
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join(PRODUCT_FILE), vec![0u8; payload]).unwrap();
            std::fs::File::open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(now - age))
                .unwrap();
        };
        stage("1234-0", 7, old);
        stage("1234-1", 3, young);
        // Not a stage name: a deliberate bystander survives.
        stage("notes", 9, old);

        // Products are state, under `analysis/`, not under `cache/`.
        let products = nle_core::state::state_dir(&project).join("analysis/products");
        std::fs::create_dir_all(&products).unwrap();
        let product = products.join("a1-0123456789abcdef");
        std::fs::write(&product, b"product").unwrap();

        let report = prune(&project, Duration::from_secs(30 * 24 * 3600), true).unwrap();
        assert_eq!((report.removed, report.kept), (4, 2), "{report:?}");
        assert_eq!(report.bytes, 10 + 5 + 4 + 7);
        assert_eq!(report.kept_bytes, 6 + 3);
        assert!(cache.join("0123456789abcdef.raw").exists(), "dry run");
        assert!(staging.join("1234-0").is_dir(), "dry run");

        let report = prune(&project, Duration::from_secs(30 * 24 * 3600), false).unwrap();
        assert_eq!((report.removed, report.kept), (4, 2), "{report:?}");
        assert!(!cache.join("0123456789abcdef.raw").exists());
        assert!(!cache.join("0123456789abcdef.json").exists());
        assert!(!cache.join("0123456789abcdef.tmp-7-3.raw").exists());
        assert!(!staging.join("1234-0").exists(), "stale staging removed");
        assert!(cache.join("fedcba9876543210.raw").exists(), "young kept");
        assert!(staging.join("1234-1").is_dir(), "young staging kept");
        assert!(staging.join("notes").is_dir(), "not a staging directory");
        assert!(product.exists(), "analysis/products is never pruned");
    }
}
