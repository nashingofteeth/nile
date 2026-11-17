//! The project file: media pool + timeline, serialized as JSON (the EDL).
//!
//! Times are exact rationals serialized as "n/d" strings; positions never
//! drift no matter how many times you edit.

use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::annotation::{Anchor, Annotation, AnnotationKind, SourceRef};
use crate::group::{MediaGroup, MediaPart};
use crate::media::RawProbe;
use crate::provenance::Provenance;
use crate::time::{Seconds, format_seconds, grid_time, rational_ser, secs_f64};
use crate::timeline::{Segment, Timeline, TimelineError, TrackKind};
use crate::timemap::{TimeMap, TimeMapError};

/// Project file format version. No migration path is kept (see
/// README "Compatibility: none, by design"): old files are rejected,
/// never silently misread. v3 added the per-track `muted` flag;
/// v4 added the per-clip `speed` rate; v5 added the stored playhead;
/// v7 stores `crop` as percentages (scale/x/y) instead of pixel w/h/x/y;
/// v8 stores `crop` as per-edge percentage insets and `scale` as percentages;
/// v9 stores each transition side's effects (they render the neighbour);
/// v10 stores the monotonic model `revision`;
/// v11 moves fps/video/timeline/playhead onto one or more `Sequence`s;
/// v12 adds logical media groups; v13 adds annotations; v14 adds
/// clip-relative timeline anchors.
pub const PROJECT_VERSION: u32 = 14;

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
    #[error("track #{0} not found")]
    TrackNotFound(usize),
    #[error("media group #{0} not found")]
    GroupNotFound(u32),
    #[error(transparent)]
    TimeMap(#[from] TimeMapError),
    #[error("media group #{0}: {1}")]
    Group(u32, String),
    #[error(
        "asset #{0} is used by media group(s) {1:?} — remove or edit them first (nile group rm ID)"
    )]
    AssetInGroup(u32, Vec<u32>),
    #[error("annotation #{0} not found")]
    AnnotationNotFound(u32),
    #[error("annotation #{0}: {1}")]
    Annotation(u32, String),
    #[error(
        "asset #{0} is used by annotation(s) {1:?} — remove them first (nile annotation rm ID)"
    )]
    AssetInAnnotation(u32, Vec<u32>),
    #[error(
        "media group #{0} is used by annotation(s) {1:?} — remove them first (nile annotation rm ID)"
    )]
    GroupInAnnotation(u32, Vec<u32>),
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
    /// Logical recordings assembled from ordered media parts.
    #[serde(default)]
    pub groups: Vec<MediaGroup>,
    /// Notes and markers, project-wide (the `sequence` field scopes each one).
    #[serde(default)]
    pub annotations: Vec<Annotation>,
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
            groups: Vec::new(),
            annotations: Vec::new(),
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

    pub fn group(&self, id: u32) -> Option<&MediaGroup> {
        self.groups.iter().find(|group| group.id == id)
    }

    /// Timeline → group time for one track/asset/group. The per-asset clip map
    /// is composed with the group's file→group map, so gaps in either axis and
    /// speed changes stay exact. A missing sequence, track or group is `None`.
    pub fn group_time_map(
        &self,
        sequence: u32,
        track: usize,
        group: u32,
        asset: u32,
    ) -> Result<TimeMap, ProjectError> {
        let sequence = self
            .sequence(sequence)
            .ok_or(ProjectError::SequenceNotFound(sequence))?;
        let track = sequence
            .timeline
            .track(track)
            .ok_or(ProjectError::TrackNotFound(track))?;
        let group = self
            .group(group)
            .ok_or(ProjectError::GroupNotFound(group))?;
        let timeline = TimeMap::from_track_for_asset(track, asset)?;
        let file = group.asset_map(asset)?;
        Ok(timeline.compose(&file)?)
    }

    /// Remove a media group by id, returning it.
    pub fn remove_group(&mut self, id: u32) -> Result<MediaGroup, ProjectError> {
        let annotations = self.group_annotation_usage(id);
        if !annotations.is_empty() {
            return Err(ProjectError::GroupInAnnotation(id, annotations));
        }
        let pos = self
            .groups
            .iter()
            .position(|group| group.id == id)
            .ok_or(ProjectError::GroupNotFound(id))?;
        Ok(self.groups.remove(pos))
    }

    /// The id the next [`Project::add_group`] will use.
    fn next_group_id(&self) -> u32 {
        self.groups.iter().map(|group| group.id).max().unwrap_or(0) + 1
    }

    /// Ordered parts for already-pooled `paths`: argument order, cumulative
    /// group starts, zero file offsets. One constructor for every caller
    /// (`nile import --group` and the EDL/OTIO importers), so the ingest paths
    /// cannot disagree. A repeated asset or an unknown duration is refused;
    /// the id in the error is the group that would have been created.
    pub fn group_parts(&self, paths: &[PathBuf]) -> Result<Vec<MediaPart>, ProjectError> {
        let id = self.next_group_id();
        let mut parts = Vec::with_capacity(paths.len());
        let mut start = Seconds::from_integer(0);
        let mut seen = HashSet::new();
        for (index, path) in paths.iter().enumerate() {
            let asset = self.asset_by_path(path).ok_or_else(|| {
                ProjectError::Group(
                    id,
                    format!("part {index}: {} is not pooled", path.display()),
                )
            })?;
            if !seen.insert(asset.id) {
                return Err(ProjectError::Group(
                    id,
                    format!("part {index}: {} is listed twice", path.display()),
                ));
            }
            let duration = asset.duration().ok_or_else(|| {
                ProjectError::Group(
                    id,
                    format!("part {index}: {} has no known duration", path.display()),
                )
            })?;
            parts.push(MediaPart {
                asset: asset.id,
                group_start: start,
                file_start: Seconds::from_integer(0),
                duration,
            });
            start += duration;
        }
        Ok(parts)
    }

    /// Add a logical media group and return its id.
    /// Model-only group creation. A front-end must add a `Document` wrapper
    /// that bumps the revision and records history before exposing this.
    pub fn add_group(
        &mut self,
        name: impl Into<String>,
        parts: Vec<MediaPart>,
    ) -> Result<u32, ProjectError> {
        let id = self.next_group_id();
        let group = MediaGroup::new(id, name, parts);
        self.validate_group(&group)?;
        self.groups.push(group);
        Ok(id)
    }

    fn validate_group(&self, group: &MediaGroup) -> Result<(), ProjectError> {
        if group.parts.is_empty() {
            return Err(ProjectError::Group(group.id, "has no parts".to_string()));
        }
        let mut previous_end = Seconds::from_integer(0);
        let mut file_parts: Vec<(u32, Seconds, Seconds, usize)> = Vec::new();
        for (part_index, part) in group.parts.iter().enumerate() {
            let asset = self.asset(part.asset).ok_or_else(|| {
                ProjectError::Group(
                    group.id,
                    format!("part {part_index}: asset #{} is not pooled", part.asset),
                )
            })?;
            if part.group_start < Seconds::from_integer(0)
                || part.file_start < Seconds::from_integer(0)
                || part.duration <= Seconds::from_integer(0)
            {
                return Err(ProjectError::Group(
                    group.id,
                    format!("part {part_index}: invalid time or duration"),
                ));
            }
            let group_end = part.group_end().ok_or_else(|| {
                ProjectError::Group(group.id, format!("part {part_index}: group time overflow"))
            })?;
            let file_end = part.file_end().ok_or_else(|| {
                ProjectError::Group(group.id, format!("part {part_index}: file time overflow"))
            })?;
            if let Some(duration) = asset.duration()
                && file_end > duration
            {
                return Err(ProjectError::Group(
                    group.id,
                    format!(
                        "part {part_index}: reads to {} s past the {duration} s asset",
                        format_seconds(&file_end)
                    ),
                ));
            }
            if part_index > 0 && part.group_start < previous_end {
                return Err(ProjectError::Group(
                    group.id,
                    format!("part {part_index}: overlaps the previous part"),
                ));
            }
            previous_end = group_end;
            file_parts.push((part.asset, part.file_start, file_end, part_index));
        }
        file_parts.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        for pair in file_parts.windows(2) {
            if pair[0].0 == pair[1].0 && pair[0].2 > pair[1].1 {
                return Err(ProjectError::Group(
                    group.id,
                    format!(
                        "parts {} and {} overlap in asset #{} file time",
                        pair[0].3, pair[1].3, pair[0].0
                    ),
                ));
            }
        }
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
        for (i, group) in self.groups.iter().enumerate() {
            if self.groups[..i].iter().any(|other| other.id == group.id) {
                return Err(ProjectError::Group(group.id, "duplicated id".to_string()));
            }
            self.validate_group(group)?;
        }
        for (i, annotation) in self.annotations.iter().enumerate() {
            if self.annotations[..i]
                .iter()
                .any(|other| other.id == annotation.id)
            {
                return Err(ProjectError::Annotation(
                    annotation.id,
                    "duplicated id".to_string(),
                ));
            }
            self.validate_annotation(annotation)?;
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

    /// Group ids that reference asset `id`.
    pub fn asset_group_usage(&self, id: u32) -> Vec<u32> {
        self.groups
            .iter()
            .filter(|group| group.parts.iter().any(|part| part.asset == id))
            .map(|group| group.id)
            .collect()
    }

    /// Remove an asset from the pool. Refuses while any clip or media group
    /// references it (the caller lists the clips/groups); nothing is silently
    /// left dangling.
    pub fn remove_asset(&mut self, id: u32) -> Result<Asset, ProjectError> {
        let used = self.asset_usage(id);
        if !used.is_empty() {
            return Err(ProjectError::AssetInUse(id, used));
        }
        let groups = self.asset_group_usage(id);
        if !groups.is_empty() {
            return Err(ProjectError::AssetInGroup(id, groups));
        }
        let annotations = self.asset_annotation_usage(id);
        if !annotations.is_empty() {
            return Err(ProjectError::AssetInAnnotation(id, annotations));
        }
        let pos = self
            .media
            .iter()
            .position(|a| a.id == id)
            .ok_or(ProjectError::AssetNotFound(id))?;
        Ok(self.media.remove(pos))
    }

    /// The annotation with `id`.
    pub fn annotation(&self, id: u32) -> Option<&Annotation> {
        self.annotations.iter().find(|a| a.id == id)
    }

    /// The clip with `id` in `sequence`, across every track.
    fn sequence_clip(&self, sequence: u32, id: u32) -> Option<&crate::timeline::Clip> {
        self.sequence(sequence)?
            .timeline
            .tracks
            .iter()
            .flat_map(|track| &track.segments)
            .find_map(|segment| match segment {
                crate::timeline::Segment::Clip(clip) if clip.id == id => Some(clip),
                _ => None,
            })
    }

    /// A timeline anchor's absolute span in `sequence`, or `None` for a
    /// source anchor or a range whose endpoints no longer resolve.
    pub fn annotation_span(&self, annotation: &Annotation) -> Option<(Seconds, Seconds)> {
        let Anchor::Timeline { start, end } = &annotation.anchor else {
            return None;
        };
        let sequence = self.sequence(annotation.sequence)?;
        let start =
            crate::timeline::clip_position(&sequence.timeline.tracks, start.clip, start.source)?;
        let end = crate::timeline::clip_position(&sequence.timeline.tracks, end.clip, end.source)?;
        (start <= end).then_some((start, end))
    }

    /// Annotations scoped to `sequence`, in project order.
    pub fn annotations_for(&self, sequence: u32) -> impl Iterator<Item = &Annotation> + '_ {
        self.annotations
            .iter()
            .filter(move |annotation| annotation.sequence == sequence)
    }

    /// Replace one sequence's annotations, leaving every other sequence (and
    /// its relative order) in place. Undo snapshots are sequence-scoped, so a
    /// write on one sequence can never invalidate another stack's entry.
    pub(crate) fn replace_annotations(&mut self, sequence: u32, annotations: Vec<Annotation>) {
        let first = self
            .annotations
            .iter()
            .position(|annotation| annotation.sequence == sequence);
        self.annotations
            .retain(|annotation| annotation.sequence != sequence);
        let at = first
            .unwrap_or(self.annotations.len())
            .min(self.annotations.len());
        self.annotations.splice(at..at, annotations);
    }

    /// Annotation ids that anchor directly to asset `id`.
    pub fn asset_annotation_usage(&self, id: u32) -> Vec<u32> {
        self.annotations
            .iter()
            .filter(|a| {
                matches!(
                    &a.anchor,
                    Anchor::Source {
                        source: SourceRef::Asset { id: asset },
                        ..
                    } if *asset == id
                )
            })
            .map(|a| a.id)
            .collect()
    }

    /// Annotation ids that anchor to media group `id`.
    pub fn group_annotation_usage(&self, id: u32) -> Vec<u32> {
        self.annotations
            .iter()
            .filter(|a| {
                matches!(
                    &a.anchor,
                    Anchor::Source {
                        source: SourceRef::Group { id: group },
                        ..
                    } if *group == id
                )
            })
            .map(|a| a.id)
            .collect()
    }

    /// Add an annotation to `sequence`, returning its project-wide id.
    pub fn add_annotation(
        &mut self,
        sequence: u32,
        kind: AnnotationKind,
        anchor: Anchor,
        provenance: Provenance,
    ) -> Result<u32, ProjectError> {
        let id = self.next_annotation_id();
        let annotation = Annotation {
            id,
            sequence,
            kind,
            anchor,
            provenance,
            status: crate::annotation::AnnotationStatus::Open,
        };
        self.validate_annotation(&annotation)?;
        self.annotations.push(annotation);
        Ok(id)
    }

    /// Remove an annotation, returning it.
    pub fn remove_annotation(&mut self, id: u32) -> Result<Annotation, ProjectError> {
        let pos = self
            .annotations
            .iter()
            .position(|a| a.id == id)
            .ok_or(ProjectError::AnnotationNotFound(id))?;
        Ok(self.annotations.remove(pos))
    }

    /// Mark an annotation resolved. Returns whether anything changed.
    pub fn resolve_annotation(&mut self, id: u32) -> Result<bool, ProjectError> {
        let annotation = self
            .annotations
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or(ProjectError::AnnotationNotFound(id))?;
        if annotation.status == crate::annotation::AnnotationStatus::Resolved {
            return Ok(false);
        }
        annotation.status = crate::annotation::AnnotationStatus::Resolved;
        Ok(true)
    }

    fn next_annotation_id(&self) -> u32 {
        self.annotations.iter().map(|a| a.id).max().unwrap_or(0) + 1
    }

    fn validate_annotation(&self, annotation: &Annotation) -> Result<(), ProjectError> {
        if self.sequence(annotation.sequence).is_none() {
            return Err(ProjectError::Annotation(
                annotation.id,
                format!("sequence #{} not found", annotation.sequence),
            ));
        }
        if let AnnotationKind::Note { text } = &annotation.kind
            && text.trim().is_empty()
        {
            return Err(ProjectError::Annotation(
                annotation.id,
                "a note needs text".to_string(),
            ));
        }
        match &annotation.anchor {
            Anchor::Timeline { start, end } => {
                for point in [start, end] {
                    let clip = self
                        .sequence_clip(annotation.sequence, point.clip)
                        .ok_or_else(|| {
                            ProjectError::Annotation(
                                annotation.id,
                                format!(
                                    "clip #{} not found in sequence #{}",
                                    point.clip, annotation.sequence
                                ),
                            )
                        })?;
                    if point.source < clip.asset_in || point.source >= clip.asset_out {
                        return Err(ProjectError::Annotation(
                            annotation.id,
                            format!(
                                "clip #{} has no media at {}",
                                point.clip,
                                format_seconds(&point.source)
                            ),
                        ));
                    }
                }
                if self.annotation_span(annotation).is_none() {
                    return Err(ProjectError::Annotation(
                        annotation.id,
                        "timeline range ends before it starts".to_string(),
                    ));
                }
            }
            Anchor::Source { source, time } => {
                if *time < Seconds::from_integer(0) {
                    return Err(ProjectError::Annotation(
                        annotation.id,
                        format!("negative source time {}", format_seconds(time)),
                    ));
                }
                let (asset, file_time) = match source {
                    SourceRef::Asset { id } => (*id, *time),
                    SourceRef::Group { id } => {
                        let group = self.group(*id).ok_or(ProjectError::GroupNotFound(*id))?;
                        group.group_time(time).ok_or_else(|| {
                            ProjectError::Annotation(
                                annotation.id,
                                format!("group #{} has no media at {}", id, format_seconds(time)),
                            )
                        })?
                    }
                };
                let asset = self.pooled(asset)?;
                if !asset.duration().is_some_and(|end| file_time < end) {
                    return Err(ProjectError::Annotation(
                        annotation.id,
                        format!(
                            "asset #{} has no media at {}",
                            asset.id,
                            format_seconds(&file_time)
                        ),
                    ));
                }
            }
        }
        Ok(())
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

/// A structural before/after diff for previews. Actions cannot add or
/// remove a sequence today, but the fields keep the type useful if they do.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ProjectDiff {
    pub sequences: Vec<SequenceDiff>,
    pub sequences_added: Vec<u32>,
    pub sequences_removed: Vec<u32>,
    pub assets_added: Vec<u32>,
    pub assets_removed: Vec<u32>,
    pub assets_changed: Vec<u32>,
    pub annotations_added: Vec<u32>,
    pub annotations_removed: Vec<u32>,
    pub annotations_changed: Vec<u32>,
}

impl ProjectDiff {
    pub fn is_empty(&self) -> bool {
        self.sequences.is_empty()
            && self.sequences_added.is_empty()
            && self.sequences_removed.is_empty()
            && self.assets_added.is_empty()
            && self.assets_removed.is_empty()
            && self.assets_changed.is_empty()
            && self.annotations_added.is_empty()
            && self.annotations_removed.is_empty()
            && self.annotations_changed.is_empty()
    }
}

/// One sequence that changed. Only differing tracks are listed, and added or
/// removed tracks are reported by index.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SequenceDiff {
    pub id: u32,
    pub settings_changed: bool,
    pub tracks_added: Vec<usize>,
    pub tracks_removed: Vec<usize>,
    pub tracks: Vec<TrackDiff>,
}

