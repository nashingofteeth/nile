//! Background work: the ffmpeg render/import job and the off-thread preview
//! build. Both are polled from the event loop; neither blocks a keystroke.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Instant;

use nle_core::media::RawProbe;

use crate::keyframes::{self, Keyframes};
use crate::mpv::Mpv;
use crate::preview;
use crate::thumbnails::{self, Thumbnail};
use crate::waveform::{self, Waveform};

use super::keys::extend::extend_state;
use super::keys::pool::pool_select;
use super::{App, is_pool, pool_sel, reload_project};
/// A background job (an ffmpeg render, a heavy import): its label for
/// notices, whether the project file changed underneath us (reload after),
/// and the channel its worker reports on. One at a time — the monitor
/// keeps playing while it runs.
pub(crate) struct Job {
    pub(crate) label: &'static str,
    pub(crate) reload: bool,
    pub(crate) rx: std::sync::mpsc::Receiver<Result<String, String>>,
}

/// A monitor preview rebuild in flight. Proxy renders are seconds of ffmpeg,
/// so the EDL is built on a worker while the status row shows progress.
pub(crate) struct WaveJob {
    pub(crate) rx: std::sync::mpsc::Receiver<(u32, Result<Waveform, String>)>,
}

/// A keyframe probe for the pooled asset. One at a time; the result is
/// keyed by asset id so a stale selection can't seek the wrong file.
pub(crate) struct KeyframeJob {
    pub(crate) asset: u32,
    pub(crate) rx: std::sync::mpsc::Receiver<(u32, Result<Keyframes, String>)>,
}

/// A thumbnail extraction for the pooled asset.
pub(crate) struct ThumbnailJob {
    pub(crate) asset: u32,
    pub(crate) rx: std::sync::mpsc::Receiver<(u32, Result<Thumbnail, String>)>,
}

pub(crate) struct PreviewJob {
    pub(crate) rx: std::sync::mpsc::Receiver<PreviewMsg>,
    /// When the build started — the status row waits a moment before
    /// announcing it, so a cached/no-proxy rebuild doesn't flash.
    pub(crate) started: Instant,
}

/// A media import probing batch. Separate from `Job` because it reports
/// per-file progress and carries the probed reports the main thread needs to
/// add assets (the worker must not mutate the project).
pub(crate) struct ImportJob {
    pub(crate) rx: Receiver<ImportMsg>,
}

pub(crate) enum ImportMsg {
    /// How many probeable files the path/directory expansion produced.
    Total(usize),
    Progress {
        done: usize,
        total: usize,
        path: PathBuf,
    },
    Done {
        outcomes: Vec<ImportOutcome>,
        errors: Vec<String>,
    },
}

pub(crate) struct ImportOutcome {
    pub(crate) path: PathBuf,
    pub(crate) probe: Result<RawProbe, String>,
}

pub(crate) enum PreviewMsg {
    /// `(finished renders, total renders, fraction of the one in flight)`.
    Progress {
        done: usize,
        total: usize,
        frac: f64,
    },
    Done(Result<preview::PreviewBuild, String>),
}

/// What the status row shows while a preview renders.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PreviewProgress {
    pub done: usize,
    pub total: usize,
    pub frac: f64,
}

