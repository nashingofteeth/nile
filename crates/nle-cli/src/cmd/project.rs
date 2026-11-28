//! Commands that read or write the project itself: init, add, show, seek,
//! the listings, the cache controls and the TUI launcher.

#[cfg(feature = "tui")]
use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, Result, bail};

use nle_core::edit;
use nle_core::project::wrong_kind;
use nle_core::state;
use nle_core::time::{parse_signed_time, parse_time};
use nle_core::timeline::{Clip, Segment, TrackKind};
use nle_core::{
    Action, Document, Project, Seconds, TimeMap, VideoSettings, format_seconds, frame_of,
    track_label,
};
use nle_ffmpeg::probe;

use super::asset_summary;
use super::import::{parse_size_arg, validate_import_path};
use super::{cli_index, run_cli_action};
#[cfg(feature = "tui")]
use crate::mpv;
use crate::preview;
use crate::proxy;
#[cfg(feature = "tui")]
use crate::tui;
use crate::{CacheCmd, GroupCmd, SequenceCmd, parse_fps_arg};
use crate::{log, say};
#[cfg(feature = "tui")]
pub(crate) fn cmd_edit(
    project_path: &Path,
    dump: bool,
    dump_size: &str,
    dump_keys: &str,
) -> Result<()> {
    // `--dump`: render once and print the state + grid, no monitor needed —
    // the diagnostic path (a rendering or placement question needs no mpv).
    if dump {
        let project = Project::load(project_path)?;
        let (w, h) = dump_size
            .split_once(['x', 'X'])
            .map_or((100, 30), |(w, h)| {
                (
                    w.trim().parse().unwrap_or(100),
                    h.trim().parse().unwrap_or(30),
                )
            });
        print!(
            "{}",
            tui::dump(project, project_path.to_path_buf(), (w, h), dump_keys)?
        );
        return Ok(());
    }
    // ratatui panics when it cannot take the terminal (`nile edit > log`,
    // ssh without -t); refuse first so the CLI keeps its 0/1/2 exit contract.
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "nile edit needs a terminal (stdin/stdout is not a TTY); \
             `nile edit --dump` works without one"
        );
    }
    // Attach to `nile serve` when its socket is live; otherwise load the file.
    let session = crate::session::Session::open(project_path)?;
    // The monitor plays the composite, so any video track with a clip counts.
    let has_clip = session
        .project()
        .timeline
        .video_tracks()
        .any(|t| t.segments.iter().any(|s| matches!(s, Segment::Clip(_))));
    if !has_clip {
        bail!("timeline is empty — add clips first (the monitor needs content)");
    }
    let media_title = format!(
        "nile — {}",
        project_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| project_path.display().to_string())
    );
    let player = mpv::MpvPlayer::spawn(
        &format!("nile monitor — {}", project_path.display()),
        &media_title,
    )?;
    player.observe()?;
    // The TUI builds the monitor's EDL off-thread (proxy renders can be
    // seconds), so it appears immediately with progress instead of hanging
    // here before the terminal is initialized.
    tui::run(session, player)?;
    Ok(())
}

/// Print the `edl://` URL the monitor plays (or one track's, for debugging).
pub(crate) fn cmd_edl(
    project_path: &Path,
    track: Option<usize>,
    sequence: Option<u32>,
) -> Result<()> {
    let mut project = Project::load(project_path)?;
    if let Some(id) = sequence {
        project.select_sequence(id)?;
    }
    let Some(track) = track else {
        say!("{}", preview::build_composite_edl(&project, project_path)?);
        return Ok(());
    };
    // 1-based, and 0 is not a track number (it used to mean "track 1").
    let ti = cli_index(track, "--track").map_err(anyhow::Error::msg)?;
    if project.timeline.track(ti).is_none() {
        bail!("no track #{track}");
    }
    say!("{}", preview::build_edl(&project, project_path, ti)?);
    Ok(())
}

