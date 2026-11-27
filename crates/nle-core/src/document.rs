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
use crate::annotation::{Anchor, AnnotationKind};
use crate::edit::{self, Actor, Batch, EditError, Histories, History, UndoOutcome};
use crate::project::{AudioSettings, Project, ProjectDiff, ProjectError, VideoSettings};
use crate::time::{Seconds, frame_of};

/// A project plus one undo stack per sequence. Undo acts on the active
/// sequence only, so a switch can never replay a change recorded against a
/// different timeline.
#[derive(Debug)]
pub struct Document {
    project: Project,
    histories: Histories,
}

/// What a batch produced: the normal action outcome plus any clamp notes
/// collected while applying it. Warnings are session-only and preview-only.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchOutcome {
    pub outcome: Outcome,
    pub warnings: Vec<String>,
}

/// What a preview produced: the hypothetical outcome, a structural diff and
/// the clamp warnings the real apply would have to repair. Nothing is saved.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewOutcome {
    pub outcome: Outcome,
    pub diff: ProjectDiff,
    pub warnings: Vec<String>,
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

    /// Run one catalog action, recording it in the history. Clamp notes and
    /// dropped-annotation warnings are appended to the outcome's description:
    /// a one-shot CLI/TUI edit has no other channel for them (the preview
    /// path returns them structured through [`Document::edit_actions`]).
    pub fn edit(
        &mut self,
        action: &Action,
        from: f64,
        register: &mut Option<Register>,
    ) -> Result<Outcome, EditError> {
        self.clear_warnings();
        let mut outcome = self.edit_action(action, from, register);
        let warnings = self.drain_warnings();
        if let Ok(outcome) = &mut outcome
            && !warnings.is_empty()
        {
            outcome.desc = format!("{} — {}", outcome.desc, warnings.join("; "));
        }
        outcome
    }

    fn edit_action(
        &mut self,
        action: &Action,
        from: f64,
        register: &mut Option<Register>,
    ) -> Result<Outcome, EditError> {
        let active = self.project.active_id();
        let history = self.histories.get_mut(active);
        action::run_action(&mut self.project, history, register, action, from)
    }

    fn clear_warnings(&mut self) {
        let active = self.project.active_id();
        self.histories.get_mut(active).clear_warnings();
    }

    fn drain_warnings(&mut self) -> Vec<String> {
        let active = self.project.active_id();
        self.histories.get_mut(active).drain_warnings()
    }

    /// Apply a prepared batch (an extend commit builds one before committing).
    pub fn apply(&mut self, batch: Batch, playhead: i64) -> Result<(), EditError> {
        self.clear_warnings();
        let result = {
            let active = self.project.active_id();
            let history = self.histories.get_mut(active);
            history.apply(&mut self.project, batch, playhead)
        };
        self.clear_warnings();
        result
    }

    /// Run several catalog actions as one undo step. The project, every
    /// history stack and the register are snapshotted first: a failure in any
    /// action restores all three, so a refused batch leaves no trace. A
    /// one-action batch is the normal [`Document::edit`] path.
    pub fn edit_actions(
        &mut self,
        actions: &[Action],
        from: f64,
        register: &mut Option<Register>,
    ) -> Result<BatchOutcome, EditError> {
        if actions.is_empty() {
            return Err(EditError::EmptyBatch);
        }
        if actions.len() > 1
            && actions
                .iter()
                .any(|action| matches!(action, Action::Undo | Action::Redo))
        {
            return Err(EditError::BatchUndoRedo);
        }

        let project = self.project.clone();
        let histories = self.histories.clone();
        let saved_register = register.clone();
        let active = self.project.active_id();
        let depth = self.depth();
        self.histories.get_mut(active).clear_warnings();
        if actions.len() == 1 {
            let outcome = self.edit_action(&actions[0], from, register)?;
            let warnings = self.drain_warnings();
            return Ok(BatchOutcome { outcome, warnings });
        }

        self.histories.get_mut(active).begin_batch();
        let mut descs = Vec::with_capacity(actions.len());
        let mut changed = false;
        let mut playhead = None;
        for action in actions {
            match self.edit_action(action, from, register) {
                Ok(outcome) => {
                    changed |= outcome.changed;
                    if let Some(frame) = outcome.playhead {
                        playhead = Some(frame);
                    }
                    descs.push(outcome.desc);
                }
                Err(e) => {
                    self.project = project;
                    self.histories = histories;
                    *register = saved_register;
                    return Err(e);
                }
            }
        }
        let added = self.depth().saturating_sub(depth);
        if added > 1 {
            let desc = format!("batch: {}", descs.join(" · "));
            let anchor = frame_of(self.project.fps.as_ref(), from);
            if let Err(e) = self
                .histories
                .get_mut(active)
                .group_newest(added, desc.clone(), anchor)
            {
                self.project = project;
                self.histories = histories;
                *register = saved_register;
                return Err(e);
            }
            self.histories.get_mut(active).end_batch();
            let warnings = self.drain_warnings();
            return Ok(BatchOutcome {
                outcome: Outcome {
                    desc,
                    changed,
                    playhead,
                },
                warnings,
            });
        }
        self.histories.get_mut(active).end_batch();
        let warnings = self.drain_warnings();
        Ok(BatchOutcome {
            outcome: Outcome {
                desc: descs.join(" · "),
                changed,
                playhead,
            },
            warnings,
        })
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

    pub fn remove_group(&mut self, id: u32, playhead: i64) -> Result<String, EditError> {
        let active = self.project.active_id();
        self.histories
            .get_mut(active)
            .remove_group(&mut self.project, id, playhead)
    }

    /// Add an annotation to the active sequence.
    pub fn add_annotation(
        &mut self,
        kind: AnnotationKind,
        anchor: Anchor,
        playhead: i64,
    ) -> Result<u32, EditError> {
        let active = self.project.active_id();
        self.histories.get_mut(active).add_annotation(
            &mut self.project,
            active,
            kind,
            anchor,
            playhead,
        )
    }

    pub fn remove_annotation(&mut self, id: u32, playhead: i64) -> Result<String, EditError> {
        let active = self.project.active_id();
        self.histories
            .get_mut(active)
            .remove_annotation(&mut self.project, active, id, playhead)
    }

    pub fn resolve_annotation(
        &mut self,
        id: u32,
        playhead: i64,
    ) -> Result<(String, bool), EditError> {
        let active = self.project.active_id();
        self.histories
            .get_mut(active)
            .resolve_annotation(&mut self.project, active, id, playhead)
    }

    /// Add assets and a media group as one undo step. `files` is the full
    /// ordered part list (existing assets are allowed); any failure restores
    /// the project and every history stack, so the asset batch is rolled back
    /// with the group.
    pub fn add_assets_with_group(
        &mut self,
        assets: Vec<(std::path::PathBuf, crate::media::RawProbe)>,
        playhead: i64,
        name: impl Into<String>,
        files: Vec<std::path::PathBuf>,
    ) -> Result<(Vec<u32>, u32), EditError> {
        let project = self.project.clone();
        let histories = self.histories.clone();
        let result = self.add_assets_with_group_inner(assets, playhead, name, files);
        if result.is_err() {
            self.project = project;
            self.histories = histories;
        }
        result
    }

    /// The fallible body; [`Self::add_assets_with_group`] owns the rollback so
    /// every early return restores both snapshots.
    fn add_assets_with_group_inner(
        &mut self,
        assets: Vec<(std::path::PathBuf, crate::media::RawProbe)>,
        playhead: i64,
        name: impl Into<String>,
        files: Vec<std::path::PathBuf>,
    ) -> Result<(Vec<u32>, u32), EditError> {
        let active = self.project.active_id();
        let depth = self.depth();
        self.histories.get_mut(active).begin_batch();
        let ids = self
            .histories
            .get_mut(active)
            .add_assets(&mut self.project, assets, playhead);
        let parts = self.project.group_parts(&files)?;
        let group =
            self.histories
                .get_mut(active)
                .add_group(&mut self.project, name, parts, playhead)?;
        let added = self.depth().saturating_sub(depth);
        if added > 1 {
            let desc = format!("batch: import {} asset(s) + group #{group}", ids.len());
            self.histories
                .get_mut(active)
                .group_newest(added, desc, playhead)?;
        }
        self.histories.get_mut(active).end_batch();
        Ok((ids, group))
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

    /// Set the active sequence's audio-post settings, recording one undo step.
    pub fn set_audio(&mut self, audio: AudioSettings, playhead: i64) -> Result<String, EditError> {
        let active = self.project.active_id();
        self.histories
            .get_mut(active)
            .set_audio(&mut self.project, audio, playhead)
    }

    /// Clamp, validate, then save. The only persisted write path: a model that
    /// would not load again (including a group over an unpooled asset) must not
    /// reach disk. A clamp that actually repaired the model is a model write,
    /// so it bumps the revision; a clean save — the normal case, and every
    /// playhead-only one — leaves it alone.
    pub fn save(&mut self, path: &Path) -> Result<(), ProjectError> {
        // Repair a copy: clamping (including each sequence's stored playhead,
        // session state a front-end may have left outside `[0, last_frame]`)
        // and the revision bump are writes. A model `validate` refuses must
        // leave the in-memory document exactly as it was; only a persist that
        // succeeded adopts the repaired copy.
        let mut project = self.project.clone();
        if !edit::clamp_project_all(&mut project).is_empty() {
            project.bump_revision();
        }
        project.validate()?;
        project.save(path)?;
        self.project = project;
        Ok(())
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

    fn without_revision(p: &Project) -> Project {
        let mut p = p.clone();
        p.revision = 0;
        for sequence in &mut p.sequences {
            sequence.manual_edits = 0;
        }
        p
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
    fn crop_keys_snap_height_and_stay_inside_the_clip() {
        use crate::timeline::{CropKey, Effect};

        let mut doc = document();
        let mut register = None;
        doc.edit(
            &crate::Action::AddEffect {
                track: 0,
                clip: 1,
                effect: Effect::Crop {
                    x: 0.0,
                    y: 0.0,
                    w: 100.0,
                    h: 100.0,
                    keys: Vec::new(),
                },
                index: None,
            },
            0.0,
            &mut register,
        )
        .unwrap();
        doc.edit(
            &crate::Action::CropKey {
                track: 0,
                clip: 1,
                at: 1.0,
                x: 10.0,
                y: 0.0,
                w: 90.0,
                h: 37.0,
            },
            0.0,
            &mut register,
        )
        .unwrap();
        let keys = |doc: &Document| -> Vec<CropKey> {
            let Segment::Clip(clip) = &doc.project.timeline.video_track().unwrap().segments[0]
            else {
                panic!("clip");
            };
            let Effect::Crop { keys, .. } = clip
                .effects
                .iter()
                .find(|effect| matches!(effect, Effect::Crop { .. }))
                .expect("crop")
            else {
                panic!("crop");
            };
            keys.clone()
        };
        let first = keys(&doc);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].h, 40.0, "37 snaps to the 40% rung");
        assert_eq!(first[0].at, Seconds::from_integer(1));

        // A key at the same timeline frame replaces the old one.
        doc.edit(
            &crate::Action::CropKey {
                track: 0,
                clip: 1,
                at: 1.0,
                x: 20.0,
                y: 0.0,
                w: 70.0,
                h: 50.0,
            },
            0.0,
            &mut register,
        )
        .unwrap();
        assert_eq!(
            keys(&doc),
            vec![CropKey {
                at: Seconds::from_integer(1),
                x: 20.0,
                y: 0.0,
                w: 70.0,
                h: 50.0
            }]
        );

        // Snapping must fit below y, not push the window past the frame.
        doc.edit(
            &crate::Action::CropKey {
                track: 0,
                clip: 1,
                at: 1.0,
                x: 0.0,
                y: 62.0,
                w: 50.0,
                h: 38.0,
            },
            0.0,
            &mut register,
        )
        .unwrap();
        assert_eq!(keys(&doc)[0].h, 33.0, "38 snaps down to fit below y=62");
        assert!(matches!(
            doc.edit(
                &crate::Action::CropKey {
                    track: 0,
                    clip: 1,
                    at: 1.0,
                    x: 0.0,
                    y: 95.0,
                    w: 5.0,
                    h: 5.0,
                },
                0.0,
                &mut register,
            ),
            Err(EditError::CropKeyHeight)
        ));

        assert!(matches!(
            doc.edit(
                &crate::Action::CropKey {
                    track: 0,
                    clip: 1,
                    at: 2.0,
                    x: 0.0,
                    y: 0.0,
                    w: 100.0,
                    h: 100.0,
                },
                0.0,
                &mut register,
            ),
            Err(EditError::CropKeyOutside)
        ));
    }

    #[test]
    fn timeline_ranges_resolve_both_ends_and_refuse_a_reverse() {
        use crate::annotation::{Anchor, AnnotationKind, ClipPoint};

        let mut doc = document();
        let mut register = None;
        doc.edit(
            &crate::Action::AnnotationAddAt {
                track: 0,
                at: 0.5,
                to: Some(1.5),
                kind: AnnotationKind::Marker,
            },
            0.0,
            &mut register,
        )
        .unwrap();
        let Anchor::Timeline { start, end } = &doc.project.annotations[0].anchor else {
            panic!("timeline anchor");
        };
        assert_eq!(
            start,
            &ClipPoint {
                clip: 1,
                source: Seconds::new(1, 2)
            }
        );
        assert_eq!(
            end,
            &ClipPoint {
                clip: 1,
                source: Seconds::new(3, 2)
            }
        );

        // Ending on the clip's tail uses the closed boundary.
        doc.edit(
            &crate::Action::AnnotationAddAt {
                track: 0,
                at: 0.5,
                to: Some(2.0),
                kind: AnnotationKind::Marker,
            },
            0.0,
            &mut register,
        )
        .unwrap();
        let Anchor::Timeline { end, .. } = &doc.project.annotations[1].anchor else {
            panic!("timeline anchor");
        };
        assert_eq!(end.source, Seconds::from_integer(2));

        assert!(matches!(
            doc.edit(
                &crate::Action::AnnotationAddAt {
                    track: 0,
                    at: 1.5,
                    to: Some(0.5),
                    kind: AnnotationKind::Marker,
                },
                0.0,
                &mut register,
            ),
            Err(EditError::AnnotationRange)
        ));
    }

    #[test]
    fn a_dropped_annotation_is_reported_in_the_outcome() {
        use crate::annotation::{Anchor, AnnotationKind, ClipPoint};

        let mut doc = document();
        doc.add_annotation(
            AnnotationKind::Marker,
            Anchor::Timeline {
                start: ClipPoint {
                    clip: 1,
                    source: Seconds::new(1, 2),
                },
                end: ClipPoint {
                    clip: 1,
                    source: Seconds::new(1, 2),
                },
            },
            0,
        )
        .unwrap();
        let mut register = None;
        let outcome = doc
            .edit(
                &crate::Action::Delete {
                    track: 0,
                    target: crate::action::Target::Clip(1),
                    ripple: false,
                },
                0.0,
                &mut register,
            )
            .unwrap();
        assert!(
            outcome.desc.contains("dropped annotation #1"),
            "{}",
            outcome.desc
        );
        assert!(doc.project.annotations.is_empty());
    }

    #[test]
    fn annotation_undo_is_scoped_to_its_sequence() {
        use crate::annotation::{Anchor, AnnotationKind, SourceRef};

        let mut doc = document();
        let first = doc.project.active_id();
        let second = second_sequence(&mut doc);
        let note = |text: &str| AnnotationKind::Note {
            text: text.to_string(),
        };
        let anchor = || Anchor::Source {
            source: SourceRef::Asset { id: 1 },
            time: Seconds::from_integer(0),
        };

        doc.select_sequence(first).unwrap();
        let one = doc.add_annotation(note("one"), anchor(), 0).unwrap();
        doc.select_sequence(second).unwrap();
        let two = doc.add_annotation(note("two"), anchor(), 0).unwrap();
        assert_eq!(doc.project.annotations.len(), 2);

        // Sequence 1's undo removes only its own note, even though sequence 2
        // added one since.
        doc.select_sequence(first).unwrap();
        doc.undo().expect("sequence 1 still undoes");
        assert_eq!(doc.project.annotations.len(), 1);
        assert_eq!(doc.project.annotations[0].id, two);
        assert!(matches!(
            doc.remove_annotation(two, 0),
            Err(EditError::AnnotationSequence(id, sequence))
                if id == two && sequence == second
        ));

        // Sequence 2's stack is independent.
        doc.select_sequence(second).unwrap();
        doc.undo().expect("sequence 2 still undoes");
        assert!(doc.project.annotations.is_empty());

        doc.redo().unwrap();
        doc.select_sequence(first).unwrap();
        doc.redo().unwrap();
        let mut ids: Vec<u32> = doc.project.annotations.iter().map(|a| a.id).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![one, two],
            "both redo stacks restore their own note"
        );
    }

    #[test]
    fn add_assets_with_group_rolls_back_when_a_part_has_no_duration() {
        let mut doc = document();
        let before = doc.project.clone();
        let depth = doc.depth();
        // `probe` with no duration mirrors a container ffprobe can't time.
        let mut nodur = probe("4");
        nodur.format.duration = None;
        let files: Vec<std::path::PathBuf> =
            vec!["/media/a.mp4".into(), "/media/nodur.webm".into()];
        let err = doc
            .add_assets_with_group(
                vec![(files[0].clone(), probe("4")), (files[1].clone(), nodur)],
                0,
                "CAM 3",
                files,
            )
            .expect_err("no duration refuses the group");
        assert!(err.to_string().contains("no known duration"), "{err}");
        assert_eq!(doc.depth(), depth, "no history entry survives");
        assert_eq!(
            without_revision(&doc.project),
            without_revision(&before),
            "pool and timeline are restored"
        );
        // The batch is closed, so later writes still record.
        let ids = doc.add_assets(vec![("/media/b.mp4".into(), probe("3"))], 0);
        assert_eq!(ids.len(), 1);
        assert_eq!(doc.depth(), depth + 1);
        doc.undo().expect("undo the later batch");
        assert_eq!(doc.depth(), depth, "the next batch undoes as one entry");
    }

    #[test]
    fn add_assets_with_group_rolls_back_when_the_group_is_refused() {
        let mut doc = document();
        let before = doc.project.clone();
        let depth = doc.depth();
        let files: Vec<std::path::PathBuf> = vec!["/media/a.mp4".into(), "/media/zero.mp4".into()];
        let err = doc
            .add_assets_with_group(
                vec![
                    (files[0].clone(), probe("4")),
                    (files[1].clone(), probe("0")),
                ],
                0,
                "BAD",
                files,
            )
            .expect_err("a zero-length part is invalid");
        assert!(err.to_string().contains("media group"), "{err}");
        assert_eq!(doc.depth(), depth, "no history entry survives");
        assert_eq!(
            without_revision(&doc.project),
            without_revision(&before),
            "the asset batch is rolled back with the group"
        );
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
    fn save_clamps_a_playhead_past_the_end() {
        let mut doc = document();
        // A front-end that forgot to write the cursor back can leave it past
        // the end; the file must never hold one, or the next session opens
        // outside the timeline.
        doc.project.playhead = Seconds::from_integer(99);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.json");
        doc.save(&path).unwrap();
        let loaded = Project::load(&path).unwrap();
        assert_eq!(loaded.playhead, loaded.last_frame(), "clamped to the end");
        assert!(
            loaded.playhead < loaded.timeline.end(),
            "never `end` itself"
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
    fn a_batch_is_one_undo_step_and_rolls_back_on_failure() {
        let mut doc = document();
        let before = doc.project().clone();
        let before_revision = doc.revision();
        doc.edit_actions(
            &[
                Action::Split { track: 0, at: 1.0 },
                Action::Split { track: 0, at: 1.5 },
            ],
            0.0,
            &mut None,
        )
        .expect("batch");
        assert_eq!(doc.depth(), 1, "the batch is one undo entry");
        assert_eq!(doc.timeline.video_track().unwrap().segments.len(), 3);
        assert!(doc.revision() > before_revision);

        doc.undo().unwrap();
        assert_eq!(without_revision(doc.project()), without_revision(&before));

        doc.redo().unwrap();
        assert_eq!(doc.timeline.video_track().unwrap().segments.len(), 3);

        let revision = doc.revision();
        let depth = doc.depth();
        let failed = doc.edit_actions(
            &[
                Action::Split { track: 0, at: 1.0 },
                Action::Split { track: 0, at: 99.0 },
            ],
            0.0,
            &mut None,
        );
        assert!(failed.is_err(), "the second action must fail");
        assert_eq!(doc.revision(), revision, "a failed batch does not bump");
        assert_eq!(doc.depth(), depth, "a failed batch records nothing");
        assert_eq!(
            doc.timeline.video_track().unwrap().segments.len(),
            3,
            "a failed batch restores the first action"
        );

        assert!(matches!(
            doc.edit_actions(&[], 0.0, &mut None),
            Err(EditError::EmptyBatch)
        ));
        assert!(matches!(
            doc.edit_actions(
                &[Action::Undo, Action::Split { track: 0, at: 1.0 }],
                0.0,
                &mut None
            ),
            Err(EditError::BatchUndoRedo)
        ));
    }

    #[test]
    fn a_batch_is_one_step_even_at_the_history_cap() {
        for start in [crate::edit::MAX_HISTORY - 1, crate::edit::MAX_HISTORY] {
            let mut doc = document();
            let active = doc.project.active_id();
            for i in 0..start {
                doc.histories.get_mut(active).add_asset(
                    &mut doc.project,
                    format!("/media/{i}.mp4").into(),
                    probe("2"),
                    0,
                );
            }
            assert_eq!(doc.depth(), start);

            let before = doc.project().clone();
            doc.edit_actions(
                &[
                    Action::Split { track: 0, at: 1.0 },
                    Action::Split { track: 0, at: 1.5 },
                ],
                0.0,
                &mut None,
            )
            .expect("batch");
            assert_eq!(
                doc.depth(),
                crate::edit::MAX_HISTORY,
                "the cap still holds after grouping"
            );

            let undone = doc.undo().unwrap();
            assert!(
                undone.desc.starts_with("batch:"),
                "the batch survived the cap: {}",
                undone.desc
            );
            assert_eq!(
                doc.timeline.video_track().unwrap().segments.len(),
                1,
                "one undo reversed both actions at start {start}"
            );
            assert_eq!(without_revision(doc.project()), without_revision(&before));
        }
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
