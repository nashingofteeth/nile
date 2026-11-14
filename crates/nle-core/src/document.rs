//! The project and its undo stacks, owned together.
//!
//! They are only meaningful as a pair: undo verifies the exact topology the
//! change was recorded from, so a project swapped without its history (or a
//! history carried across a reload) fails at runtime with "history entry does
//! not apply". Owning both makes that drift unrepresentable.
//!
//! Each sequence keeps its own stack. Switching sequences switches undo, so
//! a change recorded against one timeline can never replay against another.
//!
//! It is also the only type that saves a project, and `save` clamps to the
//! media first — so no front-end can persist a clip that reads past EOF, even
//! a new command that never heard of `clamp_project`. Construction is still a
//! plain [`Project`] (imports build one from an EDL/OTIO/detection); adoption
//! goes through [`Document::new`] or [`Document::rebuild`].

use std::ops::Deref;
use std::path::Path;

use crate::action::{self, Action, Outcome, Register};
use crate::edit::{self, Actor, Batch, EditError, Histories, History, UndoOutcome};
use crate::project::{Project, ProjectError, VideoSettings};
use crate::time::Seconds;

/// A project plus one undo stack per sequence. Undo acts on the active
/// sequence only, so a switch can never replay a change recorded against a
/// different timeline.
#[derive(Debug)]
pub struct Document {
    project: Project,
    histories: Histories,
}

impl Default for Document {
    fn default() -> Self {
        Self::new(Project::new())
    }
}

impl Document {
    /// Adopt a project as-is, with an empty stack (load, tests).
    pub fn new(project: Project) -> Self {
        let mut histories = Histories::default();
        histories.ensure(project.active_id());
        Self { project, histories }
    }

    /// Adopt a project and the stack persisted beside it (a TUI/CLI reload).
    pub fn with_history(project: Project, history: History) -> Self {
        let mut histories = Histories::from_history(project.active_id(), history);
        histories.set_actor(Actor::Human);
        Self { project, histories }
    }

    /// Adopt a project and every sequence's stack (the sidecar load).
    pub fn with_histories(project: Project, histories: Histories) -> Self {
        let mut histories = histories;
        histories.ensure(project.active_id());
        Self { project, histories }
    }

    /// Adopt a freshly built project — init, import, detect — clamped to its
    /// media, with a fresh stack. Returns the clamp notes for the caller to
    /// print; the project is already corrected either way.
    pub fn rebuild(project: Project) -> (Self, Vec<String>) {
        let mut doc = Self::new(project);
        let notes = edit::clamp_project_all(&mut doc.project);
        doc.project.bump_revision();
        (doc, notes)
    }

    /// The monotonic model revision. Compare-and-swap clients use it as the
    /// `base_revision`; playhead-only saves leave it unchanged.
    pub fn revision(&self) -> u64 {
        self.project.revision
    }

    /// Replace the active sequence's stack (`Histories::load` in tests, the
    /// TUI's scratch dump). Other sequences keep theirs.
    pub fn set_history(&mut self, history: History) {
        self.histories.set(self.project.active_id(), history);
    }

    /// Replace every sequence's stack (loading the sidecar).
    pub fn set_histories(&mut self, histories: Histories) {
        self.histories = histories;
        self.histories.ensure(self.project.active_id());
    }

    /// Set the actor recorded on subsequent history entries. The TUI and
    /// one-shot CLI leave the default (`Human`); the protocol sets an agent.
    pub fn set_actor(&mut self, actor: Actor) {
        self.histories.set_actor(actor);
    }

