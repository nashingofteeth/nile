//! The CLI's command implementations, split by what they touch. `main.rs`
//! keeps the clap surface and the dispatch; every command body lives here so
//! the TUI can call the same work functions.

use std::path::Path;

use anyhow::Result;

use nle_core::action::{EffectArgs, ExpectArgs, effect_from_args, expect_from_args};
use nle_core::timeline::TrackKind;
use nle_core::{
    Action, Asset, DEFAULT_TRANSITION_FRAMES, Edge, GapLen, Target, TransitionKind, TransitionLen,
    format_seconds, frame_of, parse_signed_time, secs_f64,
};

use crate::session::{Session, playhead};
use crate::{
    AnnotationCmd, Cmd, MediaCmd, TrackCmd, TrackKindArg, edit_message, log, parse_speed_arg, say,
    say_err,
};

pub(crate) mod analysis;
pub(crate) mod check;
pub(crate) mod detect;
pub(crate) mod export;
pub(crate) mod import;
pub(crate) mod job;
pub(crate) mod media;
pub(crate) mod project;
pub(crate) mod schedule;

/// A CLI time: seconds or timecode, optionally negative (an extend target
/// can reach back past time 0).
pub(crate) fn cli_time(s: &str) -> Result<f64, String> {
    parse_signed_time(s)
        .map(|t| *t.numer() as f64 / *t.denom() as f64)
        .map_err(|e| format!("invalid time {s:?} ({e})"))
}

/// A 1-based CLI index (`--track N`, `nile track rm N`) → 0-based model
/// index. Track numbers are 1-based everywhere, matching `v1`/`a1`.
pub(crate) fn cli_index(n: usize, what: &str) -> Result<usize, String> {
    if n >= 1 {
        Ok(n - 1)
    } else {
        Err(format!("{what} is 1-based, got {n}"))
    }
}

/// `--at`/`--clip`/`--seg`: exactly one way to address a segment.
pub(crate) fn cli_target(
    at: &Option<String>,
    clip: Option<u32>,
    seg: Option<usize>,
) -> Result<Target, String> {
    match (at, clip, seg) {
        (Some(_), None, None) => cli_time(at.as_deref().unwrap_or_default()).map(Target::At),
        (None, Some(c), None) => Ok(Target::Clip(c)),
        (None, None, Some(s)) => Ok(Target::Segment(s)),
        (None, None, None) => Err("pass --at T, --clip N, or --seg N".to_string()),
        _ => Err("use only one of --at/--clip/--seg".to_string()),
    }
}

/// `--edge in|out` (head/tail aliases accepted).
pub(crate) fn cli_edge(s: &str) -> Result<Edge, String> {
    match s {
        "in" | "head" => Ok(Edge::In),
        "out" | "tail" => Ok(Edge::Out),
        other => Err(format!("--edge expects in|out, got {other:?}")),
    }
}

/// `--from-seg`/`--to-seg`: both or neither, and the range is inclusive.
pub(crate) fn cli_seg_range(
    from: Option<usize>,
    to: Option<usize>,
    what: &str,
) -> Result<Option<(usize, usize)>, String> {
    match (from, to) {
        (None, None) => Ok(None),
        (Some(a), Some(b)) if a <= b => Ok(Some((a, b))),
        (Some(_), Some(_)) => Err(format!(
            "{what}: --from-seg must not be greater than --to-seg"
        )),
        _ => Err(format!(
            "{what}: pass both --from-seg and --to-seg for a range"
        )),
    }
}

