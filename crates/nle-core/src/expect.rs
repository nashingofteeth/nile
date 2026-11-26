//! Expectations: what an annotation says a finished render must show.
//!
//! An expectation is a typed predicate anchored to a timeline range. The
//! payload lives in the `AnnotationKind::Expectation` variant, so a note
//! cannot carry a tolerance and an expectation cannot exist without a check —
//! the "illegal state" is unrepresentable, not guarded against at run time.
//!
//! Results are **computed**, never stored: [`evaluate`] is a pure function of
//! `(project, report, annotation)`, and [`evaluate_with`] only adds the
//! project's [`AnalysisStores`] (transcripts
//! and products) for the checks that read stored analysis. A pass cannot
//! outlive the render it describes. Either reads a [`RenderReport`] and the
//! project; neither spawns a process, touches a socket or decodes a frame.
//! Asset-only `cam`/`absent` and `no_sliver` are report-only; the crop, word,
//! subject and group checks need the project's effects and probes — those are
//! refused as unverifiable when the report was rendered from another revision.
//!
//! Resolving an expectation (`AnnotationStatus::Resolved`) **acknowledges** a
//! verdict; it is not a waiver. [`evaluate`] still runs the check, still lets
//! it fail, and copies the status into the outcome, so a reader can tell
//! "known and accepted" from "not looked at yet".

use serde::{Deserialize, Serialize};

use crate::analysis::{AnalysisStores, ProductResult, SAMPLE_VERSION};
use crate::annotation::{Annotation, AnnotationKind, AnnotationStatus};
use crate::project::{Project, Sequence};
use crate::report::{FrameLocation, FrameSource, FrameSpan, RenderReport, SourceSide, frame_edge};
use crate::time::{Seconds, checked_sub, format_seconds, rational_ser, secs_f64};
use crate::timeline::{Clip, CropWindow, Effect, Segment, TransitionSource};

fn default_move_pct() -> f64 {
    25.0
}

fn yes() -> bool {
    true
}

fn default_margin_pct() -> f64 {
    5.0
}

fn one() -> f64 {
    1.0
}

fn default_min_frames() -> i64 {
    2
}

/// One check an annotation states about a render. Field names are wire-visible
/// (they are what a hand-edited `nile.json` or the `--expect` flag writes).
///
/// The payload is part of project v17: a build that predates it rejects the
/// file by version before it could misread the variant, and `deny_unknown_fields`
/// keeps a same-version payload from being read with a field it does not know.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "check", rename_all = "snake_case", deny_unknown_fields)]
pub enum Expectation {
    /// Every checked frame's topmost picture is this asset (or a member of
    /// this media group).
    Cam {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        asset: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<u32>,
        /// Frames trimmed from each end of the anchor before checking, so a
        /// one-frame boundary sliver does not fail the shot.
        #[serde(default)]
        tolerance_frames: i64,
    },
    /// The crop window on the checked material never moves faster than
    /// `max_move_pct_per_sec` percentage points per second. Units:
    /// `Effect::Crop` stores `x`/`y`/`w`/`h` as percentages of the picture,
    /// so the rate is percentage points per second of clip-relative timeline
    /// time.
    CropStable {
        #[serde(default = "default_move_pct")]
        max_move_pct_per_sec: f64,
        /// A moving window may pan but must not change size.
        #[serde(default = "yes")]
        hold_size: bool,
    },
    /// Every checked crop window keeps `margin_pct` percent of the picture on
    /// all four sides (`x >= m`, `y >= m`, `x + w <= 100 - m`, …).
    WorkInFrame {
        #[serde(default = "default_margin_pct")]
        margin_pct: f64,
    },
    /// Every visible subject box in the checked render stays inside the
    /// clip's crop window with `margin_pct` of that window on every side.
    ///
    /// `product` is the `a1-…` address of a `subject_boxes` product for
    /// `asset`; `label` (and optionally `subject_id`) selects which boxes are
    /// checked. The product's sample grid is consulted for each output frame
    /// through the render report, so this checks the delivered picture, not
    /// the clip plan.
    SubjectInFrame {
        product: String,
        asset: u32,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subject_id: Option<String>,
        #[serde(default = "default_margin_pct")]
        margin_pct: f64,
        #[serde(default)]
        min_confidence: f64,
        #[serde(default)]
        tolerance_frames: i64,
    },
    /// At least `min_coverage` of the checked frames carry unmuted audio, or
    /// with a transcript hash, of the relevant words are whole.
    ///
    /// Without `transcript` this is an audio-coverage test: it proves audio is
    /// not cut short, not that the words are there. With a transcript hash it
    /// checks whole words instead: every word the span shows must be complete
    /// in one shown clip or transition side (see [`evaluate_with`]). The
    /// transcript's asset must be shown in the anchored span, or the check is
    /// unverifiable; a missing/corrupt transcript, an absent asset or a silent
    /// asset is unverifiable too, never a panic.
    WordsComplete {
        #[serde(default = "one")]
        min_coverage: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transcript: Option<String>,
    },
    /// No checked frame shows this asset (or a member of this media group).
    /// `black: true` additionally refuses black frames (and a blend's black
    /// side), so it may stand alone: "no black anywhere". One of
    /// `asset`/`group`/`black` must be present.
    Absent {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        asset: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<u32>,
        #[serde(default)]
        black: bool,
        #[serde(default)]
        tolerance_frames: i64,
    },
    /// No visible run of one clip (or blend, or black) overlapping the anchor
    /// is shorter than `min_frames`: a one-frame stump is a cut artifact.
    NoSliver {
        #[serde(default = "default_min_frames")]
        min_frames: i64,
        /// Measure only runs whose topmost clip carries this asset.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        asset: Option<u32>,
    },
}

impl Expectation {
    /// The wire name of the check (`cam`, `crop_stable`, …).
    pub fn name(&self) -> &'static str {
        match self {
            Expectation::Cam { .. } => "cam",
            Expectation::CropStable { .. } => "crop_stable",
            Expectation::WorkInFrame { .. } => "work_in_frame",
            Expectation::SubjectInFrame { .. } => "subject_in_frame",
            Expectation::WordsComplete { .. } => "words_complete",
            Expectation::Absent { .. } => "absent",
            Expectation::NoSliver { .. } => "no_sliver",
        }
    }

    /// Why this payload cannot be persisted or evaluated, or `None`.
    ///
    /// Also re-run on load: a hand-edited project is refused with the same
    /// message `add_annotation` gives, never evaluated as if it were valid.
    pub fn validation_error(&self) -> Option<&'static str> {
        match self {
            Expectation::Cam {
                asset,
                group,
                tolerance_frames,
            } => {
                if asset.is_some() == group.is_some() {
                    return Some("set exactly one of `asset` or `group`");
                }
                if *tolerance_frames < 0 {
                    return Some("tolerance_frames must be >= 0");
                }
                None
            }
            Expectation::Absent {
                asset,
                group,
                black,
                tolerance_frames,
            } => {
                // `black` alone is a valid check ("nothing black"), so only
                // the two asset selectors conflict with each other.
                if asset.is_some() && group.is_some() {
                    return Some("set at most one of `asset` or `group`");
                }
                if asset.is_none() && group.is_none() && !black {
                    return Some("nothing to be absent: name an asset, a group, or `black`");
                }
                if *tolerance_frames < 0 {
                    return Some("tolerance_frames must be >= 0");
                }
                None
            }
            Expectation::CropStable {
                max_move_pct_per_sec,
                ..
            } => {
                if !max_move_pct_per_sec.is_finite() || *max_move_pct_per_sec < 0.0 {
                    return Some("max_move_pct_per_sec must be finite and >= 0");
                }
                None
            }
            Expectation::WorkInFrame { margin_pct } => {
                if !margin_pct.is_finite() || !(0.0..=50.0).contains(margin_pct) {
                    return Some("margin_pct must be between 0 and 50");
                }
                None
            }
            Expectation::SubjectInFrame {
                product,
                label,
                subject_id,
                margin_pct,
                min_confidence,
                tolerance_frames,
                ..
            } => {
                if !crate::analysis::is_product_hash(product) {
                    return Some("product must be an a1-… content hash");
                }
                if label.trim().is_empty() {
                    return Some("label must name a detector class");
                }
                if subject_id.as_deref() == Some("") {
                    return Some("subject_id must not be empty");
                }
                if !margin_pct.is_finite() || !(0.0..=50.0).contains(margin_pct) {
                    return Some("margin_pct must be between 0 and 50");
                }
                if !min_confidence.is_finite() || !(0.0..=1.0).contains(min_confidence) {
                    return Some("min_confidence must be between 0 and 1");
                }
                if *tolerance_frames < 0 {
                    return Some("tolerance_frames must be >= 0");
                }
                None
            }
            Expectation::WordsComplete {
                min_coverage,
                transcript,
            } => {
                if !min_coverage.is_finite() || !(0.0..=1.0).contains(min_coverage) {
                    return Some("min_coverage must be between 0 and 1");
                }
                if let Some(hash) = transcript
                    && !crate::analysis::is_transcript_hash(hash)
                {
                    return Some("transcript must be a t1-… content hash");
                }
                None
            }
            Expectation::NoSliver { min_frames, .. } => {
                if *min_frames < 1 {
                    return Some("min_frames must be >= 1");
                }
                None
            }
        }
    }

    /// Frames trimmed from each end of the anchor (0 for the checks that do
    /// not take a tolerance).
    fn tolerance(&self) -> i64 {
        match self {
            Expectation::Cam {
                tolerance_frames, ..
            }
            | Expectation::Absent {
                tolerance_frames, ..
            }
            | Expectation::SubjectInFrame {
                tolerance_frames, ..
            } => *tolerance_frames,
            _ => 0,
        }
    }

    /// Does evaluating this check need the project as well as the report?
    /// A group resolves through the project, so a group edited after the
    /// render makes the check describe a different set of assets.
    fn needs_project(&self) -> bool {
        match self {
            Expectation::Cam { group, .. } | Expectation::Absent { group, .. } => group.is_some(),
            Expectation::CropStable { .. }
            | Expectation::WorkInFrame { .. }
            | Expectation::SubjectInFrame { .. }
            | Expectation::WordsComplete { .. } => true,
            Expectation::NoSliver { .. } => false,
        }
    }
}

/// The verdict for one expectation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckResult {
    Pass,
    Fail,
    /// The question cannot be answered from this report (stale revision,
    /// vanished anchor, a payload that names a feature with no store yet).
    Unverifiable,
}

/// A timeline range on the check report's wire shape.
///
/// JSON is `{ "start": "10/1", "end": "20/1" }` — the project's exact
/// rational strings, not floats, because this is public wire data: a client
/// must be able to map a verdict back onto the timeline without drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRange {
    #[serde(with = "rational_ser")]
    pub start: Seconds,
    #[serde(with = "rational_ser")]
    pub end: Seconds,
}

/// What one expectation said about one render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckOutcome {
    pub id: u32,
    /// The check's name (`cam`, `crop_stable`, …). A `String`, not a shared
    /// enum, because [`evaluate`] also answers for a note or marker and the
    /// reply has to round-trip through both serde directions.
    pub check: String,
    /// The annotation's review state when it was checked. Resolving
    /// **acknowledges** a result; it is not a waiver — the check still runs
    /// and can still fail, and this field is how a reader tells "known and
    /// accepted" from "not yet looked at". A future `--open-only` filters on
    /// it.
    pub status: AnnotationStatus,
    pub result: CheckResult,
    /// The anchor's timeline span, when it still resolves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<TimeRange>,
    /// The half-open output frame range actually checked (after tolerance).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames: Option<(i64, i64)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl CheckOutcome {
    fn new(id: u32, check: &'static str, status: AnnotationStatus) -> Self {
        Self {
            id,
            check: check.to_string(),
            status,
            result: CheckResult::Unverifiable,
            span: None,
            frames: None,
            detail: None,
        }
    }
}

/// The report a check ran against, copied out so the result is self-describing
/// once the sidecar is gone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportRef {
    pub output: String,
    pub sequence: u32,
    pub revision: u64,
    pub frame_count: i64,
}

impl ReportRef {
    pub fn of(report: &RenderReport) -> Self {
        Self {
            output: report.output.clone(),
            sequence: report.sequence,
            revision: report.revision,
            frame_count: report.frame_count,
        }
    }
}

/// How many checks landed in each bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CheckSummary {
    pub pass: usize,
    pub fail: usize,
    pub unverifiable: usize,
}

/// Every checked expectation for one sequence's render. The sequence is the
/// report's own (`report().sequence`), so a copy cannot drift from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckReport {
    pub report: ReportRef,
    pub checks: Vec<CheckOutcome>,
    pub summary: CheckSummary,
}

impl CheckReport {
    /// Summarise `checks` against `report`. The caller decides which
    /// annotations belong to the report's sequence; an expectation on another
    /// sequence fails inside [`evaluate`] rather than being silently skipped.
    pub fn new(report: &RenderReport, checks: Vec<CheckOutcome>) -> Self {
        let mut summary = CheckSummary::default();
        for check in &checks {
            match check.result {
                CheckResult::Pass => summary.pass += 1,
                CheckResult::Fail => summary.fail += 1,
                CheckResult::Unverifiable => summary.unverifiable += 1,
            }
        }
        Self {
            report: ReportRef::of(report),
            checks,
            summary,
        }
    }

    pub fn report(&self) -> &ReportRef {
        &self.report
    }

    /// The rendered sequence: the report's own, never a second copy.
    pub fn sequence(&self) -> u32 {
        self.report.sequence
    }

    pub fn checks(&self) -> &[CheckOutcome] {
        &self.checks
    }

    pub fn summary(&self) -> &CheckSummary {
        &self.summary
    }
}

/// Evaluate one annotation's expectation against one render.
///
/// A note or marker has no expectation to check and comes back unverifiable;
/// front-ends should filter by [`AnnotationKind`] first.
pub fn evaluate(project: &Project, report: &RenderReport, annotation: &Annotation) -> CheckOutcome {
    evaluate_with(project, report, annotation, &AnalysisStores::empty())
}

