//! Undo/redo history: what changed, how to invert it, and the sidecar that
//! carries both stacks across processes.
//!
//! `History::apply` (timeline) and the asset add/remove methods (pool) are
//! the only ways in: each snapshots the affected state before mutating and
//! records the before/after pair as a `Change`. Undo and redo verify the
//! live state against the recorded side and substitute the other.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::annotation::{Anchor, Annotation, AnnotationKind};
use crate::group::MediaGroup;
use crate::media::RawProbe;
use crate::project::{Asset, Project};
use crate::provenance::{Actor, Provenance};
use crate::time::Seconds;
use crate::timeline::{
    Clip, Effect, Gap, Segment, Track, Transition, TransitionKind, TransitionSource,
};

use super::clamp::clamp_project;
use super::util::video_track_index;
use super::{Batch, EditError, EditOp, HISTORY_VERSION, MAX_HISTORY};

/// Undo/redo stack of project changes. `apply` (timeline) and the asset
/// add/remove methods (pool) are the only ways in: each snapshots the
/// affected state before mutating and stores the before/after pair as a
/// `Change`. Undo and redo verify the live state against the recorded side
/// (segment id/frame for tracks, exact pool equality for assets) and
/// substitute the other.
#[derive(Debug, Default, Clone)]
pub struct History {
    done: Vec<Change>,
    undone: Vec<Change>,
    /// Session-only actor applied to new entries; the sidecar stores the
    /// actor on each change, not a current value.
    actor: Actor,
    /// Session-only: a batch keeps its changes contiguous even at the cap, so
    /// `depth()` counts what the batch produced. `end_batch` trims once.
    defer_cap: bool,
    /// Clamp notes from the most recent action(s), drained by the preview
    /// path. Session-only: the sidecar stores changes, not transient notes.
    warnings: Vec<String>,
}

impl History {
    /// Apply `batch` (records `playhead` — the timeline frame the edit was
    /// made from — so undo/redo can return there).
    pub fn apply(
        &mut self,
        project: &mut Project,
        batch: Batch,
        playhead: i64,
    ) -> Result<(), EditError> {
        if batch.ops.is_empty() {
            project.validate()?;
            return Ok(());
        }
        // A sync ripple edits several tracks at once. Snapshot the whole track
        // list (the same shape as track add/mute/rm), so one undo restores
        // every shifted track together.
        let touched = batch_tracks(&batch);
        if touched.len() > 1 {
            let before = project.timeline.tracks.clone();
            if let Err(e) = apply_batch(project, &batch.ops) {
                project.timeline.tracks = before;
                return Err(e);
            }
            let notes = clamp_project(project);
            if let Err(e) = project.validate() {
                project.timeline.tracks = before;
                return Err(e.into());
            }
            self.warnings.extend(notes);
            self.push_change(Change::Tracks(TracksChange {
                before,
                after: project.timeline.tracks.clone(),
                desc: batch.desc,
                playhead,
                provenance: self.provenance(),
            }));
            project.bump_revision();
            project.bump_manual_edits();
            return Ok(());
        }
        let track = batch_track(&batch, project)?;
        let before = snapshot_track(&project.timeline.tracks[track]);
        let saved = project.timeline.tracks[track].segments.clone();
        // Clamp before snapshotting: the recorded `after` must be the state
        // undo restores, so a batch can never leave a clip reading past EOF
        // or a sub-frame sliver behind.
        if let Err(e) = apply_batch(project, &batch.ops) {
            project.timeline.tracks[track].segments = saved;
            return Err(e);
        }
        let notes = clamp_project(project);
        if let Err(e) = project.validate() {
            project.timeline.tracks[track].segments = saved;
            return Err(e.into());
        }
        self.warnings.extend(notes);
        let after = snapshot_track(&project.timeline.tracks[track]);
        self.push_change(Change::Track(TrackChange {
            track,
            start_frame: 0,
            before,
            after,
            desc: batch.desc,
            playhead,
            provenance: self.provenance(),
        }));
        project.bump_revision();
        project.bump_manual_edits();
        Ok(())
    }

