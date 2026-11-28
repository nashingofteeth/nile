//! A render's frame map: output frames back to the sequence and source.
//!
//! Written beside the exported file (`<output>.report.json`). The map is
//! piecewise — one span per composite cut — not one row per frame, so an
//! hour-long deliverable stays small. Source anchors and
//! expectation checks read this instead of probing the video.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::project::Project;
use crate::time::{Seconds, format_seconds, frame_to_seconds_nearest, rational_ser, secs_f64};
use crate::timeline::{Segment, TransitionSource, source_time};

/// The report schema version, independent of `nile.json`/history versions.
pub const REPORT_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum ReportError {
    #[error("report file {0:?} does not exist")]
    NotFound(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid report file: {0}")]
    Json(#[from] serde_json::Error),
    #[error("report version {0} is not supported (this build reads v{expected})", expected = REPORT_VERSION)]
    Version(u32),
    #[error("invalid report: {0}")]
    Invalid(String),
}

/// One exported render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderReport {
    pub version: u32,
    pub sequence: u32,
    pub revision: u64,
    /// The output path as the caller named it.
    pub output: String,
    #[serde(with = "rational_ser")]
    pub fps: Seconds,
    #[serde(with = "rational_ser")]
    pub duration: Seconds,
    /// Actual decoded frames in the finished file.
    pub frame_count: i64,
    #[serde(default)]
    pub windows: Vec<RenderWindow>,
    #[serde(default)]
    pub frame_map: Vec<FrameSpan>,
}

impl RenderReport {
    /// Build a report for the active sequence and an already-exported file.
    /// `frame_count` is probed from the output; the frame map is derived from
    /// the project, so it never needs a second render.
    pub fn build(
        project: &Project,
        output: &Path,
        frame_count: i64,
        encoder: Option<String>,
    ) -> Self {
        let fps = project.fps.unwrap_or_else(|| Seconds::from_integer(30));
        let duration = project.timeline.end();
        Self {
            version: REPORT_VERSION,
            sequence: project.active_id(),
            revision: project.revision,
            output: output.display().to_string(),
            fps,
            duration,
            frame_count,
            windows: vec![RenderWindow {
                start_frame: 0,
                frames: frame_count.max(0),
                encoder,
            }],
            frame_map: build_frame_map(project, fps),
        }
    }

    /// The output-relative sidecar path (`out.mp4` → `out.mp4.report.json`).
    pub fn sidecar_path(output: &Path) -> PathBuf {
        let mut path = output.as_os_str().to_os_string();
        path.push(".report.json");
        PathBuf::from(path)
    }

    pub fn save(&self, path: &Path) -> Result<(), ReportError> {
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let tmp = NamedTempFile::new_in(dir)?;
        tmp.as_file().write_all(json.as_bytes())?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, ReportError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ReportError::NotFound(path.to_path_buf()));
            }
            Err(e) => return Err(e.into()),
        };
        let report: Self = serde_json::from_str(&text)?;
        if report.version != REPORT_VERSION {
            return Err(ReportError::Version(report.version));
        }
        report.validate()?;
        Ok(report)
    }

    fn validate(&self) -> Result<(), ReportError> {
        if self.fps <= Seconds::from_integer(0) {
            return Err(ReportError::Invalid("fps must be positive".into()));
        }
        if self.frame_count < 0 {
            return Err(ReportError::Invalid("negative frame count".into()));
        }
        let mut cursor = 0;
        for span in &self.frame_map {
            if span.frames <= 0 || span.start_frame < cursor {
                return Err(ReportError::Invalid(
                    "frame spans must be positive and ordered".into(),
                ));
            }
            if span.timeline_end < span.timeline_start {
                return Err(ReportError::Invalid(
                    "a timeline span ends before it starts".into(),
                ));
            }
            cursor = span.start_frame + span.frames;
        }
        let mut cursor = 0;
        for window in &self.windows {
            if window.frames <= 0 || window.start_frame < cursor {
                return Err(ReportError::Invalid(
                    "render windows must be positive and ordered".into(),
                ));
            }
            cursor = window.start_frame + window.frames;
        }
        Ok(())
    }

    /// The span covering output frame `frame`.
    pub fn span_at(&self, frame: i64) -> Option<&FrameSpan> {
        self.frame_map
            .iter()
            .find(|span| frame >= span.start_frame && frame < span.start_frame + span.frames)
    }

    /// Output frame → timeline time + the material visible there.
    pub fn locate(&self, frame: i64) -> Option<FrameLocation> {
        if self.fps <= Seconds::from_integer(0) {
            return None;
        }
        let span = self.span_at(frame)?;
        // Checked: a report can carry a hostile fps, and `(frame /
        // span.start_frame) / fps` would panic in `Ratio` at an extreme rate.
        let offset = frame_to_seconds_nearest(frame - span.start_frame, self.fps);
        let timeline = span.timeline_start + offset;
        Some(match &span.source {
            FrameSource::Clip {
                clip,
                asset,
                source_start,
                speed,
                ..
            } => FrameLocation::Clip {
                timeline,
                clip: *clip,
                asset: *asset,
                source: source_time(*source_start, *speed, offset),
            },
            FrameSource::Blend { id, .. } => FrameLocation::Blend { timeline, id: *id },
            FrameSource::Black => FrameLocation::Black { timeline },
        })
    }

    /// One line for `nile report`.
    pub fn summary(&self, path: &Path) -> String {
        format!(
            "✓ {} — {} frames @ {}/{} · {} s · {} span(s) · {}",
            path.display(),
            self.frame_count,
            self.fps.numer(),
            self.fps.denom(),
            format_seconds(&self.duration),
            self.frame_map.len(),
            self.output
        )
    }
}

