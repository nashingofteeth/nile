//! Visual segment selection: vim's `v` over timeline segments.
//!
//! Selection is a contiguous range of whole segments on one track. `d`
//! deletes and `y` copies in one edit; `p` pastes the copied range at the
//! playhead, splitting the clip or gap under it when necessary.

use crossterm::event::{KeyCode, KeyEvent};

use nle_core::{edit, frame_of, grid_time, secs_f64};

use crate::mpv::Mpv;

use super::super::actions::persist_and_refresh;
use super::super::croc::CrocAct;
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
        KeyCode::Char('y') => yank_selected(app, state),
        KeyCode::Char('p') => paste_selected(app, player, state),
        KeyCode::Char('+') | KeyCode::Char('=') => app.zoom_timeline(1.5),
        KeyCode::Char('-') => app.zoom_timeline(1.0 / 1.5),
        KeyCode::Char('z') => app.toggle_timeline_zoom(),
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
    let Some(tr) = app.project.timeline.track(track) else {
        app.fail("track vanished");
        return;
    };
    let start_time = secs_f64(&tr.start_of(start));
    let batch = match edit::delete_range_on(&app.project, track, start, end) {
        Ok(batch) => batch,
        Err(e) => {
            app.fail(&e.to_string());
            return;
        }
    };
    let frame = frame_of(app.project.fps.as_ref(), start_time);
    let notice = batch.desc.clone();
    if let Err(e) = app.project.apply(batch, frame) {
        app.fail(&e.to_string());
        return;
    }
    let _ = state;
    app.mode = Mode::Normal;
    app.playhead = start_time.clamp(0.0, app.last_frame());
    app.croc.play(CrocAct::Chomp);
    app.set_message(&notice);
    persist_and_refresh(app, player);
}

fn yank_selected(app: &mut App, state: VisualState) {
    let Some((track, start, end)) = selection(app) else {
        return;
    };
    let Some(tr) = app.project.timeline.track(track) else {
        app.fail("track vanished");
        return;
    };
    app.visual_register = tr.segments[start..=end].to_vec();
    app.set_message(&format!(
        "yanked {} segment(s) — p at the playhead",
        end - start + 1
    ));
    let _ = state;
}

fn paste_selected(app: &mut App, player: &dyn Mpv, state: VisualState) {
    if app.visual_register.is_empty() {
        app.fail("visual register is empty");
        return;
    }
    let at = app.playhead;
    let batch = match edit::insert_segments_on(&app.project, state.track, at, &app.visual_register)
    {
        Ok(batch) => batch,
        Err(e) => {
            app.fail(&e.to_string());
            return;
        }
    };
    let frame = frame_of(app.project.fps.as_ref(), at);
    let notice = batch.desc.clone();
    if let Err(e) = app.project.apply(batch, frame) {
        app.fail(&e.to_string());
        return;
    }
    app.mode = Mode::Normal;
    app.croc.play(CrocAct::Gulp);
    app.set_message(&notice);
    persist_and_refresh(app, player);
}