    /// Remove `id` from the media pool and record it so undo restores it.
    /// `Project::remove_asset`'s guard refuses while any clip uses it; the
    /// before/after pool snapshots make the change symmetric.
    pub fn remove_asset(
        &mut self,
        project: &mut Project,
        id: u32,
        playhead: i64,
    ) -> Result<String, EditError> {
        let before = project.media.clone();
        let removed = project.remove_asset(id)?;
        let after = project.media.clone();
        let desc = format!("remove asset #{}", removed.id);
        self.push_change(Change::Assets(AssetChange {
            before,
            after,
            desc: desc.clone(),
            playhead,
            provenance: self.provenance(),
        }));
        project.bump_revision();
        Ok(desc)
    }

    /// Record a whole-track-list edit (add / mute / remove): `edit` mutates
    /// the project and returns the description, and the track lists before
    /// and after are snapshotted — so undo restores the track *set*, not
    /// just its segments. A refused edit (the last video track, a bad
    /// index) leaves the project untouched.
    pub fn edit_tracks<F>(
        &mut self,
        project: &mut Project,
        playhead: i64,
        edit: F,
    ) -> Result<String, EditError>
    where
        F: FnOnce(&mut Project) -> Result<String, EditError>,
    {
        let before = project.timeline.tracks.clone();
        let desc = match edit(project).and_then(|desc| {
            project.validate().map_err(EditError::from)?;
            Ok(desc)
        }) {
            Ok(desc) => desc,
            Err(e) => {
                project.timeline.tracks = before;
                return Err(e);
            }
        };
        self.push_change(Change::Tracks(TracksChange {
            before,
            after: project.timeline.tracks.clone(),
            desc: desc.clone(),
            playhead,
            provenance: self.provenance(),
        }));
        project.bump_revision();
        project.bump_manual_edits();
        Ok(desc)
    }

    /// Add `path` to the pool and record it, so undo removes it again —
    /// refused, loudly, while a clip still uses it (`validate` catches
    /// that). Returns the asset id and the description.
    pub fn add_asset(
        &mut self,
        project: &mut Project,
        path: PathBuf,
        probe: RawProbe,
        playhead: i64,
    ) -> (u32, String) {
        let before = project.media.clone();
        let (id, _) = project.add_asset(path, probe);
        let after = project.media.clone();
        let desc = format!("import asset #{id}");
        let changed = before != after;
        self.push_change(Change::Assets(AssetChange {
            before,
            after,
            desc: desc.clone(),
            playhead,
            provenance: self.provenance(),
        }));
        if changed {
            project.bump_revision();
        }
        (id, desc)
    }

