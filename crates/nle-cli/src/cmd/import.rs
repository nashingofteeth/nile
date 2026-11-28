//! `nile import`, `nile import-edl` and `nile import-otio`: probe media into
//! the pool and rebuild a timeline from an EDL or an OTIO document.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};

use nle_core::edit;
use nle_core::edl::{self, EdlEvent};
use nle_core::otio;
use nle_core::otio::OtioItem;
use nle_core::time::{parse_timecode, timecode_to_frames};
use nle_core::timeline::{Segment, Timeline, TrackKind};
use nle_core::{Document, Project, VideoSettings, frame_of};
use nle_ffmpeg::probe;

use super::detect::append_spans;
use super::project::{save_and_reset_history, secs_f64};
use super::{CommandReport, asset_summary};
use crate::{parse_fps_arg, say};
pub(crate) fn cmd_import(
    project_path: &Path,
    files: &[PathBuf],
    group: Option<&str>,
) -> Result<()> {
    let mut doc = Document::with_histories(
        Project::load(project_path)?,
        edit::Histories::load_project(project_path).unwrap_or_default(),
    );
    let playhead = frame_of(doc.fps.as_ref(), secs_f64(&doc.playhead));
    let (files, mut errors) = expand_import_paths(files);
    for e in &errors {
        say!("· {e}");
    }
    if files.is_empty() && !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    // `Path` is arbitrary bytes on Unix, but `nile.json` stores asset paths
    // as UTF-8 strings: refuse the batch here, before probing, so a valid
    // sibling is not half-imported and no `✓` can precede a save that is
    // guaranteed to fail.
    validate_import_paths(&files)?;

    let mut fresh: Vec<(PathBuf, nle_core::media::RawProbe)> = Vec::new();
    for file in &files {
        // Dedup before probing: re-importing a directory must not spawn
        // ffprobe for every file already in the pool.
        if let Some(a) = doc.asset_by_path(file) {
            say!("· {} already imported (asset #{})", file.display(), a.id);
            continue;
        }
        match probe(file) {
            Ok(report) => fresh.push((file.clone(), report)),
            // One unreadable file must not abort the rest of the batch (the
            // TUI reports per file and imports the good ones the same way).
            Err(e) => {
                let msg = format!("importing {}: {e}", file.display());
                say!("· {msg}");
                errors.push(msg);
            }
        }
    }
    if fresh.is_empty() && !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    if group.is_some() && !errors.is_empty() {
        bail!("cannot form a media group: {}", errors.join("; "));
    }
    if group.is_some() && files.len() < 2 {
        bail!("--group needs at least two files");
    }
    let imported_paths: Vec<PathBuf> = fresh.iter().map(|(p, _)| p.clone()).collect();
    // One undo entry for the whole command: `nile undo` peels the batch.
    let (ids, group_id) = if let Some(name) = group {
        let (ids, group_id) = doc.add_assets_with_group(fresh, playhead, name, files.clone())?;
        (ids, Some(group_id))
    } else {
        (doc.add_assets(fresh, playhead)?, None)
    };
    for (id, path) in ids.iter().zip(&imported_paths) {
        let asset = doc.asset(*id).context("asset vanished")?;
        say!(
            "✓ asset #{id}  {}  {}",
            path.display(),
            asset_summary(asset)
        );
    }
    let added = ids.len();
    if added > 0 || group_id.is_some() {
        doc.save(project_path)?;
        doc.save_history(&edit::Histories::sidecar_path(project_path))?;
    }
    if let Some(id) = group_id {
        say!(
            "✓ media group #{id} {group_name}",
            group_name = group.unwrap_or("")
        );
    }
    let clip_count = |p: &Project| {
        p.timeline
            .tracks
            .iter()
            .flat_map(|t| t.segments.iter())
            .filter(|s| matches!(s, Segment::Clip(_)))
            .count()
    };
    say!(
        "· {} — {} assets, {} clip(s)",
        project_path.display(),
        doc.media.len(),
        clip_count(&doc)
    );
    Ok(())
}

/// Media extensions probed when importing a `--media` directory.
pub(crate) const MEDIA_EXTS: &[&str] = &[
    "mp4", "mov", "mkv", "m4v", "avi", "webm", "wav", "mp3", "m4a", "aac", "flac", "ogg",
];

/// A `--group NAME=FILE,FILE,…` spec. Parsed by clap, so a malformed flag
/// fails before any file is probed and an empty name or parts list cannot
/// exist downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupSpec {
    pub name: String,
    pub parts: Vec<PathBuf>,
}

impl FromStr for GroupSpec {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let (name, list) = s
            .split_once('=')
            .ok_or_else(|| format!("expected NAME=FILE,FILE,… (got {s:?})"))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("expected a group name before '=' (got {s:?})"));
        }
        let parts = list
            .split(',')
            .map(str::trim)
            .map(|part| {
                if part.is_empty() {
                    Err(format!("group {name:?} has an empty part (got {s:?})"))
                } else {
                    Ok(PathBuf::from(part))
                }
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if parts.len() < 2 {
            return Err(format!(
                "group {name:?} needs at least two parts (NAME=FILE,FILE)"
            ));
        }
        Ok(Self {
            name: name.to_string(),
            parts,
        })
    }
}