/// Arm a background job (one at a time) and hand back its sender.
pub(crate) fn start_job(
    app: &mut App,
    label: &'static str,
    reload: bool,
) -> Option<std::sync::mpsc::Sender<Result<String, String>>> {
    if app.job.is_some() || app.import_job.is_some() {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    app.job = Some(Job { label, reload, rx });
    Some(tx)
}

/// Let a finished worker report in: swap its result into the message
/// line, and reload the project when the job rewrote it.
pub(crate) fn poll_job(app: &mut App, player: &dyn Mpv) {
    use std::sync::mpsc::TryRecvError;
    let Some((label, reload, res)) = app
        .job
        .as_ref()
        .map(|j| (j.label, j.reload, j.rx.try_recv()))
    else {
        return;
    };
    match res {
        Ok(Ok(msg)) => {
            app.job = None;
            if reload && let Err(e) = reload_project(app, player, true) {
                app.fail(&format!("{msg} — reload failed: {e}"));
            } else {
                app.set_message(&msg);
            }
        }
        Ok(Err(e)) => {
            app.job = None;
            app.fail(&format!("{label} failed: {e}"));
        }
        Err(TryRecvError::Disconnected) => {
            app.job = None;
            app.fail(&format!("{label} failed: worker died"));
        }
        Err(TryRecvError::Empty) => {}
    }
}

/// Start probing an import batch off-thread. The worker expands directories
/// and canonicalizes paths (both can hit a slow mount), then probes each file;
/// the main thread applies the reports so the project is only mutated in one
/// place.
pub(crate) fn start_import(app: &mut App, paths: Vec<PathBuf>) -> Result<String, String> {
    if app.job.is_some() || app.import_job.is_some() {
        return Err("a job is already running — wait for it".to_string());
    }
    if paths.is_empty() {
        return Err("usage: :import PATH|DIR …".to_string());
    }
    let (tx, rx) = mpsc::channel();
    app.import_job = Some(ImportJob { rx });
    std::thread::spawn(move || {
        let (files, errors) = crate::cmd::import::expand_import_paths(&paths);
        let total = files.len();
        if tx.send(ImportMsg::Total(total)).is_err() {
            return;
        }
        let mut outcomes = Vec::with_capacity(total);
        for (i, file) in files.into_iter().enumerate() {
            let probe = nle_ffmpeg::probe(&file).map_err(|e| e.to_string());
            outcomes.push(ImportOutcome {
                path: file.clone(),
                probe,
            });
            if tx
                .send(ImportMsg::Progress {
                    done: i + 1,
                    total,
                    path: file,
                })
                .is_err()
            {
                return;
            }
        }
        let _ = tx.send(ImportMsg::Done { outcomes, errors });
    });
    Ok("importing…".to_string())
}

/// Drain the import worker. Progress updates the message line; a finished
/// batch is added to the pool as one undo step, saved, and selected.
pub(crate) fn poll_import(app: &mut App, player: &dyn Mpv) {
    loop {
        let Some(msg) = app.import_job.as_ref().map(|j| j.rx.try_recv()) else {
            return;
        };
        match msg {
            Ok(ImportMsg::Total(total)) => {
                if total > 0 {
                    app.set_message(&format!("importing 0/{total}…"));
                }
            }
            Ok(ImportMsg::Progress { done, total, path }) => {
                app.set_message(&format!("importing {done}/{total} · {}", short_name(&path)));
            }
            Ok(ImportMsg::Done { outcomes, errors }) => {
                app.import_job = None;
                finish_import(app, player, outcomes, errors);
                return;
            }
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                app.import_job = None;
                app.fail("import failed: worker died");
                return;
            }
        }
    }
}

fn short_name(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let mut short: String = name.chars().take(48).collect();
    if name.chars().count() > 48 {
        short.push('…');
    }
    short
}

fn finish_import(
    app: &mut App,
    player: &dyn Mpv,
    outcomes: Vec<ImportOutcome>,
    mut errors: Vec<String>,
) {
    // A shell may have written the project while we probed — an import is
    // seconds long and pool mode holds foreign adoption. Saving over it would
    // silently drop that edit, so adopt first (the import then re-dedups
    // against the reloaded pool).
    if app.external_change()
        && let Err(e) = reload_project(app, player, true)
    {
        let mut msg = format!("import failed: reload failed: {e}");
        append_errors(&mut msg, &errors);
        app.fail(&msg);
        return;
    }
    let playhead = nle_core::frame_of(app.project.fps.as_ref(), app.playhead);
    let mut fresh = Vec::new();
    let mut existing = 0usize;
    for outcome in outcomes {
        match outcome.probe {
            Ok(report) => {
                if app.project.asset_by_path(&outcome.path).is_some() {
                    existing += 1;
                } else {
                    fresh.push((outcome.path, report));
                }
            }
            Err(e) => errors.push(format!("{}: {e}", outcome.path.display())),
        }
    }
    if fresh.is_empty() {
        let mut msg = if existing > 0 {
            format!("no new assets — {existing} already pooled")
        } else {
            "no media files imported".to_string()
        };
        append_errors(&mut msg, &errors);
        if errors.is_empty() {
            app.set_message(&msg);
        } else {
            app.fail(&msg);
        }
        return;
    }

    let ids = app.project.add_assets(fresh, playhead);
    if ids.is_empty() {
        let mut msg = "no new assets imported".to_string();
        append_errors(&mut msg, &errors);
        app.set_message(&msg);
        return;
    }
    let mut msg = format!("imported {} new", ids.len());
    if existing > 0 {
        msg.push_str(&format!(", {existing} already pooled"));
    }
    if let Err(e) = app.project.save(&app.path) {
        let mut msg = format!("imported {} — save failed: {e}", ids.len());
        append_errors(&mut msg, &errors);
        app.fail(&msg);
        return;
    }
    if let Err(e) = app
        .project
        .save_history(&nle_core::History::sidecar_path(&app.path))
    {
        let mut msg = format!("imported {} — history save failed: {e}", ids.len());
        append_errors(&mut msg, &errors);
        app.fail(&msg);
        return;
    }
    app.note_seen();
    start_waveforms(app);

    if let Some(last) = ids.last().copied()
        && let Some(idx) = app.project.media.iter().position(|a| a.id == last)
    {
        app.last_pool_sel = idx;
        app.last_pool_asset = Some(last);
        if is_pool(app) {
            pool_select(app, player, idx);
        }
    }
    append_errors(&mut msg, &errors);
    app.set_message(&msg);
}