    /// Add a batch of files to the pool as **one** undo step. An import often
    /// brings in several clips; undoing it should remove the whole batch, not
    /// require one `u` per file. Returns the new asset ids in insertion order.
    pub fn add_assets(
        &mut self,
        project: &mut Project,
        assets: Vec<(PathBuf, RawProbe)>,
        playhead: i64,
    ) -> Vec<u32> {
        let before = project.media.clone();
        let mut ids = Vec::new();
        for (path, probe) in assets {
            let (id, added) = project.add_asset(path, probe);
            if added {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            return ids;
        }
        let desc = match ids.as_slice() {
            [id] => format!("import asset #{id}"),
            _ => format!("import {} assets", ids.len()),
        };
        self.push_change(Change::Assets(AssetChange {
            before,
            after: project.media.clone(),
            desc,
            playhead,
            provenance: self.provenance(),
        }));
        project.bump_revision();
        ids
    }

    /// Create a media group and record the group list so undo removes it.
    pub fn add_group(
        &mut self,
        project: &mut Project,
        name: impl Into<String>,
        parts: Vec<crate::group::MediaPart>,
        playhead: i64,
    ) -> Result<u32, EditError> {
        let before = project.groups.clone();
        let id = project.add_group(name, parts)?;
        let desc = format!("add media group #{id}");
        self.push_change(Change::MediaGroups(MediaGroupsChange {
            before,
            after: project.groups.clone(),
            desc,
            playhead,
            provenance: self.provenance(),
        }));
        project.bump_revision();
        Ok(id)
    }

    /// Remove a media group and record the group list so undo restores it.
    pub fn remove_group(
        &mut self,
        project: &mut Project,
        id: u32,
        playhead: i64,
    ) -> Result<String, EditError> {
        let before = project.groups.clone();
        let removed = project.remove_group(id)?;
        let desc = format!("remove media group #{}", removed.id);
        self.push_change(Change::MediaGroups(MediaGroupsChange {
            before,
            after: project.groups.clone(),
            desc: desc.clone(),
            playhead,
            provenance: self.provenance(),
        }));
        project.bump_revision();
        Ok(desc)
    }

    /// Add a source-anchored annotation and record the list.
    pub fn add_annotation(
        &mut self,
        project: &mut Project,
        sequence: u32,
        kind: AnnotationKind,
        anchor: Anchor,
        playhead: i64,
    ) -> Result<u32, EditError> {
        let before = project.annotations.clone();
        let id = project.add_annotation(sequence, kind, anchor, self.provenance())?;
        let desc = format!("add annotation #{id}");
        self.push_change(Change::Annotations(AnnotationsChange {
            before,
            after: project.annotations.clone(),
            desc,
            playhead,
            provenance: self.provenance(),
        }));
        project.bump_revision();
        Ok(id)
    }

    /// Remove an annotation and record the list.
    pub fn remove_annotation(
        &mut self,
        project: &mut Project,
        id: u32,
        playhead: i64,
    ) -> Result<String, EditError> {
        let before = project.annotations.clone();
        let removed = project.remove_annotation(id)?;
        let desc = format!("remove annotation #{}", removed.id);
        self.push_change(Change::Annotations(AnnotationsChange {
            before,
            after: project.annotations.clone(),
            desc: desc.clone(),
            playhead,
            provenance: self.provenance(),
        }));
        project.bump_revision();
        Ok(desc)
    }

    /// Mark an annotation resolved. Resolving an already-resolved one is a
    /// no-op: it records nothing, so undo does not step through dead entries.
    pub fn resolve_annotation(
        &mut self,
        project: &mut Project,
        id: u32,
        playhead: i64,
    ) -> Result<(String, bool), EditError> {
        let before = project.annotations.clone();
        if !project.resolve_annotation(id)? {
            return Ok((format!("annotation #{id} is already resolved"), false));
        }
        let desc = format!("resolve annotation #{id}");
        self.push_change(Change::Annotations(AnnotationsChange {
            before,
            after: project.annotations.clone(),
            desc: desc.clone(),
            playhead,
            provenance: self.provenance(),
        }));
        project.bump_revision();
        Ok((desc, true))
    }

    /// Set the actor recorded on subsequent changes.
    pub fn set_actor(&mut self, actor: Actor) {
        self.actor = actor;
    }

    pub(crate) fn clear_warnings(&mut self) {
        self.warnings.clear();
    }

    pub(crate) fn drain_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    fn provenance(&self) -> Provenance {
        Provenance::now(&self.actor)
    }

    /// Push onto `done`, clear redo, and enforce the depth cap unless a
    /// batch is open.
    fn push_change(&mut self, change: Change) {
        self.done.push(change);
        self.undone.clear();
        if !self.defer_cap {
            self.trim();
        }
    }

    fn trim(&mut self) {
        if self.done.len() > MAX_HISTORY {
            let excess = self.done.len() - MAX_HISTORY;
            self.done.drain(0..excess);
        }
    }

    /// Keep every change a batch records until it can be grouped, even when
    /// the stack is already at `MAX_HISTORY`.
    pub(crate) fn begin_batch(&mut self) {
        self.defer_cap = true;
    }

    /// Close a batch and re-apply the depth cap. The newest group survives;
    /// only older entries are discarded.
    pub(crate) fn end_batch(&mut self) {
        self.defer_cap = false;
        self.trim();
    }

    pub fn undo(&mut self, project: &mut Project) -> Result<UndoOutcome, EditError> {
        let change = self.done.last().ok_or(EditError::NothingToUndo)?.clone();
        let outcome = UndoOutcome {
            desc: change.desc().to_string(),
            playhead: change.playhead(),
            provenance: change.provenance().clone(),
        };
        // Snapshot the whole project so a refused substitution (the live
        // state no longer matches the recorded side) leaves it untouched.
        let snapshot = project.clone();
        if let Err(e) = substitute(project, &change, false) {
            *project = snapshot;
            return Err(e);
        }
        self.done.pop();
        self.undone.push(change);
        project.bump_revision();
        Ok(outcome)
    }

    pub fn redo(&mut self, project: &mut Project) -> Result<UndoOutcome, EditError> {
        let change = self.undone.last().ok_or(EditError::NothingToRedo)?.clone();
        let outcome = UndoOutcome {
            desc: change.desc().to_string(),
            playhead: change.playhead(),
            provenance: change.provenance().clone(),
        };
        let snapshot = project.clone();
        if let Err(e) = substitute(project, &change, true) {
            *project = snapshot;
            return Err(e);
        }
        self.undone.pop();
        self.done.push(change);
        project.bump_revision();
        Ok(outcome)
    }

    /// Wrap the newest `count` changes in one `Change::Group`, so a batch of
    /// actions undoes as a single step. The group's anchor is the caller's
    /// batch `from`; its provenance is the session's current actor.
    pub(crate) fn group_newest(
        &mut self,
        count: usize,
        desc: String,
        playhead: i64,
    ) -> Result<(), EditError> {
        if count < 2 || count > self.done.len() {
            return Err(EditError::History(format!(
                "cannot group {count} history entries"
            )));
        }
        let changes = self.done.split_off(self.done.len() - count);
        let provenance = self.provenance();
        self.done.push(Change::Group(GroupChange {
            changes,
            desc,
            playhead,
            provenance,
        }));
        Ok(())
    }

    pub fn can_undo(&self) -> bool {
        !self.done.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    pub fn depth(&self) -> usize {
        self.done.len()
    }

    /// Serializable snapshot of this stack's undo entries. Both stacks are
    /// stored: `nile redo` runs in its own process, so a session-only redo
    /// stack would make it permanently empty.
    pub fn state(&self) -> HistoryState {
        HistoryState {
            done: self.done.clone(),
            undone: self.undone.clone(),
        }
    }

    /// Rebuild a History from a snapshot, honoring the depth cap.
    pub fn restore(state: HistoryState) -> Self {
        let mut done = state.done;
        if done.len() > MAX_HISTORY {
            let excess = done.len() - MAX_HISTORY;
            done.drain(0..excess);
        }
        let mut undone = state.undone;
        if undone.len() > MAX_HISTORY {
            let excess = undone.len() - MAX_HISTORY;
            undone.drain(0..excess);
        }
        Self {
            done,
            undone,
            actor: Actor::Human,
            defer_cap: false,
            warnings: Vec::new(),
        }
    }
}

/// Every sequence's undo stack, keyed by sequence id. Undo is a session
/// action against the active sequence, so keeping the stacks apart is what
/// stops it from replaying a change recorded against another timeline.
#[derive(Debug, Default, Clone)]
pub struct Histories {
    stacks: BTreeMap<u32, History>,
    /// Session-only actor applied to new entries in every stack.
    actor: Actor,
}

impl Histories {
    pub fn from_history(sequence: u32, history: History) -> Self {
        let mut stacks = BTreeMap::new();
        stacks.insert(sequence, history);
        Self {
            stacks,
            actor: Actor::Human,
        }
    }

    /// Make sure `sequence` has a stack (an untouched sequence starts empty).
    pub fn ensure(&mut self, sequence: u32) {
        if !self.stacks.contains_key(&sequence) {
            let mut history = History::default();
            history.set_actor(self.actor.clone());
            self.stacks.insert(sequence, history);
        }
    }

    pub fn get(&self, sequence: u32) -> Option<&History> {
        self.stacks.get(&sequence)
    }

    /// The stack for `sequence`, created on first use.
    pub fn get_mut(&mut self, sequence: u32) -> &mut History {
        self.ensure(sequence);
        self.stacks.get_mut(&sequence).expect("just ensured")
    }

    pub fn set(&mut self, sequence: u32, history: History) {
        self.stacks.insert(sequence, history);
    }

    pub fn set_actor(&mut self, actor: Actor) {
        self.actor = actor.clone();
        for history in self.stacks.values_mut() {
            history.set_actor(actor.clone());
        }
    }

    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// Drop every stack and remove the sidecar (a rebuild re-baselines).
    pub fn remove_sidecar(project_path: &Path) {
        let _ = std::fs::remove_file(Self::sidecar_path(project_path));
        let _ = std::fs::remove_file(crate::state::legacy_history_path(project_path));
    }

    /// Sidecar path for a project file (`nile.json` →
    /// `.nile/nile/history.json`). The histories live in the project's state
    /// directory so the whole set moves together.
    pub fn sidecar_path(project_path: &Path) -> PathBuf {
        crate::state::history_path(project_path)
    }

    /// Load a project's histories, copying an old adjacent sidecar into the
    /// state directory once if the new one does not exist yet.
    pub fn load_project(project_path: &Path) -> Result<Self, std::io::Error> {
        let path = Self::sidecar_path(project_path);
        match Self::load(&path) {
            Ok(histories) => Ok(histories),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let legacy = crate::state::legacy_history_path(project_path);
                if !legacy.exists() {
                    return Err(e);
                }
                let histories = Self::load(&legacy)?;
                let _ = histories.save(&path);
                Ok(histories)
            }
            Err(e) => Err(e),
        }
    }

    /// Atomic save to the sidecar file (crash can't corrupt it).
    pub fn save(&self, path: &Path) -> Result<(), std::io::Error> {
        let mut json = serde_json::to_string_pretty(&self.state())?;
        json.push('\n');
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;
        let tmp = NamedTempFile::new_in(dir)?;
        tmp.as_file().write_all(json.as_bytes())?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    /// Load a sidecar. Missing, corrupt or old-version file → error; use
    /// `Histories::default()` at the call site to start fresh.
    pub fn load(path: &Path) -> Result<Self, std::io::Error> {
        let text = std::fs::read_to_string(path)?;
        let state: HistoriesState = serde_json::from_str(&text)
            .map_err(|e| std::io::Error::other(format!("corrupt history file: {e}")))?;
        if state.version != HISTORY_VERSION {
            return Err(std::io::Error::other(format!(
                "history version {} unsupported (this build reads v{HISTORY_VERSION})",
                state.version
            )));
        }
        Ok(Self::restore(state))
    }

    pub fn state(&self) -> HistoriesState {
        HistoriesState {
            version: HISTORY_VERSION,
            stacks: self
                .stacks
                .iter()
                .map(|(sequence, history)| (*sequence, history.state()))
                .collect(),
        }
    }

    pub fn restore(state: HistoriesState) -> Self {
        let mut restored = Self::default();
        for (sequence, history) in state.stacks {
            restored.stacks.insert(sequence, History::restore(history));
        }
        restored
    }
}

/// Serializable snapshot of one sequence's undo stacks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryState {
    pub done: Vec<Change>,
    pub undone: Vec<Change>,
}

/// The sidecar file: every sequence's undo stack together. Both stacks per
/// sequence persist so `nile undo` / `nile redo` work across processes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoriesState {
    pub version: u32,
    pub stacks: BTreeMap<u32, HistoryState>,
}

