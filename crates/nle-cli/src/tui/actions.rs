//! Running an `Action` from the TUI and keeping the playhead honest across
//! it: the media anchor, the re-anchor after an edit, the undo/persist step,
//! and the delete/yank keys' shared plumbing.

use std::collections::HashSet;

use nle_core::action::Action;
use nle_core::timeline::Segment as TimelineSegment;
#[cfg(test)]
use nle_core::{Batch, EditError, edit};
use nle_core::{Clip, Project, Seconds, Target, clip_span};

use crate::mpv::Mpv;

use super::croc::CrocAct;
use super::jobs::start_preview;
use super::{
    App, Mode, forget_missing_assets, is_pool, load_pool_preview_at, persist_playhead, pool_pos,
    pool_sel, reload_project, secs, set_pool_sel,
};
/// The clip the playhead sits inside, if any.
pub(crate) fn clip_under(project: &Project, t: f64, ti: usize) -> Option<&Clip> {
    let track = project.timeline.track(ti)?;
    track.clip_at(grid_time(project, t))
}

/// The clip under `t` and the position of its head (start) or tail edge.
/// Shared by the `<` / `>` handling in both modes.
pub(crate) fn clip_edge_under(
    project: &Project,
    t: f64,
    head: bool,
    ti: usize,
) -> Option<(u32, f64)> {
    let c = clip_under(project, t, ti)?;
    let (s, e) = clip_span(project, c.id, ti)?;
    Some((c.id, if head { s } else { e }))
}

/// `<` / `>` (and `,` / `.`) in Normal mode: move the playhead to the
/// head or tail of the clip it is over.
pub(crate) fn seek_clip_end(app: &mut App, player: &dyn Mpv, head: bool) {
    let Some((id, edge)) = clip_edge_under(&app.project, app.playhead, head, app.active()) else {
        app.set_message("playhead is in a gap — clip start/end needs a clip");
        return;
    };
    app.seek_to(player, edge);
    app.set_message(&format!(
        "clip #{id} {}",
        if head { "start" } else { "end" }
    ));
}

pub(crate) use crate::session::playhead::edit_anchor;
#[cfg(test)]
pub(crate) use crate::session::playhead::{Anchor, GapAnchor, MediaAnchor};

#[cfg(test)]
pub(crate) fn clip_anchor(project: &Project, playhead: f64) -> Option<MediaAnchor> {
    match edit_anchor(project, playhead, 0) {
        Some(Anchor::Media(a)) => Some(a),
        _ => None,
    }
}

/// Apply the shared media-frame policy to this TUI session's active track.
/// The tests drive the raw policy over an app; production goes through
/// `playhead::land`.
#[cfg(test)]
pub(crate) fn reanchor_playhead(app: &mut App, anchor: Option<Anchor>) {
    app.playhead =
        crate::session::playhead::reanchor(&app.project, app.playhead, app.active(), anchor);
}

/// The playhead's position on the project's frame grid, as Seconds.
pub(crate) fn grid_time(project: &Project, t: f64) -> Seconds {
    nle_core::grid_time(project.fps.as_ref(), t)
}

/// The trim keys' routing, as the key handlers call it (tests drive the
/// shared [`edit::trim_at_on`] through this shim).
#[cfg(test)]
pub(crate) fn trim_key(
    project: &Project,
    ph: f64,
    out: bool,
    ripple: bool,
    ti: usize,
) -> Result<Batch, EditError> {
    edit::trim_at_on(project, ti, ph, out, ripple)
}

/// Run one catalog action — the single path every edit takes, keybind or
/// `:cmd`: apply through the undo history, reanchor the playhead, persist,
/// refresh the monitor. Returns whether it applied **and saved** (the action
/// set its own notice either way).
/// What a delete can take away: clip/transition ids and the segment count.
/// `dd` on a clip leaves a gap (the count stays), so ids are needed; deleting
/// a gap keeps the ids, so the count is needed too.
pub(crate) struct TimelineStamp {
    pub(crate) ids: HashSet<u32>,
    pub(crate) segments: usize,
}