fn append_errors(msg: &mut String, errors: &[String]) {
    if errors.is_empty() {
        return;
    }
    msg.push_str(" — failed: ");
    msg.push_str(&errors.join("; "));
}

/// Load any audio-master peaks the timeline needs, off-thread. Cached files
/// finish almost immediately; a cold decode must not stall a keystroke.
pub(crate) fn start_waveforms(app: &mut App) {
    if !app.preview_enabled || app.wave_job.is_some() {
        return;
    }
    let missing: Vec<_> = app
        .project
        .media
        .iter()
        .filter(|a| {
            a.probe.has_audio()
                && !app.waveforms.contains_key(&a.id)
                && !app.waveform_failed.contains(&a.id)
        })
        .cloned()
        .collect();
    if missing.is_empty() {
        return;
    }
    let project_path = app.path.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    app.wave_job = Some(WaveJob { rx });
    std::thread::spawn(move || {
        for asset in missing {
            let id = asset.id;
            let result = waveform::load(&project_path, &asset).map_err(|e| e.to_string());
            if tx.send((id, result)).is_err() {
                break;
            }
        }
    });
}

pub(crate) fn poll_waveforms(app: &mut App) {
    loop {
        let Some(job) = app.wave_job.as_ref() else {
            return;
        };
        match job.rx.try_recv() {
            Ok((id, Ok(wave))) => {
                app.waveforms.insert(id, wave);
            }
            Ok((id, Err(e))) => {
                // A deterministic failure (corrupt/unsupported audio): don't
                // respawn ffmpeg on every preview rebuild. A reload or a
                // re-import clears the mark.
                app.waveform_failed.insert(id);
                crate::log::line(&format!("waveform load failed: {e}"));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                app.wave_job = None;
                return;
            }
        }
    }
}

/// Start probing the pooled asset's keyframes, off-thread. The pool preview
/// still works while it runs; `J`/`K` say the probe is in flight.
pub(crate) fn start_keyframes(app: &mut App) {
    if !is_pool(app) || app.keyframe_job.is_some() {
        return;
    }
    let Some(sel) = pool_sel(app) else { return };
    let Some(asset) = app.project.media.get(sel).cloned() else {
        return;
    };
    if !asset.probe.has_video() || app.keyframes.contains_key(&asset.id) {
        return;
    }
    let id = asset.id;
    let project_path = app.path.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    app.keyframe_job = Some(KeyframeJob { asset: id, rx });
    std::thread::spawn(move || {
        let result = keyframes::load(&project_path, &asset).map_err(|e| e.to_string());
        let _ = tx.send((id, result));
    });
}

/// Start extracting the pooled asset's thumbnail, off-thread.
pub(crate) fn start_thumbnail(app: &mut App) {
    if !app.preview_enabled || !is_pool(app) || app.thumb_job.is_some() {
        return;
    }
    let Some(sel) = pool_sel(app) else { return };
    let Some(asset) = app.project.media.get(sel).cloned() else {
        return;
    };
    if !asset.probe.has_video() || app.thumbnails.contains_key(&asset.id) {
        return;
    }
    let id = asset.id;
    let project_path = app.path.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    app.thumb_job = Some(ThumbnailJob { asset: id, rx });
    std::thread::spawn(move || {
        let result = thumbnails::load(&project_path, &asset).map_err(|e| e.to_string());
        let _ = tx.send((id, result));
    });
}

pub(crate) fn poll_thumbnails(app: &mut App) {
    let Some(job) = app.thumb_job.as_ref() else {
        return;
    };
    match job.rx.try_recv() {
        Ok((id, Ok(thumbnail))) => {
            app.thumbnails.insert(id, thumbnail);
        }
        Ok((_, Err(e))) => {
            crate::log::line(&format!("thumbnail load failed: {e}"));
            app.thumb_job = None;
            return;
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => return,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            app.thumb_job = None;
            return;
        }
    }
    app.thumb_job = None;
    start_thumbnail(app);
}