/// One timeline segment, snapshotted for the undo stack. Clips are located
/// by segment id (robust to index shifts) and carry their **exact** source
/// times and speed, so undo/redo restores the clip bit-for-bit. (The old
/// frame-count form quantized source seconds to the project fps, which
/// perturbed speed-changed clips on every undo until sub-frame slivers
/// appeared and a zero-frame span failed validation.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Atom {
    Clip {
        id: u32,
        asset: u32,
        #[serde(with = "crate::time::rational_ser")]
        asset_in: Seconds,
        #[serde(with = "crate::time::rational_ser")]
        asset_out: Seconds,
        /// Playback speed as a reduced fraction; `1/1` is normal. Rational
        /// (via `rational_ser`, like every other time here) so undo/redo is
        /// exact — and so a corrupt sidecar is rejected at parse time
        /// instead of panicking inside `Ratio::new` on a zero denominator.
        #[serde(with = "crate::time::rational_ser")]
        speed: Seconds,
        /// Per-clip effects, so undo/redo restores them bit-for-bit.
        effects: Vec<Effect>,
    },
    Gap {
        #[serde(with = "crate::time::rational_ser")]
        duration: Seconds,
    },
    /// A transition, with both of its source sides (as stored on the model
    /// segment), so undo/redo restores it bit-for-bit.
    Transition {
        id: u32,
        #[serde(with = "crate::time::rational_ser")]
        dur: Seconds,
        kind: TransitionKind,
        a: TransitionSource,
        b: TransitionSource,
    },
}