/// The project used when `-p` is absent: `.nile/config.json`, else
/// `nile.json` in the current directory.
pub(crate) fn resolve_default_project(cwd: &Path) -> Result<std::path::PathBuf> {
    Ok(state::load_default_project(cwd)?.unwrap_or_else(|| std::path::PathBuf::from("nile.json")))
}

/// `nile default [FILE]`: show or set the directory's default project.
pub(crate) fn cmd_default(cwd: &Path, file: Option<&Path>, unset: bool) -> Result<()> {
    if unset && file.is_some() {
        bail!("use either a FILE or --unset, not both");
    }
    let config = state::config_path(cwd);
    if unset {
        state::save_default_project(cwd, None)?;
        say!("default project: nile.json (fallback)");
        return Ok(());
    }
    if let Some(file) = file {
        state::save_default_project(cwd, Some(file))?;
        say!("✓ default project: {}", file.display());
        return Ok(());
    }
    match state::load_default_project(cwd)? {
        Some(project) => say!("default project: {}", project.display()),
        None => say!("default project: nile.json (fallback)"),
    }
    say!("config: {}", config.display());
    Ok(())
}

pub(crate) fn cmd_init(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            path.display()
        );
    }
    save_and_reset_history(&Project::new(), path)?;
    say!("✓ created {}", path.display());
    Ok(())
}

/// Save the project after an out-of-band write and drop the undo sidecar: a
/// CLI write re-baselines the project, so any persisted stack no longer
/// matches it (the next TUI session starts fresh rather than refusing every
/// undo). See AGENTS.md "Compatibility: none, by design".
pub(crate) fn save_and_reset_history(project: &Project, path: &Path) -> Result<()> {
    // Through Document so the clamp-before-save rule holds on this path too;
    // callers that print clamp notes have already done the same clamp.
    // A rebuild replaces the payload but must not rewind the revision: carry
    // the on-disk one forward, or a compare-and-swap client that read the old
    // file would match the new one and its write would be accepted against a
    // different model. An absent or unreadable file has nothing to carry.
    let mut project = project.clone();
    if let Ok(existing) = Project::load(path) {
        project.revision = project.revision.max(existing.revision);
    }
    let (mut doc, _) = Document::rebuild(project);
    doc.save(path)?;
    doc.reset_history(path);
    log::line(&format!("saved {} (undo history reset)", path.display()));
    Ok(())
}

pub(crate) fn cmd_add(
    project_path: &Path,
    file: &Path,
    track: usize,
    at: Option<&str>,
    in_point: Option<&str>,
    out_point: Option<&str>,
) -> Result<()> {
    let mut doc = Document::with_histories(
        Project::load(project_path)?,
        edit::Histories::load_project(project_path).unwrap_or_default(),
    );
    // `--track` is 1-based (matching the `v1`/`a1` labels); 0 internally.
    // 0 is not a track number — it used to silently mean "track 1".
    let ti = cli_index(track, "--track").map_err(anyhow::Error::msg)?;
    if ti >= doc.timeline.tracks.len() {
        bail!("no track #{track}");
    }
    let playhead = frame_of(doc.fps.as_ref(), secs_f64(&doc.playhead));
    let kind = doc
        .timeline
        .track(ti)
        .map(|t| t.kind)
        .with_context(|| format!("no track #{track}"))?;
    let (scratch, imported) = stage_clip(&mut doc, file, in_point, out_point, playhead, kind)?;
    if imported && let Segment::Clip(c) = &scratch {
        say!("· auto-imported as asset #{}", c.asset);
    }

    // Ripple paste: inserts at --at (splits clips/gaps) or appends.
    let append_at = match at {
        Some(s) => parse_time(s).with_context(|| format!("invalid --at time: {s:?}"))?,
        None => doc
            .timeline
            .track(ti)
            .map(|t| t.end())
            .unwrap_or_else(|| Seconds::from_integer(0)),
    };
    // The pool/settings writes land first, with their history: a recorded
    // pool addition (undo removes the asset again) plus the first-clip
    // settings. The insert itself goes through the catalog below — the
    // same `Paste` the TUI's pool insert runs.
    doc.save(project_path)?;
    if imported {
        doc.save_history(&edit::Histories::sidecar_path(project_path))?;
    }
    run_cli_action(
        project_path,
        &Action::Paste {
            track: ti,
            at: secs_f64(&append_at),
            ripple: true,
            segment: Some(scratch),
        },
    )
}