/// The edit-shaped CLI commands as catalog [`Action`]s — the single
/// mapping behind both `main`'s dispatch and the CLI-parity test, so a
/// command can't exist without its action. `None` means "not an action"
/// (probe, export, imports, queries); `Some(Err)` is an argument problem.
///
/// `nile add` is the one exception: it is a pool write + paste in one
/// step, so it builds its `Paste` action after probing.
pub(crate) fn to_action(cmd: &Cmd) -> Option<Result<Action, String>> {
    match cmd {
        Cmd::TrimIn { at, track, ripple } => Some(cli_index(*track, "--track").and_then(|track| {
            cli_time(at).map(|at| Action::TrimIn {
                track,
                at,
                ripple: *ripple,
            })
        })),
        Cmd::TrimOut { at, track, ripple } => {
            Some(cli_index(*track, "--track").and_then(|track| {
                cli_time(at).map(|at| Action::TrimOut {
                    track,
                    at,
                    ripple: *ripple,
                })
            }))
        }
        Cmd::Split { at, track } => Some(
            cli_index(*track, "--track")
                .and_then(|track| cli_time(at).map(|at| Action::Split { track, at })),
        ),
        Cmd::Gap {
            at,
            frames,
            dur,
            track,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            let at = cli_time(at)?;
            let len = match (frames, dur) {
                (Some(_), Some(_)) => return Err("use either --frames or --dur".to_string()),
                (Some(n), None) => GapLen::Frames(*n),
                (None, Some(d)) => cli_time(d).map(GapLen::Seconds)?,
                (None, None) => GapLen::Frames(1),
            };
            match len {
                GapLen::Frames(n) if n < 1 => {
                    return Err("a gap needs at least one frame".to_string());
                }
                GapLen::Seconds(s) if s <= 0.0 => {
                    return Err("a gap needs a positive length".to_string());
                }
                _ => {}
            }
            Ok(Action::InsertGap { track, at, len })
        })),
        Cmd::Delete {
            at,
            clip,
            seg,
            from_seg,
            to_seg,
            track,
            ripple,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            match cli_seg_range(*from_seg, *to_seg, "delete")? {
                Some((start, end)) => {
                    if at.is_some() || clip.is_some() || seg.is_some() {
                        return Err(
                            "delete: use --from-seg/--to-seg or --at/--clip/--seg, not both"
                                .to_string(),
                        );
                    }
                    if *ripple {
                        return Err("delete --from-seg/--to-seg is already a ripple".to_string());
                    }
                    Ok(Action::DeleteRange { track, start, end })
                }
                None => cli_target(at, *clip, *seg).map(|target| Action::Delete {
                    track,
                    target,
                    ripple: *ripple,
                }),
            }
        })),
        Cmd::Yank {
            at,
            clip,
            seg,
            from_seg,
            to_seg,
            track,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            match cli_seg_range(*from_seg, *to_seg, "yank")? {
                Some((start, end)) => {
                    if at.is_some() || clip.is_some() || seg.is_some() {
                        return Err(
                            "yank: use --from-seg/--to-seg or --at/--clip/--seg, not both"
                                .to_string(),
                        );
                    }
                    Ok(Action::YankRange { track, start, end })
                }
                None => cli_target(at, *clip, *seg).map(|target| Action::Yank { track, target }),
            }
        })),
        Cmd::Paste {
            at,
            track,
            ripple,
            range,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            cli_time(at).and_then(|at| {
                if *range {
                    if *ripple {
                        return Err("paste --range is already ripple".to_string());
                    }
                    Ok(Action::PasteRange { track, at })
                } else {
                    Ok(Action::Paste {
                        track,
                        at,
                        ripple: *ripple,
                        segment: None,
                    })
                }
            })
        })),
        Cmd::Extend {
            clip,
            seg,
            at,
            edge,
            to,
            track,
            ripple,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            let target = cli_target(at, *clip, *seg)?;
            let edge = cli_edge(edge)?;
            let to = cli_time(to)?;
            Ok(Action::Extend {
                track,
                target,
                edge,
                to,
                ripple: *ripple,
            })
        })),
        Cmd::Remove { id, ripple } => Some(Ok(Action::Delete {
            track: 0,
            target: Target::Clip(*id),
            ripple: *ripple,
        })),
        Cmd::Speed { id, rate, track } => Some(cli_index(*track, "--track").and_then(|track| {
            parse_speed_arg(rate)
                .map_err(|e| e.to_string())
                .map(|rate| Action::SetSpeed {
                    track,
                    clip: *id,
                    rate,
                })
        })),
        Cmd::PurgeStumps { track } => Some(
            track
                .map(|n| cli_index(n, "--track"))
                .transpose()
                .map(|track| Action::PurgeStumps { track }),
        ),
        Cmd::Transition {
            kind,
            at,
            frames,
            dur,
            percent,
            track,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            let at = cli_time(at)?;
            let kind = match kind.as_deref() {
                None | Some("dissolve") => TransitionKind::Dissolve,
                Some("fade") => TransitionKind::FadeBlack,
                Some(other) => {
                    return Err(format!("unknown kind {other:?} — use dissolve or fade"));
                }
            };
            let len = match (frames, dur, percent) {
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
                    return Err("use only one of --frames, --dur, or --percent".to_string());
                }
                (Some(n), None, None) => TransitionLen::Frames(*n),
                (None, Some(d), None) => TransitionLen::Seconds(cli_time(d)?),
                (None, None, Some(p)) => {
                    if !p.is_finite() || *p <= 0.0 {
                        return Err(format!("transition percent must be positive (got {p})"));
                    }
                    TransitionLen::Percent(*p)
                }
                (None, None, None) => TransitionLen::Frames(DEFAULT_TRANSITION_FRAMES),
            };
            Ok(Action::Transition {
                track,
                at,
                kind,
                len,
            })
        })),
        Cmd::Effect {
            kind,
            clip,
            track,
            index,
            at,
            w,
            h,
            x,
            y,
            brightness,
            contrast,
            saturation,
            path,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            match kind.as_str() {
                "rm" => Ok(Action::RemoveEffect {
                    track,
                    clip: *clip,
                    index: *index,
                }),
                "clear" => Ok(Action::ClearEffects { track, clip: *clip }),
                _ => effect_from_args(
                    kind,
                    EffectArgs {
                        w: *w,
                        h: *h,
                        x: *x,
                        y: *y,
                        brightness: *brightness,
                        contrast: *contrast,
                        saturation: *saturation,
                        path: path.clone(),
                    },
                )
                .map_err(|e| e.to_string())
                .and_then(|effect| {
                    if let Some(at) = at {
                        let nle_core::timeline::Effect::Crop { x, y, w, h, .. } = effect else {
                            return Err(
                                "--at adds a crop key; other effects have no keyframes".to_string()
                            );
                        };
                        return Ok(Action::CropKey {
                            track,
                            clip: *clip,
                            at: cli_time(at)?,
                            x,
                            y,
                            w,
                            h,
                        });
                    }
                    Ok(Action::AddEffect {
                        track,
                        clip: *clip,
                        effect,
                        index: *index,
                    })
                }),
            }
        })),
        Cmd::Undo {} => Some(Ok(Action::Undo)),
        Cmd::Redo {} => Some(Ok(Action::Redo)),
        Cmd::Track { action } => match action {
            TrackCmd::Ls => None,
            TrackCmd::Add { kind } => Some(Ok(Action::TrackAdd {
                kind: match kind {
                    TrackKindArg::Video => TrackKind::Video,
                    TrackKindArg::Audio => TrackKind::Audio,
                },
            })),
            TrackCmd::Mute { index } => Some(
                index
                    .map(|n| cli_index(n, "track index"))
                    .transpose()
                    .map(|track| Action::TrackMute {
                        track: track.unwrap_or(0),
                    }),
            ),
            TrackCmd::Rm { index } => {
                Some(cli_index(*index, "track index").map(|track| Action::TrackRm { track }))
            }
        },
        Cmd::Media { action } => match action {
            MediaCmd::Ls => None,
            MediaCmd::Rm { id } => Some(Ok(Action::AssetRm { id: *id })),
        },
        Cmd::Annotation { action } => match action {
            AnnotationCmd::Ls { .. } | AnnotationCmd::Check { .. } => None,
            AnnotationCmd::Add {
                text,
                asset,
                group,
                time,
                timeline,
                at,
                to,
                track,
                expect,
                tolerance_frames,
                max_move,
                no_hold_size,
                margin,
                min_coverage,
                transcript,
                black,
                min_frames,
            } => Some((|| {
                // A check flag is meaningless without the check, so name it
                // instead of letting clap accept it and quietly ignore it.
                if expect.is_none() {
                    let stray = [
                        (tolerance_frames.is_some(), "--tolerance-frames"),
                        (max_move.is_some(), "--max-move"),
                        (*no_hold_size, "--no-hold-size"),
                        (margin.is_some(), "--margin"),
                        (min_coverage.is_some(), "--min-coverage"),
                        (transcript.is_some(), "--transcript"),
                        (*black, "--black"),
                        (min_frames.is_some(), "--min-frames"),
                    ]
                    .into_iter()
                    .find_map(|(given, name)| given.then_some(name));
                    if let Some(name) = stray {
                        return Err(format!("{name} needs --expect"));
                    }
                }
                if text.is_some() && expect.is_some() {
                    return Err("use TEXT or --expect, not both".to_string());
                }
                let expect = expect
                    .map(|check| {
                        expect_from_args(ExpectArgs {
                            check: check.name().to_string(),
                            asset: *asset,
                            group: *group,
                            tolerance_frames: *tolerance_frames,
                            max_move_pct_per_sec: *max_move,
                            no_hold_size: *no_hold_size,
                            margin_pct: *margin,
                            min_coverage: *min_coverage,
                            transcript: transcript.clone(),
                            black: *black,
                            min_frames: *min_frames,
                        })
                        .map_err(|e| e.to_string())
                    })
                    .transpose()?;
                if expect.is_some() && !*timeline {
                    return Err("--expect needs --timeline (a render span)".to_string());
                }
                if let Some(expect) = expect {
                    let at = at
                        .as_deref()
                        .ok_or("annotation add --timeline needs --at TIME")?;
                    return Ok(Action::AnnotationAddAt {
                        track: cli_index(*track, "--track")?,
                        at: cli_time(at)?,
                        to: to.as_deref().map(cli_time).transpose()?,
                        kind: nle_core::AnnotationKind::Expectation { expect },
                    });
                }
                let text = text
                    .as_deref()
                    .ok_or("annotation add needs TEXT (or --expect CHECK)")?;
                if *timeline {
                    // A note is anchored to the timeline; `--asset`/`--group`
                    // select a check's subject, so carrying them would
                    // silently ignore the flag.
                    if asset.is_some() || group.is_some() {
                        return Err("annotation add --timeline cannot carry --asset/--group — \
                             drop them, or use --expect for a check"
                            .to_string());
                    }
                    let at = at
                        .as_deref()
                        .ok_or("annotation add --timeline needs --at TIME")?;
                    return Ok(Action::AnnotationAddAt {
                        track: cli_index(*track, "--track")?,
                        at: cli_time(at)?,
                        to: to.as_deref().map(cli_time).transpose()?,
                        kind: nle_core::AnnotationKind::Note {
                            text: text.to_string(),
                        },
                    });
                }
                let time = time
                    .as_deref()
                    .ok_or("annotation add needs --time T (source time)")?;
                let time = parse_signed_time(time)
                    .map_err(|e| format!("invalid --time {time:?} ({e})"))?;
                let source = match (asset, group) {
                    (Some(id), None) => nle_core::SourceRef::Asset { id: *id },
                    (None, Some(id)) => nle_core::SourceRef::Group { id: *id },
                    _ => {
                        return Err(
                            "annotation add needs exactly one of --asset or --group".to_string()
                        );
                    }
                };
                Ok(Action::AnnotationAdd {
                    anchor: nle_core::Anchor::Source { source, time },
                    kind: nle_core::AnnotationKind::Note {
                        text: text.to_string(),
                    },
                })
            })()),
            AnnotationCmd::Rm { id } => Some(Ok(Action::AnnotationRemove { id: *id })),
            AnnotationCmd::Resolve { id } => Some(Ok(Action::AnnotationResolve { id: *id })),
        },
        _ => None,
    }
}