pub(crate) fn timeline_stamp(project: &Project) -> TimelineStamp {
    let mut stamp = TimelineStamp {
        ids: HashSet::new(),
        segments: 0,
    };
    for track in &project.timeline.tracks {
        for seg in &track.segments {
            stamp.segments += 1;
            match seg {
                TimelineSegment::Clip(c) => {
                    stamp.ids.insert(c.id);
                }
                TimelineSegment::Transition(t) => {
                    stamp.ids.insert(t.id);
                }
                TimelineSegment::Gap(_) => {}
            }
        }
    }
    stamp
}

/// Did an edit remove material? A clip/transition id vanished, or the segment
/// count fell. Trims that only shorten, splits, and inserts don't.
pub(crate) fn removed_material(before: &TimelineStamp, after: &TimelineStamp) -> bool {
    after.segments < before.segments || before.ids.iter().any(|id| !after.ids.contains(id))
}

/// Which crocodile reaction an accepted edit earns. `removed` is the
/// before/after stamp; `old_speed` is only needed to tell a speed-up
/// (`DashFast`) from a slow-down (`DashSlow`).
pub(crate) fn reaction_for(
    action: &Action,
    removed: bool,
    old_speed: Option<Seconds>,
) -> Option<CrocAct> {
    // Undo always gives the previous state back — a spit, even when what it
    // removes is a freshly-pasted clip.
    if matches!(action, Action::Undo) {
        return Some(CrocAct::Spit);
    }
    // Redo is the spit played backwards, regardless of what the change was.
    if matches!(action, Action::Redo) {
        return Some(CrocAct::Unspit);
    }
    if removed {
        return Some(CrocAct::Chomp);
    }
    match action {
        Action::TrimIn { .. } | Action::TrimOut { .. } => Some(CrocAct::Nibble),
        Action::Split { .. } => Some(CrocAct::Snap),
        Action::InsertGap { .. }
        | Action::Paste { .. }
        | Action::PasteRange { .. }
        | Action::Extend { .. }
        | Action::TrackAdd { .. } => Some(CrocAct::Gulp),
        Action::SetSpeed { rate, .. } => {
            let old = old_speed?;
            let (new, old) = (secs(rate), secs(&old));
            if new > old {
                Some(CrocAct::DashFast)
            } else if new < old {
                Some(CrocAct::DashSlow)
            } else {
                None
            }
        }
        Action::Transition { .. } => Some(CrocAct::Slither),
        Action::AddEffect { .. }
        | Action::CropKey { .. }
        | Action::RemoveEffect { .. }
        | Action::ClearEffects { .. } => Some(CrocAct::Wink),
        Action::TrackMute { .. } => Some(CrocAct::Sleep),
        Action::AssetRm { .. } => Some(CrocAct::Chomp),
        Action::PurgeStumps { .. } => Some(CrocAct::Nibble),
        Action::Delete { .. }
        | Action::DeleteRange { .. }
        | Action::Yank { .. }
        | Action::YankRange { .. }
        | Action::TrackRm { .. }
        | Action::AnnotationAdd { .. }
        | Action::AnnotationAddAt { .. }
        | Action::AnnotationRemove { .. }
        | Action::AnnotationResolve { .. } => None,
        Action::Undo | Action::Redo => unreachable!("handled above"),
    }
}

fn speed_of(project: &Project, id: u32) -> Option<Seconds> {
    project
        .timeline
        .tracks
        .iter()
        .flat_map(|track| &track.segments)
        .find_map(|seg| match seg {
            TimelineSegment::Clip(c) if c.id == id => Some(c.speed),
            _ => None,
        })
}

