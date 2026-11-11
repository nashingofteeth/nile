//! Visual segment selection: vim's `v` over timeline segments.
//!
//! Selection is a contiguous range of whole segments on one track. `d`
//! deletes and `y` copies in one edit; `p` pastes the copied range at the
//! playhead, splitting the clip or gap under it when necessary.

use crossterm::event::{KeyCode, KeyEvent};

use nle_core::{Action, grid_time};

use crate::mpv::Mpv;

use super::super::actions::run_action;
use super::super::{App, Mode, VisualState};
use super::normal_key;

/// The inclusive segment range selected on `track`, if visual mode is active.
pub(crate) fn selection(app: &App) -> Option<(usize, usize, usize)> {
    let Mode::Visual(st) = &app.mode else {
        return None;
    };
    Some((st.track, st.anchor.min(st.cursor), st.anchor.max(st.cursor)))
}

pub(crate) fn enter_visual(app: &mut App) {
    let ti = app.active();
    let Some(track) = app.project.timeline.track(ti) else {
        app.set_message("no track to select");
        return;
    };
    if track.segments.is_empty() {
        app.set_message("nothing to select");
        return;
    }
    let t = grid_time(app.project.fps.as_ref(), app.playhead);
    let idx = track
        .index_at(t)
        .or_else(|| (t >= track.end()).then_some(track.segments.len() - 1));
    let Some(idx) = idx else {
        app.set_message("move the playhead onto a segment");
        return;
    };
    app.mode = Mode::Visual(VisualState {
        track: ti,
        anchor: idx,
        cursor: idx,
    });
    app.set_message("visual — d delete · y yank · p paste at the playhead · v/esc cancel");
}

pub(crate) fn visual_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    let Some(state) = (match &app.mode {
        Mode::Visual(st) => Some(*st),
        _ => None,
    }) else {
        return;
    };
    if key
        .modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL)
    {
        // Panning never multiplies; drop a count typed for a motion so it
        // cannot ride along to the next one (normal mode consumes it on
        // every key).
        app.count = None;
        match key.code {
            KeyCode::Char('b') => app.pan_timeline(-0.5),
            KeyCode::Char('f') => app.pan_timeline(0.5),
            _ => {}
        }
        return;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('v') => {
            app.mode = Mode::Normal;
            app.set_message("timeline");
        }
        KeyCode::Char('d') => delete_selected(app, player, state),
        KeyCode::Char('y') => yank_selected(app, player, state),
        KeyCode::Char('p') => paste_selected(app, player, state),
        KeyCode::Char('+') | KeyCode::Char('=') => {
            app.count = None;
            app.zoom_timeline(1.5);
        }
        KeyCode::Char('-') => {
            app.count = None;
            app.zoom_timeline(1.0 / 1.5);
        }
        KeyCode::Char('z') => {
            app.count = None;
            app.toggle_timeline_zoom();
        }
        // `V` selects the whole current segment (which is already the seed);
        // useful after extending the range.
        KeyCode::Char('V') => {
            app.mode = Mode::Visual(VisualState {
                anchor: state.cursor,
                ..state
            });
            app.set_message("visual — one segment");
        }
        KeyCode::Char('h')
        | KeyCode::Char('l')
        | KeyCode::Char('H')
        | KeyCode::Char('L')
        | KeyCode::Char('f')
        | KeyCode::Char('F')
        | KeyCode::Char('j')
        | KeyCode::Char('k')
        | KeyCode::Char('w')
        | KeyCode::Char('b')
        | KeyCode::Char('n')
        | KeyCode::Char('N')
        | KeyCode::Char('g')
        | KeyCode::Char('G')
        | KeyCode::Char('0'..='9')
        | KeyCode::Char('<')
        | KeyCode::Char('>')
        | KeyCode::Char(',')
        | KeyCode::Char('.')
        | KeyCode::Char('$') => {
            // Reuse the normal-mode motion/count implementation without
            // letting an edit key through. Restore Visual around it.
            app.mode = Mode::Normal;
            normal_key(app, player, key);
            let t = grid_time(app.project.fps.as_ref(), app.playhead);
            let Some(track) = app.project.timeline.track(state.track) else {
                app.mode = Mode::Normal;
                return;
            };
            let cursor = track
                .index_at(t)
                .or_else(|| (t >= track.end()).then_some(track.segments.len().saturating_sub(1)))
                .unwrap_or(0);
            app.mode = Mode::Visual(VisualState { cursor, ..state });
        }
        _ => {}
    }
}

fn delete_selected(app: &mut App, player: &dyn Mpv, state: VisualState) {
    let Some((track, start, end)) = selection(app) else {
        return;
    };
    if run_action(app, player, &Action::DeleteRange { track, start, end }) {
        app.mode = Mode::Normal;
    }
    let _ = state;
}

fn yank_selected(app: &mut App, player: &dyn Mpv, state: VisualState) {
    let Some((track, start, end)) = selection(app) else {
        return;
    };
    // A yank is not an edit, so a success leaves visual mode open.
    run_action(app, player, &Action::YankRange { track, start, end });
    let _ = state;
}

fn paste_selected(app: &mut App, player: &dyn Mpv, state: VisualState) {
    if run_action(
        app,
        player,
        &Action::PasteRange {
            track: state.track,
            at: app.playhead,
        },
    ) {
        app.mode = Mode::Normal;
    }
}
