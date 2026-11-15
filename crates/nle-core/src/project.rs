//! The project file: media pool + timeline, serialized as JSON (the EDL).
//!
//! Times are exact rationals serialized as "n/d" strings; positions never
//! drift no matter how many times you edit.

use std::fs;
use std::io::Write as _;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::media::RawProbe;
use crate::time::{Seconds, grid_time, rational_ser, secs_f64};
use crate::timeline::{Segment, Timeline, TimelineError, TrackKind};

/// Project file format version. No migration path is kept (see
/// README "Compatibility: none, by design"): old files are rejected,
/// never silently misread. v3 added the per-track `muted` flag;
/// v4 added the per-clip `speed` rate; v5 added the stored playhead;
/// v7 stores `crop` as percentages (scale/x/y) instead of pixel w/h/x/y;
/// v8 stores `crop` as per-edge percentage insets and `scale` as percentages;
/// v9 stores each transition side's effects (they render the neighbour);
/// v10 stores the monotonic model `revision`;
/// v11 moves fps/video/timeline/playhead onto one or more `Sequence`s.
pub const PROJECT_VERSION: u32 = 11;

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("project file {0:?} already exists (use --force to overwrite)")]
    Exists(PathBuf),
    #[error("no project at {0:?} — run `nile init` first")]
    NotFound(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid project file: {0}")]
    Json(#[from] serde_json::Error),
    #[error("project version {0} is not supported (this build reads v{expected})", expected = PROJECT_VERSION)]
    Version(u32),
    #[error(transparent)]
    Timeline(#[from] TimelineError),
    #[error("asset #{0}: {1}")]
    Asset(u32, String),
    #[error("asset #{0} not found")]
    AssetNotFound(u32),
    #[error("asset #{0} is used by clip(s) {1:?} — remove them first")]
    AssetInUse(u32, Vec<u32>),
    #[error("project has no sequences")]
    NoSequences,
    #[error("sequence #{0} not found")]
    SequenceNotFound(u32),
    #[error("sequence #{0} is duplicated")]
    SequenceDuplicate(u32),
    #[error("sequence #{0} has a non-positive frame rate")]
    NonPositiveFps(u32),
}

/// Output sequence settings (set from the first clip, changeable later).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoSettings {
    pub width: u32,
    pub height: u32,
}

/// One file in the media pool. `probe` is stored so the file never
/// needs re-probing — the project is self-describing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Asset {
    pub id: u32,
    pub path: PathBuf,
    pub probe: RawProbe,
}

/// Why a file with this probe can't go on a `kind` track, if it can't —
/// the one rule, used by [`Project::validate`] and by the import paths
/// (which check before pooling, so a refusal leaves no half-done import).
/// A probe with no streams at all is "unknown", not "absent".
pub fn wrong_kind(path: &Path, probe: &RawProbe, kind: TrackKind) -> Option<String> {
    let (need, other, ok) = match kind {
        TrackKind::Video => ("video", "an audio", probe.has_video()),
        TrackKind::Audio => ("audio", "a video", probe.has_audio()),
    };
    if (probe.has_video() || probe.has_audio()) && !ok {
        return Some(format!(
            "{} has no {need} stream — put it on {other} track instead",
            path.display()
        ));
    }
    None
}

impl Asset {
    pub fn duration(&self) -> Option<Seconds> {
        self.probe.duration()
    }

    pub fn fps(&self) -> Option<Seconds> {
        self.probe.fps()
    }

    pub fn video_size(&self) -> Option<(u32, u32)> {
        self.probe.size()
    }

    /// See [`wrong_kind`].
    pub fn wrong_kind(&self, kind: TrackKind) -> Option<String> {
        wrong_kind(&self.path, &self.probe, kind)
    }
}

/// Where a sequence came from. Derived sequences are future work, but the
/// provenance is recorded from day one so they stay traceable.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum SequenceOrigin {
    #[default]
    Root,
    Derived {
        from: u32,
        spec: serde_json::Value,
    },
}

