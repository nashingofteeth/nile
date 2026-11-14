//! The application session: a document, its yank register, and its files.
//!
//! `Document` pairs the project with its undo history. `Session` adds the
//! on-disk path and the shared register, then owns the one load/edit/preview/
//! save order the CLI and `nile serve` use. Keeping that transaction in one
//! place is what stops the front-ends from growing subtly different save
//! orders, register handling, or preview behavior.
//!
//! The TUI still holds its own project/register/path fields; migrating it to
//! this type is the next structural step, along with moving the media-frame
//! playhead anchor policy here so every front-end agrees.

use std::path::{Path, PathBuf};

use nle_core::edit::{Actor, EditError, History};
use nle_core::{
    Action, Document, Outcome as ActionOutcome, Project, ProjectError, Register, load_register,
    save_register,
};

/// A session file failure. The project loader's own errors pass through
/// unchanged; sidecar/register problems are I/O.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Project(#[from] ProjectError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A loaded project plus the state that travels with it across commands.
pub struct Session {
    document: Document,
    register: Option<Register>,
    path: PathBuf,
}

impl Session {
    /// Load the project, its history sidecar (fresh if missing/corrupt, like
    /// the CLI), and the shared register.
    pub fn load(path: &Path) -> Result<Self, SessionError> {
        let project = Project::load(path)?;
        let history = History::load_project(path).unwrap_or_default();
        let register = load_register(path);
        Ok(Self {
            document: Document::with_history(project, history),
            register,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn project(&self) -> &Project {
        self.document.project()
    }

    pub fn revision(&self) -> u64 {
        self.document.revision()
    }

    pub fn set_actor(&mut self, actor: Actor) {
        self.document.set_actor(actor);
    }

    pub fn set_playhead(&mut self, at: f64) -> f64 {
        self.document.set_playhead(at)
    }

    /// Run one catalog action, recording it in the session's history and
    /// register. The caller decides whether to save.
    pub fn edit(&mut self, action: &Action, from: f64) -> Result<ActionOutcome, EditError> {
        self.document.edit(action, from, &mut self.register)
    }

    /// Run one action against a copy. The clone includes the history, so an
    /// undo/redo preview is real, and the cloned register keeps a preview yank
    /// away from the live clipboard.
    pub fn preview(&self, action: &Action, from: f64) -> Result<ActionOutcome, EditError> {
        let mut document = Document::with_history(
            self.document.project().clone(),
            self.document.history().clone(),
        );
        let mut register = self.register.clone();
        document.edit(action, from, &mut register)
    }

    /// Save project, history, and register. This is the CLI's one-shot path.
    pub fn save(&mut self) -> Result<(), SessionError> {
        self.save_if(true, true)
    }

    /// Save the parts a caller actually changed. A yank only needs the
    /// register; a playhead-only move needs the project but not the stack.
    pub fn save_if(&mut self, changed: bool, playhead_moved: bool) -> Result<(), SessionError> {
        if changed || playhead_moved {
            self.document.save(&self.path)?;
        }
        if changed {
            self.document
                .save_history(&History::sidecar_path(&self.path))?;
        }
        save_register(&self.path, self.register.as_ref())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_core::timeline::{Gap, Segment};
    use nle_core::{Seconds, TrackKind};
    use tempfile::tempdir;

    fn seeded() -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("p.json");
        let mut project = Project::new();
        project.fps = Some(Seconds::from_integer(30));
        project.timeline.video_track_mut().unwrap().segments = vec![Segment::Gap(Gap {
            duration: Seconds::from_integer(2),
        })];
        project.save(&path).expect("seed project");
        (dir, path)
    }

    #[test]
    fn load_save_round_trip() {
        let (_dir, path) = seeded();
        let mut session = Session::load(&path).expect("load");
        assert_eq!(
            session.project().fps,
            Some(Seconds::from_integer(30)),
            "the project loads"
        );
        session.set_playhead(1.0);
        session.save().expect("save");
        let again = Session::load(&path).expect("reload");
        assert_eq!(again.project().playhead, Seconds::from_integer(1));
    }

    #[test]
    fn preview_does_not_touch_the_live_document_or_file() {
        let (_dir, path) = seeded();
        let session = Session::load(&path).expect("load");
        let before = session.project().clone();
        let outcome = session
            .preview(
                &Action::TrackAdd {
                    kind: TrackKind::Audio,
                },
                0.0,
            )
            .expect("preview");
        assert!(outcome.changed, "the preview reports the change");
        assert_eq!(session.project(), &before, "the live project is untouched");
        let reloaded = Session::load(&path).expect("reload");
        assert_eq!(
            reloaded.project(),
            &before,
            "the preview did not save the project"
        );
    }
}