/// One contiguous output window (a chunked export has several).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderWindow {
    pub start_frame: i64,
    pub frames: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder: Option<String>,
}

/// A contiguous composite span in the output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameSpan {
    pub start_frame: i64,
    pub frames: i64,
    #[serde(with = "rational_ser")]
    pub timeline_start: Seconds,
    #[serde(with = "rational_ser")]
    pub timeline_end: Seconds,
    pub source: FrameSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrameSource {
    Clip {
        clip: u32,
        asset: u32,
        #[serde(with = "rational_ser")]
        source_start: Seconds,
        #[serde(with = "rational_ser")]
        source_end: Seconds,
        #[serde(with = "rational_ser")]
        speed: Seconds,
    },
    Blend {
        id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        a: Option<SourceSide>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        b: Option<SourceSide>,
    },
    Black,
}

/// One asset side of a blend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceSide {
    pub asset: u32,
    #[serde(with = "rational_ser")]
    pub source_start: Seconds,
    #[serde(with = "rational_ser")]
    pub source_end: Seconds,
}

/// What an output frame resolves to.
#[derive(Debug, Clone, PartialEq)]
pub enum FrameLocation {
    Clip {
        timeline: Seconds,
        clip: u32,
        asset: u32,
        source: Seconds,
    },
    Blend {
        timeline: Seconds,
        id: u32,
    },
    Black {
        timeline: Seconds,
    },
}

fn build_frame_map(project: &Project, fps: Seconds) -> Vec<FrameSpan> {
    // Every layer is padded to the timeline length, so the export can outlast
    // the last picture cut (a trailing audio bed, a longer muted track). Seed
    // the end too, or the map stops early and those frames resolve to `None`.
    let mut cuts = vec![Seconds::from_integer(0), project.timeline.end()];
    for track in project.timeline.video_tracks().filter(|track| !track.muted) {
        let mut at = Seconds::from_integer(0);
        for segment in &track.segments {
            at += segment.duration();
            cuts.push(at);
        }
    }
    cuts.sort();
    cuts.dedup();

    let mut spans = Vec::new();
    for pair in cuts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if end <= start {
            continue;
        }
        let start_frame = frame_edge(&start, &fps);
        let end_frame = frame_edge(&end, &fps);
        if end_frame <= start_frame {
            continue;
        }
        let source = topmost_source(project, start, end).unwrap_or(FrameSource::Black);
        spans.push(FrameSpan {
            start_frame,
            frames: end_frame - start_frame,
            timeline_start: start,
            timeline_end: end,
            source,
        });
    }
    spans
}

/// The output frame a timeline time lands on: `round(t × fps)`.
///
/// The frame map is built with this, and every reader of it — the expectation
/// evaluator, `nile report` — must use the same function. A second quantiser
/// drifts by a frame at 29.97 and the two disagree about which frame a cut is
/// on.
pub fn frame_edge(at: &Seconds, fps: &Seconds) -> i64 {
    (secs_f64(at) * secs_f64(fps)).round() as i64
}