/// One undoable user action. A timeline edit carries the affected track's
/// before/after topology (verified by segment id/frame counts); a pool edit
/// carries the media list before/after. The enum keeps undo/redo exhaustive:
/// a new change kind is a compile error until it is handled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    Track(TrackChange),
    Tracks(TracksChange),
    Assets(AssetChange),
    Group(GroupChange),
    MediaGroups(MediaGroupsChange),
    Annotations(AnnotationsChange),
}

impl Change {
    pub fn desc(&self) -> &str {
        match self {
            Change::Track(c) => &c.desc,
            Change::Tracks(c) => &c.desc,
            Change::Assets(c) => &c.desc,
            Change::Group(c) => &c.desc,
            Change::MediaGroups(c) => &c.desc,
            Change::Annotations(c) => &c.desc,
        }
    }

    pub fn playhead(&self) -> i64 {
        match self {
            Change::Track(c) => c.playhead,
            Change::Tracks(c) => c.playhead,
            Change::Assets(c) => c.playhead,
            Change::Group(c) => c.playhead,
            Change::MediaGroups(c) => c.playhead,
            Change::Annotations(c) => c.playhead,
        }
    }

    pub fn provenance(&self) -> &Provenance {
        match self {
            Change::Track(c) => &c.provenance,
            Change::Tracks(c) => &c.provenance,
            Change::Assets(c) => &c.provenance,
            Change::Group(c) => &c.provenance,
            Change::MediaGroups(c) => &c.provenance,
            Change::Annotations(c) => &c.provenance,
        }
    }
}

