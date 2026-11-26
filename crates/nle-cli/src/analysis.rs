//! The analysis sidecar: an NDJSON host and the hidden mean-luma analyzer.
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

use nle_core::Seconds;
use nle_core::analysis::{
    AnalysisProduct, AnalyzerInfo, ProductResult, ProductSample, ProductStore, SourceSignature,
};
use nle_ffmpeg::sampling::{FrameSpec, SAMPLE_VERSION, SampleData, SampleEntry, SampleMeta};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

/// The protocol version both sides must agree on.
const PROTOCOL: u32 = 1;

/// One control line may not exceed this many bytes. A longer line is fatal —
/// never truncated, because half a JSON object is not a smaller request.
pub(crate) const MAX_CONTROL_LINE: usize = 1024 * 1024;

/// The host's default cap for one staged product.
pub(crate) const DEFAULT_MAX_PRODUCT_BYTES: u64 = 256 * 1024 * 1024;

/// The host's sample budget until the sampler grows its own byte/frame caps.
/// A single packed sample is buffered in memory, so the window and geometry
/// are checked here, before ffmpeg runs.
pub(crate) const MAX_SAMPLE_BYTES: u64 = 256 * 1024 * 1024;

/// The one file an analyzer may report and the host may adopt.
const PRODUCT_FILE: &str = "product.json";