    pub fn actor(&self) -> &Actor {
        self.histories.actor()
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    pub fn history(&self) -> &History {
        self.histories
            .get(self.project.active_id())
            .expect("active sequence has a history")
    }

    pub fn histories(&self) -> &Histories {
        &self.histories
    }

    /// Make `id` the active sequence and switch undo to its stack.
    pub fn select_sequence(&mut self, id: u32) -> Result<(), ProjectError> {
        self.project.select_sequence(id)?;
        self.histories.ensure(id);
        Ok(())
    }

    /// Add an empty sequence (not active) and bump the model revision. A new
    /// sequence is a whole-project change, but it is not undoable.
    pub fn add_sequence(
        &mut self,
        name: impl Into<String>,
        fps: Option<Seconds>,
        video: Option<VideoSettings>,
    ) -> Result<u32, ProjectError> {
        if fps.is_some_and(|fps| fps <= Seconds::from_integer(0)) {
            return Err(ProjectError::NonPositiveFps(
                self.project.next_sequence_id(),
            ));
        }
        let id = self.project.add_sequence(name, fps, video);
        self.histories.ensure(id);
        self.project.bump_revision();
        Ok(id)
    }

    pub fn into_parts(self) -> (Project, Histories) {
        (self.project, self.histories)
    }

    pub fn set_playhead(&mut self, at: f64) -> f64 {
        self.project.set_playhead(at)
    }

    /// Fill the sequence settings from the first clip (a real NLE sets the
    /// project's rate and size once, from the clip that defines it). Filling
    /// a setting is a model write, so it bumps the revision; an already-filled
    /// sequence (the normal repeat call) does not.
    pub fn set_sequence(&mut self, fps: Option<Seconds>, video: Option<VideoSettings>) {
        let fps_set = self.project.fps.is_none() && fps.is_some();
        let video_set = self.project.video.is_none() && video.is_some();
        if fps_set {
            self.project.fps = fps;
        }
        if video_set {
            self.project.video = video;
        }
        if fps_set || video_set {
            self.project.bump_revision();
        }
    }

    /// Run one catalog action, recording it in the history.
    pub fn edit(
        &mut self,
        action: &Action,
        from: f64,
        register: &mut Option<Register>,
    ) -> Result<Outcome, EditError> {
        let active = self.project.active_id();
        let history = self.histories.get_mut(active);
        action::run_action(&mut self.project, history, register, action, from)
    }

    /// Apply a prepared batch (an extend commit builds one before committing).
    pub fn apply(&mut self, batch: Batch, playhead: i64) -> Result<(), EditError> {
        let active = self.project.active_id();
        let history = self.histories.get_mut(active);
        history.apply(&mut self.project, batch, playhead)
    }

    pub fn undo(&mut self) -> Result<UndoOutcome, EditError> {
        let active = self.project.active_id();
        let history = self.histories.get_mut(active);
        history.undo(&mut self.project)
    }

    pub fn redo(&mut self) -> Result<UndoOutcome, EditError> {
        let active = self.project.active_id();
        let history = self.histories.get_mut(active);
        history.redo(&mut self.project)
    }

    pub fn can_undo(&self) -> bool {
        self.history().can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history().can_redo()
    }

    pub fn depth(&self) -> usize {
        self.history().depth()
    }

    pub fn add_asset(
        &mut self,
        path: std::path::PathBuf,
        report: crate::media::RawProbe,
        playhead: i64,
    ) -> (u32, String) {
        let active = self.project.active_id();
        self.histories
            .get_mut(active)
            .add_asset(&mut self.project, path, report, playhead)
    }

    /// Add a batch of files as one undo entry (a picker or CLI import).
    pub fn add_assets(
        &mut self,
        assets: Vec<(std::path::PathBuf, crate::media::RawProbe)>,
        playhead: i64,
    ) -> Vec<u32> {
        let active = self.project.active_id();
        self.histories
            .get_mut(active)
            .add_assets(&mut self.project, assets, playhead)
    }

    pub fn remove_asset(&mut self, id: u32, playhead: i64) -> Result<String, EditError> {
        let active = self.project.active_id();
        self.histories
            .get_mut(active)
            .remove_asset(&mut self.project, id, playhead)
    }

    pub fn edit_tracks<F>(&mut self, playhead: i64, edit: F) -> Result<String, EditError>
    where
        F: FnOnce(&mut Project) -> Result<String, EditError>,
    {
        let active = self.project.active_id();
        self.histories
            .get_mut(active)
            .edit_tracks(&mut self.project, playhead, edit)
    }

    /// Clamp, then save. The only persisted write path. A clamp that actually
    /// repaired the model is a model write, so it bumps the revision; a clean
    /// save — the normal case, and every playhead-only one — leaves it alone.
    pub fn save(&mut self, path: &Path) -> Result<(), ProjectError> {
        if !edit::clamp_project_all(&mut self.project).is_empty() {
            self.project.bump_revision();
        }
        self.project.save(path)
    }

    /// Persist the undo stack beside the project.
    pub fn save_history(&self, path: &Path) -> std::io::Result<()> {
        self.histories.save(path)
    }

    /// Drop every stack and remove the sidecar (a rebuild re-baselines).
    pub fn reset_history(&mut self, project_path: &Path) {
        self.histories = Histories::default();
        self.histories.ensure(self.project.active_id());
        Histories::remove_sidecar(project_path);
    }
}

impl Deref for Document {
    type Target = Project;

    fn deref(&self) -> &Project {
        &self.project
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{RawFormat, RawProbe, RawStream};
    use crate::time::Seconds;
    use crate::timeline::{Clip, Segment};

    fn probe(duration: &str) -> RawProbe {
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
                duration: Some(duration.into()),
                ..Default::default()
            },
        }
    }