/// A timeline edit: the affected track's segments before and after, in
/// frames. Undo substitutes `before` for `after`; redo does the reverse.
/// `start_frame` is reserved (whole-track snapshots start at 0) for future
/// region-scoped entries. `playhead` is the timeline frame the edit was made
/// from, restored on undo/redo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackChange {
    pub track: usize,
    pub start_frame: i64,
    pub before: Vec<Atom>,
    pub after: Vec<Atom>,
    pub desc: String,
    pub playhead: i64,
    pub provenance: Provenance,
}

/// A whole-track-list edit — adding, muting, or removing a track. Segment
/// edits carry a `TrackChange` (one track's topology); this is for the
/// operations that change the track *set*, which segment snapshots can't
/// express.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TracksChange {
    pub before: Vec<Track>,
    pub after: Vec<Track>,
    pub desc: String,
    pub playhead: i64,
    pub provenance: Provenance,
}

/// A media-pool edit: the whole pool before and after (probes included).
/// Undo verifies the pool matches the recorded side, so an out-of-band change
/// refuses loudly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetChange {
    pub before: Vec<Asset>,
    pub after: Vec<Asset>,
    pub desc: String,
    pub playhead: i64,
    pub provenance: Provenance,
}

/// Several changes applied by one call, so one undo step restores the whole
/// batch. Undo runs the children in reverse, redo in order; each child still
/// verifies its own recorded side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupChange {
    pub changes: Vec<Change>,
    pub desc: String,
    pub playhead: i64,
    pub provenance: Provenance,
}