pub(crate) fn run_action(app: &mut App, player: &dyn Mpv, action: &Action) -> bool {
    // What the timeline holds before the edit — a removal opens the crocodile's
    // mouth in the status row (the chomp is transient, not a message).
    let stamp = timeline_stamp(&app.project);
    let speed_before = match action {
        Action::SetSpeed { clip, .. } => speed_of(&app.project, *clip),
        _ => None,
    };
    // Keep the playhead on the frame it was showing, not the same
    // timecode (a trim can move material under it). Undo/redo carry their
    // own restore position; an extend commit is made from the armed edge,
    // and lands on the edge it moved to.
    let restore = matches!(action, Action::Undo | Action::Redo);
    let in_extend = matches!(app.mode, Mode::Extend(_));
    let anchor = (!in_extend && !restore)
        .then(|| edit_anchor(&app.project, app.playhead, app.active()))
        .flatten();
    let from = match &app.mode {
        Mode::Extend(st) => st.edge_pos,
        _ => app.playhead,
    };
    let playhead_before = app.playhead;
    // The sequence this action applies to, before it can change the active
    // one (undoing a `:sequence use` does).
    let active_before = app.project.active_id();
    match app.project.apply_action(action, from) {
        Ok(outcome) => {
            // The one landing policy: the action's own frame, else the media
            // anchor, else the cursor clamped into the new length. A no-op
            // (a yank) must not move the playhead — `dd` yanks first, and an
            // anchor on a blend maps to its end.
            if let Some(at) = crate::session::playhead::land(
                &app.project,
                playhead_before,
                app.active(),
                anchor,
                action,
                &outcome,
            ) {
                app.playhead = at;
            }
            if (app.playhead - playhead_before).abs() > 1e-9 {
                app.follow_timeline();
            }
            if outcome.changed {
                // The report described this sequence before the edit, so its
                // verdicts can no longer be trusted for it. Other sequences
                // did not change and keep theirs.
                app.checks.remove(&active_before);
                let removed = removed_material(&stamp, &timeline_stamp(&app.project));
                if let Some(act) = reaction_for(action, removed, speed_before) {
                    app.croc.play(act);
                }
            }
            app.set_message(&outcome.desc);
            // The register is shared with the CLI (`nile yank`/`nile paste`).
            // Check only *that* file before saving it: the save re-stamps the
            // file, which would hide the shell's yank from the guard. The
            // project and its history still save — an unrelated edit must not
            // be rolled back by a foreign register.
            match persist_register(app) {
                Ok(true) => {}
                Ok(false) if !outcome.changed => {
                    // A yank's only output is the register, so a refused
                    // write is the action's result. Normal adopts the shell's
                    // file now; a held mode leaves it to `watch_foreign`,
                    // because reloading would invalidate the mode's indices.
                    if matches!(app.mode, Mode::Normal) {
                        let file = app.foreign_file().unwrap_or("a session file").to_string();
                        return adopt_foreign(app, player, &file);
                    }
                    return false;
                }
                Ok(false) => {}
                Err(e) => app.fail(&format!("register save failed: {e}")),
            }
            if outcome.changed {
                persist_and_refresh(app, player)
            } else {
                true
            }
        }
        Err(e) => {
            app.fail(&crate::edit_message(&e));
            false
        }
    }
}

/// Save the shared register unless a shell wrote *it* first. `Ok(false)` =
/// refused, so the caller adopts the file instead of clobbering it — the same
/// guard as `persist_playhead`, and it must run before the write: saving
/// re-stamps the file, so a check afterwards can never see the change the
/// write just covered.
fn persist_register(app: &mut App) -> std::io::Result<bool> {
    if app.register_changed() {
        return Ok(false);
    }
    app.project.save_register()?;
    app.note_register();
    Ok(true)
}

/// Adopt the shared file we refused to write over and report the action as
/// not applied: the reload replaces the state the action changed, so `false`
/// is the honest answer. Same notice shape as `persist_and_refresh`'s
/// refusal, naming the file that actually moved.
fn adopt_foreign(app: &mut App, player: &dyn Mpv, file: &str) -> bool {
    if let Err(e) = reload_project(app, player, true) {
        app.fail(&format!("reload failed: {e}"));
        return false;
    }
    let prev = app.message.clone();
    app.fail(&format!(
        "{prev} — not saved: {file} changed on disk (reloaded)"
    ));
    false
}