/// [`evaluate`] with the project's analysis stores, for the checks that read a
/// stored product.
///
/// The stores are a parameter, not a path: [`evaluate`] stays a pure function
/// of its three arguments (report-only callers use empty stores, and a
/// transcript/product hash comes back unverifiable), while the CLI/TUI/protocol
/// seam passes `AnalysisStores::at(project_path)`. A missing or corrupt
/// product is a detail on an unverifiable outcome, never an error that aborts
/// the report.
pub fn evaluate_with(
    project: &Project,
    report: &RenderReport,
    annotation: &Annotation,
    analysis: &AnalysisStores,
) -> CheckOutcome {
    let expect = match &annotation.kind {
        AnnotationKind::Expectation { expect } => expect,
        _ => {
            let mut out = CheckOutcome::new(annotation.id, annotation.label(), annotation.status);
            out.detail = Some("not an expectation".to_string());
            return out;
        }
    };
    let mut out = CheckOutcome::new(annotation.id, expect.name(), annotation.status);

    if annotation.sequence != report.sequence {
        return failed(
            out,
            format!(
                "rendered sequence #{}, not #{}",
                report.sequence, annotation.sequence
            ),
        );
    }
    let Some(span) = project.annotation_span(annotation) else {
        out.detail = Some("anchor no longer resolves".to_string());
        return out;
    };
    out.span = Some(TimeRange {
        start: span.0,
        end: span.1,
    });

    let (start, end) = frame_range(&span, &report.fps);
    let tolerance = expect.tolerance().max(0);
    let (start, wanted) = (start + tolerance, end - tolerance);
    if wanted <= start {
        out.detail = Some(format!(
            "tolerance_frames {tolerance} leaves no frames to check"
        ));
        return out;
    }
    // `frame_count` is probed from the finished file; the map is planned from
    // the project. A truncated export must not pass on frames that are not
    // there, so the checked range stops at the real last frame.
    let end = wanted.min(report.frame_count);
    if end <= start || end < wanted {
        out.frames = Some((start, end.max(start)));
        return failed(
            out,
            format!(
                "render has {} frame(s); the checked span needs frames {start}..{wanted}",
                report.frame_count
            ),
        );
    }
    out.frames = Some((start, end));

    // The report carries no effects or probes, so a project-dependent check
    // on a render of another revision would describe a timeline that is gone.
    if expect.needs_project() && report.revision != project.revision {
        out.detail = Some(format!(
            "project is at r{}, render at r{}; re-export",
            project.revision, report.revision
        ));
        return out;
    }
    let sections = match sections(report, start, end) {
        Ok(sections) => sections,
        Err(frame) => {
            return failed(out, format!("no frame map entry covers frame {frame}"));
        }
    };
    let ctx = Ctx {
        project,
        report,
        sequence: annotation.sequence,
        span,
        f0: start,
        f1: end,
        sections,
        analysis,
    };

    match expect {
        Expectation::Cam {
            asset,
            group,
            tolerance_frames: _,
        } => check_cam(&ctx, out, asset, group),
        Expectation::Absent {
            asset,
            group,
            black,
            tolerance_frames: _,
        } => check_absent(&ctx, out, asset, group, *black),
        Expectation::NoSliver { min_frames, asset } => {
            check_no_sliver(&ctx, out, *min_frames, asset)
        }
        Expectation::CropStable {
            max_move_pct_per_sec,
            hold_size,
        } => check_crop_stable(&ctx, out, *max_move_pct_per_sec, *hold_size),
        Expectation::WorkInFrame { margin_pct } => check_work_in_frame(&ctx, out, *margin_pct),
        Expectation::SubjectInFrame {
            product,
            asset,
            label,
            subject_id,
            margin_pct,
            min_confidence,
            tolerance_frames: _,
        } => {
            let check = SubjectCheck {
                product,
                asset: *asset,
                label,
                subject_id: subject_id.as_deref(),
                margin: *margin_pct,
                min_confidence: *min_confidence,
            };
            check_subject_in_frame(&ctx, out, &check)
        }
        Expectation::WordsComplete {
            min_coverage,
            transcript,
        } => check_words_complete(&ctx, out, *min_coverage, transcript),
    }
}

/// Everything a check needs, resolved once. `sections` covers the checked
/// frames, so no check has to walk the whole map or handle holes itself.
struct Ctx<'a> {
    project: &'a Project,
    report: &'a RenderReport,
    sequence: u32,
    /// The anchor's whole timeline span (before tolerance trimming).
    span: (Seconds, Seconds),
    f0: i64,
    f1: i64,
    sections: Vec<Section<'a>>,
    /// The project's analysis stores (empty for a report-only [`evaluate`]).
    analysis: &'a AnalysisStores,
}

/// One slice of the checked frame range inside one report span:
/// `(first frame, last frame exclusive, span)`.
type Section<'a> = (i64, i64, &'a FrameSpan);

/// The report's spans intersected with `[f0, f1)`, or the first frame the map
/// does not cover. `validate` only requires spans to be ordered, not packed,
/// so a hand-edited report can still have a hole; that is a failed check, not
/// a silent skip.
fn sections(report: &RenderReport, f0: i64, f1: i64) -> Result<Vec<Section<'_>>, i64> {
    let mut out = Vec::new();
    let mut at = f0;
    for span in &report.frame_map {
        let span_end = span.start_frame + span.frames;
        if span_end <= at {
            continue;
        }
        if span.start_frame >= f1 {
            break;
        }
        if span.start_frame > at {
            return Err(at);
        }
        let end = span_end.min(f1);
        out.push((at, end, span));
        at = end;
        if at >= f1 {
            break;
        }
    }
    if at < f1 { Err(at) } else { Ok(out) }
}

/// The anchor's output frames, using the report builder's own rounding
/// ([`frame_edge`]) — a second quantiser here would disagree with the map by a
/// frame on 29.97 material. A point is one frame; a range that rounds to zero
/// width widens to one frame rather than vanishing.
fn frame_range(span: &(Seconds, Seconds), fps: &Seconds) -> (i64, i64) {
    let (start, end) = *span;
    let first = frame_edge(&start, fps);
    if start == end {
        return (first, first + 1);
    }
    let last = frame_edge(&end, fps);
    (first, last.max(first + 1))
}

fn failed(mut out: CheckOutcome, detail: String) -> CheckOutcome {
    out.result = CheckResult::Fail;
    out.detail = Some(detail);
    out
}

fn passed(mut out: CheckOutcome) -> CheckOutcome {
    out.result = CheckResult::Pass;
    out
}

/// `asset`/`group` as the set of asset ids the check expects. A missing group
/// or a payload with neither is unverifiable, not a pass or a fail. `absent`
/// handles its black-only form before calling this.
fn expected_assets(
    project: &Project,
    asset: &Option<u32>,
    group: &Option<u32>,
) -> Result<Vec<u32>, String> {
    match (asset, group) {
        (Some(id), None) => Ok(vec![*id]),
        (None, Some(id)) => {
            let group = project
                .group(*id)
                .ok_or_else(|| format!("media group #{id} not found"))?;
            Ok(group.parts.iter().map(|part| part.asset).collect())
        }
        _ => Err("set exactly one of `asset` or `group`".to_string()),
    }
}

fn describe_assets(assets: &[u32]) -> String {
    match assets {
        [one] => format!("asset #{one}"),
        many => format!(
            "assets {}",
            many.iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join("/")
        ),
    }
}

fn describe_blend(id: u32, a: &Option<SourceSide>, b: &Option<SourceSide>) -> String {
    let side = |side: &Option<SourceSide>| match side {
        Some(side) => format!("#{}", side.asset),
        None => "black".to_string(),
    };
    format!("blend #{id} ({} → {})", side(a), side(b))
}

fn check_cam(
    ctx: &Ctx<'_>,
    mut out: CheckOutcome,
    asset: &Option<u32>,
    group: &Option<u32>,
) -> CheckOutcome {
    let want = match expected_assets(ctx.project, asset, group) {
        Ok(want) => want,
        Err(detail) => {
            out.detail = Some(detail);
            return out;
        }
    };
    for &(from, _, span) in &ctx.sections {
        match &span.source {
            FrameSource::Clip { asset, .. } => {
                if !want.contains(asset) {
                    return failed(
                        out,
                        format!(
                            "frame {from} shows asset #{asset}, not {}",
                            describe_assets(&want)
                        ),
                    );
                }
            }
            FrameSource::Blend { id, a, b } => {
                let matches = |side: &Option<SourceSide>| {
                    side.as_ref().is_some_and(|side| want.contains(&side.asset))
                };
                if !matches(a) || !matches(b) {
                    return failed(
                        out,
                        format!(
                            "frame {from} is {}, not {}",
                            describe_blend(*id, a, b),
                            describe_assets(&want)
                        ),
                    );
                }
            }
            FrameSource::Black => return failed(out, format!("frame {from} is black")),
        }
    }
    passed(out)
}

fn check_absent(
    ctx: &Ctx<'_>,
    mut out: CheckOutcome,
    asset: &Option<u32>,
    group: &Option<u32>,
    black: bool,
) -> CheckOutcome {
    let want = if asset.is_none() && group.is_none() {
        // `black: true` alone: there is no asset side to match, so an empty
        // set leaves only the black checks below.
        Vec::new()
    } else {
        match expected_assets(ctx.project, asset, group) {
            Ok(want) => want,
            Err(detail) => {
                out.detail = Some(detail);
                return out;
            }
        }
    };
    for &(from, _, span) in &ctx.sections {
        match &span.source {
            FrameSource::Clip { asset, .. } => {
                if want.contains(asset) {
                    return failed(out, format!("frame {from} shows asset #{asset}"));
                }
            }
            FrameSource::Blend { id, a, b } => {
                if let Some(side) = [a, b]
                    .into_iter()
                    .filter_map(|side| side.as_ref())
                    .find(|side| want.contains(&side.asset))
                {
                    return failed(
                        out,
                        format!(
                            "frame {from} is blend #{id} and includes asset #{}",
                            side.asset
                        ),
                    );
                }
                // A blend with a black side is only half picture; `black`
                // mirrors `cam`, where such a side fails.
                if black && (a.is_none() || b.is_none()) {
                    return failed(
                        out,
                        format!("frame {from} is blend #{id} with a black side"),
                    );
                }
            }
            FrameSource::Black => {
                if black {
                    return failed(out, format!("frame {from} is black"));
                }
            }
        }
    }
    passed(out)
}

/// A maximal run of adjacent spans with one identity — the unit `no_sliver`
/// measures. An overlay cut can split a clip's spans without changing what is
/// on screen, so runs merge on the clip id (or blend id, or black), not on the
/// span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunId {
    Clip(u32),
    Blend(u32),
    Black,
}

#[derive(Debug, Clone)]
struct Run {
    id: RunId,
    /// The topmost clip's asset, for [`Expectation::NoSliver`]'s filter.
    asset: Option<u32>,
    start: i64,
    frames: i64,
    timeline_start: Seconds,
    timeline_end: Seconds,
}

impl Run {
    fn overlaps_timeline(&self, span: (Seconds, Seconds)) -> bool {
        self.timeline_start < span.1 && self.timeline_end > span.0
    }
}

fn runs(report: &RenderReport) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for span in &report.frame_map {
        let (id, asset) = match &span.source {
            FrameSource::Clip { clip, asset, .. } => (RunId::Clip(*clip), Some(*asset)),
            FrameSource::Blend { id, .. } => (RunId::Blend(*id), None),
            FrameSource::Black => (RunId::Black, None),
        };
        if let Some(last) = runs.last_mut()
            && last.id == id
            && last.start + last.frames == span.start_frame
            && last.timeline_end == span.timeline_start
        {
            last.frames += span.frames;
            last.timeline_end = span.timeline_end;
            continue;
        }
        runs.push(Run {
            id,
            asset,
            start: span.start_frame,
            frames: span.frames,
            timeline_start: span.timeline_start,
            timeline_end: span.timeline_end,
        });
    }
    runs
}

fn describe_run(run: &Run) -> String {
    match run.id {
        RunId::Clip(id) => match run.asset {
            Some(asset) => format!("clip #{id} (asset #{asset})"),
            None => format!("clip #{id}"),
        },
        RunId::Blend(id) => format!("blend #{id}"),
        RunId::Black => "black".to_string(),
    }
}

fn check_no_sliver(
    ctx: &Ctx<'_>,
    out: CheckOutcome,
    min_frames: i64,
    asset: &Option<u32>,
) -> CheckOutcome {
    for run in runs(ctx.report) {
        if run.start + run.frames <= ctx.f0 || run.start >= ctx.f1 {
            continue;
        }
        if let Some(filter) = asset
            && run.asset != Some(*filter)
        {
            continue;
        }
        if run.frames < min_frames {
            // The merged run is what is measured: clipping it to the anchor
            // would turn a one-frame annotation on a long shot into a "sliver".
            return failed(
                out,
                format!(
                    "{} runs {} frame(s) from f{} (min {min_frames})",
                    describe_run(&run),
                    run.frames,
                    run.start
                ),
            );
        }
    }
    passed(out)
}

/// The clip with `id` in `sequence` and its timeline start, for the checks
/// that need the project's effects.
fn clip_start(sequence: &Sequence, id: u32) -> Option<(&Clip, Seconds)> {
    for track in &sequence.timeline.tracks {
        for (index, segment) in track.segments.iter().enumerate() {
            if let Segment::Clip(clip) = segment
                && clip.id == id
            {
                return Some((clip, track.start_of(index)));
            }
        }
    }
    None
}

/// The part of `run` that overlaps the anchor, in clip-relative seconds. A
/// point anchor yields a degenerate window (`from == to`); the caller decides
/// whether a point is meaningful for it.
fn run_window(
    run: &Run,
    span: (Seconds, Seconds),
    clip_start: Seconds,
) -> Option<(Seconds, Seconds)> {
    let from = run.timeline_start.max(span.0) - clip_start;
    let to = run.timeline_end.min(span.1) - clip_start;
    (to >= from).then_some((from, to))
}

fn check_crop_stable(
    ctx: &Ctx<'_>,
    mut out: CheckOutcome,
    max_move: f64,
    hold_size: bool,
) -> CheckOutcome {
    let Some(sequence) = ctx.project.sequence(ctx.sequence) else {
        out.detail = Some(format!("sequence #{} not found", ctx.sequence));
        return out;
    };
    for run in runs(ctx.report) {
        if !run.overlaps_timeline(ctx.span) {
            continue;
        }
        let RunId::Clip(id) = run.id else {
            continue;
        };
        let Some((clip, start)) = clip_start(sequence, id) else {
            out.detail = Some(format!("clip #{id} is not in sequence #{}", ctx.sequence));
            return out;
        };
        let Some(Effect::Crop { x, y, w, h, keys }) = clip
            .effects
            .iter()
            .find(|effect| matches!(effect, Effect::Crop { .. }))
        else {
            continue; // no crop: nothing to stabilise
        };
        let Some((from, to)) = run_window(&run, ctx.span, start) else {
            continue;
        };
        if from == to {
            continue; // a point is vacuously stable: no interval to move over
        }
        // The static window is a virtual key at t=0 (unless a real key
        // already sits there): the exporter interpolates from it toward the
        // first key, so that ramp is movement too.
        let zero = Seconds::from_integer(0);
        let mut points: Vec<(Seconds, f64, f64, f64, f64)> = Vec::with_capacity(keys.len() + 1);
        if !keys.iter().any(|key| key.at <= zero) {
            points.push((zero, *x, *y, *w, *h));
        }
        points.extend(
            keys.iter()
                .filter(|key| key.at >= zero)
                .map(|key| (key.at, key.x, key.y, key.w, key.h)),
        );
        points.sort_by_key(|point| point.0);
        for pair in points.windows(2) {
            let (before, after) = (pair[0], pair[1]);
            // Only the interpolation ramps that cover the checked window
            // matter; a move entirely outside the anchor is somebody else's
            // check.
            if after.0 <= from || before.0 >= to {
                continue;
            }
            let Some(dt) = checked_sub(&after.0, &before.0) else {
                return failed(out, "crop key times overflow".to_string());
            };
            let dt = secs_f64(&dt);
            if dt <= 0.0 {
                return failed(
                    out,
                    format!("crop keys collide at {} s", format_seconds(&after.0)),
                );
            }
            let dx = (after.1 - before.1).abs() / dt;
            let dy = (after.2 - before.2).abs() / dt;
            if dx > max_move || dy > max_move {
                return failed(
                    out,
                    format!(
                        "clip #{id} crop moves {:.2} %/s at {} s (max {max_move} %/s)",
                        dx.max(dy),
                        format_seconds(&after.0)
                    ),
                );
            }
            if hold_size && ((after.3 - before.3).abs() > 1e-9 || (after.4 - before.4).abs() > 1e-9)
            {
                return failed(
                    out,
                    format!(
                        "clip #{id} crop size changes between {} s and {} s (hold_size)",
                        format_seconds(&before.0),
                        format_seconds(&after.0)
                    ),
                );
            }
        }
    }
    passed(out)
}

