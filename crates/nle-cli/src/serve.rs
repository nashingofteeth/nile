//! `nile serve --stdio`: the protocol handler over a loaded project.
//!
//! The server state is a [`Session`] — project, history, register, and path —
//! plus the queued events and the save-failure poison. Every mutation goes
//! through [`ServeHandler::apply`], so compare-and-swap, the edit, playhead
//! conversion, persistence, and event emission cannot drift between
//! `action.apply` and the dedicated `undo`/`redo` aliases.
//!
//! stdout is the protocol channel. Nothing here may `say!` or print; errors
//! leave through the JSON-RPC response, and `main` sends command failures to
//! stderr.

use std::io;
use std::path::Path;

use anyhow::{Context, Result};

use nle_core::edit::Actor;
use nle_core::{Action, EditError, Outcome as ActionOutcome, Seconds, frames_to_seconds, secs_f64};
use nle_protocol::{Call, Event, Handler, Hello, Outcome as WireOutcome, Reply, RpcError, Server};

use crate::session::Session;

/// Run the blocking JSON-RPC loop on stdin/stdout.
pub(crate) fn cmd_serve(project_path: &Path, _stdio: bool) -> Result<()> {
    let mut session = Session::load(project_path)?;
    session.set_actor(Actor::Agent("nile-serve".into()));

    let handler = ServeHandler {
        session,
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
    session: Session,
    events: Vec<Event>,
    poisoned: Option<String>,
}

impl Handler for ServeHandler {
    fn handle(&mut self, call: &Call) -> Result<Reply, RpcError> {
        match call {
            Call::Hello => Ok(Reply::Hello(Hello::new(self.session.revision()))),
            Call::ProjectShow => Ok(Reply::Project(self.session.project().clone())),
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
        if base_revision == self.session.revision() {
            Ok(())
        } else {
            Err(RpcError::revision_conflict(
                base_revision,
                self.session.revision(),
            ))
        }
    }

    /// The undo anchor: the request's `from`, or the shared playhead.
    fn anchor_seconds(&self, from: Option<Seconds>) -> f64 {
        from.map(|t| secs_f64(&t))
            .unwrap_or_else(|| secs_f64(&self.session.project().playhead))
    }

    /// A history frame as exact timeline seconds. Without an fps the model's
    /// frame is a second (the same fallback `frame_to_secs` uses).
    fn frame_seconds(&self, frame: i64) -> Seconds {
        match self.session.project().fps.as_ref() {
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
                if self.session.project().timeline.track(ti).is_none() {
                    return Err(RpcError::invalid_params(format!(
                        "no track {ti} (the protocol uses 0-based tracks)"
                    )));
                }
                crate::preview::build_edl(self.session.project(), self.session.path(), ti)
            }
            None => {
                crate::preview::build_composite_edl(self.session.project(), self.session.path())
            }
        }
        .map_err(|e| RpcError::internal(format!("EDL: {e}")))?;
        Ok(Reply::Edl(text))
    }

    /// Run one action against a copy. `Session::preview` clones the history
    /// so an undo/redo preview is real, and the register so a preview yank
    /// cannot touch the live clipboard. Nothing is saved and no events are
    /// queued.
    fn preview(
        &self,
        base_revision: u64,
        action: &Action,
        from: Option<Seconds>,
    ) -> Result<Reply, RpcError> {
        self.check_revision(base_revision)?;
        let outcome = self
            .session
            .preview(action, self.anchor_seconds(from))
            .map_err(edit_rpc)?;
        Ok(Reply::Outcome(
            self.wire_outcome(outcome, self.session.revision()),
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
            .session
            .apply_action(action, self.anchor_seconds(from))
            .map_err(edit_rpc)?;

        let playhead = match outcome.playhead {
            Some(frame) => {
                let at = self.frame_seconds(frame);
                self.session.set_playhead(secs_f64(&at));
                Some(self.session.project().playhead)
            }
            None => None,
        };

        if let Err(e) = self.session.save_if(outcome.changed, playhead.is_some()) {
            self.poisoned = Some(e.to_string());
            return Err(RpcError::internal(format!("save failed: {e}")));
        }

        let revision = self.session.revision();
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

    fn handler() -> ServeHandler {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.json");
        let mut project = Project::new();
        project.fps = Some(Seconds::from_integer(30));
        project.save(&path).expect("save");
        let session = Session::load(&path).expect("load");
        ServeHandler {
            session,
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
        let path = dir.path().join("p.json");
        Project::new().save(&path).expect("save");
        let session = Session::load(&path).expect("load");
        drop(dir); // the project directory is gone, so the next save fails
        let mut handler = ServeHandler {
            session,
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