/// The highest unmuted video texture at `at`; a gap lets the layer below
/// show through, and no layer at all is black.
fn topmost_source(project: &Project, at: Seconds, until: Seconds) -> Option<FrameSource> {
    for track in project
        .timeline
        .tracks
        .iter()
        .rev()
        .filter(|track| track.kind == crate::timeline::TrackKind::Video)
    {
        if track.muted {
            continue;
        }
        let Some(index) = track.index_at(at) else {
            continue;
        };
        match &track.segments[index] {
            Segment::Gap(_) => continue,
            Segment::Clip(clip) => {
                let start = track.start_of(index);
                let segment_end = start + track.segments[index].duration();
                return Some(FrameSource::Clip {
                    clip: clip.id,
                    asset: clip.asset,
                    source_start: clip.source_at(at - start),
                    source_end: clip.source_at(until.min(segment_end) - start),
                    speed: clip.speed,
                });
            }
            Segment::Transition(transition) => {
                return Some(FrameSource::Blend {
                    id: transition.id,
                    a: side(&transition.a),
                    b: side(&transition.b),
                });
            }
        }
    }
    None
}

fn side(source: &TransitionSource) -> Option<SourceSide> {
    match source {
        TransitionSource::Clip {
            asset,
            src_in,
            src_out,
            ..
        } => Some(SourceSide {
            asset: *asset,
            source_start: *src_in,
            source_end: *src_out,
        }),
        TransitionSource::Black => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{RawFormat, RawProbe, RawStream};
    use crate::timeline::{Clip, Gap, Track, TrackKind, Transition, TransitionKind};

    fn probe() -> RawProbe {
        RawProbe {
            streams: vec![RawStream {
                index: 0,
                codec_type: Some("video".into()),
                width: Some(320),
                height: Some(240),
                r_frame_rate: Some("30/1".into()),
                ..Default::default()
            }],
            format: RawFormat {
                format_name: "mp4".into(),
                duration: Some("10".into()),
                ..Default::default()
            },
        }
    }

    fn clip(id: u32, asset: u32, in_: i64, out: i64) -> Segment {
        Segment::Clip(Clip::new(
            id,
            asset,
            Seconds::from_integer(in_),
            Seconds::from_integer(out),
        ))
    }

    fn gap(duration: i64) -> Segment {
        Segment::Gap(Gap {
            duration: Seconds::from_integer(duration),
        })
    }

    fn track(segments: Vec<Segment>) -> Track {
        Track {
            kind: TrackKind::Video,
            muted: false,
            segments,
        }
    }

    fn project(tracks: Vec<Track>) -> Project {
        let mut project = Project::new();
        project.fps = Some(Seconds::from_integer(30));
        project.video = Some(crate::VideoSettings {
            width: 320,
            height: 240,
        });
        for name in ["a.mp4", "b.mp4", "c.mp4", "d.mp4"] {
            project
                .add_asset(format!("/media/{name}").into(), probe())
                .expect("asset");
        }
        project.timeline.tracks = tracks;
        project
    }

    #[test]
    fn the_map_uses_the_topmost_unmuted_track_and_goes_black() {
        let project = project(vec![
            track(vec![clip(1, 1, 0, 2)]),
            track(vec![gap(1), clip(2, 2, 0, 1), gap(1)]),
            {
                let mut muted = track(vec![clip(3, 3, 0, 3)]);
                muted.muted = true;
                muted
            },
        ]);
        let report = RenderReport::build(&project, Path::new("out.mp4"), 90, None);

        assert_eq!(report.frame_map.len(), 3);
        assert_eq!(report.frame_map[0].start_frame, 0);
        assert!(matches!(
            report.frame_map[0].source,
            FrameSource::Clip { clip: 1, .. }
        ));
        assert!(matches!(
            report.frame_map[1].source,
            FrameSource::Clip { clip: 2, .. }
        ));
        // Base ends at 2 s; the overlay is a gap there and the muted track
        // does not count, so the tail is black.
        assert_eq!(report.frame_map[2].start_frame, 60);
        assert_eq!(report.frame_map[2].frames, 30);
        assert_eq!(report.frame_map[2].source, FrameSource::Black);

        assert_eq!(
            report.locate(45),
            Some(FrameLocation::Clip {
                timeline: Seconds::new(3, 2),
                clip: 2,
                asset: 2,
                source: Seconds::new(1, 2),
            })
        );
        assert_eq!(report.locate(90), None, "the last frame is exclusive");
    }

    #[test]
    fn a_trailing_audio_bed_extends_the_black_tail() {
        let mut audio = track(vec![clip(2, 2, 0, 5)]);
        audio.kind = TrackKind::Audio;
        let project = project(vec![track(vec![clip(1, 1, 0, 2)]), audio]);
        let report = RenderReport::build(&project, Path::new("out.mp4"), 150, None);
        assert_eq!(report.duration, Seconds::from_integer(5));
        assert_eq!(report.frame_count, 150);
        let last = report.frame_map.last().expect("tail span");
        assert_eq!(last.start_frame, 60, "black starts where picture ends");
        assert_eq!(last.start_frame + last.frames, 150);
        assert_eq!(last.source, FrameSource::Black);
        assert_eq!(
            report.locate(90),
            Some(FrameLocation::Black {
                timeline: Seconds::from_integer(3)
            })
        );
    }

    #[test]
    fn source_time_scales_with_speed() {
        let mut c = clip(1, 1, 0, 4);
        if let Segment::Clip(clip) = &mut c {
            clip.speed = Seconds::from_integer(2);
        }
        let project = project(vec![track(vec![c])]);
        let report = RenderReport::build(&project, Path::new("out.mp4"), 60, None);
        assert!(matches!(
            report.locate(30),
            Some(FrameLocation::Clip {
                source,
                ..
            }) if source == Seconds::from_integer(2)
        ));
    }

    #[test]
    fn a_blend_reports_both_sides() {
        let transition = Segment::Transition(Transition {
            id: 7,
            dur: Seconds::from_integer(1),
            kind: TransitionKind::Dissolve,
            a: TransitionSource::Clip {
                asset: 1,
                src_in: Seconds::from_integer(0),
                src_out: Seconds::from_integer(1),
                effects: Vec::new(),
            },
            b: TransitionSource::Clip {
                asset: 2,
                src_in: Seconds::from_integer(2),
                src_out: Seconds::from_integer(3),
                effects: Vec::new(),
            },
        });
        let project = project(vec![track(vec![transition])]);
        let report = RenderReport::build(&project, Path::new("out.mp4"), 30, None);
        assert!(matches!(
            &report.frame_map[0].source,
            FrameSource::Blend {
                id: 7,
                a: Some(a),
                b: Some(b),
            } if a.asset == 1 && b.asset == 2
        ));
        assert_eq!(
            report.locate(15),
            Some(FrameLocation::Blend {
                timeline: Seconds::new(1, 2),
                id: 7
            })
        );
    }

    #[test]
    fn locate_falls_back_to_a_whole_second_at_an_unrepresentable_grid() {
        // A report is local state and can be hand-edited; `locate` is a reader
        // and must not panic when a frame / fps pair overflows `Ratio<i64>`.
        let report = RenderReport {
            version: REPORT_VERSION,
            sequence: 1,
            revision: 0,
            output: "out.mp4".into(),
            fps: Seconds::new(1, i64::MAX),
            duration: Seconds::from_integer(1),
            frame_count: 3,
            windows: Vec::new(),
            frame_map: vec![FrameSpan {
                start_frame: 0,
                frames: 3,
                timeline_start: Seconds::from_integer(0),
                timeline_end: Seconds::from_integer(1),
                source: FrameSource::Black,
            }],
        };
        assert_eq!(
            report.locate(2),
            Some(FrameLocation::Black {
                timeline: Seconds::from_integer(i64::MAX),
            }),
            "the nearest-safe whole second, not a panic"
        );
    }

    #[test]
    fn the_sidecar_round_trips_and_rejects_an_old_version() {
        let project = project(vec![track(vec![clip(1, 1, 0, 2)])]);
        let report = RenderReport::build(&project, Path::new("out.mp4"), 60, Some("CPU".into()));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.mp4.report.json");
        report.save(&path).unwrap();
        assert_eq!(RenderReport::load(&path).unwrap(), report);
        assert_eq!(
            RenderReport::sidecar_path(Path::new("out.mp4")),
            PathBuf::from("out.mp4.report.json")
        );

        let mut old = report.clone();
        old.version = 0;
        old.save(&path).unwrap();
        assert!(matches!(
            RenderReport::load(&path),
            Err(ReportError::Version(0))
        ));

        let mut bad_fps = report;
        bad_fps.fps = Seconds::from_integer(0);
        bad_fps.save(&path).unwrap();
        assert!(matches!(
            RenderReport::load(&path),
            Err(ReportError::Invalid(_))
        ));
    }
}