/// Probe `file` into the pool if it is new, validate the source range,
/// and stage the clip to insert — shared by `nile add` and the TUI's
/// `:add`. Returns the clip and whether the pool was written (a recorded
/// pool addition, so it is undoable). The first clip defines the sequence
/// settings, like a real NLE.
pub(crate) fn stage_clip(
    doc: &mut Document,
    file: &Path,
    in_point: Option<&str>,
    out_point: Option<&str>,
    playhead: i64,
    kind: TrackKind,
) -> Result<(Segment, bool)> {
    // `add` auto-imports into the pool, which is stored as UTF-8: refuse a
    // non-UTF-8 path before canonicalize/probe so no clip or success line can
    // precede the save failure.
    validate_import_path(file)?;
    let canonical = file
        .canonicalize()
        .with_context(|| format!("{} not found", file.display()))?;
    // The canonical path is what the pool stores: a valid argument can still
    // resolve through a symlink to a non-UTF-8 target.
    validate_import_path(&canonical)?;

    let existing = doc.asset_by_path(&canonical).map(|a| a.id);
    let (asset_id, imported) = match existing {
        Some(id) => {
            // Refuse a kind mismatch *before* touching the pool, so the
            // error isn't preceded by a successful import.
            if let Some(asset) = doc.asset(id)
                && let Some(reason) = asset.wrong_kind(kind)
            {
                bail!("{reason}");
            }
            (id, false)
        }
        None => {
            let report = probe(file).with_context(|| format!("importing {}", file.display()))?;
            if let Some(reason) = wrong_kind(&canonical, &report, kind) {
                bail!("{reason}");
            }
            let (id, _) = doc.add_asset(canonical, report, playhead)?;
            (id, true)
        }
    };

    let in_t = match in_point {
        Some(s) => parse_time(s).with_context(|| format!("invalid --in time: {s:?}"))?,
        None => Seconds::from_integer(0),
    };
    let out_t = match out_point {
        Some(s) => parse_time(s).with_context(|| format!("invalid --out time: {s:?}"))?,
        None => doc
            .asset(asset_id)
            .and_then(|a| a.duration())
            .ok_or_else(|| anyhow::anyhow!("asset has unknown duration — pass --out"))?,
    };

    let asset = doc.asset(asset_id).context("asset vanished after import")?;
    if in_t >= out_t {
        bail!("--in must be before --out");
    }
    if let Some(d) = asset.duration()
        && out_t > d
    {
        bail!("--out {out_point:?} is beyond the asset's duration {d}");
    }
    // The first clip defines the sequence settings (like a real NLE).
    let asset_fps = asset.fps();
    let asset_size = asset.video_size();
    doc.set_sequence(
        asset_fps,
        asset_size.map(|(w, h)| VideoSettings {
            width: w,
            height: h,
        }),
    );

    let scratch = Segment::Clip(Clip {
        id: 0,
        asset: asset_id,
        asset_in: in_t,
        asset_out: out_t,
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    Ok((scratch, imported))
}

pub(crate) fn secs_f64(t: &Seconds) -> f64 {
    *t.numer() as f64 / *t.denom() as f64
}

fn secs_string(t: &Seconds) -> String {
    format!("{}/{}", t.numer(), t.denom())
}

/// The project JSON plus derived per-track/asset time maps. The maps are not
/// persisted: they are a view of the clips, so they cannot drift. Only
/// `show --json --maps` takes this path; plain `show --json` stays the raw
/// persisted document (and therefore matches `project.show`).
fn map_spans_json(map: &TimeMap) -> Vec<serde_json::Value> {
    map.spans()
        .iter()
        .map(|span| {
            let (timeline_start, timeline_end) = span.timeline();
            let (source_start, source_end) = span.source();
            serde_json::json!({
                "timeline": [secs_string(&timeline_start), secs_string(&timeline_end)],
                "source": [secs_string(&source_start), secs_string(&source_end)],
            })
        })
        .collect()
}

fn show_json_with_maps(project: &Project) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(project)?;
    let mut maps = Vec::new();
    for (track_index, track) in project.timeline.tracks.iter().enumerate() {
        let assets: std::collections::BTreeSet<u32> = track
            .segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Clip(clip) => Some(clip.asset),
                _ => None,
            })
            .collect();
        for asset in assets {
            let map = TimeMap::from_track_for_asset(track, asset)?;
            if map.is_empty() {
                continue;
            }
            maps.push(serde_json::json!({
                "sequence": project.active_id(),
                "track_index": track_index,
                "asset": asset,
                "spans": map_spans_json(&map),
            }));
        }
    }
    value["time_maps"] = serde_json::Value::Array(maps);

    let mut group_maps = Vec::new();
    for group in &project.groups {
        for (track_index, track) in project.timeline.tracks.iter().enumerate() {
            let assets: std::collections::BTreeSet<u32> = track
                .segments
                .iter()
                .filter_map(|segment| match segment {
                    Segment::Clip(clip) => Some(clip.asset),
                    _ => None,
                })
                .collect();
            for asset in assets {
                let map =
                    project.group_time_map(project.active_id(), track_index, group.id, asset)?;
                if map.is_empty() {
                    continue;
                }
                group_maps.push(serde_json::json!({
                    "sequence": project.active_id(),
                    "track_index": track_index,
                    "group": group.id,
                    "asset": asset,
                    "spans": map_spans_json(&map),
                }));
            }
        }
    }
    value["group_maps"] = serde_json::Value::Array(group_maps);
    Ok(value)
}