/// One crop pass, mirroring `nle-ffmpeg`'s `crop_passes`: `x`/`y` run
/// linearly from `(x0, y0)` to `(x1, y1)` over `[start, end)`, while `w`/`h`
/// are constant (the exporter steps size at a key, it does not interpolate it).
struct CropSegment {
    start: Seconds,
    end: Seconds,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    w: f64,
    h: f64,
}

impl CropSegment {
    fn window_at(&self, at: Seconds) -> CropWindow {
        let span = secs_f64(&(self.end - self.start));
        let fraction = if span > 0.0 {
            (secs_f64(&(at - self.start)) / span).clamp(0.0, 1.0)
        } else {
            1.0
        };
        CropWindow {
            x: self.x0 + (self.x1 - self.x0) * fraction,
            y: self.y0 + (self.y1 - self.y0) * fraction,
            w: self.w,
            h: self.h,
        }
    }
}

/// The crop timeline the exporter renders, in clip-relative seconds:
/// a static window when unkeyed, a base→first-key ramp when the first key
/// starts late, one segment per key pair, and the last window held to the
/// clip's end.
///
/// The head ramp is why `crop_window_at(0)` is not enough: a key *at* zero
/// holds the key's own window from the first frame, while the static fields
/// are a stale fallback the exporter never shows there.
fn crop_segments(effect: &Effect, duration: Seconds) -> Vec<CropSegment> {
    let zero = Seconds::from_integer(0);
    let Effect::Crop { x, y, w, h, keys } = effect else {
        return Vec::new();
    };
    let mut keys: Vec<_> = keys.iter().filter(|key| key.at >= zero).collect();
    keys.sort_by_key(|key| key.at);
    let Some(first) = keys.first() else {
        return vec![CropSegment {
            start: zero,
            end: duration,
            x0: *x,
            y0: *y,
            x1: *x,
            y1: *y,
            w: *w,
            h: *h,
        }];
    };
    let mut segments = Vec::new();
    if first.at > zero {
        let end = first.at.min(duration);
        if end > zero {
            // A first key past the clip's end never arrives: the base holds.
            let (x1, y1) = if first.at <= duration {
                (first.x, first.y)
            } else {
                (*x, *y)
            };
            segments.push(CropSegment {
                start: zero,
                end,
                x0: *x,
                y0: *y,
                x1,
                y1,
                w: *w,
                h: *h,
            });
        }
    }
    for (index, key) in keys.iter().enumerate() {
        if key.at >= duration {
            continue;
        }
        let end = keys
            .get(index + 1)
            .map(|next| next.at.min(duration))
            .unwrap_or(duration);
        if end <= key.at {
            continue;
        }
        let (x1, y1) = keys
            .get(index + 1)
            .map(|next| (next.x, next.y))
            .unwrap_or((key.x, key.y));
        segments.push(CropSegment {
            start: key.at,
            end,
            x0: key.x,
            y0: key.y,
            x1,
            y1,
            w: key.w,
            h: key.h,
        });
    }
    segments
}

fn check_work_in_frame(ctx: &Ctx<'_>, mut out: CheckOutcome, margin: f64) -> CheckOutcome {
    let Some(sequence) = ctx.project.sequence(ctx.sequence) else {
        out.detail = Some(format!("sequence #{} not found", ctx.sequence));
        return out;
    };
    let limit = 100.0 - margin;
    for run in runs(ctx.report) {
        if !run.overlaps_timeline(ctx.span) {
            continue;
        }
        let RunId::Clip(id) = run.id else {
            continue;
        };
        let Some((clip, start)) = clip_start(sequence, id) else {
            out.detail = Some(format!("clip #{id} is not in sequence #{}", ctx.sequence));
            return out;
        };
        let Some(crop) = clip
            .effects
            .iter()
            .find(|effect| matches!(effect, Effect::Crop { .. }))
        else {
            continue; // no crop: the whole picture is in frame
        };
        let Some((from, to)) = run_window(&run, ctx.span, start) else {
            continue;
        };
        let segments = crop_segments(crop, clip.duration());
        // A point anchor samples the one frame it names. An interval samples
        // each segment's ends: x/y are linear inside a segment and w/h are
        // constant, so a margin extreme is always at an end — no per-frame
        // walk.
        let windows = if from == to {
            segments
                .iter()
                .rev()
                .find(|segment| segment.start <= from)
                .map(|segment| vec![segment.window_at(from)])
                .unwrap_or_default()
        } else {
            let mut windows = Vec::new();
            for segment in &segments {
                let a = segment.start.max(from);
                let b = segment.end.min(to);
                if b <= a {
                    continue;
                }
                windows.push(segment.window_at(a));
                windows.push(segment.window_at(b));
            }
            windows
        };
        for window in windows {
            if window.x < margin
                || window.y < margin
                || window.x + window.w > limit
                || window.y + window.h > limit
            {
                return failed(
                    out,
                    format!(
                        "clip #{id} crop leaves the {margin}% margin: x {:.1}, y {:.1}, w {:.1}, h {:.1}",
                        window.x, window.y, window.w, window.h
                    ),
                );
            }
        }
    }
    passed(out)
}

struct SubjectCheck<'a> {
    product: &'a str,
    asset: u32,
    label: &'a str,
    subject_id: Option<&'a str>,
    margin: f64,
    min_confidence: f64,
}

fn check_subject_in_frame(
    ctx: &Ctx<'_>,
    mut out: CheckOutcome,
    check: &SubjectCheck<'_>,
) -> CheckOutcome {
    let product = check.product;
    let asset = check.asset;
    let label = check.label;
    let subject_id = check.subject_id;
    let margin = check.margin;
    let min_confidence = check.min_confidence;
    let Some(sequence) = ctx.project.sequence(ctx.sequence) else {
        out.detail = Some(format!("sequence #{} not found", ctx.sequence));
        return out;
    };
    if !margin.is_finite() || !(0.0..=50.0).contains(&margin) {
        out.detail = Some("margin_pct must be between 0 and 50".to_string());
        return out;
    }
    if !min_confidence.is_finite() || !(0.0..=1.0).contains(&min_confidence) {
        out.detail = Some("min_confidence must be between 0 and 1".to_string());
        return out;
    }
    let stored = match ctx.analysis.products.load(product) {
        Ok(product) => product,
        Err(error) => {
            out.detail = Some(format!("product {product}: {error}"));
            return out;
        }
    };
    if !stored.is_compatible(SAMPLE_VERSION) {
        out.detail = Some(format!(
            "product schema v{} / sample v{} is incompatible with this build (product v{}, sample v{SAMPLE_VERSION})",
            stored.version,
            stored.sample.version,
            crate::analysis::PRODUCT_VERSION,
        ));
        return out;
    }
    let ProductResult::SubjectBoxes {
        grid,
        labels,
        floor_milli,
        frames,
    } = &stored.result
    else {
        out.detail = Some(format!(
            "product {product} is not subject boxes (kind {})",
            result_kind(&stored.result)
        ));
        return out;
    };
    if !labels.iter().any(|declared| declared == label) {
        out.detail = Some(format!(
            "product {product} detected [{}], not {label:?}",
            labels.join(", ")
        ));
        return out;
    }
    let min_milli = (min_confidence * 1000.0).round() as u32;
    if min_milli < *floor_milli {
        out.detail = Some(format!(
            "the product retained confidence {floor_milli} but the check asks for {min_milli}; re-run with a lower floor"
        ));
        return out;
    }
    let offline = match stored.freshness() {
        crate::analysis::Freshness::Stale => {
            out.detail = Some(
                "source changed since the product was made; re-run `nile analysis detect`"
                    .to_string(),
            );
            return out;
        }
        crate::analysis::Freshness::Offline => true,
        crate::analysis::Freshness::Fresh => false,
    };
    let Some(media) = ctx.project.asset(asset) else {
        out.detail = Some(format!("asset #{asset} not found"));
        return out;
    };
    // The product names the source it sampled; a product for another file
    // must not be used just because a box happens to fit.
    let expected = std::path::absolute(&media.path)
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok());
    let recorded = &stored.sample.source;
    let source_matches = expected
        .as_deref()
        .is_some_and(|path| path == recorded.source_arg || path == recorded.source);
    if !source_matches {
        out.detail = Some(format!(
            "product {product} sampled {}, not this asset's source",
            recorded.source_arg
        ));
        return out;
    }

    let mut checked = 0_i64;
    for frame in ctx.f0..ctx.f1 {
        let Some(location) = ctx.report.locate(frame) else {
            out.detail = Some(format!("no frame map entry covers frame {frame}"));
            return out;
        };
        let (clip_id, visible_asset, source) = match location {
            FrameLocation::Clip {
                clip,
                asset,
                source,
                ..
            } => (clip, asset, source),
            FrameLocation::Blend { .. } => {
                out.detail = Some(format!(
                    "frame {frame} is a blend; subject-in-frame has no single source picture"
                ));
                return out;
            }
            FrameLocation::Black { .. } => {
                return failed(out, format!("frame {frame} is black"));
            }
        };
        if visible_asset != asset {
            return failed(
                out,
                format!("frame {frame} shows asset #{visible_asset}, not #{asset}"),
            );
        }
        let Some((clip, _)) = clip_start(sequence, clip_id) else {
            out.detail = Some(format!(
                "clip #{clip_id} is not in sequence #{}",
                ctx.sequence
            ));
            return out;
        };
        if let Some(effect) = clip
            .effects
            .iter()
            .find(|effect| matches!(effect, Effect::Scale { .. } | Effect::Offset { .. }))
        {
            out.detail = Some(format!(
                "frame {frame}: subject-in-frame does not model {}",
                effect.kind()
            ));
            return out;
        }
        let at = clip.timeline_offset_of(source);
        let window = match clip
            .effects
            .iter()
            .find(|effect| matches!(effect, Effect::Crop { .. }))
        {
            None => CropWindow {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 100.0,
            },
            Some(crop) => {
                let segments = crop_segments(crop, clip.duration());
                match segments
                    .iter()
                    .rev()
                    .find(|segment| segment.start <= at)
                    .map(|segment| segment.window_at(at))
                {
                    Some(window) if window.w > 0.0 && window.h > 0.0 => window,
                    _ => {
                        out.detail = Some(format!(
                            "frame {frame}: the crop window is empty at the clip's {} s",
                            format_seconds(&at)
                        ));
                        return out;
                    }
                }
            }
        };
        let Some(index) = grid.index_at(source) else {
            out.detail = Some(format!(
                "frame {frame} is source {} s, before the product grid",
                format_seconds(&source)
            ));
            return out;
        };
        let Some(sample) = frames.get(usize::try_from(index).unwrap_or(usize::MAX)) else {
            out.detail = Some(format!(
                "frame {frame} is source {} s, outside the product grid's {} frames",
                format_seconds(&source),
                frames.len()
            ));
            return out;
        };
        let mut selected = 0;
        let mut matched = 0;
        let mut highest = 0;
        for detected in sample.boxes.iter().filter(|detected| {
            detected.subject.label == label
                && subject_id.is_none_or(|id| detected.subject.id.as_deref() == Some(id))
        }) {
            matched += 1;
            highest = highest.max(detected.confidence_milli);
            if detected.confidence_milli < min_milli {
                continue;
            }
            selected += 1;
            let bx = (f64::from(detected.rect.x) / 10_000.0 - window.x) * 100.0 / window.w;
            let by = (f64::from(detected.rect.y) / 10_000.0 - window.y) * 100.0 / window.h;
            let bw = f64::from(detected.rect.w) / 10_000.0 * 100.0 / window.w;
            let bh = f64::from(detected.rect.h) / 10_000.0 * 100.0 / window.h;
            let limit = 100.0 - margin;
            if bx < margin || by < margin || bx + bw > limit || by + bh > limit {
                return failed(
                    out,
                    format!(
                        "frame {frame} (source {}): {} box leaves the {margin}% margin — x {:.1}, y {:.1}, w {:.1}, h {:.1} in the crop window",
                        format_seconds(&source),
                        detected.subject.label,
                        bx,
                        by,
                        bw,
                        bh
                    ),
                );
            }
        }
        if selected == 0 {
            let suffix = subject_id.map(|id| format!(" #{id}")).unwrap_or_default();
            if matched > 0 {
                return failed(
                    out,
                    format!(
                        "no {label}{suffix} at or above min_confidence {min_confidence:.2} at frame {frame} (source {}); the highest box is {:.2}",
                        format_seconds(&source),
                        f64::from(highest) / 1000.0
                    ),
                );
            }
            return failed(
                out,
                format!(
                    "no {label}{suffix} at frame {frame} (source {})",
                    format_seconds(&source)
                ),
            );
        }
        checked += 1;
    }
    out.detail = Some(format!(
        "{checked}/{checked} frame(s) kept {label}{}{}",
        subject_id.map(|id| format!(" #{id}")).unwrap_or_default(),
        if offline {
            "; source is offline, product checked as recorded"
        } else {
            ""
        }
    ));
    passed(out)
}

fn result_kind(result: &ProductResult) -> &'static str {
    match result {
        ProductResult::MeanLuma { .. } => "mean_luma",
        ProductResult::SubjectBoxes { .. } => "subject_boxes",
    }
}

/// Timeline ranges where the mix carries sound, matching the exporter: a
/// clip whose asset probe reports audio, or a transition with at least one
/// clip side carrying audio (a black side is silence, see `graph.rs`'s
/// `acrossfade` inputs). Blends themselves stay video-only.
fn audible_spans(project: &Project, sequence: u32) -> Vec<(Seconds, Seconds)> {
    let Some(sequence) = project.sequence(sequence) else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    for track in &sequence.timeline.tracks {
        if track.muted {
            continue;
        }
        let mut at = Seconds::from_integer(0);
        for segment in &track.segments {
            let end = at + segment.duration();
            if audible(project, segment) {
                spans.push((at, end));
            }
            at = end;
        }
    }
    spans.sort_by_key(|span| span.0);
    let mut merged: Vec<(Seconds, Seconds)> = Vec::new();
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.0 <= last.1 => last.1 = last.1.max(span.1),
            _ => merged.push(span),
        }
    }
    merged
}