/// Parse the TUI's raw `--group` values with the same grammar the CLI's clap
/// parser enforces.
#[cfg(feature = "tui")]
pub(crate) fn parse_group_specs(raw: &[String]) -> Result<Vec<GroupSpec>> {
    raw.iter()
        .map(|spec| GroupSpec::from_str(spec).map_err(anyhow::Error::msg))
        .collect()
}

/// Locate one explicit group part: as given, then in the importer's media
/// directory, then by a unique pooled file name. Unlike the directory scan,
/// a group part is a promise — a miss is fatal, not a warning.
fn resolve_group_part(project: &Project, raw: &Path, dir: &Path) -> Result<PathBuf> {
    let raw = expand_tilde(raw);
    for candidate in [raw.clone(), dir.join(&raw)] {
        if candidate.is_file() {
            return candidate
                .canonicalize()
                .with_context(|| format!("reading {}", candidate.display()));
        }
    }
    if let Some(name) = raw.file_name() {
        let matches: Vec<&PathBuf> = project
            .media
            .iter()
            .filter(|asset| asset.path.file_name() == Some(name))
            .map(|asset| &asset.path)
            .collect();
        match matches.as_slice() {
            [one] => return Ok((*one).clone()),
            [] => {}
            many => bail!(
                "group part {} matches {} pooled files — give a path",
                raw.display(),
                many.len()
            ),
        }
    }
    bail!("group part {} not found", raw.display())
}

/// Probe and pool a resolved part if it is new.
fn pool_group_part(project: &mut Project, path: &Path) -> Result<()> {
    if project.asset_by_path(path).is_some() {
        return Ok(());
    }
    let report = probe(path).with_context(|| format!("probing {}", path.display()))?;
    project.add_asset(path.to_path_buf(), report)?;
    Ok(())
}

/// Turn pooled parts into one group through the model's constructor, so
/// `nile import --group` and the importers share one part layout.
fn add_group_parts(project: &mut Project, spec: &GroupSpec, paths: &[PathBuf]) -> Result<u32> {
    // `ProjectError::Group` carries the id and part index, so it is already
    // self-contained — no context to hide it behind (the TUI flattens errors
    // with `to_string`).
    let parts = project.group_parts(paths)?;
    Ok(project.add_group(&spec.name, parts)?)
}

/// Parse `--size WxH` into pixels. Both `x` and `×` are accepted; zero is
/// refused here rather than reaching the export graph as a division by zero.
pub(crate) fn parse_size_arg(s: &str) -> Result<(u32, u32)> {
    let split = s
        .split_once(['x', 'X'])
        .or_else(|| s.split_once('×'))
        .ok_or_else(|| anyhow::anyhow!("invalid --size {s:?} (expected WxH, e.g. 1280x720)"))?;
    let parse = |what: &str, value: &str| -> Result<u32> {
        value.trim().parse::<u32>().map_err(|_| {
            anyhow::anyhow!("invalid --size {s:?}: {what} {value:?} is not a positive integer")
        })
    };
    let (width, height) = (parse("width", split.0)?, parse("height", split.1)?);
    if width == 0 || height == 0 {
        bail!("--size must be non-zero (got {s:?})");
    }
    Ok((width, height))
}

/// Settle the sequence's output size before anything is written.
///
/// A fresh import takes `--size` first, then the first video asset's size. An
/// append keeps the loaded sequence's size and refuses a conflicting
/// `--size` (like `--fps`); it fills a missing size from `--size` or a video
/// asset. Either way the final size must be known: an audio-only import still
/// needs a canvas for export's black base, and writing a project that cannot
/// render is not a successful import.
fn settle_video_size(
    project: &mut Project,
    append: bool,
    requested: Option<&str>,
    inferred: Option<(u32, u32)>,
) -> Result<()> {
    let requested = requested.map(parse_size_arg).transpose()?;
    if append && let Some(existing) = project.video.as_ref() {
        if let Some((width, height)) = requested
            && (existing.width, existing.height) != (width, height)
        {
            bail!(
                "--append keeps the project's {}x{}; --size {width}x{height} would rebuild the canvas",
                existing.width,
                existing.height
            );
        }
        return Ok(());
    }
    let Some((width, height)) = requested.or(inferred) else {
        bail!(
            "no video size known — pass --size WxH (an audio-only import still needs an export canvas)"
        );
    };
    project.video = Some(VideoSettings { width, height });
    Ok(())
}

/// Find an existing track of `kind` with the same mute state that no incoming
/// track has claimed yet.
fn matching_track(
    project: &Project,
    used: &HashSet<usize>,
    kind: TrackKind,
    muted: bool,
) -> Option<usize> {
    project
        .timeline
        .tracks
        .iter()
        .enumerate()
        .find(|(i, t)| !used.contains(i) && t.kind == kind && t.muted == muted)
        .map(|(i, _)| i)
}