/// One track whose kind, mute flag, or segments changed.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TrackDiff {
    pub index: usize,
    pub before_segments: usize,
    pub after_segments: usize,
    pub segments_changed: bool,
    pub kind_changed: bool,
    pub muted_changed: bool,
}

impl Project {
    /// Compare two project snapshots for `action.preview`. The comparison is
    /// structural: sequence settings, track topology/mute/kind, and the media
    /// pool. It deliberately ignores `revision` and `playhead`, which are
    /// session bookkeeping rather than the edit's content.
    pub fn diff(&self, after: &Project) -> ProjectDiff {
        let mut diff = ProjectDiff::default();
        for before in &self.sequences {
            match after.sequence(before.id) {
                Some(after) => {
                    let mut sequence = SequenceDiff {
                        id: before.id,
                        settings_changed: before.name != after.name
                            || before.fps != after.fps
                            || before.video != after.video,
                        ..Default::default()
                    };
                    let before_tracks = &before.timeline.tracks;
                    let after_tracks = &after.timeline.tracks;
                    let common = before_tracks.len().min(after_tracks.len());
                    for i in 0..common {
                        let b = &before_tracks[i];
                        let a = &after_tracks[i];
                        if b != a {
                            sequence.tracks.push(TrackDiff {
                                index: i,
                                before_segments: b.segments.len(),
                                after_segments: a.segments.len(),
                                segments_changed: b.segments != a.segments,
                                kind_changed: b.kind != a.kind,
                                muted_changed: b.muted != a.muted,
                            });
                        }
                    }
                    sequence.tracks_removed.extend(common..before_tracks.len());
                    sequence.tracks_added.extend(common..after_tracks.len());
                    if sequence.settings_changed
                        || !sequence.tracks_added.is_empty()
                        || !sequence.tracks_removed.is_empty()
                        || !sequence.tracks.is_empty()
                    {
                        diff.sequences.push(sequence);
                    }
                }
                None => diff.sequences_removed.push(before.id),
            }
        }
        for after in &after.sequences {
            if self.sequence(after.id).is_none() {
                diff.sequences_added.push(after.id);
            }
        }

        let before_assets: std::collections::BTreeMap<u32, &Asset> =
            self.media.iter().map(|a| (a.id, a)).collect();
        let after_assets: std::collections::BTreeMap<u32, &Asset> =
            after.media.iter().map(|a| (a.id, a)).collect();
        for (id, asset) in &before_assets {
            match after_assets.get(id) {
                Some(after) if *asset != *after => diff.assets_changed.push(*id),
                Some(_) => {}
                None => diff.assets_removed.push(*id),
            }
        }
        for id in after_assets.keys() {
            if !before_assets.contains_key(id) {
                diff.assets_added.push(*id);
            }
        }

        let before_annotations: std::collections::BTreeMap<u32, &Annotation> =
            self.annotations.iter().map(|a| (a.id, a)).collect();
        let after_annotations: std::collections::BTreeMap<u32, &Annotation> =
            after.annotations.iter().map(|a| (a.id, a)).collect();
        for (id, annotation) in &before_annotations {
            match after_annotations.get(id) {
                Some(after) if *annotation != *after => diff.annotations_changed.push(*id),
                Some(_) => {}
                None => diff.annotations_removed.push(*id),
            }
        }
        for id in after_annotations.keys() {
            if !before_annotations.contains_key(id) {
                diff.annotations_added.push(*id);
            }
        }
        diff
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Clip;
    use crate::annotation::ClipPoint;
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
        assert_eq!(PROJECT_VERSION, 14);
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
    fn media_groups_round_trip_and_validate() {
        let mut p = sample_project();
        let id = p
            .add_group(
                "CAM 3",
                vec![
                    MediaPart {
                        asset: 1,
                        group_start: Seconds::from_integer(0),
                        file_start: Seconds::from_integer(0),
                        duration: Seconds::from_integer(2),
                    },
                    MediaPart {
                        asset: 2,
                        group_start: Seconds::from_integer(3),
                        file_start: Seconds::from_integer(2),
                        duration: Seconds::from_integer(2),
                    },
                ],
            )
            .unwrap();
        assert_eq!(id, 1);
        assert_eq!(
            p.group(id).unwrap().group_time(&Seconds::from_integer(1)),
            Some((1, Seconds::from_integer(1)))
        );
        let text = serde_json::to_string(&p).unwrap();
        let back: Project = serde_json::from_str(&text).unwrap();
        assert_eq!(back.groups, p.groups);
        assert!(p.validate().is_ok());

        let mut bad = p.clone();
        bad.groups[0].parts[0].file_start = Seconds::from_integer(i64::MAX);
        assert!(matches!(
            bad.validate(),
            Err(ProjectError::Group(1, ref msg)) if msg.contains("file time overflow")
        ));

        assert!(matches!(
            p.add_group(
                "bad",
                vec![MediaPart {
                    asset: 99,
                    group_start: Seconds::from_integer(0),
                    file_start: Seconds::from_integer(0),
                    duration: Seconds::from_integer(1),
                }]
            ),
            Err(ProjectError::Group(_, ref msg)) if msg.contains("not pooled")
        ));

        let mut bad = p.clone();
        bad.groups[0].parts[0].duration = Seconds::from_integer(11);
        assert!(matches!(
            bad.validate(),
            Err(ProjectError::Group(1, ref msg)) if msg.contains("past")
        ));

        let mut bad = p.clone();
        bad.groups[0].parts[1].group_start = Seconds::from_integer(1);
        assert!(matches!(
            bad.validate(),
            Err(ProjectError::Group(1, ref msg)) if msg.contains("overlaps")
        ));

        let mut bad = p.clone();
        bad.groups[0].parts.push(MediaPart {
            asset: 1,
            group_start: Seconds::from_integer(6),
            file_start: Seconds::from_integer(1),
            duration: Seconds::from_integer(2),
        });
        assert!(matches!(
            bad.validate(),
            Err(ProjectError::Group(1, ref msg)) if msg.contains("overlap in asset")
        ));

        let mut bad = p.clone();
        bad.groups[0].parts[0].asset = 99;
        assert!(matches!(bad.validate(), Err(ProjectError::Group(1, _))));

        let mut bad = p.clone();
        bad.groups[0].parts.clear();
        assert!(matches!(bad.validate(), Err(ProjectError::Group(1, _))));

        let mut bad = p.clone();
        bad.groups.push(bad.groups[0].clone());
        assert!(matches!(bad.validate(), Err(ProjectError::Group(1, _))));
    }

    #[test]
    fn group_time_map_composes_timeline_file_and_group_time() {
        use crate::timeline::{Clip, Gap, Segment};
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (asset, _) = p.add_asset(
            "/media/cam.mp4".into(),
            probe_report("20", 320, 240, "30/1", false),
        );
        p.timeline.video_track_mut().unwrap().segments = vec![
            Segment::Clip(Clip::new(
                asset,
                asset,
                Seconds::from_integer(0),
                Seconds::from_integer(2),
            )),
            Segment::Gap(Gap {
                duration: Seconds::from_integer(1),
            }),
            Segment::Clip(Clip::new(
                2,
                asset,
                Seconds::from_integer(10),
                Seconds::from_integer(12),
            )),
        ];
        let group = p
            .add_group(
                "CAM 3",
                vec![
                    MediaPart {
                        asset,
                        group_start: Seconds::from_integer(0),
                        file_start: Seconds::from_integer(0),
                        duration: Seconds::from_integer(2),
                    },
                    MediaPart {
                        asset,
                        group_start: Seconds::from_integer(3),
                        file_start: Seconds::from_integer(10),
                        duration: Seconds::from_integer(2),
                    },
                ],
            )
            .unwrap();
        let map = p.group_time_map(1, 0, group, asset).unwrap();
        assert_eq!(
            map.map(&Seconds::from_integer(1)),
            Some(Seconds::from_integer(1))
        );
        assert_eq!(map.map(&Seconds::from_integer(2)), None, "timeline gap");
        assert_eq!(map.map(&Seconds::new(7, 2)), Some(Seconds::new(7, 2)));
        assert_eq!(map.inverse(&Seconds::new(7, 2)), Some(Seconds::new(7, 2)));
        assert_eq!(map.inverse(&Seconds::new(5, 2)), None, "group gap");
        let second = p
            .add_group(
                "CAM 3 copy",
                vec![MediaPart {
                    asset,
                    group_start: Seconds::from_integer(10),
                    file_start: Seconds::from_integer(0),
                    duration: Seconds::from_integer(2),
                }],
            )
            .unwrap();
        let map2 = p.group_time_map(1, 0, second, asset).unwrap();
        assert_eq!(
            map2.map(&Seconds::from_integer(0)),
            Some(Seconds::from_integer(10))
        );
        assert!(p.group_time_map(1, 0, group, 99).unwrap().is_empty());
        assert!(matches!(
            p.group_time_map(1, 0, 99, asset),
            Err(ProjectError::GroupNotFound(99))
        ));
        assert!(matches!(
            p.group_time_map(1, 99, group, asset),
            Err(ProjectError::TrackNotFound(99))
        ));
        assert!(matches!(
            p.group_time_map(99, 0, group, asset),
            Err(ProjectError::SequenceNotFound(99))
        ));
    }

    fn provenance() -> Provenance {
        Provenance {
            actor: crate::Actor::Human,
            at: 0,
        }
    }

    #[test]
    fn annotations_validate_their_source_anchor() {
        let mut p = Project::new();
        let (asset, _) = p.add_asset(
            "/media/cam.mp4".into(),
            probe_report("2", 320, 240, "30/1", false),
        );
        let note = |text: &str| AnnotationKind::Note {
            text: text.to_string(),
        };
        let id = p
            .add_annotation(
                1,
                note("hello"),
                Anchor::Source {
                    source: SourceRef::Asset { id: asset },
                    time: Seconds::new(3, 2),
                },
                provenance(),
            )
            .unwrap();
        assert_eq!(p.annotation(id).unwrap().text(), Some("hello"));

        assert!(matches!(
            p.add_annotation(
                1,
                note(" "),
                Anchor::Source {
                    source: SourceRef::Asset { id: asset },
                    time: Seconds::from_integer(0),
                },
                provenance(),
            ),
            Err(ProjectError::Annotation(_, ref e)) if e.contains("needs text")
        ));
        assert!(matches!(
            p.add_annotation(
                1,
                note("past the end"),
                Anchor::Source {
                    source: SourceRef::Asset { id: asset },
                    time: Seconds::from_integer(2),
                },
                provenance(),
            ),
            Err(ProjectError::Annotation(_, ref e)) if e.contains("no media")
        ));
        assert!(matches!(
            p.add_annotation(
                1,
                note("negative"),
                Anchor::Source {
                    source: SourceRef::Asset { id: asset },
                    time: -Seconds::from_integer(1),
                },
                provenance(),
            ),
            Err(ProjectError::Annotation(_, ref e)) if e.contains("negative")
        ));

        let group = p
            .add_group(
                "CAM 3",
                vec![MediaPart {
                    asset,
                    group_start: Seconds::from_integer(0),
                    file_start: Seconds::from_integer(0),
                    duration: Seconds::from_integer(2),
                }],
            )
            .unwrap();
        p.add_annotation(
            1,
            note("on the group clock"),
            Anchor::Source {
                source: SourceRef::Group { id: group },
                time: Seconds::from_integer(1),
            },
            provenance(),
        )
        .unwrap();
        assert!(matches!(
            p.add_annotation(
                1,
                note("group end is exclusive"),
                Anchor::Source {
                    source: SourceRef::Group { id: group },
                    time: Seconds::from_integer(2),
                },
                provenance(),
            ),
            Err(ProjectError::Annotation(_, ref e)) if e.contains("no media")
        ));
        assert!(matches!(
            p.add_annotation(
                99,
                note("missing sequence"),
                Anchor::Source {
                    source: SourceRef::Asset { id: asset },
                    time: Seconds::from_integer(0),
                },
                provenance(),
            ),
            Err(ProjectError::Annotation(_, ref e)) if e.contains("sequence #99")
        ));
    }

    #[test]
    fn timeline_anchors_validate_the_clip_and_source() {
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (asset, _) = p.add_asset(
            "/media/cam.mp4".into(),
            probe_report("2", 320, 240, "30/1", false),
        );
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip::new(
            1,
            asset,
            Seconds::from_integer(0),
            Seconds::from_integer(2),
        ))];
        let note = || AnnotationKind::Note {
            text: "on the timeline".into(),
        };
        let point = |source: Seconds| ClipPoint { clip: 1, source };