/// One editable timeline. A project may hold several (e.g. 16:9 and 9:16
/// deliverables); the active one is the session scope the front-ends edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sequence {
    pub id: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub origin: SequenceOrigin,
    /// Count of manual edits applied to this sequence (derived sequences
    /// later use it to decide what can be regenerated).
    #[serde(default)]
    pub manual_edits: u64,
    /// Output frame rate — None until the first clip sets it.
    #[serde(default, with = "rational_ser::opt")]
    pub fps: Option<Seconds>,
    /// Output size — None until the first clip sets it.
    #[serde(default)]
    pub video: Option<VideoSettings>,
    #[serde(default)]
    pub timeline: Timeline,
    /// The stored playhead for this sequence.
    #[serde(default, with = "rational_ser")]
    pub playhead: Seconds,
}

impl Sequence {
    pub fn new(id: u32, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            origin: SequenceOrigin::Root,
            manual_edits: 0,
            fps: None,
            video: None,
            timeline: Timeline::new(),
            playhead: Seconds::from_integer(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    /// Project file format — v3 is the segment model with multi-track
    /// (`Track.muted`). (v1/v2 files are not readable; the only v1/v2
    /// project was the regenerable demo.)
    version: u32,
    /// Monotonic model revision. Edits, pool/track changes, undo/redo and
    /// whole-project rebuilds bump it; playhead-only saves do not (a save
    /// whose clamp repaired the model does).
    #[serde(default)]
    pub revision: u64,
    /// The media pool is shared by every sequence.
    #[serde(default)]
    pub media: Vec<Asset>,
    /// One or more editable timelines.
    #[serde(default)]
    pub sequences: Vec<Sequence>,
    /// The active sequence id. `Deref` exposes its `fps`/`video`/`timeline`/
    /// `playhead` so existing edit paths stay sequence-agnostic.
    #[serde(default = "default_active")]
    active: u32,
}

fn default_active() -> u32 {
    1
}

impl Deref for Project {
    type Target = Sequence;

    fn deref(&self) -> &Sequence {
        self.active_sequence()
    }
}

impl DerefMut for Project {
    fn deref_mut(&mut self) -> &mut Sequence {
        self.active_sequence_mut()
    }
}

impl Default for Project {
    fn default() -> Self {
        Self::new()
    }
}

impl Project {
    pub fn new() -> Self {
        Self {
            version: PROJECT_VERSION,
            revision: 0,
            media: Vec::new(),
            sequences: vec![Sequence::new(1, "Sequence 1")],
            active: 1,
        }
    }

    pub fn active_id(&self) -> u32 {
        self.active
    }

    pub fn active_sequence(&self) -> &Sequence {
        self.sequences
            .iter()
            .find(|sequence| sequence.id == self.active)
            .expect("active sequence")
    }

    pub fn active_sequence_mut(&mut self) -> &mut Sequence {
        self.sequences
            .iter_mut()
            .find(|sequence| sequence.id == self.active)
            .expect("active sequence")
    }

    pub fn sequence(&self, id: u32) -> Option<&Sequence> {
        self.sequences.iter().find(|sequence| sequence.id == id)
    }

    pub fn sequence_mut(&mut self, id: u32) -> Option<&mut Sequence> {
        self.sequences.iter_mut().find(|sequence| sequence.id == id)
    }

    /// Make `id` the active sequence. Callers that own a per-sequence undo
    /// history must switch that too.
    pub fn select_sequence(&mut self, id: u32) -> Result<(), ProjectError> {
        if self.sequence(id).is_none() {
            return Err(ProjectError::SequenceNotFound(id));
        }
        self.active = id;
        Ok(())
    }

    /// Add an empty sequence and return its id. It does not become active.
    pub fn add_sequence(
        &mut self,
        name: impl Into<String>,
        fps: Option<Seconds>,
        video: Option<VideoSettings>,
    ) -> u32 {
        let id = self.next_sequence_id();
        let mut sequence = Sequence::new(id, name);
        sequence.fps = fps;
        sequence.video = video;
        self.sequences.push(sequence);
        id
    }

    pub(crate) fn next_sequence_id(&self) -> u32 {
        self.sequences
            .iter()
            .map(|sequence| sequence.id)
            .max()
            .unwrap_or(0)
            + 1
    }

    /// Count one manual edit on the active sequence.
    pub(crate) fn bump_manual_edits(&mut self) {
        let edits = self.active_sequence().manual_edits;
        self.active_sequence_mut().manual_edits = edits.saturating_add(1);
    }

    /// Bump the monotonic model revision. Crate-private: the mutation paths
    /// (`History`, `Document::rebuild`, `Document::set_sequence` and
    /// `Document::save`) own the rule.
    pub(crate) fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }

    /// The last representable playhead position: a timeline of `dur` spans
    /// `[0, dur)`, so its final frame is `dur - 1/fps` (0 on an empty one).
    pub fn last_frame(&self) -> Seconds {
        let end = self.timeline.end();
        match self.fps.as_ref() {
            Some(f) if *f > Seconds::from_integer(0) => {
                (end - Seconds::from_integer(1) / *f).max(Seconds::from_integer(0))
            }
            _ => end,
        }
    }

    /// Move the stored playhead to `at`: snapped onto the frame grid and
    /// clamped into the timeline. Returns where it landed, in seconds.
    pub fn set_playhead(&mut self, at: f64) -> f64 {
        let grid = grid_time(self.fps.as_ref(), at);
        let landed = grid.clamp(Seconds::from_integer(0), self.last_frame());
        self.playhead = landed;
        secs_f64(&landed)
    }

    pub fn load(path: &Path) -> Result<Self, ProjectError> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ProjectError::NotFound(path.to_path_buf()));
            }
            Err(e) => return Err(e.into()),
        };
        let project: Project = serde_json::from_str(&text)?;
        if project.version != PROJECT_VERSION {
            return Err(ProjectError::Version(project.version));
        }
        project.validate()?;
        Ok(project)
    }

    /// Atomic save: write a temp file in the same directory, then rename
    /// over the target. A crash mid-write can't corrupt the project.
    pub fn save(&self, path: &Path) -> Result<(), ProjectError> {
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

    /// Structural sanity of the whole project.
    pub fn validate(&self) -> Result<(), ProjectError> {
        if self.sequences.is_empty() {
            return Err(ProjectError::NoSequences);
        }
        for (i, sequence) in self.sequences.iter().enumerate() {
            if self.sequences[..i]
                .iter()
                .any(|other| other.id == sequence.id)
            {
                return Err(ProjectError::SequenceDuplicate(sequence.id));
            }
        }
        if self.sequence(self.active).is_none() {
            return Err(ProjectError::SequenceNotFound(self.active));
        }
        for sequence in &self.sequences {
            if sequence
                .fps
                .is_some_and(|fps| fps <= Seconds::from_integer(0))
            {
                return Err(ProjectError::NonPositiveFps(sequence.id));
            }
            sequence.timeline.validate()?;
            for track in &sequence.timeline.tracks {
                for seg in &track.segments {
                    match seg {
                        Segment::Gap(_) => {}
                        Segment::Clip(clip) => {
                            let asset = self.pooled(clip.asset)?;
                            // A clip must carry the track's kind of stream, or
                            // export can't build the graph (an audio-only asset on
                            // a video track failed deep inside ffmpeg; a video-only
                            // asset on an audio track exported silence).
                            if let Some(reason) = asset.wrong_kind(track.kind) {
                                return Err(ProjectError::Asset(clip.asset, reason));
                            }
                        }
                        // A transition owns its source spans too, so its assets
                        // must be pooled exactly like a clip's (`remove_asset`
                        // already refuses to strand one) — and carry picture: the
                        // export's xfade reads `[input:v]`, so an audio-only side
                        // would only fail inside ffmpeg.
                        Segment::Transition(tr) => {
                            for side in [&tr.a, &tr.b] {
                                if let Some(asset_id) = side.asset() {
                                    let asset = self.pooled(asset_id)?;
                                    if asset.wrong_kind(TrackKind::Video).is_some() {
                                        return Err(ProjectError::Asset(
                                            asset_id,
                                            format!(
                                                "{} has no video stream — a transition side needs picture",
                                                asset.path.display()
                                            ),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The pooled asset with `id`, or the error `validate` reports.
    fn pooled(&self, id: u32) -> Result<&Asset, ProjectError> {
        self.media
            .iter()
            .find(|a| a.id == id)
            .ok_or_else(|| ProjectError::Asset(id, "not in the media pool".into()))
    }

    /// Add an asset, or return the existing one. Returns (id, was_new).
    pub fn add_asset(&mut self, path: PathBuf, probe: RawProbe) -> (u32, bool) {
        if let Some(a) = self.media.iter().find(|a| a.path == path) {
            return (a.id, false);
        }
        let id = self.next_asset_id();
        self.media.push(Asset { id, path, probe });
        (id, true)
    }

    pub fn asset(&self, id: u32) -> Option<&Asset> {
        self.media.iter().find(|a| a.id == id)
    }

    pub fn asset_by_path(&self, path: &Path) -> Option<&Asset> {
        self.media.iter().find(|a| a.path == path)
    }

    /// `(sequence id, segment id)` pairs for every clip or transition that
    /// references asset `id`. Segment ids are only unique within a sequence,
    /// so protocol clients need the qualifier.
    pub fn asset_usage_scoped(&self, id: u32) -> Vec<(u32, u32)> {
        let mut used = Vec::new();
        for sequence in &self.sequences {
            for track in &sequence.timeline.tracks {
                for segment in &track.segments {
                    match segment {
                        Segment::Clip(c) if c.asset == id => used.push((sequence.id, c.id)),
                        // A transition owns its source spans, so it uses the
                        // asset too — otherwise `remove_asset` would drop it
                        // out from under the transition.
                        Segment::Transition(t)
                            if t.a.asset() == Some(id) || t.b.asset() == Some(id) =>
                        {
                            used.push((sequence.id, t.id));
                        }
                        _ => {}
                    }
                }
            }
        }
        used
    }

    /// Segment ids across every sequence that reference asset `id`. Flat
    /// because the in-model consumers (`remove_asset`, the TUI) only need to
    /// know whether anything uses it.
    pub fn asset_usage(&self, id: u32) -> Vec<u32> {
        self.asset_usage_scoped(id)
            .into_iter()
            .map(|(_, segment)| segment)
            .collect()
    }

    /// Remove an asset from the pool. Refuses while any clip references it
    /// (the caller lists the clips); no clip is ever silently dropped.
    pub fn remove_asset(&mut self, id: u32) -> Result<Asset, ProjectError> {
        let used = self.asset_usage(id);
        if !used.is_empty() {
            return Err(ProjectError::AssetInUse(id, used));
        }
        let pos = self
            .media
            .iter()
            .position(|a| a.id == id)
            .ok_or(ProjectError::AssetNotFound(id))?;
        Ok(self.media.remove(pos))
    }

    fn next_asset_id(&self) -> u32 {
        self.media.iter().map(|a| a.id).max().unwrap_or(0) + 1
    }

    pub(crate) fn next_clip_id(&self) -> u32 {
        self.timeline
            .tracks
            .iter()
            .flat_map(|t| {
                t.segments.iter().filter_map(|s| match s {
                    Segment::Clip(c) => Some(c.id),
                    Segment::Transition(t) => Some(t.id),
                    Segment::Gap(_) => None,
                })
            })
            .max()
            .unwrap_or(0)
            + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Clip;
    use crate::edit::{self, History};
    use crate::media::{RawFormat, RawStream};
    use crate::time::Seconds;
    use crate::timeline::{
        Effect, Gap, Segment, TrackKind, Transition, TransitionKind, TransitionSource,
    };

    fn probe_report(duration: &str, w: u32, h: u32, rate: &str, audio: bool) -> RawProbe {
        let mut streams = vec![RawStream {
            index: 0,
            codec_type: Some("video".into()),
            codec_name: Some("h264".into()),
            width: Some(w),
            height: Some(h),
            r_frame_rate: Some(rate.into()),
            pix_fmt: Some("yuv420p".into()),
            ..Default::default()
        }];
        if audio {
            streams.push(RawStream {
                index: 1,
                codec_type: Some("audio".into()),
                codec_name: Some("aac".into()),
                sample_rate: Some("48000".into()),
                channels: Some(2),
                channel_layout: Some("stereo".into()),
                ..Default::default()
            });
        }
        RawProbe {
            streams,
            format: RawFormat {
                format_name: "mp4".into(),
                duration: Some(duration.into()),
                ..Default::default()
            },
        }
    }

    fn sample_project() -> Project {
        let mut p = Project::new();
        let (a0, _) = p.add_asset("a.mp4".into(), probe_report("10", 1920, 1080, "30/1", true));
        let (a1, _) = p.add_asset(
            "b.mp4".into(),
            probe_report("8", 1280, 720, "30000/1001", false),
        );
        // [0..2) a.mp4 src 0..2 · [2..5) b.mp4 src 2..5
        let mut h = History::default();
        let batch = edit::paste_ripple(
            &p,
            &Segment::Clip(Clip {
                id: 0,
                asset: a0,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
                speed: Seconds::from_integer(1),
                effects: Vec::new(),
            }),
            0.0,
        )
        .unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let batch = edit::paste_ripple(
            &p,
            &Segment::Clip(Clip {
                id: 0,
                asset: a1,
                asset_in: Seconds::from_integer(2),
                asset_out: Seconds::from_integer(5),
                speed: Seconds::from_integer(1),
                effects: Vec::new(),
            }),
            2.0,
        )
        .unwrap();
        println!("second batch: {batch:?}");
        h.apply(&mut p, batch, 0).unwrap();
        p
    }

    #[test]
    fn asset_dedup_by_path() {
        let mut p = Project::new();
        let (id1, added1) = p.add_asset(
            "/x/a.mp4".into(),
            probe_report("10", 1920, 1080, "30/1", true),
        );
        let (id2, added2) = p.add_asset(
            "/x/a.mp4".into(),
            probe_report("10", 1920, 1080, "30/1", true),
        );
        assert!(added1 && !added2);
        assert_eq!(id1, id2);
        assert_eq!(p.media.len(), 1);
    }

    #[test]
    fn asset_usage_and_guarded_removal() {
        let mut p = sample_project(); // two assets, both used by a clip
        assert_eq!(p.asset_usage(1), vec![1]);
        assert_eq!(p.asset_usage(2), vec![2]);
        // In use: refuses, naming the clips.
        assert!(
            matches!(p.remove_asset(1), Err(ProjectError::AssetInUse(1, ref v)) if v == &vec![1])
        );
        assert_eq!(p.media.len(), 2);
        // Unused: an imported-but-unplaced asset removes cleanly.
        let (id, _) = p.add_asset(
            "/x/spare.mp4".into(),
            probe_report("5", 320, 240, "30/1", false),
        );
        assert!(p.asset_usage(id).is_empty());
        assert!(p.remove_asset(id).is_ok());
        assert!(p.remove_asset(id).is_err(), "already gone");
    }

    #[test]
    fn clip_ids_are_fresh_across_inserts() {
        let p = sample_project();
        let ids: Vec<u32> = p
            .timeline
            .video_track()
            .unwrap()
            .segments
            .iter()
            .filter_map(|s| match s {
                Segment::Clip(c) => Some(c.id),
                Segment::Gap(_) | Segment::Transition(_) => None,
            })
            .collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn validate_rejects_missing_asset_refs() {
        let mut p = sample_project();
        if let Segment::Clip(c) = &mut p.timeline.video_track_mut().unwrap().segments[0] {
            c.asset = 99;
        }
        assert!(matches!(p.validate(), Err(ProjectError::Asset(99, _))));
    }

    #[test]
    fn serde_round_trip_is_exact() {
        let p = sample_project();
        let json = serde_json::to_string_pretty(&p).unwrap();
        let back: Project = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
        // human-readable exact times in the file itself
        assert!(json.contains("\"30000/1001\""), "{json}");
    }

    #[test]
    fn effect_floats_round_trip_exactly() {
        let mut p = sample_project();
        if let Segment::Clip(c) = &mut p.timeline.video_track_mut().unwrap().segments[0] {
            c.effects.push(Effect::Crop {
                left: 25.936_916_977_875_356,
                right: -51.972_412_671_360_22,
                top: 109.115_355_159_373_05,
                bottom: 137.207_469_284_410_93,
            });
        }
        let json = serde_json::to_string(&p).unwrap();
        let back: Project = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn save_and_load_round_trip() {
        let p = sample_project();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.json");
        p.save(&path).unwrap();
        let loaded = Project::load(&path).unwrap();
        assert_eq!(p, loaded);
    }

    #[test]
    fn the_playhead_snaps_to_the_grid_and_clamps_into_the_timeline() {
        let mut p = sample_project();
        p.fps = Some(Seconds::new(30000, 1001));
        assert_eq!(p.set_playhead(-5.0), 0.0, "never before the start");
        let last = secs_f64(&p.last_frame());
        assert_eq!(
            p.set_playhead(last + 10.0),
            last,
            "never past the last frame"
        );
        // An off-grid request lands on the project's frame grid.
        let landed = p.set_playhead(1.0005);
        let fps = secs_f64(&p.fps.unwrap());
        let frames = landed * fps;
        assert!(
            (frames - frames.round()).abs() < 1e-6,
            "{landed} s is not on the {fps} fps grid"
        );
        assert_eq!(secs_f64(&p.playhead), landed, "the stored value agrees");
    }

    #[test]
    fn a_clip_must_carry_the_tracks_kind_of_stream() {
        let audio_only = || RawProbe {
            streams: vec![RawStream {
                index: 0,
                codec_type: Some("audio".into()),
                codec_name: Some("aac".into()),
                ..Default::default()
            }],
            format: RawFormat {
                format_name: "m4a".into(),
                duration: Some("10".into()),
                ..Default::default()
            },
        };
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (a, _) = p.add_asset("/media/music.m4a".into(), audio_only());
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip::new(
            1,
            a,
            Seconds::from_integer(0),
            Seconds::from_integer(1),
        ))];
        // An audio-only file on a video track can't be exported at all.
        assert!(
            matches!(p.validate(), Err(ProjectError::Asset(1, msg)) if msg.contains("no video stream")),
            "{:?}",
            p.validate()
        );

        // And the reverse: a video-only file on an audio track.
        let video_only = probe_report("10", 320, 240, "30/1", false);
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (a, _) = p.add_asset("/media/silent.mp4".into(), video_only);
        let ai = p.timeline.add_track(TrackKind::Audio);
        p.timeline.track_mut(ai).unwrap().segments = vec![Segment::Clip(Clip::new(
            1,
            a,
            Seconds::from_integer(0),
            Seconds::from_integer(1),
        ))];
        assert!(
            matches!(p.validate(), Err(ProjectError::Asset(1, msg)) if msg.contains("no audio stream")),
            "{:?}",
            p.validate()
        );

        // A probe with no stream info is "unknown", not "absent".
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (a, _) = p.add_asset("/media/unknown.mp4".into(), RawProbe::default());
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip::new(
            1,
            a,
            Seconds::from_integer(0),
            Seconds::from_integer(1),
        ))];
        assert!(p.validate().is_ok(), "{:?}", p.validate());
    }

    #[test]
    fn the_project_file_is_byte_stable() {
        // Probe tags were a HashMap: its per-process iteration order made
        // two identical projects differ byte-for-byte (noisy diffs, and
        // anything comparing files saw changes that weren't there).
        let mut tags = std::collections::BTreeMap::new();
        tags.insert("encoder".to_string(), "Lavf".to_string());
        tags.insert("major_brand".to_string(), "isom".to_string());
        let mut p = sample_project();
        p.media[0].probe.format.tags = Some(tags);

        let first = serde_json::to_string_pretty(&p).unwrap();
        let back: Project = serde_json::from_str(&first).unwrap();
        let second = serde_json::to_string_pretty(&back).unwrap();
        assert_eq!(first, second, "a round trip must not reorder the file");
    }

    #[test]
    fn a_new_project_has_one_root_sequence() {
        assert_eq!(PROJECT_VERSION, 11);
        let p = Project::new();
        assert_eq!(p.revision, 0);
        assert_eq!(p.sequences.len(), 1);
        assert_eq!(p.active_id(), 1);
        assert_eq!(p.active_sequence().name, "Sequence 1");
        assert_eq!(p.active_sequence().origin, SequenceOrigin::Root);
        assert_eq!(p.active_sequence().manual_edits, 0);
        assert!(p.fps.is_none());
        assert!(p.video.is_none());
        assert_eq!(p.timeline.tracks.len(), 1);
    }

    #[test]
    fn sequences_keep_their_own_settings_and_timelines() {
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Gap(Gap {
            duration: Seconds::from_integer(2),
        })];
        let second = p.add_sequence(
            "Vertical",
            Some(Seconds::from_integer(24)),
            Some(VideoSettings {
                width: 1080,
                height: 1920,
            }),
        );
        assert_eq!(second, 2);
        p.select_sequence(second).unwrap();
        assert_eq!(p.active_id(), second);
        assert_eq!(p.fps, Some(Seconds::from_integer(24)));
        assert_eq!(
            p.video,
            Some(VideoSettings {
                width: 1080,
                height: 1920
            })
        );
        assert!(p.timeline.video_track().unwrap().segments.is_empty());
        p.select_sequence(1).unwrap();
        assert_eq!(p.fps, Some(Seconds::from_integer(30)));
        assert_eq!(p.timeline.video_track().unwrap().segments.len(), 1);
    }

    #[test]
    fn project_serializes_sequences_not_a_flat_timeline() {
        let value = serde_json::to_value(Project::new()).unwrap();
        assert!(value["sequences"].is_array());
        assert_eq!(value["active"], 1);
        assert!(value.get("timeline").is_none());
        assert!(value.get("fps").is_none());
    }

    #[test]
    fn asset_usage_spans_sequences() {
        let mut p = sample_project();
        let before = p.asset_usage(1).len();
        let second = p.add_sequence("Second", None, None);
        p.select_sequence(second).unwrap();
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip::new(
            10,
            1,
            Seconds::from_integer(0),
            Seconds::from_integer(1),
        ))];
        assert_eq!(p.asset_usage(1).len(), before + 1);
    }

    #[test]
    fn validate_rejects_broken_sequence_lists() {
        let mut p = Project::new();
        p.sequences.clear();
        assert!(matches!(p.validate(), Err(ProjectError::NoSequences)));

        let mut p = Project::new();
        p.sequences.push(p.sequences[0].clone());
        assert!(matches!(
            p.validate(),
            Err(ProjectError::SequenceDuplicate(1))
        ));

        let mut p = Project::new();
        p.active = 99;
        assert!(matches!(
            p.validate(),
            Err(ProjectError::SequenceNotFound(99))
        ));
    }

    #[test]
    fn select_sequence_refuses_an_unknown_id_and_keeps_the_active_one() {
        let mut p = Project::new();
        assert!(matches!(
            p.select_sequence(99),
            Err(ProjectError::SequenceNotFound(99))
        ));
        assert_eq!(p.active_id(), 1);
    }

    #[test]
    fn validate_rejects_a_non_positive_frame_rate() {
        for rate in [Seconds::from_integer(0), Seconds::from_integer(-30)] {
            let mut p = Project::new();
            p.fps = Some(rate);
            assert!(
                matches!(p.validate(), Err(ProjectError::NonPositiveFps(1))),
                "{rate:?} must be refused"
            );
        }
    }

    #[test]
    fn load_rejects_a_zero_frame_rate_without_panicking() {
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(0));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nile.json");
        std::fs::write(&path, serde_json::to_string(&p).unwrap()).unwrap();
        assert!(matches!(
            Project::load(&path),
            Err(ProjectError::NonPositiveFps(1))
        ));
    }

    #[test]
    fn a_sequence_counts_manual_edits_but_not_undo() {
        let mut p = sample_project();
        p.fps = Some(Seconds::from_integer(30));
        let before = p.active_sequence().manual_edits;
        let mut history = History::default();
        let batch = edit::split_on(&p, 0, 1.0).unwrap();
        history.apply(&mut p, batch, 0).unwrap();
        assert_eq!(p.active_sequence().manual_edits, before + 1);
        history.undo(&mut p).unwrap();
        assert_eq!(
            p.active_sequence().manual_edits,
            before + 1,
            "undo does not rewrite the manual-edit counter"
        );
    }

    #[test]
    fn a_new_project_starts_at_revision_zero() {
        let p = Project::new();
        assert_eq!(p.revision, 0);
        let text = serde_json::to_string(&p).unwrap();
        let back: Project = serde_json::from_str(&text).unwrap();
        assert_eq!(back.revision, 0);
    }

    #[test]
    fn rejects_other_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.json");
        std::fs::write(&path, "{\"version\": 1}").unwrap();
        assert!(matches!(
            Project::load(&path),
            Err(ProjectError::Version(1))
        ));
    }

    #[test]
    fn version_error_names_the_supported_version() {
        let msg = ProjectError::Version(1).to_string();
        // The message must name the version THIS build reads (the constant),
        // not a stale hardcoded one.
        assert!(msg.contains(&format!("v{PROJECT_VERSION}")), "{msg}");
        assert!(msg.contains("version 1"), "{msg}");
    }

    #[test]
    fn validate_rejects_a_transition_on_a_missing_asset() {
        // A transition owns its spans, so a hand-edited/corrupt reference to an
        // unpooled asset must be refused like a clip's (`remove_asset` cannot
        // strand one).
        let mut p = sample_project();
        p.timeline
            .video_track_mut()
            .unwrap()
            .segments
            .push(Segment::Transition(Transition {
                id: 99,
                dur: Seconds::from_integer(1),
                kind: TransitionKind::Dissolve,
                a: TransitionSource::Clip {
                    asset: 999,
                    src_in: Seconds::from_integer(0),
                    src_out: Seconds::from_integer(1),
                    effects: Vec::new(),
                },
                b: TransitionSource::Black,
            }));
        assert!(matches!(
            p.validate(),
            Err(ProjectError::Asset(999, ref why)) if why.contains("media pool")
        ));
    }

    #[test]
    fn validate_rejects_a_transition_side_without_picture() {
        // A transition is rendered with xfade, which reads `[input:v]`; an
        // audio-only side would only fail inside ffmpeg. The clip rule
        // (`wrong_kind`) applies to transition sides too.
        let mut p = sample_project();
        let (music, _) = p.add_asset(
            "/media/music.m4a".into(),
            RawProbe {
                streams: vec![RawStream {
                    index: 0,
                    codec_type: Some("audio".into()),
                    codec_name: Some("aac".into()),
                    ..Default::default()
                }],
                format: RawFormat {
                    format_name: "m4a".into(),
                    duration: Some("10".into()),
                    ..Default::default()
                },
            },
        );
        p.timeline
            .video_track_mut()
            .unwrap()
            .segments
            .push(Segment::Transition(Transition {
                id: 99,
                dur: Seconds::from_integer(1),
                kind: TransitionKind::Dissolve,
                a: TransitionSource::Clip {
                    asset: music,
                    src_in: Seconds::from_integer(0),
                    src_out: Seconds::from_integer(1),
                    effects: Vec::new(),
                },
                b: TransitionSource::Black,
            }));
        assert!(
            matches!(p.validate(), Err(ProjectError::Asset(id, ref why)) if id == music && why.contains("no video stream") && why.contains("transition side")),
            "{:?}",
            p.validate()
        );
    }

    #[test]
    fn asset_usage_counts_transition_sources() {
        // A transition owns its source spans, so an asset used only by a
        // transition is still in use — `remove_asset` must refuse it. (The
        // integration trap: `asset_usage` used to walk only clips.)
        let mut p = sample_project();
        let (spare, _) = p.add_asset(
            "/x/spare.mp4".into(),
            probe_report("5", 320, 240, "30/1", false),
        );
        p.timeline
            .video_track_mut()
            .unwrap()
            .segments
            .push(Segment::Transition(Transition {
                id: 99,
                dur: Seconds::from_integer(1),
                kind: TransitionKind::Dissolve,
                a: TransitionSource::Clip {
                    asset: spare,
                    src_in: Seconds::from_integer(0),
                    src_out: Seconds::from_integer(1),
                    effects: Vec::new(),
                },
                b: TransitionSource::Black,
            }));
        assert_eq!(p.asset_usage(spare), vec![99]);
        assert_eq!(p.asset_usage_scoped(spare), vec![(1, 99)]);
        assert!(
            matches!(p.remove_asset(spare), Err(ProjectError::AssetInUse(id, ref v)) if id == spare && v == &vec![99])
        );
    }
}