/// Add a built import to the existing sequence at its end.
///
/// Each incoming track keeps its kind **and mute state**: it reuses the first
/// unclaimed existing track with the same pair, otherwise a new track is
/// created with that mute flag. The first video track goes to `video_track`
/// when given, and a mute mismatch there is refused rather than silently
/// unmuting (or muting) the target. Every target is padded to the common
/// `start` with a gap (`append_spans` coalesces it with a leading gap), so
/// tracks stay aligned instead of starting at their own ends. Clip ids are
/// renumbered from the project's next free id: the builders number from 1,
/// which would collide with existing clips.
fn append_timeline(
    project: &mut Project,
    mut incoming: Timeline,
    video_track: Option<usize>,
) -> Result<()> {
    let start = project.timeline.end();
    let mut next_id = Some(project.next_clip_id()?);
    for track in &mut incoming.tracks {
        for segment in &mut track.segments {
            match segment {
                Segment::Clip(clip) => clip.id = Project::take_clip_id(&mut next_id)?,
                Segment::Transition(tr) => tr.id = Project::take_clip_id(&mut next_id)?,
                Segment::Gap(_) => {}
            }
        }
    }

    // Validate an explicit target even when the incoming document has no
    // video material: a bad `--track` is a user error regardless.
    if let Some(n) = video_track {
        let i = super::cli_index(n, "--track").map_err(anyhow::Error::msg)?;
        let track = project
            .timeline
            .track(i)
            .with_context(|| format!("no track #{n}"))?;
        if track.kind != TrackKind::Video {
            bail!("track #{n} is not a video track");
        }
    }

    let mut used: HashSet<usize> = HashSet::new();
    let mut first_video = true;
    for track in incoming.tracks {
        // An audio-only EDL still carries the model's empty base video track;
        // there is no material to append from it.
        if track.segments.is_empty() {
            continue;
        }
        let target = if first_video && track.kind == TrackKind::Video {
            first_video = false;
            match video_track {
                Some(n) => {
                    let i = super::cli_index(n, "--track").map_err(anyhow::Error::msg)?;
                    let existing = project
                        .timeline
                        .track(i)
                        .with_context(|| format!("no track #{n}"))?;
                    if existing.muted != track.muted {
                        bail!(
                            "--track {n} is {} but the imported video track is {} — pick a track with the same mute state",
                            if existing.muted { "muted" } else { "unmuted" },
                            if track.muted { "muted" } else { "unmuted" }
                        );
                    }
                    i
                }
                None => matching_track(project, &used, track.kind, track.muted)
                    .unwrap_or_else(|| add_matching_track(project, track.kind, track.muted)),
            }
        } else {
            matching_track(project, &used, track.kind, track.muted)
                .unwrap_or_else(|| add_matching_track(project, track.kind, track.muted))
        };
        used.insert(target);
        append_spans(project, target, start, track.segments);
    }
    Ok(())
}

/// Append a new empty track with the incoming track's kind and mute flag.
fn add_matching_track(project: &mut Project, kind: TrackKind, muted: bool) -> usize {
    let i = project.timeline.add_track(kind);
    project.timeline.tracks[i].muted = muted;
    i
}

/// Expand `~`/`~/…` without a shell. `~user` is left alone: resolving
/// another account's home is not needed here.
pub(crate) fn expand_tilde(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return path.to_path_buf();
    };
    if text == "~" {
        return home;
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home.join(rest);
    }
    path.to_path_buf()
}

/// Reject a path the project could never store. `Path` is an `OsStr`
/// (arbitrary bytes on Unix); the model serializes an asset path as a UTF-8
/// `String`, so a non-UTF-8 path only fails at save — after probing, adding
/// and printing the asset. Validate first instead: the error names the lossy
/// display (`Path::display`) so the user can find the file.
pub(crate) fn validate_import_path(path: &Path) -> Result<()> {
    if path.to_str().is_some() {
        return Ok(());
    }
    bail!(
        "{}: path is not valid UTF-8 — nile.json stores paths as UTF-8, so rename it and import again",
        path.display()
    )
}

/// The batch form of [`validate_import_path`]: refuse every non-UTF-8 member
/// before probing, adding or printing anything, so a valid sibling is not
/// half-imported.
pub(crate) fn validate_import_paths(paths: &[PathBuf]) -> Result<()> {
    let bad: Vec<String> = paths
        .iter()
        .filter_map(|path| validate_import_path(path).err().map(|e| e.to_string()))
        .collect();
    if bad.is_empty() {
        Ok(())
    } else {
        bail!("{}", bad.join("; "))
    }
}

/// Expand import arguments into canonical files. A directory contributes its
/// immediate media files, matching the EDL `--media` scan; nested directories
/// are left to the picker. Missing paths and empty directories are returned as
/// warnings instead of aborting the batch.
pub(crate) fn expand_import_paths(paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<String>) {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    let mut seen = HashSet::new();
    for raw in paths {
        let raw = expand_tilde(raw);
        let canonical = match raw.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                errors.push(format!("{}: not found", raw.display()));
                continue;
            }
        };
        if canonical.is_dir() {
            let mut found: Vec<PathBuf> = Vec::new();
            match std::fs::read_dir(&canonical) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let is_media = path
                            .extension()
                            .and_then(|e| e.to_str())
                            .is_some_and(|e| MEDIA_EXTS.iter().any(|m| e.eq_ignore_ascii_case(m)));
                        if path.is_file() && is_media {
                            found.push(path);
                        }
                    }
                }
                Err(e) => {
                    errors.push(format!("{}: {e}", raw.display()));
                    continue;
                }
            }
            found.sort();
            if found.is_empty() {
                errors.push(format!("no media files in {}", raw.display()));
                continue;
            }
            for path in found {
                if let Ok(path) = path.canonicalize()
                    && seen.insert(path.clone())
                {
                    files.push(path);
                }
            }
        } else if canonical.is_file() {
            if seen.insert(canonical.clone()) {
                files.push(canonical);
            }
        } else {
            errors.push(format!("{}: not a file or directory", raw.display()));
        }
    }
    (files, errors)
}