/// Run one catalog action against the project file: load, apply through
/// the undo history, persist project + sidecar + register. A CLI edit is
/// undoable (`nile undo`) exactly like a TUI edit — same executor, same
/// history.
pub(crate) fn run_cli_action(project_path: &Path, action: &Action) -> Result<()> {
    let mut session = Session::load(project_path)?;
    // The edit is "made from" its own position, so undoing it returns
    // there (the TUI passes the playhead instead — its session state).
    let from = match action {
        Action::TrimIn { at, .. }
        | Action::TrimOut { at, .. }
        | Action::Split { at, .. }
        | Action::InsertGap { at, .. }
        | Action::Paste { at, .. }
        | Action::PasteRange { at, .. } => *at,
        Action::Delete { target, .. } | Action::Yank { target, .. } => match target {
            Target::At(t) => *t,
            _ => 0.0,
        },
        _ => 0.0,
    };
    let outcome = session
        .apply_action(action, from)
        .map_err(|e| anyhow::Error::msg(edit_message(&e)))?;
    // Store where the playhead landed, so the next command and the TUI open on
    // it — a length-changing edit must not leave the cursor past the new end.
    // A one-shot command has no active track, so there is no media anchor to
    // re-find: the policy clamps instead (see `playhead::land`).
    let current = secs_f64(&session.project().playhead);
    if let Some(at) = playhead::land(session.project(), current, 0, None, action, &outcome) {
        session.set_playhead(at);
    }
    session.persist()?;
    log::line(&format!("action {}", outcome.desc));
    let landed = frame_of(
        session.project().fps.as_ref(),
        secs_f64(&session.project().playhead),
    );
    match outcome.playhead {
        Some(_) => say!("✓ {} (playhead f{landed})", outcome.desc),
        None => say!("✓ {}", outcome.desc),
    }
    Ok(())
}

