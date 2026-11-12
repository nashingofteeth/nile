//! `nile serve --stdio`: the protocol handler over a loaded project.
//!
//! This is the one place server state lives: a [`Document`] (project plus
//! undo history), the shared yank register, and the queued events. Every
//! mutation goes through [`ServeHandler::apply`], so compare-and-swap,
//! playhead conversion, persistence, and event emission cannot drift between
//! `action.apply` and the dedicated `undo`/`redo` aliases.
//!
//! stdout is the protocol channel. Nothing here may `say!` or print; errors
//! leave through the JSON-RPC response, and `main` sends command failures to
//! stderr.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use nle_core::edit::Actor;
use nle_core::{
    Action, Document, EditError, History, Outcome as ActionOutcome, Project, Register, Seconds,
    frames_to_seconds, load_register, save_register, secs_f64,
};
use nle_protocol::{Call, Event, Handler, Hello, Outcome as WireOutcome, Reply, RpcError, Server};

/// Run the blocking JSON-RPC loop on stdin/stdout.
pub(crate) fn cmd_serve(project_path: &Path, _stdio: bool) -> Result<()> {
    let project = Project::load(project_path)?;
    let history = History::load_project(project_path).unwrap_or_default();
    let mut document = Document::with_history(project, history);
    document.set_actor(Actor::Agent("nile-serve".into()));

    let handler = ServeHandler {
        document,
        register: load_register(project_path),
        path: project_path.to_path_buf(),
        events: Vec::new(),
        poisoned: None,
    };

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut server = Server::new(stdin.lock(), stdout.lock(), handler);
    server.serve().context("serve the project protocol")?;
    Ok(())
}

/// The live server state. One instance serves one project on one connection.
struct ServeHandler {
    document: Document,
    register: Option<Register>,
    path: PathBuf,
    events: Vec<Event>,
    poisoned: Option<String>,
}

impl Handler for ServeHandler {
    fn handle(&mut self, call: &Call) -> Result<Reply, RpcError> {
        match call {
            Call::Hello => Ok(Reply::Hello(Hello::new(self.document.revision()))),
            Call::ProjectShow => Ok(Reply::Project(self.document.project().clone())),
            Call::TimelineEdl { track } => self.edl(*track),
            Call::ActionPreview {
                base_revision,
                action,
                from,
            } => self.preview(*base_revision, action, *from),
            Call::ActionApply {
                base_revision,
                action,
                from,
            } => self.apply(*base_revision, action, *from),
            Call::Undo { base_revision } => self.apply(*base_revision, &Action::Undo, None),
            Call::Redo { base_revision } => self.apply(*base_revision, &Action::Redo, None),
        }
    }

    fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
}

impl ServeHandler {
    fn check_revision(&self, base_revision: u64) -> Result<(), RpcError> {
        if base_revision == self.document.revision() {
            Ok(())
        } else {
            Err(RpcError::revision_conflict(
                base_revision,
                self.document.revision(),
            ))
        }
    }

    /// The undo anchor: the request's `from`, or the shared playhead.
    fn anchor_seconds(&self, from: Option<Seconds>) -> f64 {
        from.map(|t| secs_f64(&t))
            .unwrap_or_else(|| secs_f64(&self.document.project().playhead))
    }

    /// A history frame as exact timeline seconds. Without an fps the model's
    /// frame is a second (the same fallback `frame_to_secs` uses).
    fn frame_seconds(&self, frame: i64) -> Seconds {
        match self.document.project().fps.as_ref() {
            Some(fps) => frames_to_seconds(frame, *fps),
            None => Seconds::from_integer(frame),
        }
    }

    fn wire_outcome(&self, outcome: ActionOutcome, revision: u64) -> WireOutcome {
        WireOutcome {
            revision,
            desc: outcome.desc,
            changed: outcome.changed,
            playhead: outcome.playhead.map(|frame| self.frame_seconds(frame)),
        }
    }

    fn edl(&self, track: Option<usize>) -> Result<Reply, RpcError> {
        let text = match track {
            Some(ti) => {
                if self.document.project().timeline.track(ti).is_none() {
                    return Err(RpcError::invalid_params(format!(
                        "no track {ti} (the protocol uses 0-based tracks)"
                    )));
                }
                crate::preview::build_edl(self.document.project(), &self.path, ti)
            }
            None => crate::preview::build_composite_edl(self.document.project(), &self.path),
        }
        .map_err(|e| RpcError::internal(format!("EDL: {e}")))?;
        Ok(Reply::Edl(text))
    }