/// Match a reel/clip name to a pooled asset in three tiers — exact
/// filename, exact stem, then a case-insensitive substring at a stem
/// **edge** — and only when a tier binds **exactly one** asset. Two
/// constraints keep generic reels from silently binding an unrelated
/// file: `AX` (the placeholder for unnamed aux media) is a substring of
/// stems like `taxi` but aligns with none of their edges, and a tie (e.g.
/// `red_a` + `red_b` for needle `red`) stays unresolved so the caller can
/// report the real problem. The uniqueness rule applies to the exact
/// tiers too: with `/a/red.mp4` and `/b/red.mp4` pooled, a bare
/// `red.mp4` is ambiguous — first-match-in-pool-order is a decision, not
/// evidence.
pub(crate) fn match_asset(project: &Project, needle: &str) -> Option<u32> {
    let needle = needle.to_lowercase();
    let needle_stem = Path::new(&needle)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| needle.clone());
    let mut by_file: Vec<u32> = Vec::new();
    let mut by_stem: Vec<u32> = Vec::new();
    let mut fuzzy: Vec<u32> = Vec::new();
    for a in &project.media {
        let file = a
            .path
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let stem = a
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        // Filename is stronger evidence than stem: `red.mp4` must prefer
        // the `.mp4` even when a `red.mkv` is pooled alongside.
        if file == needle {
            by_file.push(a.id);
            continue;
        }
        if stem == needle || file == needle_stem || stem == needle_stem {
            by_stem.push(a.id);
            continue;
        }
        // An empty stem (or needle stem) matches everything — that is not
        // information, don't match on it.
        if !stem.is_empty() && !needle_stem.is_empty() && (fuzzy_edge(&stem, &needle_stem)) {
            fuzzy.push(a.id);
        }
    }
    unique(by_file)
        .or_else(|| unique(by_stem))
        .or_else(|| unique(fuzzy))
}

/// The one id in `ids`, or `None` when zero or several — an ambiguous
/// match is not a match.
pub(crate) fn unique(mut ids: Vec<u32>) -> Option<u32> {
    if ids.len() == 1 { ids.pop() } else { None }
}

/// Substring match at an edge: `red` matches `red_start` (as a prefix) and
/// `out_red` (as a suffix), but not inside `taxi` for needle `ax`.
pub(crate) fn fuzzy_edge(needle: &str, hay: &str) -> bool {
    hay.starts_with(needle)
        || hay.ends_with(needle)
        || needle.starts_with(hay)
        || needle.ends_with(hay)
}

/// Resolve an EDL event to an asset id: an explicit `--map` reel, then the
/// `SOURCE FILE`/`FROM CLIP NAME` filename, then the reel field.
pub(crate) fn resolve_reel(
    project: &Project,
    map: &HashMap<String, PathBuf>,
    ev: &EdlEvent,
) -> Option<u32> {
    if let Some(path) = map.get(&ev.reel)
        && let Some(a) = project.asset_by_path(path)
    {
        return Some(a.id);
    }
    for cand in [ev.source_file.as_deref(), ev.clip_name.as_deref()]
        .into_iter()
        .flatten()
    {
        if let Some(id) = match_asset(project, cand) {
            return Some(id);
        }
    }
    // A black reel is empty space, not a clip: the fuzzy edge rule would
    // latch `BL` onto `blue.mp4`. Only an explicit `--map` or a
    // `SOURCE FILE`/`FROM CLIP NAME` hint resolves it; `build` turns the
    // unresolved black reel into a gap.
    if edl::is_black_reel(&ev.reel) {
        return None;
    }
    match_asset(project, &ev.reel)
}

/// Arguments for [`cmd_import_edl`] and [`import_edl_work`]. A parameter
/// struct keeps the call sites readable and clippy's `too_many_arguments`
/// quiet — an options object instead of a long positional list.
pub(crate) struct EdlImport<'a> {
    pub project: &'a Path,
    pub file: &'a Path,
    pub media: &'a Path,
    pub fps: Option<&'a str>,
    pub src_offset: &'a str,
    pub maps: &'a [String],
    pub groups: &'a [GroupSpec],
    pub append: bool,
    pub track: Option<usize>,
    pub size: Option<&'a str>,
    pub force: bool,
}

pub(crate) fn cmd_import_edl(args: EdlImport<'_>) -> Result<()> {
    import_edl_work(args).map(|r| r.print())
}