/// What a work function produced: warnings for stderr (or the log) and
/// the summary line the caller shows. Keeps the TUI free of stray prints
/// — a `say!` would draw over the terminal UI.
pub(crate) struct CommandReport {
    pub(crate) warnings: Vec<String>,
    pub(crate) summary: String,
}

impl CommandReport {
    pub(crate) fn new(summary: String) -> Self {
        Self {
            warnings: Vec::new(),
            summary,
        }
    }

    pub(crate) fn warn(&mut self, msg: String) {
        self.warnings.push(msg);
    }

    /// The CLI's rendering: warnings on stderr, summary on stdout.
    pub(crate) fn print(&self) {
        for w in &self.warnings {
            say_err!("{w}");
        }
        say!("{}", self.summary);
    }
}

pub(crate) fn asset_summary(a: &Asset) -> String {
    let mut s = match a.duration() {
        Some(d) => format!("{} s", format_seconds(&d)),
        None => "? s".to_string(),
    };
    if let Some((w, h)) = a.video_size() {
        s += &format!(" · {w}×{h}");
    }
    if let Some(fps) = a.fps() {
        s += &format!(" @ {}/{}", fps.numer(), fps.denom());
    }
    s += if a.probe.has_audio() {
        " · audio ✓"
    } else {
        " · no audio"
    };
    s
}