/// The analyzer name/version the bundled subcommand writes.
const ANALYZER_NAME: &str = "mean-luma";
const ANALYZER_VERSION: u32 = 1;

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
    if bytes > MAX_SAMPLE_BYTES {
        return Err(SidecarError::RequestTooLarge(format!(
            "{frames} frames at {}×{} need {bytes} bytes (limit {MAX_SAMPLE_BYTES})",
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
) -> Result<String, SidecarError> {
    let exe = std::path::absolute(std::env::current_exe()?)?;
    let mut child = Command::new(exe)
        .arg("analysis-analyzer")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let outcome = converse(&mut child, spec, entry, stage.path(), store);
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

    let data = wire_path(entry.data_path())?;
    let meta = wire_path(entry.meta_path())?;
    let stage_wire = wire_path(stage)?;
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
            request: GridRequest {
                mode: "grid",
                start: rational(&spec.start),
                duration: rational(&spec.duration),
                fps: rational(&spec.fps),
                width: spec.width,
                height: spec.height,
            },
            stage: &stage_wire,
            limits: Limits {
                max_product_bytes: DEFAULT_MAX_PRODUCT_BYTES,
            },
        },
    )?;
    let ok: AnalyzeOk = expect_ok(read_response(&mut stdout)?, 2)?;
    let hash = adopt_product(store, entry, stage, &ok)?;

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
    require_one_result_per_frame(&product, &sample_meta.data)?;
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

/// Metadata, not the product, says how many frames were packed. A result
/// with a different length is a truncated or padded analysis, never a
/// "shorter sample": refuse it before the store sees it.
fn require_one_result_per_frame(
    product: &AnalysisProduct,
    data: &SampleData,
) -> Result<(), SidecarError> {
    let ProductResult::MeanLuma { luma_milli } = &product.result;
    let SampleData::Frames { frames, .. } = data else {
        return Err(SidecarError::Product(
            "mean luma needs a frame sample".to_string(),
        ));
    };
    if luma_milli.len() as u64 != *frames {
        return Err(SidecarError::Product(format!(
            "the product has {} frame results but the sample packed {frames}",
            luma_milli.len()
        )));
    }
    Ok(())
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
                    &serde_json::json!({ "protocol": PROTOCOL }),
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
    request: GridRequestIn,
    stage: String,
    limits: LimitsIn,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InputRefOwned {
    kind: String,
    sample: String,
    data: String,
    meta: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GridRequestIn {
    mode: String,
    start: String,
    duration: String,
    fps: String,
    width: u32,
    height: u32,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsIn {
    max_product_bytes: u64,
}

#[derive(Debug)]
struct Grid {
    duration: Seconds,
    fps: Seconds,
    width: u32,
    height: u32,
}

impl Grid {
    fn parse(request: &GridRequestIn) -> Result<Self, SidecarError> {
        if request.mode != "grid" {
            return Err(SidecarError::Unsupported(format!(
                "mode {:?} (this analyzer samples a grid)",
                request.mode
            )));
        }
        let start = parse_rational(&request.start, "start")?;
        let duration = parse_rational(&request.duration, "duration")?;
        let fps = parse_rational(&request.fps, "fps")?;
        if start < Seconds::from_integer(0) {
            return Err(SidecarError::BadRequest(
                "start must not be negative".to_string(),
            ));
        }
        if duration <= Seconds::from_integer(0) {
            return Err(SidecarError::BadRequest(
                "duration must be positive".to_string(),
            ));
        }
        if fps <= Seconds::from_integer(0) {
            return Err(SidecarError::BadRequest("fps must be positive".to_string()));
        }
        if request.width == 0 || request.height == 0 {
            return Err(SidecarError::BadRequest(
                "width and height must be non-zero".to_string(),
            ));
        }
        Ok(Self {
            duration,
            fps,
            width: request.width,
            height: request.height,
        })
    }

    /// `floor(duration × fps)` in exact integers, matching the sampler.
    fn frame_count(&self) -> u64 {
        floor_product(self.duration, self.fps)
    }
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

fn floor_product(a: Seconds, b: Seconds) -> u64 {
    let numerator = *a.numer() as i128 * *b.numer() as i128;
    let denominator = *a.denom() as i128 * *b.denom() as i128;
    if numerator <= 0 || denominator <= 0 {
        return 0;
    }
    (numerator / denominator).min(u64::MAX as i128) as u64
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
    let grid = Grid::parse(&request.request)?;

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
    } = &meta.data
    else {
        return Err(SidecarError::Unsupported(
            "audio samples are not mean-luma input".to_string(),
        ));
    };
    if *width != grid.width || *height != grid.height {
        return Err(SidecarError::CorruptInput(format!(
            "metadata is {width}×{height}, request is {}×{}",
            grid.width, grid.height
        )));
    }
    let wanted = grid.frame_count();
    if *frames == 0 || *frames > wanted {
        return Err(SidecarError::CorruptInput(format!(
            "metadata has {frames} frames, request allows at most {wanted}"
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

    let luma_milli = mean_luma(&data, *width, *height, *frames)?;
    let product = AnalysisProduct {
        version: nle_core::analysis::PRODUCT_VERSION,
        analyzer: AnalyzerInfo {
            name: ANALYZER_NAME.to_string(),
            version: ANALYZER_VERSION,
        },
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
        result: ProductResult::MeanLuma { luma_milli },
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
    request: GridRequest,
    stage: &'a str,
    limits: Limits,
}

#[derive(Debug, Serialize)]
struct InputRef<'a> {
    kind: &'a str,
    sample: &'a str,
    data: &'a str,
    meta: &'a str,
}

#[derive(Debug, Serialize)]
struct GridRequest {
    mode: &'static str,
    start: String,
    duration: String,
    fps: String,
    width: u32,
    height: u32,
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
    fn the_analyzer_cross_checks_the_window_and_metadata() {
        let grid = Grid::parse(&GridRequestIn {
            mode: "grid".to_string(),
            start: "0/1".to_string(),
            duration: "1/2".to_string(),
            fps: "2/1".to_string(),
            width: 32,
            height: 24,
        })
        .unwrap();
        assert_eq!(grid.frame_count(), 1);
        assert_eq!(grid.duration, Seconds::new(1, 2));

        let error = Grid::parse(&GridRequestIn {
            mode: "grid".to_string(),
            start: "-1/1".to_string(),
            duration: "1/2".to_string(),
            fps: "2/1".to_string(),
            width: 32,
            height: 24,
        })
        .unwrap_err();
        assert!(matches!(error, SidecarError::BadRequest(_)));
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
    fn a_product_must_have_one_result_per_packed_frame() {
        let product = AnalysisProduct {
            version: nle_core::analysis::PRODUCT_VERSION,
            analyzer: AnalyzerInfo {
                name: ANALYZER_NAME.to_string(),
                version: ANALYZER_VERSION,
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
        let error = require_one_result_per_frame(
            &product,
            &SampleData::Frames {
                width: 32,
                height: 24,
                frames: 3,
            },
        )
        .unwrap_err();
        assert!(matches!(error, SidecarError::Product(_)), "{error:?}");
        assert!(error.to_string().contains("2 frame results"), "{error}");

        assert!(
            require_one_result_per_frame(
                &product,
                &SampleData::Frames {
                    width: 32,
                    height: 24,
                    frames: 2,
                },
            )
            .is_ok()
        );

        let error = require_one_result_per_frame(
            &product,
            &SampleData::Audio {
                sample_rate: 48_000,
                channels: 2,
                samples: 2,
            },
        )
        .unwrap_err();
        assert!(matches!(error, SidecarError::Product(_)), "{error:?}");
    }

    #[test]
    fn a_budget_refuses_an_oversized_window_before_sampling() {
        let spec = FrameSpec {
            source: PathBuf::from("/media/clip.mp4"),
            start: Seconds::from_integer(0),
            duration: Seconds::from_integer(600),
            fps: Seconds::from_integer(30),
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
}