/// The work behind `import-edl` (and the TUI's `:edl-import`).
pub(crate) fn import_edl_work(args: EdlImport<'_>) -> Result<CommandReport> {
    let EdlImport {
        project: project_path,
        file: edl_path,
        media: media_dir,
        fps: fps_arg,
        src_offset,
        maps,
        groups,
        append,
        track,
        size: size_arg,
        force,
    } = args;
    if append && force {
        bail!(
            "--append and --force are mutually exclusive: --append adds to the existing project, --force replaces it"
        );
    }
    let mut project = if append {
        Project::load(project_path).with_context(|| {
            format!(
                "--append needs an existing project at {}",
                project_path.display()
            )
        })?
    } else {
        if project_path.exists() && !force {
            bail!(
                "{} already exists (use --force to overwrite, or --append)",
                project_path.display()
            );
        }
        Project::new()
    };
    let text = std::fs::read_to_string(edl_path)
        .with_context(|| format!("reading {}", edl_path.display()))?;
    let doc = edl::parse(&text).with_context(|| format!("parsing {}", edl_path.display()))?;

    // Probe + pool every media file, deduped by path. `pool_media_dir`
    // validates the whole scan first (the pool is stored as UTF-8), so a bad
    // name refuses the import before any file is probed.
    let add = |project: &mut Project, p: &Path| -> Result<PathBuf> {
        let canonical = p
            .canonicalize()
            .with_context(|| format!("{} not found", p.display()))?;
        if project.asset_by_path(&canonical).is_none() {
            let report = probe(&canonical)?;
            project.add_asset(canonical.clone(), report)?;
        }
        Ok(canonical)
    };

    let (_, mut notes) = pool_media_dir(&mut project, media_dir)?;
    let mut map: HashMap<String, PathBuf> = HashMap::new();
    for m in maps {
        let (reel, path) = m
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--map expects REEL=PATH, got {m:?}"))?;
        let canonical = add(&mut project, Path::new(path))?;
        map.insert(reel.to_string(), canonical);
    }

    // Explicit groups are part of the ingest: pool their parts and create the
    // recording before the timeline is built, so `show --json --maps` can
    // resolve group time.
    for spec in groups {
        let mut paths = Vec::with_capacity(spec.parts.len());
        for raw in &spec.parts {
            let resolved = resolve_group_part(&project, raw, media_dir)?;
            pool_group_part(&mut project, &resolved)?;
            paths.push(resolved);
        }
        add_group_parts(&mut project, spec, &paths)?;
    }
    // A wrong `--media` directory is still an error, but explicit parts
    // (`--map`, `--group`) are pooled media in their own right.
    if project.media.is_empty() {
        bail!("no media files found in {}", media_dir.display());
    }

    // Prefer the first video event's asset for settings; fall back to any.
    let (fps, size) = {
        let first_asset = doc
            .events
            .iter()
            .filter(|e| edl::channel_has(&e.channel, 'V'))
            .find_map(|e| resolve_reel(&project, &map, e))
            .and_then(|id| project.asset(id));
        let infer = || {
            first_asset
                .and_then(|a| a.fps())
                .or_else(|| project.media.iter().find_map(|a| a.fps()))
                .ok_or_else(|| anyhow::anyhow!("no frame rate known — pass --fps"))
        };
        let fps = match (append, project.fps, fps_arg) {
            // An append build must land on the sequence's own grid: a
            // different rate silently bends every record timecode and the
            // appended block drifts off the frame grid.
            (true, Some(project_fps), Some(arg)) => {
                let requested = parse_fps_arg(arg)?;
                if requested != project_fps {
                    bail!(
                        "--append keeps the project's frame rate ({}/{}); --fps {arg:?} would rebuild the grid",
                        project_fps.numer(),
                        project_fps.denom()
                    );
                }
                project_fps
            }
            (true, Some(project_fps), None) => project_fps,
            (_, _, Some(arg)) => parse_fps_arg(arg)?,
            (_, _, None) => infer()?,
        };
        let size = first_asset
            .and_then(|a| a.video_size())
            .or_else(|| project.media.iter().find_map(|a| a.video_size()));
        (fps, size)
    };
    if project.fps.is_none() {
        project.fps = Some(fps);
    }
    settle_video_size(&mut project, append, size_arg, size)?;

    let offset_frames = timecode_to_frames(parse_timecode(src_offset)?, fps, doc.drop_frame)
        .with_context(|| {
            format!("--src-offset {src_offset:?} is too long to represent at this frame rate")
        })?;
    let timeline = edl::build(&doc, fps, offset_frames, &|ev| {
        resolve_reel(&project, &map, ev)
    })
    .with_context(|| format!("building timeline from {}", edl_path.display()))?;
    if append {
        append_timeline(&mut project, timeline, track)?;
    } else {
        project.timeline = timeline;
    }
    // A non-cut event (a dissolve, wipe, key) was imported as a cut: say so,
    // rather than silently dropping what the EDL asked for.
    for w in &doc.warnings {
        notes.push(format!("warning: {w}"));
    }
    // Keep every clip inside its media and on the frame grid: an EDL's source
    // out-point can run past the file, and the model must not disagree with
    // what export can render (see `edit::clamp_project`).
    for n in edit::clamp_project(&mut project) {
        notes.push(format!("warning: {n}"));
    }
    project.validate()?;
    save_and_reset_history(&project, project_path)?;
    let action = if append { "appended" } else { "imported" };
    let mut report = CommandReport::new(format!(
        "✓ {} — {} {} event(s) → {} track(s), {} asset(s)",
        project_path.display(),
        action,
        doc.events.len(),
        project.timeline.tracks.len(),
        project.media.len()
    ));
    report.warnings = notes;
    Ok(report)
}