    fn document() -> Document {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let (asset, _) = p.add_asset("/media/red.mp4".into(), probe("4"));
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
            id: 1,
            asset,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })];
        Document::new(p)
    }

    #[test]
    fn save_clamps_a_clip_that_reads_past_its_media() {
        let mut doc = document();
        if let Segment::Clip(c) = &mut doc.project.timeline.video_track_mut().unwrap().segments[0] {
            c.asset_out = Seconds::from_integer(9);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.json");
        doc.save(&path).unwrap();
        let loaded = Project::load(&path).unwrap();
        let seg = &loaded.timeline.video_track().unwrap().segments[0];
        assert!(
            seg.duration() <= Seconds::from_integer(4),
            "saved {} s of a 4 s asset",
            crate::time::format_seconds(&seg.duration())
        );
    }

    #[test]
    fn save_clamps_sequences_that_are_not_active() {
        let mut doc = document();
        let second = second_sequence(&mut doc);
        doc.select_sequence(1).unwrap();
        if let Segment::Clip(c) = &mut doc
            .project
            .sequence_mut(second)
            .unwrap()
            .timeline
            .video_track_mut()
            .unwrap()
            .segments[0]
        {
            c.asset_out = Seconds::from_integer(9);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.json");
        doc.save(&path).unwrap();
        let loaded = Project::load(&path).unwrap();
        let seg = &loaded
            .sequence(second)
            .unwrap()
            .timeline
            .video_track()
            .unwrap()
            .segments[0];
        assert!(
            seg.duration() <= Seconds::from_integer(4),
            "clamped to media"
        );
        assert_eq!(loaded.active_id(), 1, "saving kept the active sequence");
    }

    #[test]
    fn rebuild_clamps_and_drops_the_old_stack() {
        let mut doc = document();
        let batch = edit::split_on(&doc, 0, 1.0).unwrap();
        doc.apply(batch, 0).unwrap();
        assert!(doc.can_undo());

        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let (asset, _) = p.add_asset("/media/red.mp4".into(), probe("4"));
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
            id: 7,
            asset,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(9),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })];
        let (doc, notes) = Document::rebuild(p);
        assert_eq!(doc.depth(), 0);
        assert_eq!(doc.revision(), 1, "a rebuild is one model revision");
        assert!(!notes.is_empty(), "the clamp reported the adjustment");
        let seg = &doc.timeline.video_track().unwrap().segments[0];
        assert!(seg.duration() <= Seconds::from_integer(4));
    }

    fn second_sequence(doc: &mut Document) -> u32 {
        let id = doc
            .project
            .add_sequence("Second", Some(Seconds::new(30, 1)), None);
        doc.select_sequence(id).unwrap();
        doc.project.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
            id: 2,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })];
        id
    }

    #[test]
    fn default_document_has_a_usable_history() {
        let doc = Document::default();
        assert_eq!(doc.project().active_id(), 1);
        assert!(!doc.can_undo());
        assert_eq!(doc.depth(), 0);
    }

    #[test]
    fn adding_a_sequence_rejects_a_non_positive_rate_before_mutating() {
        let mut doc = document();
        let before = doc.project.sequences.len();
        assert!(matches!(
            doc.add_sequence("Zero", Some(Seconds::from_integer(0)), None),
            Err(ProjectError::NonPositiveFps(2))
        ));
        assert_eq!(doc.project.sequences.len(), before);
        assert_eq!(doc.revision(), 0, "a refused add is not a model write");
    }

    #[test]
    fn undo_is_scoped_to_the_active_sequence() {
        let mut doc = document();
        let second = second_sequence(&mut doc);
        let batch = edit::split_on(&doc, 0, 1.0).unwrap();
        doc.apply(batch, 0).unwrap();
        assert_eq!(doc.depth(), 1);
        let edited_second = doc.timeline.clone();

        doc.select_sequence(1).unwrap();
        assert_eq!(doc.depth(), 0, "the first sequence's stack is untouched");
        assert!(matches!(doc.undo(), Err(EditError::NothingToUndo)));

        let batch = edit::split_on(&doc, 0, 1.0).unwrap();
        doc.apply(batch, 0).unwrap();
        assert_eq!(doc.depth(), 1);
        doc.undo().unwrap();
        assert_eq!(doc.timeline.video_track().unwrap().segments.len(), 1);
        assert_eq!(
            doc.project.sequence(second).unwrap().timeline,
            edited_second,
            "undoing the first sequence left the second alone"
        );
        assert!(doc.can_redo(), "the first sequence has its redo");

        doc.select_sequence(second).unwrap();
        assert_eq!(doc.depth(), 1, "the second sequence kept its edit");
        assert!(
            !doc.can_redo(),
            "the first sequence's redo did not leak over"
        );
        doc.undo().unwrap();
        assert_eq!(doc.depth(), 0);
        assert_eq!(doc.timeline.video_track().unwrap().segments.len(), 1);
    }

    #[test]
    fn a_pool_edit_belongs_to_the_active_sequences_stack() {
        let mut doc = document();
        let second = second_sequence(&mut doc);
        doc.select_sequence(1).unwrap();
        doc.add_asset("/media/extra.mp4".into(), probe("2"), 0);
        assert_eq!(doc.depth(), 1);
        doc.select_sequence(second).unwrap();
        assert_eq!(doc.depth(), 0, "the pool entry stayed with sequence 1");
        doc.select_sequence(1).unwrap();
        doc.undo().unwrap();
        assert!(
            doc.project
                .asset_by_path(Path::new("/media/extra.mp4"))
                .is_none()
        );
    }

    #[test]
    fn the_sidecar_round_trips_every_sequence() {
        let mut doc = document();
        let second = second_sequence(&mut doc);
        let batch = edit::split_on(&doc, 0, 1.0).unwrap();
        doc.apply(batch, 0).unwrap();
        doc.select_sequence(1).unwrap();
        let batch = edit::split_on(&doc, 0, 1.0).unwrap();
        doc.apply(batch, 0).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.history.json");
        doc.save_history(&path).unwrap();
        let loaded = Histories::load(&path).unwrap();
        assert_eq!(loaded.get(1).unwrap().depth(), 1);
        assert_eq!(loaded.get(second).unwrap().depth(), 1);

        let (project, _) = doc.into_parts();
        let mut reloaded = Document::with_histories(project, loaded);
        assert_eq!(reloaded.depth(), 1);
        reloaded.undo().unwrap();
        reloaded.select_sequence(second).unwrap();
        assert_eq!(reloaded.depth(), 1);
        reloaded.undo().unwrap();
        assert_eq!(reloaded.depth(), 0);
    }

    #[test]
    fn an_edit_is_recorded_and_undone() {
        let mut doc = document();
        let batch = edit::split_on(&doc, 0, 1.0).unwrap();
        doc.apply(batch, 0).unwrap();
        assert_eq!(doc.depth(), 1);
        assert_eq!(doc.timeline.video_track().unwrap().segments.len(), 2);
        doc.undo().unwrap();
        assert_eq!(doc.timeline.video_track().unwrap().segments.len(), 1);
    }

    #[test]
    fn revision_bumps_on_model_changes_not_playhead_saves() {
        let mut doc = document();
        assert_eq!(doc.revision(), 0, "adopting a project does not bump");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nile.json");

        let before = doc.revision();
        doc.set_playhead(1.0);
        doc.save(&path).unwrap();
        assert_eq!(
            doc.revision(),
            before,
            "a playhead-only save leaves the revision alone"
        );

        let batch = edit::split_on(&doc, 0, 1.0).unwrap();
        doc.apply(batch, 0).unwrap();
        assert_eq!(doc.revision(), before + 1);
        doc.undo().unwrap();
        assert_eq!(doc.revision(), before + 2);
        doc.redo().unwrap();
        assert_eq!(doc.revision(), before + 3);

        doc.save(&path).unwrap();
        assert_eq!(Project::load(&path).unwrap().revision, doc.revision());

        let before_asset = doc.revision();
        doc.add_asset("/media/extra.mp4".into(), probe("2"), 0);
        assert_eq!(doc.revision(), before_asset + 1, "a pool add bumps");
    }

    #[test]
    fn filling_the_sequence_settings_bumps_the_revision_once() {
        let mut doc = Document::new(Project::new());
        let before = doc.revision();
        doc.set_sequence(Some(Seconds::new(30, 1)), None);
        assert_eq!(doc.revision(), before + 1, "filling fps is a model write");
        doc.set_sequence(Some(Seconds::new(24, 1)), None);
        assert_eq!(
            doc.revision(),
            before + 1,
            "an already-filled sequence is not rewritten"
        );
    }

    #[test]
    fn a_repairing_save_bumps_the_revision() {
        let mut doc = document();
        if let Segment::Clip(c) = &mut doc.project.timeline.video_track_mut().unwrap().segments[0] {
            c.asset_out = Seconds::from_integer(9);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nile.json");
        let before = doc.revision();
        doc.save(&path).unwrap();
        assert_eq!(doc.revision(), before + 1, "the clamp changed the model");
        assert_eq!(Project::load(&path).unwrap().revision, before + 1);
    }

    #[test]
    fn the_document_sets_the_history_actor() {
        let mut doc = document();
        doc.set_actor(Actor::Agent("planner".into()));
        let batch = edit::split_on(&doc, 0, 1.0).unwrap();
        doc.apply(batch, 0).unwrap();
        let state = doc.history().state();
        let entry = state.done.last().unwrap();
        assert!(matches!(
            &entry.provenance().actor,
            Actor::Agent(name) if name == "planner"
        ));
    }

    #[test]
    fn a_rebuild_bumps_the_revision_once() {
        let (doc, _) = Document::rebuild(Project::new());
        assert_eq!(doc.revision(), 1);
    }
}