/// Is any of `segment`'s audio-bearing? A tapeless asset on a video track
/// still contributes sound; an audio-track clip with a video-only asset
/// contributes none, exactly like the export's per-input `has_audio`.
fn audible(project: &Project, segment: &Segment) -> bool {
    match segment {
        Segment::Clip(clip) => asset_has_audio(project, clip.asset),
        Segment::Transition(transition) => {
            side_has_audio(project, &transition.a) || side_has_audio(project, &transition.b)
        }
        Segment::Gap(_) => false,
    }
}

fn side_has_audio(project: &Project, side: &TransitionSource) -> bool {
    match side {
        TransitionSource::Clip { asset, .. } => asset_has_audio(project, *asset),
        TransitionSource::Black => false,
    }
}

fn asset_has_audio(project: &Project, asset: u32) -> bool {
    project
        .asset(asset)
        .is_some_and(|asset| asset.probe.has_audio())
}

fn check_words_complete(
    ctx: &Ctx<'_>,
    out: CheckOutcome,
    min_coverage: f64,
    transcript: &Option<String>,
) -> CheckOutcome {
    match transcript {
        None => check_audio_coverage(ctx, out, min_coverage),
        Some(hash) => check_transcript_words(ctx, out, min_coverage, hash),
    }
}

fn check_audio_coverage(ctx: &Ctx<'_>, mut out: CheckOutcome, min_coverage: f64) -> CheckOutcome {
    let audible = audible_spans(ctx.project, ctx.sequence);
    let mut covered = 0;
    for &(from, to, span) in &ctx.sections {
        for (start, end) in &audible {
            // The audio bed's cuts are not in the frame map (it is built from
            // video cuts), so intersect in seconds and count whole frames with
            // the report's own rounding.
            let lo = frame_edge(&(*start).max(span.timeline_start), &ctx.report.fps).max(from);
            let hi = frame_edge(&(*end).min(span.timeline_end), &ctx.report.fps).min(to);
            if hi > lo {
                covered += hi - lo;
            }
        }
    }
    let total = ctx.f1 - ctx.f0;
    let coverage = covered as f64 / total as f64;
    // 1.0 must pass when every frame is covered: the division can land a
    // hair below it in binary.
    let note = "this proves audio is not cut short, not that words are present";
    if coverage + 1e-9 >= min_coverage {
        out.detail = Some(format!(
            "audio covers {:.0}% of the checked frames; {note}",
            coverage * 100.0
        ));
        return passed(out);
    }
    failed(
        out,
        format!(
            "audio covers {:.0}% of the checked frames, need {:.0}%; {note}",
            coverage * 100.0,
            min_coverage * 100.0
        ),
    )
}

/// One place the transcript's asset is shown in the checked sequence, as a
/// linear map from asset source time to timeline time.
///
/// A clip is `(asset_in, asset_out)` over `speed`; a transition side is its
/// `(src_in, src_out)` over the blend's `dur` — the exporter renders the side
/// into `dur` at `(src_out - src_in) / dur`, so the same endpoints give the
/// mapping the picture actually has. Keeping the map as `(start, speed)`
/// rather than a closure means it is `Copy`, comparable in tests, and reads
/// like the model it mirrors.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Shown {
    src_start: Seconds,
    src_end: Seconds,
    tl_start: Seconds,
    speed: Seconds,
}

impl Shown {
    fn timeline_of(&self, source: Seconds) -> Seconds {
        self.tl_start + (source - self.src_start) / self.speed
    }
}

/// Every shown span of `asset` on the sequence's **unmuted** tracks. Muted
/// tracks are skipped entirely: their material is not in the mix and the
/// monitor/export never show it.
fn shown_ranges(project: &Project, sequence: u32, asset: u32) -> Vec<Shown> {
    let Some(sequence) = project.sequence(sequence) else {
        return Vec::new();
    };
    let zero = Seconds::from_integer(0);
    let mut shown = Vec::new();
    for track in &sequence.timeline.tracks {
        if track.muted {
            continue;
        }
        let mut at = Seconds::from_integer(0);
        for segment in &track.segments {
            match segment {
                Segment::Clip(clip) if clip.asset == asset => shown.push(Shown {
                    src_start: clip.asset_in,
                    src_end: clip.asset_out,
                    tl_start: at,
                    speed: clip.speed,
                }),
                Segment::Transition(transition) => {
                    for side in [&transition.a, &transition.b] {
                        let TransitionSource::Clip {
                            asset: side_asset,
                            src_in,
                            src_out,
                            ..
                        } = side
                        else {
                            continue;
                        };
                        // A degenerate side has no rate to map with; the
                        // model refuses it, and skipping keeps a hand-edited
                        // project from dividing by zero here.
                        if *side_asset != asset || transition.dur <= zero || *src_out <= *src_in {
                            continue;
                        }
                        shown.push(Shown {
                            src_start: *src_in,
                            src_end: *src_out,
                            tl_start: at,
                            speed: (*src_out - *src_in) / transition.dur,
                        });
                    }
                }
                _ => {}
            }
            // Every segment advances the timeline, muted material included:
            // the skip above is the track, not the position.
            at += segment.duration();
        }
    }
    shown
}