/// Probe every media file in `dir` into the pool; returns how many files were
/// seen (whether or not they were already pooled).
pub(crate) fn pool_media_dir(project: &mut Project, dir: &Path) -> Result<(usize, Vec<String>)> {
    let mut notes = Vec::new();
    let mut candidates: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("reading media dir {}", dir.display()))?
    {
        let path = entry?.path();
        let is_media = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| MEDIA_EXTS.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false);
        if !path.is_file() || !is_media {
            continue;
        }
        // A candidate list, not a promise: skip what can't be read.
        match path.canonicalize() {
            Ok(canonical) => candidates.push((path, canonical)),
            Err(_) => notes.push(format!("warning: skipping {} — not found", path.display())),
        }
    }
    // The pool is serialized into `nile.json` as UTF-8 paths: refuse the whole
    // scan before probing, so a valid neighbour is not half-pooled and the
    // eventual save cannot fail after the assets were already added. The
    // canonical spelling is what the project stores; the original spelling is
    // what the user sees in a skip note.
    let canonical_paths: Vec<PathBuf> = candidates.iter().map(|(_, p)| p.clone()).collect();
    validate_import_paths(&canonical_paths)?;
    let mut seen = 0usize;
    for (path, canonical) in candidates {
        if project.asset_by_path(&canonical).is_none() {
            match probe(&canonical) {
                Ok(report) => {
                    project.add_asset(canonical, report)?;
                }
                Err(e) => {
                    notes.push(format!("warning: skipping {} — {e}", path.display()));
                    continue;
                }
            }
        }
        seen += 1;
    }
    Ok((seen, notes))
}

/// Locate a clip's media: an explicit `--remap OLD=NEW`, then the path
/// itself (relative paths resolved against the `.otio` file), then a
/// same-named file already pooled from `--media`.
pub(crate) fn resolve_otio_path(
    project: &Project,
    raw: &Path,
    remap: &HashMap<String, PathBuf>,
    base: &Path,
) -> Result<PathBuf> {
    if let Some(new) = remap.get(&raw.to_string_lossy().into_owned()) {
        return Ok(new.clone());
    }
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        base.join(raw)
    };
    if candidate.exists() {
        return Ok(candidate);
    }
    if let Some(name) = raw.file_name() {
        let needle = name.to_string_lossy();
        for a in &project.media {
            if a.path
                .file_name()
                .map(|n| n.to_string_lossy() == needle)
                .unwrap_or(false)
            {
                return Ok(a.path.clone());
            }
        }
    }
    bail!(
        "{} not found (use --remap or --media to relocate it)",
        candidate.display()
    )
}

/// Arguments for [`cmd_import_otio`] and [`import_otio_work`].
pub(crate) struct OtioImport<'a> {
    pub project: &'a Path,
    pub file: &'a Path,
    pub media: Option<&'a Path>,
    pub fps: Option<&'a str>,
    pub remaps: &'a [String],
    pub groups: &'a [GroupSpec],
    pub append: bool,
    pub track: Option<usize>,
    pub size: Option<&'a str>,
    pub force: bool,
}

pub(crate) fn cmd_import_otio(args: OtioImport<'_>) -> Result<()> {
    import_otio_work(args).map(|r| r.print())
}