    /// Run one action against a copy. The clone includes the history, so an
    /// undo/redo preview is real; the register clone keeps a preview yank
    /// away from the live clipboard. Nothing is saved and no events are
    /// queued.
    fn preview(
        &self,
        base_revision: u64,
        action: &Action,
        from: Option<Seconds>,
    ) -> Result<Reply, RpcError> {
        self.check_revision(base_revision)?;
        let mut document = Document::with_history(
            self.document.project().clone(),
            self.document.history().clone(),
        );
        let mut register = self.register.clone();
        let outcome = document
            .edit(action, self.anchor_seconds(from), &mut register)
            .map_err(edit_rpc)?;
        Ok(Reply::Outcome(
            self.wire_outcome(outcome, self.document.revision()),
        ))
    }

    /// The only mutation path: CAS, edit, playhead, save, events.
    fn apply(
        &mut self,
        base_revision: u64,
        action: &Action,
        from: Option<Seconds>,
    ) -> Result<Reply, RpcError> {
        if let Some(reason) = &self.poisoned {
            return Err(RpcError::internal(format!(
                "server is read-only after a failed save: {reason}"
            )));
        }
        self.check_revision(base_revision)?;

        let outcome = self
            .document
            .edit(action, self.anchor_seconds(from), &mut self.register)
            .map_err(edit_rpc)?;

        let playhead = match outcome.playhead {
            Some(frame) => {
                let at = self.frame_seconds(frame);
                self.document.set_playhead(secs_f64(&at));
                Some(self.document.project().playhead)
            }
            None => None,
        };

        if let Err(e) = self.persist(outcome.changed, playhead.is_some()) {
            self.poisoned = Some(e.to_string());
            return Err(RpcError::internal(format!("save failed: {e}")));
        }

        let revision = self.document.revision();
        if outcome.changed {
            self.events.push(Event::ProjectChanged { revision });
        }
        if let Some(at) = playhead {
            self.events.push(Event::Playhead { at });
        }
        Ok(Reply::Outcome(WireOutcome {
            revision,
            desc: outcome.desc,
            changed: outcome.changed,
            playhead,
        }))
    }

    /// Persist in the same order as `run_cli_action`: project, history,
    /// register. A playhead-only change saves the project but not the stack.
    fn persist(&mut self, changed: bool, playhead_moved: bool) -> Result<()> {
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

fn edit_rpc(error: EditError) -> RpcError {
    match &error {
        EditError::NothingToUndo | EditError::NothingToRedo => {
            RpcError::invalid_request(error.to_string())
        }
        EditError::History(_) => RpcError::internal(error.to_string()),
        _ => RpcError::invalid_params(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_core::Project;
    use nle_core::timeline::Timeline;

    fn handler() -> ServeHandler {
        let mut project = Project::new();
        project.fps = Some(Seconds::from_integer(30));
        project.timeline = Timeline::new();
        ServeHandler {
            document: Document::new(project),
            register: None,
            path: PathBuf::from("/tmp/does-not-exist.json"),
            events: Vec::new(),
            poisoned: None,
        }
    }

    #[test]
    fn a_stale_base_revision_is_a_conflict() {
        let h = handler();
        let error = h.check_revision(9).expect_err("stale");
        assert_eq!(error.code, nle_protocol::REVISION_CONFLICT);
    }

    #[test]
    fn frames_convert_with_the_project_rate() {
        let h = handler();
        assert_eq!(h.frame_seconds(30), Seconds::from_integer(1));
        assert_eq!(h.frame_seconds(15), Seconds::new(1, 2));
    }

    #[test]
    fn a_save_failure_poisons_mutations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut handler = ServeHandler {
            document: Document::new(Project::new()),
            register: None,
            // The parent does not exist, so the first save fails.
            path: dir.path().join("missing").join("p.json"),
            events: Vec::new(),
            poisoned: None,
        };

        let error = handler
            .apply(
                0,
                &Action::TrackAdd {
                    kind: nle_core::TrackKind::Audio,
                },
                None,
            )
            .expect_err("the save must fail");
        assert_eq!(error.code, nle_protocol::INTERNAL_ERROR);
        assert!(handler.poisoned.is_some(), "mutations are poisoned");

        let error = handler
            .apply(
                1,
                &Action::TrackAdd {
                    kind: nle_core::TrackKind::Audio,
                },
                None,
            )
            .expect_err("poisoned");
        assert_eq!(error.code, nle_protocol::INTERNAL_ERROR);
        assert!(error.message.contains("read-only"), "{}", error.message);

        assert!(matches!(
            handler.handle(&Call::ProjectShow),
            Ok(Reply::Project(_))
        ));
    }

    #[test]
    fn nothing_to_undo_is_an_invalid_request() {
        assert_eq!(
            edit_rpc(EditError::NothingToUndo).code,
            nle_protocol::INVALID_REQUEST
        );
        assert_eq!(
            edit_rpc(EditError::NoClip).code,
            nle_protocol::INVALID_PARAMS
        );
    }
}