/// Whole-word coverage for a stored transcript.
///
/// A word is **relevant** when the part of it inside a shown source range maps
/// into the anchor span; it is **complete** only when one shown range holds the
/// whole word *and* its mapped span lies inside the anchor. So a trim, a split
/// or the span's own edges cut a word into an incomplete one, which is the
/// point: a viewer hears a fragment.
///
/// The anchor's exact span (`ctx.span`), not its frame-trimmed range, is the
/// boundary — `WordsComplete` has no tolerance, so a word half a frame outside
/// the anchor is cut.
fn check_transcript_words(
    ctx: &Ctx<'_>,
    mut out: CheckOutcome,
    min_coverage: f64,
    hash: &str,
) -> CheckOutcome {
    // A point has no span a whole word could occupy, so "no words" would be a
    // vacuous pass. Refuse the shape instead of pretending the word check ran.
    if ctx.span.0 == ctx.span.1 {
        out.detail =
            Some("a transcript words check needs a timeline range, not a point".to_string());
        return out;
    }
    let transcript = match ctx.analysis.transcripts.load(hash) {
        Ok(transcript) => transcript,
        Err(error) => {
            out.detail = Some(error.to_string());
            return out;
        }
    };
    let Some(asset) = ctx.project.asset(transcript.asset) else {
        out.detail = Some(format!(
            "transcript asset #{} is not in the project",
            transcript.asset
        ));
        return out;
    };
    if !asset.probe.has_audio() {
        out.detail = Some(format!(
            "transcript asset #{} has no audio",
            transcript.asset
        ));
        return out;
    }
    let shown = shown_ranges(ctx.project, ctx.sequence, transcript.asset);
    if shown.is_empty() {
        out.detail = Some(format!(
            "transcript asset #{} is not on the checked sequence",
            transcript.asset
        ));
        return out;
    }
    // On the sequence is not enough: the anchored span has to show it. A words
    // check over a span that shows another asset cannot describe this span, so
    // refuse the shape instead of passing "no words".
    let in_span = shown.iter().any(|range| {
        let end = range.tl_start + (range.src_end - range.src_start) / range.speed;
        range.tl_start < ctx.span.1 && end > ctx.span.0
    });
    if !in_span {
        out.detail = Some(format!(
            "transcript asset #{} is not in the anchored span",
            transcript.asset
        ));
        return out;
    }

    let mut relevant = 0usize;
    let mut complete = 0usize;
    let mut first_incomplete = None;
    for word in &transcript.words {
        let mut is_relevant = false;
        let mut is_complete = false;
        for range in &shown {
            let overlap_start = word.start.max(range.src_start);
            let overlap_end = word.end.min(range.src_end);
            if overlap_end > overlap_start {
                // The overlap, not the whole word: a word half outside the
                // clip still shows its inside half.
                let from = range.timeline_of(overlap_start);
                let to = range.timeline_of(overlap_end);
                if from < ctx.span.1 && to > ctx.span.0 {
                    is_relevant = true;
                }
            }
            // One range must hold the word whole: a split cuts it even when
            // the two halves together cover the span.
            if word.start >= range.src_start && word.end <= range.src_end {
                let from = range.timeline_of(word.start);
                let to = range.timeline_of(word.end);
                if from >= ctx.span.0 && to <= ctx.span.1 {
                    is_complete = true;
                }
            }
        }
        if is_relevant {
            relevant += 1;
            if is_complete {
                complete += 1;
            } else if first_incomplete.is_none() {
                first_incomplete = Some(word);
            }
        }
    }

    if relevant == 0 {
        out.detail = Some("no transcript words in the anchored span".to_string());
        return passed(out);
    }
    let coverage = complete as f64 / relevant as f64;
    if coverage + 1e-9 >= min_coverage {
        out.detail = Some(format!("{complete}/{relevant} words complete"));
        return passed(out);
    }
    // A shortfall always has an incomplete word when `min_coverage` is in
    // [0, 1]; a payload from outside `add_annotation` may not, so the fallback
    // reports the count instead of panicking.
    let detail = match first_incomplete {
        Some(word) => format!(
            "{complete}/{relevant} words complete; \"{}\" at source {} s is cut",
            word.text,
            format_seconds(&word.start)
        ),
        None => format!("{complete}/{relevant} words complete"),
    };
    failed(out, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VideoSettings;
    use crate::analysis::{
        AnalysisProduct, AnalysisStores, AnalyzerInfo, BoxRect, DetectedBox, FrameBoxes,
        ProductResult, ProductSample, ProductStore, SAMPLE_VERSION, SampleGrid, SourceSignature,
        SubjectKey, Transcript, TranscriptStore, Word,
    };
    use crate::annotation::{Anchor, AnnotationStatus, ClipPoint};
    use crate::group::MediaPart;
    use crate::media::{RawFormat, RawProbe, RawStream};
    use crate::provenance::{Actor, Provenance};
    use crate::report::{REPORT_VERSION, RenderWindow};
    use crate::timeline::{
        CropKey, Gap, Track, TrackKind, Transition, TransitionKind, TransitionSource,
    };
    use std::path::Path;

    fn s(n: i64) -> Seconds {
        Seconds::from_integer(n)
    }

    fn probe(audio: bool) -> RawProbe {
        let mut streams = vec![RawStream {
            index: 0,
            codec_type: Some("video".into()),
            width: Some(1920),
            height: Some(1080),
            r_frame_rate: Some("30/1".into()),
            ..Default::default()
        }];
        if audio {
            streams.push(RawStream {
                index: 1,
                codec_type: Some("audio".into()),
                ..Default::default()
            });
        }
        RawProbe {
            streams,
            format: RawFormat {
                format_name: "mp4".into(),
                duration: Some("60".into()),
                ..Default::default()
            },
        }
    }

    fn clip(id: u32, asset: u32, out: i64) -> Segment {
        Segment::Clip(Clip::new(id, asset, s(0), s(out)))
    }

    fn clip_with(effects: Vec<Effect>) -> Segment {
        Segment::Clip(Clip {
            effects,
            ..Clip::new(1, 1, s(0), s(10))
        })
    }

    fn track(segments: Vec<Segment>) -> Track {
        Track {
            kind: TrackKind::Video,
            muted: false,
            segments,
        }
    }

    fn audio_track(segments: Vec<Segment>) -> Track {
        Track {
            kind: TrackKind::Audio,
            muted: false,
            segments,
        }
    }

    /// Four video assets: #1, #2 and #4 carry audio, #3 does not.
    fn base_project() -> Project {
        let mut project = Project::new();
        project.fps = Some(Seconds::from_integer(30));
        project.video = Some(VideoSettings {
            width: 1920,
            height: 1080,
        });
        for (name, audio) in [
            ("a.mp4", true),
            ("b.mp4", true),
            ("c.mp4", false),
            ("d.mp4", true),
        ] {
            project.add_asset(format!("/media/{name}").into(), probe(audio));
        }
        project
    }

    fn project_with(tracks: Vec<Track>) -> Project {
        let mut project = base_project();
        project.timeline.tracks = tracks;
        project
    }

    fn shot(clip: u32, asset: u32) -> FrameSource {
        FrameSource::Clip {
            clip,
            asset,
            source_start: s(0),
            source_end: s(0),
            speed: s(1),
        }
    }

    fn blend(id: u32, a: Option<u32>, b: Option<u32>) -> FrameSource {
        let side = |asset: Option<u32>| {
            asset.map(|asset| SourceSide {
                asset,
                source_start: s(0),
                source_end: s(0),
            })
        };
        FrameSource::Blend {
            id,
            a: side(a),
            b: side(b),
        }
    }

    fn report_at(fps: Seconds, parts: &[(i64, FrameSource)]) -> RenderReport {
        let mut start = 0;
        let mut frame_map = Vec::with_capacity(parts.len());
        for (frames, source) in parts {
            frame_map.push(FrameSpan {
                start_frame: start,
                frames: *frames,
                timeline_start: Seconds::from_integer(start) / fps,
                timeline_end: Seconds::from_integer(start + frames) / fps,
                source: source.clone(),
            });
            start += frames;
        }
        RenderReport {
            version: REPORT_VERSION,
            sequence: 1,
            revision: 0,
            output: "out.mp4".into(),
            fps,
            duration: Seconds::from_integer(start) / fps,
            frame_count: start,
            windows: vec![RenderWindow {
                start_frame: 0,
                frames: start,
                encoder: None,
            }],
            frame_map,
        }
    }

    fn report(parts: &[(i64, FrameSource)]) -> RenderReport {
        report_at(Seconds::from_integer(30), parts)
    }

    fn expectation(expect: Expectation, clip: u32, from: Seconds, to: Seconds) -> Annotation {
        Annotation {
            id: 1,
            sequence: 1,
            kind: AnnotationKind::Expectation { expect },
            anchor: Anchor::Timeline {
                start: ClipPoint { clip, source: from },
                end: ClipPoint { clip, source: to },
            },
            provenance: Provenance {
                actor: Actor::Human,
                at: 0,
            },
            status: AnnotationStatus::Open,
        }
    }

    fn cam(asset: Option<u32>, group: Option<u32>, tolerance: i64) -> Expectation {
        Expectation::Cam {
            asset,
            group,
            tolerance_frames: tolerance,
        }
    }

    fn absent(asset: Option<u32>, group: Option<u32>, black: bool, tolerance: i64) -> Expectation {
        Expectation::Absent {
            asset,
            group,
            black,
            tolerance_frames: tolerance,
        }
    }

    fn range(expect: Expectation, from: i64, to: i64) -> Annotation {
        expectation(expect, 1, s(from), s(to))
    }

    fn point(expect: Expectation, at: i64) -> Annotation {
        expectation(expect, 1, s(at), s(at))
    }

    fn crop(keys: Vec<CropKey>) -> Effect {
        Effect::Crop {
            x: 10.0,
            y: 10.0,
            w: 80.0,
            h: 80.0,
            keys,
        }
    }

    fn words(min: f64) -> Expectation {
        Expectation::WordsComplete {
            min_coverage: min,
            transcript: None,
        }
    }

    /// A temp transcript store beside a (not necessarily existing) project
    /// file, plus the tempdir that owns it.
    fn temp_store() -> (tempfile::TempDir, TranscriptStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = TranscriptStore::at(&dir.path().join("nile.json"));
        (dir, store)
    }

    fn detected(x: u32, y: u32, w: u32, h: u32, confidence_milli: u32) -> DetectedBox {
        DetectedBox {
            subject: SubjectKey {
                label: "face".to_string(),
                id: None,
            },
            rect: BoxRect { x, y, w, h },
            confidence_milli,
        }
    }

    fn frame_boxes(index: u64, boxes: Vec<DetectedBox>) -> FrameBoxes {
        FrameBoxes { index, boxes }
    }

    fn subject_product(
        source: &Path,
        start: Seconds,
        fps: Seconds,
        floor_milli: u32,
        frames: Vec<FrameBoxes>,
    ) -> AnalysisProduct {
        AnalysisProduct {
            version: crate::analysis::PRODUCT_VERSION,
            analyzer: AnalyzerInfo {
                name: "test-detector".to_string(),
                version: 1,
            },
            sample: ProductSample {
                version: SAMPLE_VERSION,
                key: "0123456789abcdef".to_string(),
                params: "fedcba9876543210".to_string(),
                source: SourceSignature {
                    source: source.display().to_string(),
                    source_arg: source.display().to_string(),
                    source_size: 1,
                    source_mtime_secs: 2,
                    source_mtime_nanos: 3,
                },
            },
            result: ProductResult::SubjectBoxes {
                grid: SampleGrid {
                    start,
                    fps,
                    width: 64,
                    height: 36,
                },
                labels: vec!["face".to_string()],
                floor_milli,
                frames,
            },
        }
    }

    /// A product for `/media/a.mp4` with the same box in every one of
    /// `count` frames at `fps`.
    fn subject_fixture(fps: Seconds, count: u64, boxes: Vec<DetectedBox>) -> AnalysisProduct {
        let frames = (0..count)
            .map(|index| frame_boxes(index, boxes.clone()))
            .collect();
        subject_product(Path::new("/media/a.mp4"), s(0), fps, 0, frames)
    }

    /// A temp product store holding `product`, plus the tempdir that owns it.
    fn subject_stores(product: &AnalysisProduct) -> (tempfile::TempDir, AnalysisStores) {
        let dir = tempfile::tempdir().unwrap();
        let store = ProductStore::at(&dir.path().join("nile.json"));
        store.import(product).expect("import the subject product");
        (dir, AnalysisStores::with_products(store))
    }

    fn subject(
        product: &str,
        label: &str,
        subject_id: Option<&str>,
        margin: f64,
        min_confidence: f64,
    ) -> Expectation {
        Expectation::SubjectInFrame {
            product: product.to_string(),
            asset: 1,
            label: label.to_string(),
            subject_id: subject_id.map(str::to_string),
            margin_pct: margin,
            min_confidence,
            tolerance_frames: 0,
        }
    }

    fn word(text: &str, start: i64, end: i64) -> Word {
        Word {
            text: text.to_string(),
            start: s(start),
            end: s(end),
        }
    }

    fn transcript(asset: u32, words: Vec<Word>) -> Transcript {
        Transcript::new(asset, words)
    }

    fn with_transcript(hash: &str, min: f64) -> Expectation {
        Expectation::WordsComplete {
            min_coverage: min,
            transcript: Some(hash.to_string()),
        }
    }

    /// Clip #1 reading `asset_in..asset_out` at `speed`.
    fn spanned_clip(asset: u32, asset_in: i64, asset_out: i64, speed: i64) -> Segment {
        Segment::Clip(Clip {
            speed: s(speed),
            ..Clip::new(1, asset, s(asset_in), s(asset_out))
        })
    }

    fn dissolve(a: u32, b: Option<u32>, dur: i64) -> Segment {
        let side = |asset: u32| TransitionSource::Clip {
            asset,
            src_in: s(0),
            src_out: s(dur),
            effects: Vec::new(),
        };
        Segment::Transition(Transition {
            id: 7,
            dur: s(dur),
            kind: TransitionKind::Dissolve,
            a: side(a),
            b: b.map(side).unwrap_or(TransitionSource::Black),
        })
    }

    fn key(at: i64, x: f64, y: f64, w: f64, h: f64) -> CropKey {
        CropKey {
            at: s(at),
            x,
            y,
            w,
            h,
        }
    }

    #[test]
    fn cam_passes_when_every_frame_shows_the_expected_asset() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report(&[(150, shot(1, 1))]);
        let outcome = evaluate(&project, &report, &range(cam(Some(1), None, 0), 0, 5));
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
        assert_eq!(outcome.check, "cam");
        assert_eq!(outcome.frames, Some((0, 150)));
        assert_eq!(
            outcome.span,
            Some(TimeRange {
                start: s(0),
                end: s(5)
            })
        );
    }

    #[test]
    fn cam_fails_on_another_asset_and_names_the_frame() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report(&[(75, shot(1, 1)), (75, shot(2, 2))]);
        let outcome = evaluate(&project, &report, &range(cam(Some(1), None, 0), 0, 5));
        assert_eq!(outcome.result, CheckResult::Fail);
        let detail = outcome.detail.expect("detail");
        assert!(detail.contains("frame 75"), "{detail}");
        assert!(detail.contains("asset #2"), "{detail}");
    }

    #[test]
    fn a_cam_on_a_blend_needs_both_sides_to_match() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let one_side = report(&[(150, blend(7, Some(1), Some(2)))]);
        let outcome = evaluate(&project, &one_side, &range(cam(Some(1), None, 0), 0, 5));
        assert_eq!(outcome.result, CheckResult::Fail, "one matching side");
        let both = report(&[(150, blend(7, Some(1), Some(1)))]);
        let outcome = evaluate(&project, &both, &range(cam(Some(1), None, 0), 0, 5));
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
    }

    #[test]
    fn a_cam_across_a_fade_to_black_fails() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report(&[(150, blend(7, Some(1), None))]);
        let outcome = evaluate(&project, &report, &range(cam(Some(1), None, 0), 0, 5));
        assert_eq!(outcome.result, CheckResult::Fail);
        assert!(outcome.detail.unwrap().contains("black"));
    }

    #[test]
    fn crop_stable_passes_a_static_window_and_a_slow_pan() {
        let report = report(&[(150, shot(1, 1))]);
        let expect = Expectation::CropStable {
            max_move_pct_per_sec: 25.0,
            hold_size: true,
        };
        let static_crop = project_with(vec![track(vec![clip_with(vec![crop(Vec::new())])])]);
        let outcome = evaluate(&static_crop, &report, &range(expect.clone(), 0, 5));
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");

        // 10 → 20 percent over two seconds is 5 %/s.
        let pan = project_with(vec![track(vec![clip_with(vec![crop(vec![
            key(0, 10.0, 10.0, 80.0, 80.0),
            key(2, 20.0, 10.0, 80.0, 80.0),
        ])])])]);
        let outcome = evaluate(&pan, &report, &range(expect, 0, 5));
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
    }

    #[test]
    fn crop_stable_fails_a_fast_pan_and_a_size_change() {
        let expecting = Expectation::CropStable {
            max_move_pct_per_sec: 25.0,
            hold_size: true,
        };
        let fast = project_with(vec![track(vec![clip_with(vec![crop(vec![
            key(0, 10.0, 10.0, 80.0, 80.0),
            key(1, 50.0, 10.0, 80.0, 80.0),
        ])])])]);
        let report = report(&[(150, shot(1, 1))]);
        let outcome = evaluate(&fast, &report, &range(expecting.clone(), 0, 5));
        assert_eq!(outcome.result, CheckResult::Fail);
        assert!(outcome.detail.unwrap().contains("40.00 %/s"));

        let grows = project_with(vec![track(vec![clip_with(vec![crop(vec![
            key(0, 10.0, 10.0, 80.0, 80.0),
            key(2, 10.0, 10.0, 60.0, 80.0),
        ])])])]);
        let outcome = evaluate(&grows, &report, &range(expecting, 0, 5));
        assert_eq!(outcome.result, CheckResult::Fail, "hold_size");
        assert!(outcome.detail.unwrap().contains("hold_size"));

        let free = Expectation::CropStable {
            max_move_pct_per_sec: 25.0,
            hold_size: false,
        };
        let outcome = evaluate(&grows, &report, &range(free, 0, 5));
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
    }

    fn with_crop(effect: Effect) -> Project {
        project_with(vec![track(vec![clip_with(vec![effect])])])
    }

    #[test]
    fn work_in_frame_passes_inside_the_margin_and_fails_outside_it() {
        let report = report(&[(150, shot(1, 1))]);
        let inside = with_crop(crop(Vec::new())); // x10 y10 w80 h80
        let outcome = evaluate(
            &inside,
            &report,
            &range(Expectation::WorkInFrame { margin_pct: 5.0 }, 0, 5),
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");

        let outside = with_crop(Effect::Crop {
            x: 0.0,
            y: 0.0,
            w: 80.0,
            h: 80.0,
            keys: Vec::new(),
        });
        let outcome = evaluate(
            &outside,
            &report,
            &range(Expectation::WorkInFrame { margin_pct: 5.0 }, 0, 5),
        );
        assert_eq!(outcome.result, CheckResult::Fail);
        assert!(outcome.detail.unwrap().contains("margin"));
    }

    #[test]
    fn subject_in_frame_passes_a_box_that_keeps_the_margin() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report_at(s(5), &[(5, shot(1, 1))]);
        let product = subject_fixture(
            s(5),
            5,
            vec![detected(300_000, 300_000, 200_000, 200_000, 1000)],
        );
        let (_dir, stores) = subject_stores(&product);
        let outcome = evaluate_with(
            &project,
            &report,
            &range(subject(&product.hash(), "face", None, 5.0, 1.0), 0, 1),
            &stores,
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
        assert_eq!(outcome.check, "subject_in_frame");
        assert!(outcome.detail.unwrap().contains("5/5"));
    }

    #[test]
    fn subject_in_frame_fails_a_box_outside_the_margin() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report_at(s(5), &[(5, shot(1, 1))]);
        let product = subject_fixture(s(5), 5, vec![detected(0, 300_000, 200_000, 200_000, 1000)]);
        let (_dir, stores) = subject_stores(&product);
        let outcome = evaluate_with(
            &project,
            &report,
            &range(subject(&product.hash(), "face", None, 5.0, 1.0), 0, 1),
            &stores,
        );
        assert_eq!(outcome.result, CheckResult::Fail, "{outcome:?}");
        assert!(outcome.detail.unwrap().contains("margin"));
    }

    #[test]
    fn subject_in_frame_checks_the_box_against_the_crop_window() {
        let product = subject_fixture(
            s(5),
            5,
            vec![detected(600_000, 300_000, 100_000, 200_000, 1000)],
        );
        let (_dir, stores) = subject_stores(&product);
        let project = project_with(vec![track(vec![clip_with(vec![Effect::Crop {
            x: 50.0,
            y: 0.0,
            w: 50.0,
            h: 100.0,
            keys: Vec::new(),
        }])])]);
        let report = report_at(s(5), &[(5, shot(1, 1))]);
        let outcome = evaluate_with(
            &project,
            &report,
            &range(subject(&product.hash(), "face", None, 5.0, 1.0), 0, 1),
            &stores,
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");

        // A box left of the crop window is outside the visible picture.
        let product = subject_fixture(
            s(5),
            5,
            vec![detected(100_000, 300_000, 100_000, 200_000, 1000)],
        );
        let (_dir, stores) = subject_stores(&product);
        let outcome = evaluate_with(
            &project,
            &report,
            &range(subject(&product.hash(), "face", None, 5.0, 1.0), 0, 1),
            &stores,
        );
        assert_eq!(outcome.result, CheckResult::Fail, "{outcome:?}");
    }

    #[test]
    fn subject_in_frame_fails_when_the_subject_is_absent_or_the_frame_is_wrong() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report_at(s(5), &[(5, shot(1, 1))]);
        let empty = subject_fixture(s(5), 5, Vec::new());
        let (_dir, stores) = subject_stores(&empty);
        let outcome = evaluate_with(
            &project,
            &report,
            &range(subject(&empty.hash(), "face", None, 5.0, 1.0), 0, 1),
            &stores,
        );
        assert_eq!(outcome.result, CheckResult::Fail, "{outcome:?}");
        assert!(outcome.detail.unwrap().contains("no face"));

        // A different asset on screen is a failure, not an unverifiable pass.
        let product = subject_fixture(
            s(5),
            5,
            vec![detected(300_000, 300_000, 200_000, 200_000, 1000)],
        );
        let (_dir, stores) = subject_stores(&product);
        let other = report_at(s(5), &[(5, shot(1, 2))]);
        let outcome = evaluate_with(
            &project,
            &other,
            &range(subject(&product.hash(), "face", None, 5.0, 1.0), 0, 1),
            &stores,
        );
        assert_eq!(outcome.result, CheckResult::Fail, "{outcome:?}");
        assert!(outcome.detail.unwrap().contains("asset #2"));

        let black = report_at(s(5), &[(5, FrameSource::Black)]);
        let outcome = evaluate_with(
            &project,
            &black,
            &range(subject(&product.hash(), "face", None, 5.0, 1.0), 0, 1),
            &stores,
        );
        assert_eq!(outcome.result, CheckResult::Fail, "{outcome:?}");
        assert!(outcome.detail.unwrap().contains("black"));
    }

    #[test]
    fn subject_in_frame_is_unverifiable_without_a_single_source_picture() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let product = subject_fixture(
            s(5),
            5,
            vec![detected(300_000, 300_000, 200_000, 200_000, 1000)],
        );
        let (_dir, stores) = subject_stores(&product);
        let expect = range(subject(&product.hash(), "face", None, 5.0, 1.0), 0, 1);

        let blend = report_at(s(5), &[(5, blend(7, Some(1), Some(2)))]);
        let outcome = evaluate_with(&project, &blend, &expect, &stores);
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
        assert!(outcome.detail.unwrap().contains("blend"));

        let scaled = project_with(vec![track(vec![clip_with(vec![Effect::Scale {
            w: 100.0,
            h: 100.0,
        }])])]);
        let report = report_at(s(5), &[(5, shot(1, 1))]);
        let outcome = evaluate_with(&scaled, &report, &expect, &stores);
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
        assert!(outcome.detail.unwrap().contains("scale"));
    }

    #[test]
    fn subject_in_frame_reports_missing_grid_coverage_and_bad_products() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report_at(s(5), &[(5, shot(1, 1))]);
        let short = subject_fixture(
            s(5),
            2,
            vec![detected(300_000, 300_000, 200_000, 200_000, 1000)],
        );
        let (_dir, stores) = subject_stores(&short);
        let outcome = evaluate_with(
            &project,
            &report,
            &range(subject(&short.hash(), "face", None, 5.0, 1.0), 0, 1),
            &stores,
        );
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
        assert!(outcome.detail.unwrap().contains("outside the product grid"));

        let mut incompatible = subject_fixture(
            s(5),
            5,
            vec![detected(300_000, 300_000, 200_000, 200_000, 1000)],
        );
        incompatible.sample.version = SAMPLE_VERSION + 1;
        let (_dir, stores) = subject_stores(&incompatible);
        let outcome = evaluate_with(
            &project,
            &report,
            &range(subject(&incompatible.hash(), "face", None, 5.0, 1.0), 0, 1),
            &stores,
        );
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
        assert!(outcome.detail.unwrap().contains("incompatible"));

        let missing = subject("a1-0000000000000000", "face", None, 5.0, 1.0);
        let outcome = evaluate_with(&project, &report, &range(missing, 0, 1), &stores);
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
    }

    #[test]
    fn subject_in_frame_filters_label_instance_and_confidence() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report_at(s(5), &[(5, shot(1, 1))]);
        let boxes = vec![
            DetectedBox {
                subject: SubjectKey {
                    label: "face".to_string(),
                    id: Some("spk-2".to_string()),
                },
                rect: BoxRect {
                    x: 300_000,
                    y: 300_000,
                    w: 200_000,
                    h: 200_000,
                },
                confidence_milli: 400,
            },
            DetectedBox {
                subject: SubjectKey {
                    label: "face".to_string(),
                    id: Some("spk-3".to_string()),
                },
                rect: BoxRect {
                    x: 0,
                    y: 0,
                    w: 100_000,
                    h: 100_000,
                },
                confidence_milli: 1000,
            },
        ];
        let product = subject_fixture(s(5), 5, boxes);
        let (_dir, stores) = subject_stores(&product);
        let expect = subject(&product.hash(), "face", None, 5.0, 1.0);
        let outcome = evaluate_with(&project, &report, &range(expect, 0, 1), &stores);
        assert_eq!(outcome.result, CheckResult::Fail, "{outcome:?}");

        // Narrow to the inside box by id; the low-confidence outside box is
        // ignored by the threshold.
        let inside = subject(&product.hash(), "face", Some("spk-2"), 5.0, 0.3);
        let outcome = evaluate_with(&project, &report, &range(inside, 0, 1), &stores);
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");

        // A product that retained less than the asked-for threshold cannot
        // answer "no face" — it is unverifiable.
        let mut floored = subject_fixture(
            s(5),
            5,
            vec![detected(300_000, 300_000, 200_000, 200_000, 940)],
        );
        if let ProductResult::SubjectBoxes { floor_milli, .. } = &mut floored.result {
            *floor_milli = 900;
        }
        let (_dir, stores) = subject_stores(&floored);
        let outcome = evaluate_with(
            &project,
            &report,
            &range(subject(&floored.hash(), "face", None, 5.0, 0.5), 0, 1),
            &stores,
        );
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
    }

    #[test]
    fn subject_in_frame_names_boxes_below_the_threshold() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report_at(s(5), &[(5, shot(1, 1))]);
        let product = subject_fixture(
            s(5),
            5,
            vec![detected(300_000, 300_000, 200_000, 200_000, 400)],
        );
        let (_dir, stores) = subject_stores(&product);
        let outcome = evaluate_with(
            &project,
            &report,
            &range(subject(&product.hash(), "face", None, 5.0, 0.5), 0, 1),
            &stores,
        );
        assert_eq!(outcome.result, CheckResult::Fail, "{outcome:?}");
        let detail = outcome.detail.unwrap();
        assert!(detail.contains("min_confidence 0.50"), "{detail}");
        assert!(detail.contains("highest box is 0.40"), "{detail}");
    }

    #[test]
    fn subject_in_frame_is_unverifiable_when_the_source_moved() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a.mp4");
        std::fs::write(&source, b"first").unwrap();
        let meta = std::fs::metadata(&source).unwrap();
        let (secs, nanos) = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| (d.as_secs(), d.subsec_nanos()))
            .unwrap();
        let mut product = subject_fixture(
            s(5),
            5,
            vec![detected(300_000, 300_000, 200_000, 200_000, 1000)],
        );
        product.sample.source = SourceSignature {
            source: source.display().to_string(),
            source_arg: source.display().to_string(),
            source_size: meta.len(),
            source_mtime_secs: secs,
            source_mtime_nanos: nanos,
        };
        let (_dir, stores) = subject_stores(&product);
        let mut project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        project.media[0].path = source.clone();
        let report = report_at(s(5), &[(5, shot(1, 1))]);
        let outcome = evaluate_with(
            &project,
            &report,
            &range(subject(&product.hash(), "face", None, 5.0, 1.0), 0, 1),
            &stores,
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");

        let file = std::fs::File::options().write(true).open(&source).unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        drop(file);
        let outcome = evaluate_with(
            &project,
            &report,
            &range(subject(&product.hash(), "face", None, 5.0, 1.0), 0, 1),
            &stores,
        );
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
        assert!(outcome.detail.unwrap().contains("source changed"));
    }

    #[test]
    fn words_complete_passes_when_audio_covers_the_range() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report(&[(150, shot(1, 1))]);
        let outcome = evaluate(
            &project,
            &report,
            &range(
                Expectation::WordsComplete {
                    min_coverage: 1.0,
                    transcript: None,
                },
                0,
                5,
            ),
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
        assert!(
            outcome
                .detail
                .unwrap()
                .contains("not that words are present"),
            "the limitation must be stated"
        );
    }

    #[test]
    fn words_complete_fails_a_silent_picture_and_accepts_partial_coverage() {
        // #3 has no audio stream and there is no audio track: 0% coverage.
        let silent = project_with(vec![track(vec![clip(1, 3, 10)])]);
        let report = report(&[(150, shot(1, 3))]);
        let outcome = evaluate(
            &silent,
            &report,
            &range(
                Expectation::WordsComplete {
                    min_coverage: 0.5,
                    transcript: None,
                },
                0,
                5,
            ),
        );
        assert_eq!(outcome.result, CheckResult::Fail);

        // A three-second audio bed over a five-second shot is 60%.
        let partial = project_with(vec![
            track(vec![clip(1, 3, 10)]),
            audio_track(vec![clip(2, 1, 3), Segment::Gap(Gap { duration: s(2) })]),
        ]);
        let at_edge = evaluate(
            &partial,
            &report,
            &range(
                Expectation::WordsComplete {
                    min_coverage: 0.6,
                    transcript: None,
                },
                0,
                5,
            ),
        );
        assert_eq!(at_edge.result, CheckResult::Pass, "{at_edge:?}");
        let too_much = evaluate(
            &partial,
            &report,
            &range(
                Expectation::WordsComplete {
                    min_coverage: 0.7,
                    transcript: None,
                },
                0,
                5,
            ),
        );
        assert_eq!(too_much.result, CheckResult::Fail);
    }

    #[test]
    fn words_complete_counts_transition_audio() {
        // The transition is the only audible material: the clip the anchor
        // resolves through sits on a second track with a silent asset.
        let anchored = |transition: Segment| {
            project_with(vec![track(vec![transition]), track(vec![clip(1, 3, 5)])])
        };
        let report = report(&[(150, shot(1, 3))]);
        // Clip → clip: both sides carry audio, so the dissolve sounds.
        let pair = anchored(dissolve(1, Some(2), 5));
        let outcome = evaluate(&pair, &report, &range(words(1.0), 0, 5));
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");

        // Clip → black: the acrossfade still takes the clip side's audio.
        let fade = anchored(dissolve(1, None, 5));
        let outcome = evaluate(&fade, &report, &range(words(1.0), 0, 5));
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");

        // Both sides silent: black is silence and asset #3 has no audio.
        let silent = anchored(dissolve(3, Some(3), 5));
        let outcome = evaluate(&silent, &report, &range(words(1.0), 0, 5));
        assert_eq!(outcome.result, CheckResult::Fail);
    }

    #[test]
    fn audio_is_judged_per_asset_not_per_track() {
        // A video-only asset on an audio track is silent, exactly as the
        // exporter's per-input `has_audio` sees it.
        let silent = project_with(vec![
            track(vec![clip(1, 3, 10)]),
            audio_track(vec![clip(2, 3, 5)]),
        ]);
        let report = report(&[(150, shot(1, 3))]);
        let outcome = evaluate(&silent, &report, &range(words(1.0), 0, 5));
        assert_eq!(
            outcome.result,
            CheckResult::Fail,
            "an audio-track clip with a silent asset"
        );

        // A video clip's own audio counts even with no audio track.
        let sound = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let outcome = evaluate(&sound, &report, &range(words(1.0), 0, 5));
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
    }

    #[test]
    fn a_transcript_payload_is_unverifiable() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report(&[(150, shot(1, 1))]);
        let outcome = evaluate(
            &project,
            &report,
            &range(
                Expectation::WordsComplete {
                    min_coverage: 1.0,
                    transcript: Some("t1-0000000000000000".into()),
                },
                0,
                5,
            ),
        );
        assert_eq!(outcome.result, CheckResult::Unverifiable);
        assert!(outcome.detail.unwrap().contains("not found"));
    }

    #[test]
    fn a_stored_word_inside_one_clip_passes() {
        let project = project_with(vec![track(vec![spanned_clip(1, 0, 10, 1)])]);
        let report = report(&[(300, shot(1, 1))]);
        let (_dir, store) = temp_store();
        let hash = store
            .import(&transcript(1, vec![word("hello", 1, 2)]))
            .unwrap();
        let outcome = evaluate_with(
            &project,
            &report,
            &range(with_transcript(&hash, 1.0), 0, 5),
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
        assert_eq!(outcome.detail.as_deref(), Some("1/1 words complete"));
    }

    #[test]
    fn a_word_cut_by_a_trim_is_incomplete() {
        // The clip starts at source 2, so the word's first second is gone.
        let project = project_with(vec![track(vec![spanned_clip(1, 2, 12, 1)])]);
        let report = report(&[(300, shot(1, 1))]);
        let (_dir, store) = temp_store();
        let hash = store
            .import(&transcript(1, vec![word("hello", 1, 3)]))
            .unwrap();
        let outcome = evaluate_with(
            &project,
            &report,
            // source 2..7 is timeline 0..5
            &range(with_transcript(&hash, 1.0), 2, 7),
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Fail, "{outcome:?}");
        let detail = outcome.detail.unwrap();
        assert!(detail.contains("0/1 words complete"), "{detail}");
        assert!(
            detail.contains("\"hello\" at source 1 s is cut"),
            "{detail}"
        );
    }

    #[test]
    fn a_word_cut_by_either_span_edge_is_incomplete() {
        let project = project_with(vec![track(vec![spanned_clip(1, 0, 10, 1)])]);
        let report = report(&[(300, shot(1, 1))]);
        let (_dir, store) = temp_store();
        let hash = store
            .import(&transcript(
                1,
                vec![word("early", 1, 3), word("late", 6, 8)],
            ))
            .unwrap();
        // The anchor is timeline 2..7: both words straddle an edge.
        let outcome = evaluate_with(
            &project,
            &report,
            &range(with_transcript(&hash, 1.0), 2, 7),
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Fail, "{outcome:?}");
        let detail = outcome.detail.unwrap();
        assert!(detail.contains("0/2 words complete"), "{detail}");
        assert!(
            detail.contains("\"early\" at source 1 s is cut"),
            "{detail}"
        );
    }

    #[test]
    fn a_transcript_words_check_refuses_a_point_anchor() {
        let project = project_with(vec![track(vec![spanned_clip(1, 0, 10, 1)])]);
        let report = report(&[(300, shot(1, 1))]);
        let (_dir, store) = temp_store();
        let hash = store
            .import(&transcript(1, vec![word("hello", 1, 2)]))
            .unwrap();
        // A point has no interval a whole word could occupy; a vacuous pass
        // would claim more than the check can prove.
        let outcome = evaluate_with(
            &project,
            &report,
            &range(with_transcript(&hash, 1.0), 2, 2),
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
        let detail = outcome.detail.unwrap_or_default();
        assert!(detail.contains("timeline range"), "{detail}");
    }

    #[test]
    fn a_speed_change_maps_words_through_the_rate() {
        // A 2x clip reads source 0..10 over timeline 0..5, so the anchor's
        // source 4..8 is timeline 2..4.
        let project = project_with(vec![track(vec![spanned_clip(1, 0, 10, 2)])]);
        let report = report(&[(300, shot(1, 1))]);
        let (_dir, store) = temp_store();
        let hash = store
            .import(&transcript(1, vec![word("inside", 4, 6)]))
            .unwrap();
        let outcome = evaluate_with(
            &project,
            &report,
            &range(with_transcript(&hash, 1.0), 4, 8),
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
        // "no words" would also pass; the count proves the word mapped in.
        assert_eq!(outcome.detail.as_deref(), Some("1/1 words complete"));

        // The same rate makes source 3..6 time out to timeline 1.5..3, which
        // starts before the anchor at 2.
        let hash = store
            .import(&transcript(
                1,
                vec![word("straddle", 3, 6), word("inside", 4, 6)],
            ))
            .unwrap();
        let outcome = evaluate_with(
            &project,
            &report,
            &range(with_transcript(&hash, 1.0), 4, 8),
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Fail, "{outcome:?}");
        assert!(
            outcome
                .detail
                .unwrap()
                .contains("1/2 words complete; \"straddle\" at source 3 s is cut")
        );
    }

    #[test]
    fn a_split_inside_a_word_is_incomplete() {
        let project = project_with(vec![track(vec![
            spanned_clip(1, 0, 5, 1),
            Segment::Clip(Clip::new(2, 1, s(5), s(10))),
        ])]);
        let report = report(&[(300, shot(1, 1))]);
        let (_dir, store) = temp_store();
        let hash = store
            .import(&transcript(1, vec![word("split", 4, 6)]))
            .unwrap();
        let outcome = evaluate_with(
            &project,
            &report,
            &range(with_transcript(&hash, 1.0), 0, 5),
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Fail, "{outcome:?}");
        assert!(
            outcome
                .detail
                .unwrap()
                .contains("0/1 words complete; \"split\" at source 4 s is cut")
        );
    }

    #[test]
    fn a_word_outside_the_span_is_a_vacuous_pass() {
        let project = project_with(vec![track(vec![spanned_clip(1, 0, 10, 1)])]);
        let report = report(&[(300, shot(1, 1))]);
        let (_dir, store) = temp_store();
        let hash = store
            .import(&transcript(1, vec![word("later", 8, 9)]))
            .unwrap();
        let outcome = evaluate_with(
            &project,
            &report,
            &range(with_transcript(&hash, 1.0), 0, 5),
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
        assert_eq!(
            outcome.detail.as_deref(),
            Some("no transcript words in the anchored span")
        );
    }

    #[test]
    fn a_transcript_asset_outside_the_anchor_is_unverifiable() {
        // Asset #1 is on the sequence at timeline 10..15; the anchor is on
        // clip #2 at 0..5, where the transcript cannot describe the picture.
        let project = project_with(vec![track(vec![
            clip(2, 2, 5),
            Segment::Gap(Gap { duration: s(5) }),
            clip(1, 1, 5),
        ])]);
        let report = report(&[(150, shot(2, 2)), (150, shot(1, 1))]);
        let (_dir, store) = temp_store();
        let hash = store
            .import(&transcript(1, vec![word("hello", 1, 2)]))
            .unwrap();
        let annotation = expectation(with_transcript(&hash, 1.0), 2, s(0), s(5));
        let outcome = evaluate_with(
            &project,
            &report,
            &annotation,
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
        let detail = outcome.detail.unwrap_or_default();
        assert!(detail.contains("not in the anchored span"), "{detail}");
    }

    #[test]
    fn partial_word_coverage_honours_min_coverage() {
        let project = project_with(vec![track(vec![spanned_clip(1, 0, 10, 1)])]);
        let report = report(&[(300, shot(1, 1))]);
        let (_dir, store) = temp_store();
        let hash = store
            .import(&transcript(1, vec![word("whole", 1, 2), word("cut", 4, 7)]))
            .unwrap();
        let annotation = range(with_transcript(&hash, 0.5), 0, 5);
        let outcome = evaluate_with(
            &project,
            &report,
            &annotation,
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
        assert_eq!(outcome.detail.as_deref(), Some("1/2 words complete"));

        let strict = range(with_transcript(&hash, 0.75), 0, 5);
        let outcome = evaluate_with(
            &project,
            &report,
            &strict,
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Fail, "{outcome:?}");
        assert!(outcome.detail.unwrap().contains("1/2 words complete"));
    }

    #[test]
    fn a_missing_or_misspelled_transcript_is_unverifiable() {
        let project = project_with(vec![track(vec![spanned_clip(1, 0, 10, 1)])]);
        let report = report(&[(300, shot(1, 1))]);
        let (_dir, store) = temp_store();

        let missing = range(with_transcript("t1-0000000000000000", 1.0), 0, 5);
        let outcome = evaluate_with(
            &project,
            &report,
            &missing,
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
        let detail = outcome.detail.unwrap();
        assert!(detail.contains("not found"), "{detail}");
        assert!(detail.contains("t1-0000000000000000"), "{detail}");

        let misspelled = range(with_transcript("../take", 1.0), 0, 5);
        let outcome = evaluate_with(
            &project,
            &report,
            &misspelled,
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
        assert!(outcome.detail.unwrap().contains("not a transcript hash"));
    }

    #[test]
    fn a_transcript_asset_must_exist_sound_and_be_shown() {
        let report = report(&[(300, shot(1, 1))]);
        let (_dir, store) = temp_store();

        // The transcript names an asset the project does not have.
        let project = project_with(vec![track(vec![spanned_clip(1, 0, 10, 1)])]);
        let hash = store
            .import(&transcript(9, vec![word("ghost", 1, 2)]))
            .unwrap();
        let outcome = evaluate_with(
            &project,
            &report,
            &range(with_transcript(&hash, 1.0), 0, 5),
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
        assert!(outcome.detail.unwrap().contains("not in the project"));

        // Asset #3 probes without an audio stream.
        let silent = project_with(vec![track(vec![Segment::Clip(Clip::new(
            1,
            3,
            s(0),
            s(10),
        ))])]);
        let hash = store
            .import(&transcript(3, vec![word("mute", 1, 2)]))
            .unwrap();
        let outcome = evaluate_with(
            &silent,
            &report,
            &range(with_transcript(&hash, 1.0), 0, 5),
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
        assert!(outcome.detail.unwrap().contains("has no audio"));

        // Asset #4 exists with audio but is on nobody's track here.
        let hash = store
            .import(&transcript(4, vec![word("away", 1, 2)]))
            .unwrap();
        let outcome = evaluate_with(
            &project,
            &report,
            &range(with_transcript(&hash, 1.0), 0, 5),
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
        assert!(
            outcome
                .detail
                .unwrap()
                .contains("not on the checked sequence")
        );
    }

    #[test]
    fn a_transition_side_shows_words_and_a_muted_track_does_not() {
        let report = report(&[(150, shot(1, 3))]);
        let (_dir, store) = temp_store();
        let hash = store
            .import(&transcript(1, vec![word("blend", 1, 2)]))
            .unwrap();
        let expect = range(with_transcript(&hash, 1.0), 0, 5);

        // Track 1's dissolve side is asset #1's material; the anchor resolves
        // on the silent clip below it.
        let crossfade = project_with(vec![
            track(vec![clip(1, 3, 5)]),
            track(vec![dissolve(1, Some(2), 5)]),
        ]);
        let outcome = evaluate_with(
            &crossfade,
            &report,
            &expect,
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
        assert_eq!(outcome.detail.as_deref(), Some("1/1 words complete"));

        // The same clip on a muted track is not in the mix.
        let mut muted = track(vec![Segment::Clip(Clip::new(2, 1, s(0), s(5)))]);
        muted.muted = true;
        let hidden = project_with(vec![track(vec![clip(1, 3, 5)]), muted]);
        let outcome = evaluate_with(
            &hidden,
            &report,
            &expect,
            &AnalysisStores::with_transcripts(store.clone()),
        );
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
        assert!(
            outcome
                .detail
                .unwrap()
                .contains("not on the checked sequence")
        );
    }

    #[test]
    fn absent_passes_when_the_asset_is_gone() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report(&[(150, shot(2, 2))]);
        let outcome = evaluate(
            &project,
            &report,
            &range(
                Expectation::Absent {
                    asset: Some(1),
                    group: None,
                    black: false,
                    tolerance_frames: 0,
                },
                0,
                5,
            ),
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
    }

    #[test]
    fn absent_fails_a_matching_blend_side_and_a_black_frame() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let on_blend = report(&[(150, blend(7, Some(2), Some(1)))]);
        let absent = Expectation::Absent {
            asset: Some(1),
            group: None,
            black: false,
            tolerance_frames: 0,
        };
        let outcome = evaluate(&project, &on_blend, &range(absent, 0, 5));
        assert_eq!(outcome.result, CheckResult::Fail);
        assert!(outcome.detail.unwrap().contains("blend #7"));

        let black_matters = Expectation::Absent {
            asset: Some(1),
            group: None,
            black: true,
            tolerance_frames: 0,
        };
        let report = report(&[(75, shot(2, 2)), (75, FrameSource::Black)]);
        let outcome = evaluate(&project, &report, &range(black_matters.clone(), 0, 5));
        assert_eq!(outcome.result, CheckResult::Fail);
        assert!(outcome.detail.unwrap().contains("frame 75 is black"));
        let outcome = evaluate(
            &project,
            &report,
            &range(
                Expectation::Absent {
                    asset: Some(1),
                    group: None,
                    black: false,
                    tolerance_frames: 0,
                },
                0,
                5,
            ),
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
    }

    #[test]
    fn absent_black_alone_passes_a_lit_range_and_fails_a_black_frame() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let black_only = absent(None, None, true, 0);
        let lit = report(&[(150, shot(2, 2))]);
        let outcome = evaluate(&project, &lit, &range(black_only.clone(), 0, 5));
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");

        let black = report(&[(75, shot(2, 2)), (75, FrameSource::Black)]);
        let outcome = evaluate(&project, &black, &range(black_only, 0, 5));
        assert_eq!(outcome.result, CheckResult::Fail);
        assert!(outcome.detail.unwrap().contains("frame 75 is black"));
    }

    #[test]
    fn no_sliver_merges_adjacent_spans_of_one_clip() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        // The overlay's cut split one clip into two spans; neither piece is a
        // sliver on its own only because the run merges them.
        let report = report(&[(1, shot(1, 1)), (1, shot(1, 1)), (149, shot(1, 1))]);
        let outcome = evaluate(
            &project,
            &report,
            &range(
                Expectation::NoSliver {
                    min_frames: 2,
                    asset: None,
                },
                0,
                5,
            ),
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
    }

    #[test]
    fn no_sliver_fails_a_real_one_frame_clip() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report(&[(75, shot(1, 1)), (1, shot(2, 2)), (74, shot(1, 1))]);
        let outcome = evaluate(
            &project,
            &report,
            &range(
                Expectation::NoSliver {
                    min_frames: 2,
                    asset: None,
                },
                0,
                5,
            ),
        );
        assert_eq!(outcome.result, CheckResult::Fail);
        let detail = outcome.detail.unwrap();
        assert!(detail.contains("clip #2"), "{detail}");
        assert!(detail.contains("from f75"), "{detail}");
    }

    #[test]
    fn no_sliver_measures_the_whole_shot_at_a_point_anchor() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report(&[(300, shot(1, 1)), (300, shot(2, 2))]);
        // The point sits in a 300-frame run; measuring only the one-frame
        // anchor would wrongly call it a sliver.
        let outcome = evaluate(
            &project,
            &report,
            &point(
                Expectation::NoSliver {
                    min_frames: 10,
                    asset: None,
                },
                5,
            ),
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
        assert_eq!(outcome.frames, Some((150, 151)), "the point is one frame");
    }

    #[test]
    fn no_sliver_filters_by_asset() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report(&[(1, shot(2, 2)), (30, shot(1, 1)), (29, shot(3, 3))]);
        let filtered = evaluate(
            &project,
            &report,
            &range(
                Expectation::NoSliver {
                    min_frames: 2,
                    asset: Some(1),
                },
                0,
                2,
            ),
        );
        assert_eq!(filtered.result, CheckResult::Pass, "{filtered:?}");
        let everything = evaluate(
            &project,
            &report,
            &range(
                Expectation::NoSliver {
                    min_frames: 2,
                    asset: None,
                },
                0,
                2,
            ),
        );
        assert_eq!(everything.result, CheckResult::Fail);
        assert!(everything.detail.unwrap().contains("asset #2"));
    }

    #[test]
    fn tolerance_trims_one_boundary_frame_from_each_end() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        // 30 frames: a stray frame of another asset at each boundary.
        let report = report(&[(1, shot(2, 2)), (28, shot(1, 1)), (1, shot(2, 2))]);
        let outcome = evaluate(&project, &report, &range(cam(Some(1), None, 1), 0, 1));
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
        assert_eq!(outcome.frames, Some((1, 29)));
        let strict = evaluate(&project, &report, &range(cam(Some(1), None, 0), 0, 1));
        assert_eq!(strict.result, CheckResult::Fail);
    }

    #[test]
    fn a_tolerance_that_swallows_the_span_is_unverifiable() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report(&[(3, shot(2, 2))]);
        let annotation = expectation(cam(Some(1), None, 2), 1, s(0), Seconds::new(1, 10));
        let outcome = evaluate(&project, &report, &annotation);
        assert_eq!(outcome.result, CheckResult::Unverifiable, "{outcome:?}");
        assert!(outcome.detail.unwrap().contains("tolerance_frames 2"));
    }

    #[test]
    fn a_29_97_range_rounds_with_the_report_builder() {
        let fps = Seconds::new(30000, 1001);
        let mut project = base_project();
        project.fps = Some(fps);
        project.timeline.tracks = vec![track(vec![clip(1, 1, 10)])];
        let report = report_at(fps, &[(1, shot(1, 1)), (29, shot(2, 2))]);
        let frame = Seconds::new(1001, 30000);
        let annotation = expectation(cam(Some(1), None, 0), 1, s(0), frame);
        let outcome = evaluate(&project, &report, &annotation);
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
        assert_eq!(outcome.frames, Some((0, 1)));

        // A range narrower than half a frame rounds to zero width: widen it
        // to one frame instead of checking nothing.
        let hair = expectation(cam(Some(1), None, 0), 1, s(0), Seconds::new(1, 100));
        let outcome = evaluate(&project, &report, &hair);
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
        assert_eq!(outcome.frames, Some((0, 1)));
    }

    #[test]
    fn a_point_anchor_is_exactly_one_frame() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report(&[(151, shot(1, 1)), (149, shot(2, 2))]);
        let outcome = evaluate(&project, &report, &point(cam(Some(1), None, 0), 5));
        assert_eq!(outcome.frames, Some((150, 151)));
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
    }

    #[test]
    fn a_sequence_mismatch_fails_before_the_anchor_is_read() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report(&[(150, shot(1, 1))]);
        let mut annotation = range(cam(Some(1), None, 0), 0, 5);
        annotation.sequence = 2;
        let outcome = evaluate(&project, &report, &annotation);
        assert_eq!(outcome.result, CheckResult::Fail);
        assert_eq!(outcome.detail.unwrap(), "rendered sequence #1, not #2");
    }

    #[test]
    fn a_revision_mismatch_only_blocks_the_project_dependent_checks() {
        let project = project_with(vec![track(vec![Segment::Clip(Clip {
            effects: vec![crop(Vec::new())],
            ..match clip(1, 1, 10) {
                Segment::Clip(clip) => clip,
                _ => unreachable!(),
            }
        })])]);
        let mut report = report(&[(150, shot(1, 1))]);
        report.revision = 1;
        let cam_outcome = evaluate(&project, &report, &range(cam(Some(1), None, 0), 0, 5));
        assert_eq!(cam_outcome.result, CheckResult::Pass);
        for expect in [
            Expectation::CropStable {
                max_move_pct_per_sec: 25.0,
                hold_size: true,
            },
            Expectation::WorkInFrame { margin_pct: 5.0 },
            Expectation::WordsComplete {
                min_coverage: 1.0,
                transcript: None,
            },
        ] {
            let outcome = evaluate(&project, &report, &range(expect.clone(), 0, 5));
            assert_eq!(
                outcome.result,
                CheckResult::Unverifiable,
                "{} at a stale revision",
                expect.name()
            );
            assert!(outcome.detail.unwrap().contains("re-export"));
        }
    }

    #[test]
    fn a_group_cam_needs_the_project_but_an_asset_cam_does_not() {
        let mut project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let group = project
            .add_group(
                "CAM 2",
                vec![MediaPart {
                    asset: 1,
                    group_start: s(0),
                    file_start: s(0),
                    duration: s(5),
                }],
            )
            .unwrap();
        let mut report = report(&[(150, shot(1, 1))]);
        report.revision = project.revision + 1;
        let by_asset = evaluate(&project, &report, &range(cam(Some(1), None, 0), 0, 5));
        assert_eq!(by_asset.result, CheckResult::Pass, "report-only");
        let by_group = evaluate(&project, &report, &range(cam(None, Some(group), 0), 0, 5));
        assert_eq!(
            by_group.result,
            CheckResult::Unverifiable,
            "a group edit after the render changes the expected set"
        );
        assert!(by_group.detail.unwrap().contains("re-export"));
    }

    #[test]
    fn a_frame_map_that_ends_early_fails_naming_the_frame() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let short = report(&[(30, shot(1, 1))]);
        let outcome = evaluate(&project, &short, &range(cam(Some(1), None, 0), 0, 5));
        assert_eq!(outcome.result, CheckResult::Fail);
        assert!(
            outcome
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("needs frames 0..150"),
            "{outcome:?}"
        );
        let empty = report(&[]);
        let outcome = evaluate(&project, &empty, &range(cam(Some(1), None, 0), 0, 5));
        assert_eq!(outcome.result, CheckResult::Fail);
        assert!(outcome.detail.unwrap().contains("render has 0 frame(s)"));
    }

    #[test]
    fn a_clamped_check_range_never_passes_a_truncated_render() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let mut report = report(&[(150, shot(1, 1))]);
        report.frame_count = 1; // the file holds one frame, the map claims 150
        let outcome = evaluate(&project, &report, &range(cam(Some(1), None, 0), 0, 5));
        assert_eq!(outcome.result, CheckResult::Fail, "{outcome:?}");
        assert_eq!(outcome.frames, Some((0, 1)));
        assert!(outcome.detail.unwrap().contains("render has 1 frame(s)"));
    }

    #[test]
    fn a_group_resolves_to_its_part_assets() {
        let mut project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let group = project
            .add_group(
                "CAM 2",
                vec![
                    MediaPart {
                        asset: 1,
                        group_start: s(0),
                        file_start: s(0),
                        duration: s(5),
                    },
                    MediaPart {
                        asset: 2,
                        group_start: s(5),
                        file_start: s(0),
                        duration: s(5),
                    },
                ],
            )
            .unwrap();
        let report = report(&[(150, shot(2, 2))]);
        let outcome = evaluate(&project, &report, &range(cam(None, Some(group), 0), 0, 5));
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
        let wrong = evaluate(&project, &report, &range(cam(Some(1), None, 0), 0, 5));
        assert_eq!(wrong.result, CheckResult::Fail);
    }

    #[test]
    fn validation_refuses_bad_payloads() {
        let cases = [
            (cam(Some(1), Some(2), 0), "exactly one"),
            (cam(None, None, 0), "exactly one"),
            (cam(Some(1), None, -1), "tolerance_frames"),
            (absent(None, None, false, 0), "nothing to be absent"),
            (absent(Some(1), Some(2), true, 0), "at most one"),
            (absent(Some(1), None, true, -1), "tolerance_frames"),
            (
                Expectation::CropStable {
                    max_move_pct_per_sec: -1.0,
                    hold_size: true,
                },
                "max_move_pct_per_sec",
            ),
            (
                Expectation::CropStable {
                    max_move_pct_per_sec: f64::NAN,
                    hold_size: true,
                },
                "max_move_pct_per_sec",
            ),
            (Expectation::WorkInFrame { margin_pct: 50.1 }, "margin_pct"),
            (
                Expectation::WorkInFrame {
                    margin_pct: f64::INFINITY,
                },
                "margin_pct",
            ),
            (
                Expectation::WordsComplete {
                    min_coverage: 1.1,
                    transcript: None,
                },
                "min_coverage",
            ),
            (
                Expectation::WordsComplete {
                    min_coverage: 1.0,
                    transcript: Some("take".into()),
                },
                "content hash",
            ),
            (
                Expectation::SubjectInFrame {
                    product: "take".into(),
                    asset: 1,
                    label: "face".into(),
                    subject_id: None,
                    margin_pct: 5.0,
                    min_confidence: 1.0,
                    tolerance_frames: 0,
                },
                "product",
            ),
            (
                Expectation::SubjectInFrame {
                    product: "a1-0000000000000000".into(),
                    asset: 1,
                    label: " ".into(),
                    subject_id: None,
                    margin_pct: 5.0,
                    min_confidence: 1.0,
                    tolerance_frames: 0,
                },
                "label",
            ),
            (
                Expectation::SubjectInFrame {
                    product: "a1-0000000000000000".into(),
                    asset: 1,
                    label: "face".into(),
                    subject_id: Some(String::new()),
                    margin_pct: 5.0,
                    min_confidence: 1.0,
                    tolerance_frames: 0,
                },
                "subject_id",
            ),
            (
                Expectation::SubjectInFrame {
                    product: "a1-0000000000000000".into(),
                    asset: 1,
                    label: "face".into(),
                    subject_id: None,
                    margin_pct: 50.1,
                    min_confidence: 1.0,
                    tolerance_frames: 0,
                },
                "margin_pct",
            ),
            (
                Expectation::SubjectInFrame {
                    product: "a1-0000000000000000".into(),
                    asset: 1,
                    label: "face".into(),
                    subject_id: None,
                    margin_pct: 5.0,
                    min_confidence: 1.1,
                    tolerance_frames: 0,
                },
                "min_confidence",
            ),
            (
                Expectation::SubjectInFrame {
                    product: "a1-0000000000000000".into(),
                    asset: 1,
                    label: "face".into(),
                    subject_id: None,
                    margin_pct: 5.0,
                    min_confidence: 1.0,
                    tolerance_frames: -1,
                },
                "tolerance_frames",
            ),
            (
                Expectation::NoSliver {
                    min_frames: 0,
                    asset: None,
                },
                "min_frames",
            ),
        ];
        for (expect, needle) in cases {
            let error = expect
                .validation_error()
                .unwrap_or_else(|| panic!("{} should be refused", expect.name()));
            assert!(error.contains(needle), "{}: {error}", expect.name());
        }
        for ok in [
            cam(Some(1), None, 0),
            absent(None, None, true, 0),
            absent(Some(1), None, true, 0),
            absent(None, Some(1), false, 0),
            Expectation::CropStable {
                max_move_pct_per_sec: 0.0,
                hold_size: false,
            },
            Expectation::WorkInFrame { margin_pct: 50.0 },
            Expectation::SubjectInFrame {
                product: "a1-0000000000000000".into(),
                asset: 1,
                label: "face".into(),
                subject_id: None,
                margin_pct: 0.0,
                min_confidence: 0.0,
                tolerance_frames: 0,
            },
            Expectation::WordsComplete {
                min_coverage: 0.0,
                transcript: None,
            },
            Expectation::WordsComplete {
                min_coverage: 1.0,
                transcript: Some("t1-0000000000000000".into()),
            },
            Expectation::NoSliver {
                min_frames: 1,
                asset: None,
            },
        ] {
            assert_eq!(ok.validation_error(), None, "{}", ok.name());
        }
    }

    #[test]
    fn the_project_refuses_a_source_anchored_expectation() {
        let mut project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let anchor = Anchor::Source {
            source: crate::annotation::SourceRef::Asset { id: 1 },
            time: s(1),
        };
        let error = project
            .add_annotation(
                1,
                AnnotationKind::Expectation {
                    expect: cam(Some(1), None, 0),
                },
                anchor,
                Provenance {
                    actor: Actor::Human,
                    at: 0,
                },
            )
            .expect_err("a source anchor has no frame range to check");
        let message = error.to_string();
        assert!(message.contains("timeline"), "{message}");
    }

    #[test]
    fn a_hand_edited_expectation_is_refused_on_load() {
        let mut project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        project
            .add_annotation(
                1,
                AnnotationKind::Expectation {
                    expect: Expectation::WorkInFrame { margin_pct: 5.0 },
                },
                Anchor::Timeline {
                    start: ClipPoint {
                        clip: 1,
                        source: s(0),
                    },
                    end: ClipPoint {
                        clip: 1,
                        source: s(5),
                    },
                },
                Provenance {
                    actor: Actor::Human,
                    at: 0,
                },
            )
            .unwrap();
        let value = serde_json::to_value(&project).unwrap();
        let anchor = value["annotations"][0]["anchor"].clone();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nile.json");

        let mut source_anchored = value.clone();
        source_anchored["annotations"][0]["anchor"] = serde_json::json!({
            "on": "source",
            "source": { "of": "asset", "id": 1 },
            "time": "1/1",
        });
        std::fs::write(&path, source_anchored.to_string()).unwrap();
        let error = Project::load(&path).expect_err("a source anchor has no frames");
        assert!(error.to_string().contains("timeline"), "{error}");

        let mut bad_margin = value;
        bad_margin["annotations"][0]["anchor"] = anchor;
        bad_margin["annotations"][0]["kind"]["expect"]["margin_pct"] = 80.0.into();
        std::fs::write(&path, bad_margin.to_string()).unwrap();
        let error = Project::load(&path).expect_err("margin 80 is out of range");
        assert!(error.to_string().contains("margin_pct"), "{error}");
    }

    #[test]
    fn an_expectation_round_trips_through_the_annotation_json() {
        let annotation = range(
            Expectation::Cam {
                asset: Some(1),
                group: None,
                tolerance_frames: 2,
            },
            0,
            5,
        );
        let value = serde_json::to_value(&annotation).unwrap();
        assert_eq!(value["kind"]["kind"], "expectation");
        assert_eq!(value["kind"]["expect"]["check"], "cam");
        assert_eq!(value["kind"]["expect"]["asset"], 1);
        assert_eq!(value["kind"]["expect"]["tolerance_frames"], 2);
        assert!(
            value["kind"]["expect"].get("group").is_none(),
            "None fields stay out of the file: {value}"
        );
        let back: Annotation = serde_json::from_value(value).unwrap();
        assert_eq!(back, annotation);

        // Defaults are part of the wire contract.
        let defaults: Expectation = serde_json::from_value(serde_json::json!({
            "check": "work_in_frame"
        }))
        .unwrap();
        assert_eq!(defaults, Expectation::WorkInFrame { margin_pct: 5.0 });
        let defaults: Expectation = serde_json::from_value(serde_json::json!({
            "check": "no_sliver"
        }))
        .unwrap();
        assert_eq!(
            defaults,
            Expectation::NoSliver {
                min_frames: 2,
                asset: None
            }
        );
    }

    #[test]
    fn unknown_payload_fields_are_refused() {
        let bad = serde_json::json!({ "check": "cam", "asset": 1, "bogus": true });
        assert!(serde_json::from_value::<Expectation>(bad).is_err());
    }

    #[test]
    fn a_check_report_summarises_its_outcomes() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report(&[(150, shot(1, 1))]);
        // Resolving acknowledges a verdict; it does not waive the check.
        let mut acknowledged = range(cam(Some(2), None, 0), 0, 5);
        acknowledged.status = AnnotationStatus::Resolved;
        let checks = vec![
            evaluate(&project, &report, &range(cam(Some(1), None, 0), 0, 5)),
            evaluate(&project, &report, &acknowledged),
            evaluate(
                &project,
                &report,
                &range(
                    Expectation::WordsComplete {
                        min_coverage: 1.0,
                        transcript: Some("t1-0000000000000000".into()),
                    },
                    0,
                    5,
                ),
            ),
        ];
        let report = CheckReport::new(&report, checks);
        assert_eq!(report.sequence(), 1, "the report's own sequence");
        assert_eq!(report.summary().pass, 1);
        assert_eq!(report.summary().fail, 1, "resolved still fails");
        assert_eq!(report.summary().unverifiable, 1);
        assert_eq!(report.report().frame_count, 150);
        assert_eq!(report.report().revision, 0);
        assert_eq!(report.checks().len(), 3);
        assert_eq!(report.checks()[1].status, AnnotationStatus::Resolved);
        let json = serde_json::to_value(&report).unwrap();
        assert!(json.get("sequence").is_none(), "no duplicate sequence");
        assert_eq!(json["summary"]["fail"], 1);
        assert_eq!(json["checks"][1]["result"], "fail");
        assert_eq!(json["checks"][0]["status"], "open");
        assert_eq!(json["checks"][1]["status"], "resolved");
        assert_eq!(
            json["checks"][0]["span"],
            serde_json::json!({ "start": "0/1", "end": "5/1" })
        );
        let back: CheckReport = serde_json::from_value(json).unwrap();
        assert_eq!(back, report, "the reply round-trips through the wire");
    }

    #[test]
    fn work_in_frame_reads_the_key_at_zero_not_the_static_fallback() {
        // The static fields say 10,10,80,80, but the key at 0 opens the
        // window to the full frame for the first two seconds.
        let project = with_crop(Effect::Crop {
            x: 10.0,
            y: 10.0,
            w: 80.0,
            h: 80.0,
            keys: vec![
                key(0, 0.0, 0.0, 100.0, 100.0),
                key(2, 10.0, 10.0, 80.0, 80.0),
            ],
        });
        let report = report(&[(150, shot(1, 1))]);
        let outcome = evaluate(
            &project,
            &report,
            &range(Expectation::WorkInFrame { margin_pct: 5.0 }, 0, 5),
        );
        assert_eq!(outcome.result, CheckResult::Fail, "{outcome:?}");
        assert!(outcome.detail.unwrap().contains("x 0.0"));
    }

    #[test]
    fn work_in_frame_checks_a_point_anchor() {
        let report = report(&[(150, shot(1, 1))]);
        let outside = with_crop(Effect::Crop {
            x: 0.0,
            y: 0.0,
            w: 50.0,
            h: 50.0,
            keys: Vec::new(),
        });
        let outcome = evaluate(
            &outside,
            &report,
            &point(Expectation::WorkInFrame { margin_pct: 5.0 }, 1),
        );
        assert_eq!(outcome.result, CheckResult::Fail, "{outcome:?}");

        let inside = with_crop(crop(Vec::new())); // 10,10,80,80
        let outcome = evaluate(
            &inside,
            &report,
            &point(Expectation::WorkInFrame { margin_pct: 5.0 }, 1),
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
    }

    #[test]
    fn a_blend_span_has_nothing_to_check_for_work_in_frame() {
        let project = with_crop(crop(Vec::new()));
        let report = report(&[(150, blend(7, Some(1), None))]);
        let outcome = evaluate(
            &project,
            &report,
            &range(Expectation::WorkInFrame { margin_pct: 5.0 }, 0, 5),
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
    }

    #[test]
    fn a_crop_outside_the_anchor_is_not_checked() {
        // The clip pans too fast, but only after 4 s; the anchor ends at 2 s.
        let project = with_crop(crop(vec![
            key(4, 10.0, 10.0, 80.0, 80.0),
            key(6, 90.0, 10.0, 5.0, 80.0),
        ]));
        let report = report(&[(150, shot(1, 1))]);
        let outcome = evaluate(
            &project,
            &report,
            &range(
                Expectation::CropStable {
                    max_move_pct_per_sec: 5.0,
                    hold_size: false,
                },
                0,
                2,
            ),
        );
        assert_eq!(outcome.result, CheckResult::Pass, "{outcome:?}");
    }

    #[test]
    fn a_non_expectation_annotation_is_unverifiable() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report(&[(150, shot(1, 1))]);
        let annotation = Annotation {
            kind: AnnotationKind::Marker,
            ..range(cam(Some(1), None, 0), 0, 5)
        };
        let outcome = evaluate(&project, &report, &annotation);
        assert_eq!(outcome.result, CheckResult::Unverifiable);
        assert_eq!(outcome.check, "marker");
        assert_eq!(outcome.detail.unwrap(), "not an expectation");
    }

    #[test]
    fn a_missing_group_is_unverifiable() {
        let project = project_with(vec![track(vec![clip(1, 1, 10)])]);
        let report = report(&[(150, shot(1, 1))]);
        let outcome = evaluate(&project, &report, &range(cam(None, Some(9), 0), 0, 5));
        assert_eq!(outcome.result, CheckResult::Unverifiable);
        assert!(outcome.detail.unwrap().contains("group #9"));
    }

    #[test]
    fn a_transition_payload_is_reachable_from_the_timeline() {
        // A real transition on the track is reported as a blend; the camera
        // check reads the report, so the project need not be consulted.
        let transition = Segment::Transition(crate::timeline::Transition {
            id: 4,
            dur: s(1),
            kind: TransitionKind::Dissolve,
            a: TransitionSource::Clip {
                asset: 1,
                src_in: s(0),
                src_out: s(1),
                effects: Vec::new(),
            },
            b: TransitionSource::Black,
        });
        let project = project_with(vec![track(vec![clip(1, 1, 2), transition])]);
        let report = report(&[(60, shot(1, 1)), (30, blend(4, Some(1), None))]);
        let outcome = evaluate(&project, &report, &range(cam(Some(1), None, 0), 0, 3));
        assert_eq!(outcome.result, CheckResult::Fail, "the black side fails");
    }
}