/// The work behind `import-otio` (and the TUI's `:otio-import`).
pub(crate) fn import_otio_work(args: OtioImport<'_>) -> Result<CommandReport> {
    let OtioImport {
        project: project_path,
        file: otio_path,
        media: media_dir,
        fps: fps_arg,
        remaps,
        groups,
        append,
        track,
        size: size_arg,
        force,
    } = args;
    if append && force {
        bail!(
            "--append and --force are mutually exclusive: --append adds to the existing project, --force replaces it"
        );
    }
    let mut project = if append {
        Project::load(project_path).with_context(|| {
            format!(
                "--append needs an existing project at {}",
                project_path.display()
            )
        })?
    } else {
        if project_path.exists() && !force {
            bail!(
                "{} already exists (use --force to overwrite, or --append)",
                project_path.display()
            );
        }
        Project::new()
    };
    let text = std::fs::read_to_string(otio_path)
        .with_context(|| format!("reading {}", otio_path.display()))?;
    let doc = otio::parse(&text).with_context(|| format!("parsing {}", otio_path.display()))?;

    let mut scan_notes = Vec::new();
    if let Some(dir) = media_dir {
        let (_, notes) = pool_media_dir(&mut project, dir)?;
        scan_notes = notes;
    }

    let remap: HashMap<String, PathBuf> = remaps
        .iter()
        .map(|m| {
            let (old, new) = m
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("--remap expects OLD=NEW, got {m:?}"))?;
            Ok((old.to_string(), PathBuf::from(new)))
        })
        .collect::<Result<_>>()?;
    let base = otio_path.parent().unwrap_or_else(|| Path::new("."));

    // Explicit groups pool their parts before clip resolution, so a group
    // part can satisfy a clip whose path moved (the pooled-name fallback),
    // exactly as the EDL importer does.
    for spec in groups {
        let mut paths = Vec::with_capacity(spec.parts.len());
        for raw in &spec.parts {
            let resolved = resolve_group_part(&project, raw, base)?;
            pool_group_part(&mut project, &resolved)?;
            paths.push(resolved);
        }
        add_group_parts(&mut project, spec, &paths)?;
    }

    // Resolve + probe every distinct clip path.
    let mut by_path: HashMap<PathBuf, u32> = HashMap::new();
    for track in &doc.tracks {
        for item in &track.items {
            let OtioItem::Clip(c) = item else { continue };
            if by_path.contains_key(&c.path) {
                continue;
            }
            let located = resolve_otio_path(&project, &c.path, &remap, base)
                .with_context(|| format!("clip {:?}", c.name))?;
            let canonical = located
                .canonicalize()
                .with_context(|| format!("clip {:?}: {} not found", c.name, located.display()))?;
            // Append starts from a populated pool: reuse an asset already
            // there rather than re-probing (and possibly adding a second
            // entry for the same path).
            if let Some(asset) = project.asset_by_path(&canonical) {
                by_path.insert(c.path.clone(), asset.id);
                continue;
            }
            let report = probe(&canonical)?;
            let (id, _) = project.add_asset(canonical, report)?;
            by_path.insert(c.path.clone(), id);
        }
    }

    let fps = match (append, project.fps, fps_arg) {
        // As with EDL append, the existing sequence's grid wins; a
        // conflicting `--fps` would quantize the same spans differently.
        (true, Some(project_fps), Some(arg)) => {
            let requested = parse_fps_arg(arg)?;
            if requested != project_fps {
                bail!(
                    "--append keeps the project's frame rate ({}/{}); --fps {arg:?} would rebuild the grid",
                    project_fps.numer(),
                    project_fps.denom()
                );
            }
            project_fps
        }
        (true, Some(project_fps), None) => project_fps,
        (_, _, Some(arg)) => parse_fps_arg(arg)?,
        (_, _, None) => doc.fps,
    };
    if project.fps.is_none() {
        project.fps = Some(fps);
    }
    let size = doc
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video)
        .flat_map(|t| &t.items)
        .find_map(|item| match item {
            OtioItem::Clip(c) => by_path
                .get(&c.path)
                .and_then(|id| project.asset(*id))
                .and_then(|a| a.video_size()),
            OtioItem::Gap(_) => None,
        });
    settle_video_size(&mut project, append, size_arg, size)?;

    let timeline = otio::build(&doc, &|p| by_path.get(p).copied())
        .with_context(|| format!("building timeline from {}", otio_path.display()))?;
    if append {
        append_timeline(&mut project, timeline, track)?;
    } else {
        project.timeline = timeline;
    }
    // Keep every clip inside its media and on the frame grid.
    let clamp_notes = edit::clamp_project(&mut project);
    project.validate()?;
    save_and_reset_history(&project, project_path)?;

    let mut report = CommandReport::new(String::new());
    report.warnings = scan_notes;
    for w in doc.warnings.iter().chain(clamp_notes.iter()) {
        report.warn(format!("warning: {w}"));
    }
    let clips = project
        .timeline
        .tracks
        .iter()
        .flat_map(|t| t.segments.iter())
        .filter(|s| matches!(s, Segment::Clip(_)))
        .count();
    let action = if append { "appended" } else { "imported" };
    report.summary = format!(
        "✓ {} — {} {} track(s), {} asset(s), {} clip(s)",
        project_path.display(),
        action,
        project.timeline.tracks.len(),
        project.media.len(),
        clips
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_core::Seconds;
    use nle_core::media::{RawFormat, RawProbe, RawStream};

    fn fake_probe(duration: &str) -> RawProbe {
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

    #[test]
    fn group_spec_keeps_argument_order() {
        let spec: GroupSpec = "CAM 3=a.mp4, b.mp4".parse().unwrap();
        assert_eq!(spec.name, "CAM 3");
        assert_eq!(
            spec.parts,
            vec![PathBuf::from("a.mp4"), PathBuf::from("b.mp4")]
        );
    }

    #[test]
    fn size_arg_parses_wxh_and_refuses_bad_values() {
        assert_eq!(parse_size_arg("1280x720").unwrap(), (1280, 720));
        assert_eq!(parse_size_arg("1920X1080").unwrap(), (1920, 1080));
        assert_eq!(parse_size_arg("640×360").unwrap(), (640, 360));
        for bad in ["1280", "1280x", "x720", "0x720", "1280x0", "a x b", "-2x4"] {
            assert!(parse_size_arg(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn group_spec_refuses_malformed_flags() {
        for bad in ["CAM 3", "=a,b", "CAM 3=a", "CAM 3=a,,b", "CAM 3=,a"] {
            assert!(bad.parse::<GroupSpec>().is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn group_parts_use_argument_order_and_cumulative_starts() {
        let mut p = Project::new();
        let (first, _) = p
            .add_asset("/media/a.mp4".into(), fake_probe("2"))
            .expect("asset");
        let (second, _) = p
            .add_asset("/media/b.mp4".into(), fake_probe("3"))
            .expect("asset");
        let spec: GroupSpec = "CAM 3=a.mp4,b.mp4".parse().unwrap();
        let id = add_group_parts(
            &mut p,
            &spec,
            &["/media/a.mp4".into(), "/media/b.mp4".into()],
        )
        .unwrap();

        let group = p.group(id).expect("group");
        assert_eq!(group.parts[0].asset, first);
        assert_eq!(group.parts[0].group_start, Seconds::from_integer(0));
        assert_eq!(group.parts[1].asset, second);
        assert_eq!(group.parts[1].group_start, Seconds::from_integer(2));
        assert_eq!(group.end(), Some(Seconds::from_integer(5)));
    }

    #[test]
    fn group_parts_refuse_a_duplicate_part() {
        let mut p = Project::new();
        p.add_asset("/media/a.mp4".into(), fake_probe("2"))
            .expect("asset");
        let spec: GroupSpec = "CAM 3=a.mp4,a.mp4".parse().unwrap();
        let err = add_group_parts(
            &mut p,
            &spec,
            &["/media/a.mp4".into(), "/media/a.mp4".into()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("listed twice"), "{err}");
        assert!(p.groups.is_empty(), "nothing was added");
    }

    #[test]
    fn group_parts_refuse_an_unknown_duration() {
        let mut p = Project::new();
        p.add_asset("/media/a.mp4".into(), fake_probe("2"))
            .expect("asset");
        let mut nodur = fake_probe("2");
        nodur.format.duration = None;
        p.add_asset("/media/b.webm".into(), nodur).expect("asset");
        let spec: GroupSpec = "CAM 3=a.mp4,b.webm".parse().unwrap();
        let err = add_group_parts(
            &mut p,
            &spec,
            &["/media/a.mp4".into(), "/media/b.webm".into()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("no known duration"), "{err}");
        assert!(p.groups.is_empty(), "nothing was added");
    }

    #[test]
    fn resolve_group_part_tries_the_path_then_the_media_dir_then_the_pool() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("local.mp4"), b"").unwrap();
        let mut p = Project::new();
        p.add_asset("/archive/pooled.mp4".into(), fake_probe("2"))
            .expect("asset");
        p.add_asset("/archive/moved.mp4".into(), fake_probe("2"))
            .expect("asset");

        let absolute = dir.path().join("local.mp4");
        assert_eq!(
            resolve_group_part(&p, &absolute, dir.path()).unwrap(),
            absolute.canonicalize().unwrap(),
            "as given"
        );
        assert_eq!(
            resolve_group_part(&p, Path::new("local.mp4"), dir.path()).unwrap(),
            absolute.canonicalize().unwrap(),
            "relative to the media dir"
        );
        assert_eq!(
            resolve_group_part(&p, Path::new("pooled.mp4"), dir.path()).unwrap(),
            PathBuf::from("/archive/pooled.mp4"),
            "a unique pooled name"
        );
    }

    #[test]
    fn resolve_group_part_refuses_a_missing_or_ambiguous_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Project::new();
        p.add_asset("/one/cam.mp4".into(), fake_probe("2"))
            .expect("asset");
        p.add_asset("/two/cam.mp4".into(), fake_probe("2"))
            .expect("asset");

        let err = resolve_group_part(&p, Path::new("cam.mp4"), dir.path()).unwrap_err();
        assert!(err.to_string().contains("matches 2 pooled files"), "{err}");
        let err = resolve_group_part(&p, Path::new("nope.mp4"), dir.path()).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[test]
    fn expand_tilde_uses_home_for_the_quoted_flag_form() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        assert_eq!(expand_tilde(Path::new("~/a.mp4")), home.join("a.mp4"));
    }

    #[cfg(unix)]
    #[test]
    fn validate_import_paths_refuses_only_the_non_utf8_member() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let good = PathBuf::from("/media/good.mp4");
        assert!(validate_import_path(&good).is_ok());
        assert!(validate_import_paths(std::slice::from_ref(&good)).is_ok());

        let bad = PathBuf::from(OsStr::from_bytes(b"/media/bad-\xff.mp4"));
        let err = validate_import_path(&bad).unwrap_err().to_string();
        assert!(err.contains("UTF-8"), "{err}");
        assert!(err.contains(bad.to_string_lossy().as_ref()), "{err}");

        let err = validate_import_paths(&[good, bad.clone()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("UTF-8"), "{err}");
        assert!(err.contains(bad.to_string_lossy().as_ref()), "{err}");
    }

    #[cfg(feature = "tui")]
    #[test]
    fn parse_group_specs_rejects_a_bad_tui_flag() {
        let err = parse_group_specs(&["CAM 3".to_string()]).unwrap_err();
        assert!(err.to_string().contains("NAME=FILE"), "{err}");
    }
}