        let id = p
            .add_annotation(
                1,
                note(),
                Anchor::Timeline {
                    start: point(Seconds::new(3, 2)),
                    end: point(Seconds::new(3, 2)),
                },
                provenance(),
            )
            .unwrap();
        assert_eq!(
            p.annotation_span(p.annotation(id).unwrap()),
            Some((Seconds::new(3, 2), Seconds::new(3, 2)))
        );

        let range = p
            .add_annotation(
                1,
                note(),
                Anchor::Timeline {
                    start: point(Seconds::from_integer(0)),
                    end: point(Seconds::from_integer(1)),
                },
                provenance(),
            )
            .unwrap();
        assert_eq!(
            p.annotation_span(p.annotation(range).unwrap()),
            Some((Seconds::from_integer(0), Seconds::from_integer(1)))
        );

        assert!(
            p.add_annotation(
                1,
                note(),
                Anchor::Timeline {
                    start: point(Seconds::from_integer(0)),
                    end: point(Seconds::from_integer(0)),
                },
                provenance(),
            )
            .is_ok()
        );
        assert!(matches!(
            p.add_annotation(
                1,
                note(),
                Anchor::Timeline {
                    start: point(Seconds::from_integer(0)),
                    end: point(Seconds::from_integer(2)),
                },
                provenance(),
            ),
            Err(ProjectError::Annotation(_, ref e)) if e.contains("no media")
        ));
        assert!(matches!(
            p.add_annotation(
                1,
                note(),
                Anchor::Timeline {
                    start: point(Seconds::from_integer(1)),
                    end: point(Seconds::from_integer(0)),
                },
                provenance(),
            ),
            Err(ProjectError::Annotation(_, ref e)) if e.contains("before it starts")
        ));
        assert!(matches!(
            p.add_annotation(
                1,
                note(),
                Anchor::Timeline {
                    start: ClipPoint {
                        clip: 99,
                        source: Seconds::from_integer(0),
                    },
                    end: point(Seconds::from_integer(0)),
                },
                provenance(),
            ),
            Err(ProjectError::Annotation(_, ref e)) if e.contains("clip #99")
        ));
    }

    #[test]
    fn annotations_guard_the_asset_and_group_they_anchor_to() {
        let mut p = Project::new();
        let (used, _) = p.add_asset(
            "/media/used.mp4".into(),
            probe_report("2", 320, 240, "30/1", false),
        );
        let (spare, _) = p.add_asset(
            "/media/spare.mp4".into(),
            probe_report("2", 320, 240, "30/1", false),
        );
        let id = p
            .add_annotation(
                1,
                AnnotationKind::Marker,
                Anchor::Source {
                    source: SourceRef::Asset { id: used },
                    time: Seconds::from_integer(0),
                },
                provenance(),
            )
            .unwrap();
        assert!(matches!(
            p.remove_asset(used),
            Err(ProjectError::AssetInAnnotation(asset, ref ids))
                if asset == used && ids == &vec![id]
        ));
        assert!(p.asset(used).is_some(), "the asset survives the refusal");

        let group = p
            .add_group(
                "CAM 3",
                vec![MediaPart {
                    asset: spare,
                    group_start: Seconds::from_integer(0),
                    file_start: Seconds::from_integer(0),
                    duration: Seconds::from_integer(2),
                }],
            )
            .unwrap();
        let group_annotation = p
            .add_annotation(
                1,
                AnnotationKind::Marker,
                Anchor::Source {
                    source: SourceRef::Group { id: group },
                    time: Seconds::from_integer(1),
                },
                provenance(),
            )
            .unwrap();
        assert!(matches!(
            p.remove_group(group),
            Err(ProjectError::GroupInAnnotation(id, ref ids))
                if id == group && ids == &vec![group_annotation]
        ));
        assert!(p.group(group).is_some(), "the group survives the refusal");
    }

    #[test]
    fn annotations_round_trip_through_the_project_file() {
        let mut p = Project::new();
        let (asset, _) = p.add_asset(
            "/media/cam.mp4".into(),
            probe_report("10", 320, 240, "30/1", false),
        );
        p.add_annotation(
            1,
            AnnotationKind::Note {
                text: "check the cut".into(),
            },
            Anchor::Source {
                source: SourceRef::Asset { id: asset },
                time: Seconds::new(5, 2),
            },
            provenance(),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nile.json");
        p.save(&path).unwrap();
        let loaded = Project::load(&path).unwrap();
        assert_eq!(loaded.annotations, p.annotations);
        assert_eq!(loaded.annotations[0].text(), Some("check the cut"));
    }

    #[test]
    fn group_parts_follow_argument_order() {
        let mut p = Project::new();
        let (a, _) = p.add_asset(
            "/media/a.mp4".into(),
            probe_report("2", 320, 240, "30/1", false),
        );
        let (b, _) = p.add_asset(
            "/media/b.mp4".into(),
            probe_report("3", 320, 240, "30/1", false),
        );
        let parts = p
            .group_parts(&["/media/b.mp4".into(), "/media/a.mp4".into()])
            .unwrap();

        assert_eq!(parts[0].asset, b);
        assert_eq!(parts[0].group_start, Seconds::from_integer(0));
        assert_eq!(parts[1].asset, a);
        assert_eq!(parts[1].group_start, Seconds::from_integer(3));
        assert!(
            parts
                .iter()
                .all(|part| part.file_start == Seconds::from_integer(0))
        );
    }

    #[test]
    fn group_parts_refuse_duplicates_missing_and_unknown_durations() {
        let mut p = Project::new();
        p.add_asset(
            "/media/a.mp4".into(),
            probe_report("2", 320, 240, "30/1", false),
        );
        let mut nodur = probe_report("2", 320, 240, "30/1", false);
        nodur.format.duration = None;
        p.add_asset("/media/b.webm".into(), nodur);

        assert!(matches!(
            p.group_parts(&["/media/a.mp4".into(), "/media/a.mp4".into()]),
            Err(ProjectError::Group(_, ref detail)) if detail.contains("listed twice")
        ));
        assert!(matches!(
            p.group_parts(&["/media/a.mp4".into(), "/media/nope.mp4".into()]),
            Err(ProjectError::Group(_, ref detail)) if detail.contains("not pooled")
        ));
        assert!(matches!(
            p.group_parts(&["/media/a.mp4".into(), "/media/b.webm".into()]),
            Err(ProjectError::Group(_, ref detail)) if detail.contains("no known duration")
        ));
        assert!(p.groups.is_empty(), "refusals add nothing");
    }

    #[test]
    fn group_time_map_composes_a_retimed_clip() {
        use crate::timeline::{Clip, Segment};
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (asset, _) = p.add_asset(
            "/media/cam.mp4".into(),
            probe_report("20", 320, 240, "30/1", false),
        );
        let mut clip = Clip::new(
            1,
            asset,
            Seconds::from_integer(0),
            Seconds::from_integer(10),
        );
        clip.speed = Seconds::from_integer(2);
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(clip)];
        let group = p
            .add_group(
                "CAM 3",
                vec![
                    MediaPart {
                        asset,
                        group_start: Seconds::from_integer(0),
                        file_start: Seconds::from_integer(0),
                        duration: Seconds::from_integer(4),
                    },
                    MediaPart {
                        asset,
                        group_start: Seconds::from_integer(5),
                        file_start: Seconds::from_integer(4),
                        duration: Seconds::from_integer(6),
                    },
                ],
            )
            .unwrap();
        let map = p.group_time_map(1, 0, group, asset).unwrap();
        assert_eq!(
            map.map(&Seconds::from_integer(1)),
            Some(Seconds::from_integer(2))
        );
        assert_eq!(
            map.map(&Seconds::from_integer(3)),
            Some(Seconds::from_integer(7))
        );
        assert_eq!(
            map.inverse(&Seconds::from_integer(7)),
            Some(Seconds::from_integer(3))
        );
        assert_eq!(map.map(&Seconds::from_integer(5)), None, "timeline end");
    }

    #[test]
    fn remove_asset_refuses_a_group_only_reference() {
        let mut p = sample_project();
        let (spare, _) = p.add_asset(
            "/media/spare.mp4".into(),
            probe_report("5", 320, 240, "30/1", true),
        );
        let group = p
            .add_group(
                "MIC 1",
                vec![MediaPart {
                    asset: spare,
                    group_start: Seconds::from_integer(0),
                    file_start: Seconds::from_integer(0),
                    duration: Seconds::from_integer(2),
                }],
            )
            .unwrap();
        assert_eq!(p.asset_group_usage(spare), vec![group]);
        assert!(matches!(
            p.remove_asset(spare),
            Err(ProjectError::AssetInGroup(id, ref groups)) if id == spare && groups == &vec![group]
        ));
        assert!(p.asset(spare).is_some(), "the asset survives the refusal");
    }

    #[test]
    fn project_diff_reports_tracks_assets_and_sequences() {
        let before = sample_project();
        let mut after = before.clone();
        after.timeline.video_track_mut().unwrap().segments.pop();
        let diff = before.diff(&after);
        assert_eq!(diff.sequences.len(), 1);
        assert_eq!(diff.sequences[0].id, 1);
        let track = &diff.sequences[0].tracks[0];
        assert_eq!(track.index, 0);
        assert_eq!(track.before_segments, 2);
        assert_eq!(track.after_segments, 1);
        assert!(track.segments_changed);
        assert!(!diff.is_empty());

        let mut after = before.clone();
        let (asset, _) = after.add_asset(
            "/media/new.mp4".into(),
            probe_report("2", 320, 240, "30/1", false),
        );
        let diff = before.diff(&after);
        assert_eq!(diff.assets_added, vec![asset]);
        assert!(
            diff.sequences.is_empty(),
            "the pool edit touched no timeline"
        );

        let mut with_sequence = before.clone();
        let id = with_sequence.add_sequence("Second", None, None);
        let diff = before.diff(&with_sequence);
        assert_eq!(diff.sequences_added, vec![id]);
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