/// Atomic save + view refresh. Called after every successful edit (and
/// undo/redo) — no manual save state to get wrong. The undo history sidecar
/// is written alongside the project, then the monitor is reloaded for
/// whichever view is showing: the pool's raw file when the pool is open, or
/// the timeline EDL (with the playhead baked into the load) otherwise.
pub(crate) fn persist_and_refresh(app: &mut App, player: &dyn Mpv) -> bool {
    // The stored playhead follows the session — it is what `nile seek`
    // reads and where the next TUI session opens.
    match persist_playhead(app) {
        Ok(true) => {}
        Ok(false) => {
            // A shell wrote the project first: adopt theirs rather than
            // clobber it, and say so — this edit was not saved.
            let file = app.foreign_file().unwrap_or("a session file").to_string();
            return adopt_foreign(app, player, &file);
        }
        Err(e) => {
            app.fail(&format!("save failed: {e}"));
            return false;
        }
    }
    let history_saved = if let Err(e) = app.project.save_history() {
        app.fail(&format!("history save failed: {e}"));
        false
    } else {
        app.note_history();
        true
    };
    // An asset removed (or an undone import) frees its id; drop any cache
    // entries so a later import that reuses the id starts clean.
    forget_missing_assets(app);
    if is_pool(app) {
        // A remove/undo changed the list; keep the selection in range and
        // reload the (possibly different) asset. Doing this here is why an
        // undo while the pool is open no longer kicks the monitor to the
        // timeline.
        let sel = pool_sel(app)
            .unwrap_or(0)
            .min(app.project.media.len().saturating_sub(1));
        set_pool_sel(app, sel);
        crate::log::line(&format!("load pool preview #{sel}"));
        // An undo/edit shouldn't rewind the preview; clamp the old clock to
        // the (possibly different) selected asset.
        let pos = pool_pos(app);
        load_pool_preview_at(app, player, pos);
    } else {
        // Proxy renders are seconds of ffmpeg: build the EDL off-thread and
        // let the status row show progress. A newer edit supersedes this one.
        start_preview(app);
    }
    history_saved
}

/// Switch the active sequence and its undo stack. Session state, not an
/// action: the project's `active` field is saved with the playhead, the
/// target sequence's stored playhead becomes the session cursor, and the
/// monitor reloads the new timeline. Tracks are per sequence, so the active
/// track resets to the first rather than pointing into another timeline.
pub(crate) fn select_sequence(app: &mut App, player: &dyn Mpv, id: u32) -> Option<String> {
    // Flush the cursor into the sequence we are leaving; otherwise a move
    // since the last idle save is lost when it is reloaded later.
    app.project.set_playhead(app.playhead);
    if let Err(e) = app.project.select_sequence(id) {
        let msg = e.to_string();
        app.fail(&msg);
        return None;
    }
    app.active_track = 0;
    app.playhead = secs(&app.project.playhead);
    app.follow_timeline();
    app.save_session_state();
    let sequence = app.project.active_sequence();
    let fps = sequence
        .fps
        .map(|fps| format!("{}/{} fps", fps.numer(), fps.denom()))
        .unwrap_or_else(|| "no fps".into());
    let video = sequence
        .video
        .as_ref()
        .map(|v| format!("{}×{}", v.width, v.height))
        .unwrap_or_else(|| "no size".into());
    let note = format!("sequence #{id} {} · {fps} · {video}", sequence.name);
    if !persist_and_refresh(app, player) {
        return None;
    }
    app.set_message(&note);
    Some(note)
}

pub(crate) fn edit_delete(app: &mut App, player: &dyn Mpv) {
    // dd yanks first (vim: dd fills the register). If that yank was refused
    // (a shell's register is on disk), don't delete: the composite would save
    // the project with the shell's register and replace the adoption notice.
    if !edit_yank(app, player) {
        return;
    }
    run_action(
        app,
        player,
        &Action::Delete {
            track: app.active(),
            target: Target::At(app.playhead),
            ripple: false,
        },
    );
}

pub(crate) fn edit_yank(app: &mut App, player: &dyn Mpv) -> bool {
    run_action(
        app,
        player,
        &Action::Yank {
            track: app.active(),
            target: Target::At(app.playhead),
        },
    )
}