/// A media-group list edit: creating or removing a logical recording.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaGroupsChange {
    pub before: Vec<MediaGroup>,
    pub after: Vec<MediaGroup>,
    pub desc: String,
    pub playhead: i64,
    pub provenance: Provenance,
}

/// An annotation-list edit: adding, removing or resolving an annotation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnnotationsChange {
    pub before: Vec<Annotation>,
    pub after: Vec<Annotation>,
    pub desc: String,
    pub playhead: i64,
    pub provenance: Provenance,
}

/// What an undo/redo restored: the description and the playhead frame.
#[derive(Debug, Clone, PartialEq)]
pub struct UndoOutcome {
    pub desc: String,
    pub playhead: i64,
    pub provenance: Provenance,
}

fn atom_from_segment(seg: &Segment) -> Atom {
    match seg {
        Segment::Clip(c) => Atom::Clip {
            id: c.id,
            asset: c.asset,
            asset_in: c.asset_in,
            asset_out: c.asset_out,
            speed: c.speed,
            effects: c.effects.clone(),
        },
        Segment::Gap(g) => Atom::Gap {
            duration: g.duration,
        },
        Segment::Transition(t) => Atom::Transition {
            id: t.id,
            dur: t.dur,
            kind: t.kind,
            a: t.a.clone(),
            b: t.b.clone(),
        },
    }
}

fn segment_from_atom(atom: &Atom) -> Segment {
    match atom {
        Atom::Clip {
            id,
            asset,
            asset_in,
            asset_out,
            speed,
            effects,
        } => Segment::Clip(Clip {
            id: *id,
            asset: *asset,
            asset_in: *asset_in,
            asset_out: *asset_out,
            speed: *speed,
            effects: effects.clone(),
        }),
        Atom::Gap { duration } => Segment::Gap(Gap {
            duration: *duration,
        }),
        Atom::Transition {
            id,
            dur,
            kind,
            a,
            b,
        } => Segment::Transition(Transition {
            id: *id,
            dur: *dur,
            kind: *kind,
            a: a.clone(),
            b: b.clone(),
        }),
    }
}

fn snapshot_track(track: &Track) -> Vec<Atom> {
    track.segments.iter().map(atom_from_segment).collect()
}

/// The distinct tracks a batch touches, in first-seen order.
fn batch_tracks(batch: &Batch) -> Vec<usize> {
    let mut tracks = Vec::new();
    for op in &batch.ops {
        let track = match op {
            EditOp::Insert { track, .. }
            | EditOp::Remove { track, .. }
            | EditOp::Replace { track, .. } => *track,
        };
        if !tracks.contains(&track) {
            tracks.push(track);
        }
    }
    tracks
}

/// Which track a single-track batch touches.
fn batch_track(batch: &Batch, project: &Project) -> Result<usize, EditError> {
    match batch.ops.first() {
        Some(EditOp::Insert { track, .. })
        | Some(EditOp::Remove { track, .. })
        | Some(EditOp::Replace { track, .. }) => Ok(*track),
        None => video_track_index(project),
    }
}