pub(crate) fn cmd_show(
    project_path: &Path,
    json: bool,
    maps: bool,
    sequence: Option<u32>,
) -> Result<()> {
    let mut project = Project::load(project_path)?;
    if json {
        if sequence.is_some() {
            bail!("--json prints the whole project; omit --sequence");
        }
        let value = if maps {
            show_json_with_maps(&project)?
        } else {
            serde_json::to_value(&project)?
        };
        say!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    if let Some(id) = sequence {
        project.select_sequence(id)?;
    }
    say!(
        "{} — {}",
        project_path.display(),
        show_report(&project).trim_end()
    );
    Ok(())
}

/// `nile seek [TIME]` — move the stored playhead (snapped to the frame grid,
/// clamped into the timeline) or print it. The TUI opens where it lands.
pub(crate) fn cmd_seek(project_path: &Path, at: Option<&str>) -> Result<()> {
    let mut project = Project::load(project_path)?;
    let Some(s) = at else {
        say!("{}", format_seconds(&project.playhead));
        return Ok(());
    };
    let t = parse_signed_time(s).with_context(|| format!("invalid time {s:?}"))?;
    let landed = project.set_playhead(secs_f64(&t));
    let mut doc = Document::new(project);
    doc.save(project_path)?;
    log::line(&format!("seek -> {landed} s"));
    let frame = frame_of(doc.fps.as_ref(), landed);
    match doc.fps {
        Some(_) => say!(
            "✓ playhead → {} s (f{frame})",
            format_seconds(&doc.playhead)
        ),
        None => say!("✓ playhead → {} s", format_seconds(&doc.playhead)),
    }
    Ok(())
}

/// `nile media ls` — the pool listing. (`nile media rm` is a catalog
/// action; `main` routes it there, undoably.)
pub(crate) fn cmd_media_ls(project_path: &Path) -> Result<()> {
    let project = Project::load(project_path)?;
    if project.media.is_empty() {
        say!("media pool is empty — `nile import <file>`");
        return Ok(());
    }
    say!(
        "{} — {} asset(s)",
        project_path.display(),
        project.media.len()
    );
    for a in &project.media {
        let used = project.asset_usage(a.id);
        let usage = if used.is_empty() {
            "unused".to_string()
        } else {
            format!("used by {used:?}")
        };
        say!("  #{:<4} {:<9} {}", a.id, asset_summary(a), usage);
    }
    Ok(())
}

/// `nile track ls` — the track listing. (add/mute/rm are catalog actions.)
/// `nile sequence ls|add|derive|use|audio` — the project's timelines.
/// Selection is session state: edits and undo follow the active sequence.
pub(crate) fn cmd_sequence(project_path: &Path, action: &SequenceCmd) -> Result<()> {
    match action {
        SequenceCmd::Ls => {
            let project = Project::load(project_path)?;
            say!(
                "{} — {} sequence(s)",
                project_path.display(),
                project.sequences.len()
            );
            for sequence in &project.sequences {
                let mark = if sequence.id == project.active_id() {
                    "*"
                } else {
                    " "
                };
                let fps = sequence
                    .fps
                    .map(|fps| format!("{}/{} fps", fps.numer(), fps.denom()))
                    .unwrap_or_else(|| "no fps".into());
                let video = sequence
                    .video
                    .as_ref()
                    .map(|v| format!("{}×{}", v.width, v.height))
                    .unwrap_or_else(|| "no size".into());
                let clips: usize = sequence
                    .timeline
                    .tracks
                    .iter()
                    .flat_map(|track| track.segments.iter())
                    .filter(|seg| matches!(seg, Segment::Clip(_)))
                    .count();
                let audio = if sequence.audio.is_active() {
                    format!(" · audio {}", sequence.audio.summary())
                } else {
                    String::new()
                };
                say!(
                    "{mark} #{:<2} {:<20} {fps:<12} {video:<11} {clips} clip(s){audio}",
                    sequence.id,
                    sequence.name
                );
            }
            Ok(())
        }
        SequenceCmd::Add {
            name,
            width,
            height,
            fps,
        } => {
            let video = match (width, height) {
                (Some(width), Some(height)) => Some(VideoSettings {
                    width: *width,
                    height: *height,
                }),
                (None, None) => None,
                _ => bail!("--width and --height go together"),
            };
            let fps = fps.as_deref().map(parse_fps_arg).transpose()?;
            let mut doc = Document::new(Project::load(project_path)?);
            let id = doc.add_sequence(name.clone(), fps, video)?;
            doc.save(project_path)?;
            say!("✓ sequence #{id} {name} (not active — `nile sequence use {id}`)");
            Ok(())
        }
        SequenceCmd::Derive {
            from,
            name,
            size,
            fps,
        } => {
            let video = size.as_deref().map(parse_size_arg).transpose()?;
            let fps = fps.as_deref().map(parse_fps_arg).transpose()?;
            let mut doc = Document::new(Project::load(project_path)?);
            let id = doc.derive_sequence(
                *from,
                name.clone(),
                fps,
                video.map(|(width, height)| VideoSettings { width, height }),
            )?;
            doc.save(project_path)?;
            say!(
                "✓ sequence #{id} {name} (derived from #{from}, not active — `nile sequence use {id}`)"
            );
            Ok(())
        }
        SequenceCmd::Use { id } => {
            let mut project = Project::load(project_path)?;
            project.select_sequence(*id)?;
            let name = project.active_sequence().name.clone();
            Document::new(project).save(project_path)?;
            say!("✓ sequence #{id} {name} is active");
            Ok(())
        }
        SequenceCmd::Audio {
            normalize,
            fade_in,
            fade_out,
        } => {
            let project = Project::load(project_path)?;
            if normalize.is_none() && fade_in.is_none() && fade_out.is_none() {
                say!(
                    "#{} {} · audio {}",
                    project.active_id(),
                    project.active_sequence().name,
                    project.audio.summary()
                );
                return Ok(());
            }
            let mut audio = project.audio;
            if let Some(normalize) = normalize {
                audio.normalize = *normalize;
            }
            if let Some(fade_in) = fade_in {
                audio.fade_in = parse_time(fade_in)
                    .with_context(|| format!("invalid --fade-in {fade_in:?}"))?;
            }
            if let Some(fade_out) = fade_out {
                audio.fade_out = parse_time(fade_out)
                    .with_context(|| format!("invalid --fade-out {fade_out:?}"))?;
            }
            let histories = edit::Histories::load_project(project_path).unwrap_or_default();
            let mut doc = Document::with_histories(project, histories);
            let playhead = frame_of(doc.fps.as_ref(), secs_f64(&doc.playhead));
            let desc = doc.set_audio(audio, playhead)?;
            doc.save(project_path)?;
            doc.save_history(&edit::Histories::sidecar_path(project_path))?;
            say!("✓ {desc}");
            Ok(())
        }
    }
}

/// `nile group ls|rm` — logical media recordings.
pub(crate) fn cmd_group(project_path: &Path, action: &GroupCmd) -> Result<()> {
    match action {
        GroupCmd::Ls => {
            let project = Project::load(project_path)?;
            if project.groups.is_empty() {
                say!("no media groups — `nile import --group NAME FILES…`");
                return Ok(());
            }
            say!(
                "{} — {} media group(s)",
                project_path.display(),
                project.groups.len()
            );
            for group in &project.groups {
                let end = group
                    .end()
                    .map(|end| format_seconds(&end))
                    .unwrap_or_else(|| "?".into());
                say!(
                    "  #{:<3} {:<20} {} part(s) · 0–{} s",
                    group.id,
                    group.name,
                    group.parts.len(),
                    end
                );
            }
            Ok(())
        }
        GroupCmd::Rm { id } => {
            let project = Project::load(project_path)?;
            let histories = edit::Histories::load_project(project_path).unwrap_or_default();
            let mut doc = Document::with_histories(project, histories);
            let playhead = frame_of(doc.fps.as_ref(), secs_f64(&doc.playhead));
            let desc = doc.remove_group(*id, playhead)?;
            doc.save(project_path)?;
            doc.save_history(&edit::Histories::sidecar_path(project_path))?;
            say!("✓ {desc}");
            Ok(())
        }
    }
}

/// How one annotation's anchor reads in a listing (shared with the TUI).
pub(crate) fn annotation_anchor_label(
    project: &Project,
    annotation: &nle_core::Annotation,
) -> String {
    match &annotation.anchor {
        nle_core::Anchor::Source { source, time } => match source {
            nle_core::SourceRef::Asset { id } => {
                format!("asset #{id} @ {} s", format_seconds(time))
            }
            nle_core::SourceRef::Group { id } => {
                format!("group #{id} @ {} s", format_seconds(time))
            }
        },
        nle_core::Anchor::Timeline { .. } => match project.annotation_span(annotation) {
            Some((start, end)) if start == end => {
                format!("timeline {} s", format_seconds(&start))
            }
            Some((start, end)) => format!(
                "timeline {}–{} s",
                format_seconds(&start),
                format_seconds(&end)
            ),
            None => "timeline (unresolved)".to_string(),
        },
    }
}

/// `nile report FILE [--json]` — the sidecar written beside an export.
pub(crate) fn cmd_report(file: &Path, json: bool) -> Result<()> {
    let sidecar = super::check::report_sidecar(file);
    let report = nle_core::RenderReport::load(&sidecar)?;
    if json {
        say!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        say!("{}", report.summary(&sidecar));
    }
    Ok(())
}

/// `nile annotation ls [--sequence ID]` — notes and markers, newest id last.
pub(crate) fn cmd_annotation_ls(project_path: &Path, sequence: Option<u32>) -> Result<()> {
    let project = Project::load(project_path)?;
    let sequence = sequence.unwrap_or(project.active_id());
    project
        .sequence(sequence)
        .ok_or(nle_core::ProjectError::SequenceNotFound(sequence))?;
    let annotations: Vec<&nle_core::Annotation> = project
        .annotations
        .iter()
        .filter(|a| a.sequence == sequence)
        .collect();
    if annotations.is_empty() {
        say!("no annotations on sequence #{sequence} — `nile annotation add TEXT`");
        return Ok(());
    }
    say!(
        "{} — {} annotation(s) on sequence #{sequence}",
        project_path.display(),
        annotations.len()
    );
    for annotation in annotations {
        let anchor = annotation_anchor_label(&project, annotation);
        let status = match annotation.status {
            nle_core::AnnotationStatus::Open => "open",
            nle_core::AnnotationStatus::Resolved => "resolved",
        };
        match &annotation.kind {
            nle_core::AnnotationKind::Note { text } => say!(
                "  #{:<3} [{status}] {:<6} {anchor} — {text:?}",
                annotation.id,
                "note"
            ),
            nle_core::AnnotationKind::Marker => say!(
                "  #{:<3} [{status}] {:<6} {anchor}",
                annotation.id,
                "marker"
            ),
            nle_core::AnnotationKind::Expectation { expect } => say!(
                "  #{:<3} [{status}] {:<6} {anchor} — {}",
                annotation.id,
                "expect",
                expect.name()
            ),
        }
    }
    Ok(())
}

pub(crate) fn cmd_track_ls(project_path: &Path) -> Result<()> {
    let project = Project::load(project_path)?;
    say!(
        "{} — {} track(s)",
        project_path.display(),
        project.timeline.tracks.len()
    );
    for (i, t) in project.timeline.tracks.iter().enumerate() {
        let kind = match t.kind {
            TrackKind::Video => "video",
            TrackKind::Audio => "audio",
        };
        let mute = if t.muted { " (muted)" } else { "" };
        say!(
            "  {:<3} {:<6} {} segment(s){}",
            i + 1,
            kind,
            t.segments.len(),
            mute
        );
    }
    Ok(())
}

pub(crate) fn cmd_cache(project_path: &Path, action: &CacheCmd) -> Result<()> {
    match action {
        CacheCmd::Prune { days, dry_run } => {
            if *days == 0 {
                bail!("--days must be at least 1 (there is no --all)");
            }
            let max_age = std::time::Duration::from_secs(days.saturating_mul(86_400));
            let report = proxy::prune(project_path, max_age, *dry_run)?;
            let audio = crate::preview::prune_audio(project_path, max_age, *dry_run)?;
            let wave = crate::waveform::prune(project_path, max_age, *dry_run)?;
            let keyframes = crate::keyframes::prune(project_path, max_age, *dry_run)?;
            let thumbs = crate::thumbnails::prune(project_path, max_age, *dry_run)?;
            let filmstrip = crate::filmstrip::prune(project_path, max_age, *dry_run)?;
            let export = super::schedule::prune_export(project_path, max_age, *dry_run)?;
            let analysis = crate::analysis::prune(project_path, max_age, *dry_run)?;
            let prefix = if *dry_run { "prune --dry-run" } else { "prune" };
            say!(
                "{prefix}: {}; audio: {}; wave: {}; keyframes: {}; thumbs: {}; filmstrip: {}; export: {}; analysis: {}",
                report.summary_named(*dry_run, "proxy render"),
                audio.summary_named(*dry_run, "preview audio"),
                wave.summary_named(*dry_run, "waveform"),
                keyframes.summary_named(*dry_run, "keyframe list"),
                thumbs.summary_named(*dry_run, "thumbnail"),
                filmstrip.summary_named(*dry_run, "filmstrip"),
                export.summary_named(*dry_run, "export chunk"),
                analysis.summary_named(*dry_run, "analysis entry")
            );
            Ok(())
        }
    }
}

/// The human timeline report behind `nile show` (and the TUI's `:show`):
/// a summary line, then one line per segment (`[i]` is the segment index
/// `--seg` addresses). A trailing newline.
pub(crate) fn show_report(project: &Project) -> String {
    let clip_count: usize = project
        .timeline
        .tracks
        .iter()
        .flat_map(|t| t.segments.iter())
        .filter(|s| matches!(s, Segment::Clip(_)))
        .count();

    let mut out = String::new();
    if project.sequences.len() > 1 {
        out += &format!(
            "#{} {} · ",
            project.active_id(),
            project.active_sequence().name
        );
    }
    out += &format!(
        "{} clip(s) · {} s",
        clip_count,
        format_seconds(&project.timeline.end())
    );
    if let Some(fps) = &project.fps {
        out += &format!(" · {}/{} fps", fps.numer(), fps.denom());
    }
    if let Some(v) = &project.video {
        out += &format!(" · {}×{}", v.width, v.height);
    }
    if project.audio.is_active() {
        out += &format!(" · audio {}", project.audio.summary());
    }
    out += &format!(" · at {} s", format_seconds(&project.playhead));
    out.push('\n');

    for (ti, track) in project.timeline.tracks.iter().enumerate() {
        let mute = if track.muted { " (muted)" } else { "" };
        out += &format!("  {}{mute}\n", track_label(project, ti));

        let mut cursor = Seconds::from_integer(0);
        for (si, seg) in track.segments.iter().enumerate() {
            match seg {
                Segment::Transition(t) => {
                    let end = cursor + t.dur;
                    let frames = project
                        .fps
                        .map_or(0, |fps| (t.dur * fps).round().to_integer());
                    out += &format!(
                        "      [{si}] ⟶ transition #{} {}–{} ({frames}f)\n",
                        t.id,
                        format_seconds(&cursor),
                        format_seconds(&end)
                    );
                    cursor = end;
                }
                Segment::Gap(g) => {
                    let end = cursor + g.duration;
                    out += &format!(
                        "      [{si}] ┄ gap {}–{}\n",
                        format_seconds(&cursor),
                        format_seconds(&end)
                    );
                    cursor = end;
                }
                Segment::Clip(clip) => {
                    let asset = project
                        .asset(clip.asset)
                        .map(|a| a.path.display().to_string())
                        .unwrap_or_else(|| "?".into());
                    let start = cursor;
                    let end = start + clip.duration();
                    let speed = if clip.speed != Seconds::from_integer(1) {
                        format!(" @{}×", format_seconds(&clip.speed))
                    } else {
                        String::new()
                    };
                    // A clip's effects apply in list order; show the kinds
                    // (the parameters are one `X` away in the panel).
                    let fx = if clip.effects.is_empty() {
                        String::new()
                    } else {
                        let kinds: Vec<&str> = clip.effects.iter().map(|e| e.kind()).collect();
                        format!("  · fx: {}", kinds.join(", "))
                    };
                    out += &format!(
                        "    [{si}] #{:<4}{:>9} – {:<9} {}{speed}  (src {}–{}){fx}\n",
                        clip.id,
                        format_seconds(&start),
                        format_seconds(&end),
                        asset,
                        format_seconds(&clip.asset_in),
                        format_seconds(&clip.asset_out)
                    );
                    cursor = end;
                }
            }
        }
    }
    if !project.groups.is_empty() {
        out += "  media groups\n";
        for group in &project.groups {
            out += &format!(
                "    group #{} {} · {} part(s)\n",
                group.id,
                group.name,
                group.parts.len()
            );
        }
    }
    out
}
