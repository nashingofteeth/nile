//! The job queue panel: server-owned render jobs, not the TUI's own
//! import/preview workers in `tui::jobs`.
//!
//! The queue lives on `nile serve`. This panel lists it, cancels a job and
//! points at a completed render's retained report; it never runs a render
//! itself.

use crossterm::event::{KeyCode, KeyEvent};

use crate::mpv::Mpv;

use super::super::jobs::{cancel_selected, refresh_jobs};
use super::super::{App, Mode};

pub(crate) fn jobs_key(app: &mut App, _player: &dyn Mpv, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = Mode::Normal;
            app.set_message("timeline");
        }
        KeyCode::Char('j') | KeyCode::Down => move_sel(app, 1),
        KeyCode::Char('k') | KeyCode::Up => move_sel(app, -1),
        KeyCode::Home | KeyCode::Char('g') => set_sel(app, 0),
        KeyCode::End | KeyCode::Char('G') => set_sel(app, app.jobs.len().saturating_sub(1)),
        KeyCode::Char('r') => match refresh_jobs(app) {
            Ok(()) => app.set_message("jobs reloaded"),
            Err(e) => app.fail(&e),
        },
        KeyCode::Char('x') | KeyCode::Char('c') => cancel_selected(app),
        KeyCode::Enter | KeyCode::Char('o') => point_at_report(app),
        _ => {}
    }
}

fn move_sel(app: &mut App, delta: isize) {
    let Some(sel) = jobs_sel(app) else { return };
    let next = (sel as isize + delta).clamp(0, app.jobs.len().saturating_sub(1) as isize) as usize;
    set_sel(app, next);
}

fn set_sel(app: &mut App, sel: usize) {
    if let Mode::Jobs(state) = &mut app.mode {
        state.sel = sel.min(app.jobs.len().saturating_sub(1));
    }
}

pub(crate) fn jobs_sel(app: &App) -> Option<usize> {
    match &app.mode {
        Mode::Jobs(state) => Some(state.sel.min(app.jobs.len().saturating_sub(1))),
        _ => None,
    }
}

/// `o`/Enter: point at the retained report. Opening a viewer would take over
/// the terminal; the path in the message line is the deliberate hand-off to
/// `nile report` / `nile annotation check`.
fn point_at_report(app: &mut App) {
    let Some(sel) = jobs_sel(app) else { return };
    let Some(job) = app.jobs.get(sel) else {
        app.set_message("no jobs");
        return;
    };
    // Prefer the live record: the panel's copy may predate the render's end.
    let report = app
        .project
        .job(job.id)
        .ok()
        .and_then(|live| live.report)
        .or_else(|| job.report.clone());
    match report {
        Some(report) => app.set_message(&format!("report: {}", report.display())),
        None => app.set_message(&format!(
            "job #{} has no report ({})",
            job.id,
            job.state.label()
        )),
    }
}