/// Verify the live project still matches one side of `change` and substitute
/// the other. `forward == false` is undo (expect `after`, write `before`);
/// `true` is redo. Revalidates afterwards so a stale/foreign entry refuses
/// rather than corrupting the project.
fn substitute(project: &mut Project, change: &Change, forward: bool) -> Result<(), EditError> {
    match change {
        Change::Track(c) => substitute_topology(project, c, forward)?,
        Change::Tracks(c) => substitute_tracks(project, c, forward)?,
        Change::Assets(c) => substitute_assets(project, c, forward)?,
        Change::MediaGroups(c) => substitute_groups(project, c, forward)?,
        Change::Annotations(c) => substitute_annotations(project, c, forward)?,
        Change::Group(c) => {
            if forward {
                for change in &c.changes {
                    substitute(project, change, true)?;
                }
            } else {
                for change in c.changes.iter().rev() {
                    substitute(project, change, false)?;
                }
            }
        }
    }
    project.validate().map_err(EditError::from)
}

fn substitute_tracks(
    project: &mut Project,
    change: &TracksChange,
    forward: bool,
) -> Result<(), EditError> {
    let (expected, write) = if forward {
        (&change.before, &change.after)
    } else {
        (&change.after, &change.before)
    };
    if project.timeline.tracks != *expected {
        return Err(EditError::History(
            "track list changed outside the undo stack".to_string(),
        ));
    }
    project.timeline.tracks = write.clone();
    Ok(())
}

/// Verify the live track matches one side of `change` (by segment id and
/// frame counts) and substitute the other.
fn substitute_topology(
    project: &mut Project,
    change: &TrackChange,
    forward: bool,
) -> Result<(), EditError> {
    let track = project
        .timeline
        .tracks
        .get_mut(change.track)
        .ok_or(EditError::TrackNotFound(change.track))?;
    let (expected, write) = if forward {
        (&change.before, &change.after)
    } else {
        (&change.after, &change.before)
    };
    let live = snapshot_track(track);
    if live != *expected {
        return Err(EditError::History(
            "timeline changed outside the undo stack".to_string(),
        ));
    }
    track.segments = write.iter().map(segment_from_atom).collect();
    Ok(())
}

/// Verify the live group list matches one side of `change` (exact equality)
/// and substitute the other.
fn substitute_groups(
    project: &mut Project,
    change: &MediaGroupsChange,
    forward: bool,
) -> Result<(), EditError> {
    let (expected, write) = if forward {
        (&change.before, &change.after)
    } else {
        (&change.after, &change.before)
    };
    if project.groups != *expected {
        return Err(EditError::History(
            "media groups changed outside the undo stack".to_string(),
        ));
    }
    project.groups = write.clone();
    Ok(())
}

/// Verify the live media pool matches one side of `change` (exact equality,
/// including probes) and substitute the other.
/// Verify the live annotation list matches one side of `change` (exact
/// equality) and substitute the other.
fn substitute_annotations(
    project: &mut Project,
    change: &AnnotationsChange,
    forward: bool,
) -> Result<(), EditError> {
    let (expected, write) = if forward {
        (&change.before, &change.after)
    } else {
        (&change.after, &change.before)
    };
    if project.annotations != *expected {
        return Err(EditError::History(
            "annotations changed outside the undo stack".to_string(),
        ));
    }
    project.annotations = write.clone();
    Ok(())
}

fn substitute_assets(
    project: &mut Project,
    change: &AssetChange,
    forward: bool,
) -> Result<(), EditError> {
    let (expected, write) = if forward {
        (&change.before, &change.after)
    } else {
        (&change.after, &change.before)
    };
    if project.media != *expected {
        return Err(EditError::History(
            "media pool changed outside the undo stack".to_string(),
        ));
    }
    project.media = write.clone();
    Ok(())
}

/// Apply ops in order; on any failure, roll back the applied prefix in
/// reverse so the project is exactly as it was before.
fn apply_batch(project: &mut Project, ops: &[EditOp]) -> Result<(), EditError> {
    for (applied, op) in ops.iter().enumerate() {
        if let Err(e) = op.apply(project) {
            for prev in ops[..applied].iter().rev() {
                // Inverses of applied ops must succeed; if they don't,
                // that's a bug we want to see loudly, not silently corrupt.
                prev.invert().apply(project).expect("history rollback");
            }
            return Err(e);
        }
    }
    Ok(())
}