pub(crate) fn poll_keyframes(app: &mut App) {
    let Some(job) = app.keyframe_job.as_ref() else {
        return;
    };
    match job.rx.try_recv() {
        Ok((id, Ok(keyframes))) => {
            app.keyframes.insert(id, keyframes.times);
        }
        Ok((_, Err(e))) => {
            crate::log::line(&format!("keyframe probe failed: {e}"));
            // Don't immediately respawn the same failing probe; a press of
            // `J`/`K` retries, and selecting another asset starts its own.
            app.keyframe_job = None;
            return;
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => return,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            app.keyframe_job = None;
            return;
        }
    }
    app.keyframe_job = None;
    // The selection may have moved while this probe ran; catch up.
    start_keyframes(app);
}

/// Rebuild the monitor's EDL at the current playhead, off-thread: a proxy
/// render is seconds of ffmpeg, and blocking the key handler froze the UI.
/// A second edit while one runs just marks it dirty — the finished render
/// still warms the cache — and the newest build follows.
pub(crate) fn start_preview(app: &mut App) {
    if !app.preview_enabled {
        return;
    }
    start_waveforms(app);
    if app.preview.is_some() {
        app.preview_dirty = true;
        return;
    }
    let project = app.project.clone();
    let project_path = app.path.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    app.preview = Some(PreviewJob {
        rx,
        started: Instant::now(),
    });
    app.preview_dirty = false;
    app.preview_progress = Some(PreviewProgress::default());
    std::thread::spawn(move || {
        let progress_tx = tx.clone();
        let mut report = move |done, total, frac| {
            let _ = progress_tx.send(PreviewMsg::Progress { done, total, frac });
        };
        let result = preview::build_composite_preview_with(&project, &project_path, &mut report)
            .map_err(|e| e.to_string());
        let _ = tx.send(PreviewMsg::Done(result));
    });
}

/// Drain the preview worker: progress feeds the status row, and a completed
/// (non-stale) build loads the monitor at wherever the playhead is now. While
/// the pool is open its asset keeps the monitor — `close_pool` rebuilds.
pub(crate) fn poll_preview(app: &mut App, player: &dyn Mpv) {
    use std::sync::mpsc::TryRecvError;
    loop {
        let Some(res) = app.preview.as_ref().map(|j| j.rx.try_recv()) else {
            return;
        };
        match res {
            Ok(PreviewMsg::Progress { done, total, frac }) => {
                app.preview_progress = Some(PreviewProgress { done, total, frac });
            }
            Ok(PreviewMsg::Done(result)) => {
                let stale = app.preview_dirty;
                let pool = is_pool(app);
                let extending = extend_state(app).is_some();
                app.preview = None;
                app.preview_progress = None;
                if stale {
                    app.preview_dirty = false;
                    if !pool && !extending {
                        start_preview(app);
                    }
                } else if !pool && !extending {
                    // Extend mode owns the monitor like the pool does: over
                    // the material preview it handed to mpv, whose clock is
                    // translated through `preview_off`. A timeline build
                    // finishing mid-session must not replace that picture
                    // (`exit_extend` reloads when extend leaves).
                    match result {
                        Ok(build) => {
                            // Keep ignoring the pooled clock until the EDL's
                            // own file reports the landing (see `sync`).
                            if app.pool_on_monitor {
                                app.pool_handoff = Some(app.playhead);
                            }
                            player.load_with_audio(
                                &build.url,
                                Some(app.playhead),
                                build.audio.as_deref(),
                            );
                            if let Some(paused) = app.pending_pause.take() {
                                player.set_pause(paused);
                            }
                        }
                        Err(e) => preview_failed(app, &e.to_string()),
                    }
                }
                return;
            }
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                app.preview = None;
                app.preview_progress = None;
                preview_failed(app, "worker died");
                return;
            }
        }
    }
}

/// Keep the timeline's notice: a monitor problem must not erase what
/// happened to the model (the same rule as the old inline rebuild).
/// A failed rebuild during a hand-off (pool close, extend exit/commit)
/// also gives the hand-off up: the timeline EDL will never land, and
/// `sync` gates on `pool_on_monitor`, so the playhead would ignore mpv
/// forever. The monitor keeps the old (paused) file; the next action's
/// rebuild fixes the picture — and this notice says what didn't.
/// In the pool the pooled asset legitimately owns the monitor: there the
/// flag stands.
pub(crate) fn preview_failed(app: &mut App, why: &str) {
    if !is_pool(app) {
        app.pool_on_monitor = false;
        app.pool_handoff = None;
    }
    let msg = if app.message.is_empty() {
        format!("preview failed: {why}")
    } else {
        format!("{} — preview failed: {why}", app.message)
    };
    app.fail(&msg);
}
