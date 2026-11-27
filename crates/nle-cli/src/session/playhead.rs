//! Media-frame playhead anchoring, shared by the TUI and `nile serve`.
//!
//! An edit can move material under the playhead. This module keeps the same
//! source frame where it can, falls back to the seam when a clip disappears,
//! and clamps the result into the new timeline. The front-ends only decide
//! which track is active and when to apply the policy; the rule itself lives
//! here so the TUI and the protocol server cannot drift.

use nle_core::timeline::{Segment, Track};
use nle_core::{Action, Outcome, Project, Seconds, frame_to_secs, secs_f64};

/// The clip and source frame under the playhead — the media anchor an edit
/// should preserve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaAnchor {
    pub id: u32,
    pub source: Seconds,
    /// The clip's timeline start before the edit — where a ripple delete
    /// leaves the seam between its neighbours.
    pub start: f64,
    /// The playhead before the edit — the offset a plain delete keeps inside
    /// the gap it leaves.
    pub at: f64,
}

/// A position anchor for a gap or transition — neither is media, so the
/// playhead cannot be re-found by frame. It stays put while the segment
/// survives; a removal leaves the seam (`start`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GapAnchor {
    /// The seam a removal leaves: a gap's start, a transition's end.
    pub start: f64,
    /// The playhead before the edit.
    pub at: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Anchor {
    Media(MediaAnchor),
    Gap(GapAnchor),
}

/// The gap or transition whose span *ends* exactly at `t`.
fn gap_ending_at(track: &Track, t: Seconds) -> Option<usize> {
    let mut acc = Seconds::from_integer(0);
    for (i, s) in track.segments.iter().enumerate() {
        acc += s.duration();
        if acc == t && matches!(s, Segment::Gap(_) | Segment::Transition(_)) {
            return Some(i);
        }
    }
    None
}

/// Anchor the playhead's position before an edit: the clip + source frame
/// over a clip, or the gap/transition seam over other segments.
pub fn edit_anchor(project: &Project, playhead: f64, ti: usize) -> Option<Anchor> {
    let track = project.timeline.track(ti)?;
    let t = nle_core::grid_time(project.fps.as_ref(), playhead);
    let idx = gap_ending_at(track, t).or_else(|| track.index_at(t))?;
    let at = secs_f64(&t);
    match &track.segments[idx] {
        Segment::Clip(c) => Some(Anchor::Media(MediaAnchor {
            id: c.id,
            // Timeline → source scales by the clip's speed.
            source: c.source_at(t - track.start_of(idx)),
            start: secs_f64(&track.start_of(idx)),
            at,
        })),
        Segment::Gap(_) => Some(Anchor::Gap(GapAnchor {
            start: secs_f64(&track.start_of(idx)),
            at,
        })),
        // A transition is removed by handing its trimmed spans back, so the
        // neighbours meet at its *end*.
        Segment::Transition(tr) => Some(Anchor::Gap(GapAnchor {
            start: secs_f64(&(track.start_of(idx) + tr.dur)),
            at,
        })),
    }
}

/// The timeline position of `source` in clip `id`, clamped to the clip's
/// current extent. `None` when the clip is gone.
fn resolve_anchor(project: &Project, id: u32, source: Seconds, ti: usize) -> Option<f64> {
    let track = project.timeline.track(ti)?;
    let mut acc = Seconds::from_integer(0);
    for seg in &track.segments {
        if let Segment::Clip(c) = seg
            && c.id == id
        {
            let pos = if source < c.asset_in {
                acc
            } else if source >= c.asset_out {
                acc + c.duration()
            } else {
                acc + c.timeline_offset_of(source)
            };
            return Some(secs_f64(&pos));
        }
        acc += seg.duration();
    }
    None
}

/// Where the playhead lands when the anchored clip is gone. A plain delete
/// replaces it with a gap of the same length, so keep the same offset; a
/// ripple delete removes it and closes the gap, so land on the seam.
fn fallback_position(project: &Project, a: &MediaAnchor, ti: usize) -> f64 {
    if let Some(track) = project.timeline.track(ti)
        && let Some(idx) = track.index_at(nle_core::grid_time(project.fps.as_ref(), a.at))
        && matches!(track.segments[idx], Segment::Gap(_))
    {
        return a.at;
    }
    a.start
}

/// Apply an anchor to the post-edit project, returning the playhead position
/// clamped into the current timeline. `playhead` is used when there is no
/// anchor.
pub fn reanchor(project: &Project, playhead: f64, ti: usize, anchor: Option<Anchor>) -> f64 {
    let Some(anchor) = anchor else {
        return playhead;
    };
    let pos = match anchor {
        Anchor::Media(a) => resolve_anchor(project, a.id, a.source, ti)
            .unwrap_or_else(|| fallback_position(project, &a, ti)),
        Anchor::Gap(g) => {
            let idx = project.timeline.track(ti).and_then(|track| {
                let t = nle_core::grid_time(project.fps.as_ref(), g.at);
                gap_ending_at(track, t).or_else(|| track.index_at(t))
            });
            match idx {
                Some(i) if !matches!(project.timeline.tracks[ti].segments[i], Segment::Clip(_)) => {
                    g.at
                }
                _ => g.start,
            }
        }
    };
    pos.clamp(0.0, secs_f64(&project.last_frame()))
}

/// Where the playhead lands after one action — the single policy the TUI,
/// `nile serve` and the CLI all apply. `None` means "leave it alone": a no-op
/// edit must not move it.
///
/// `current` and `anchor` describe the pre-edit state; `ti` is the track the
/// edit ran on. The result is clamped into the post-edit timeline, so a caller
/// may persist it directly.
pub fn land(
    project: &Project,
    current: f64,
    ti: usize,
    anchor: Option<Anchor>,
    action: &Action,
    outcome: &Outcome,
) -> Option<f64> {
    if let Some(frame) = outcome.playhead {
        let at = frame_to_secs(project.fps.as_ref(), frame);
        return Some(at.clamp(0.0, secs_f64(&project.last_frame())));
    }
    if !outcome.changed {
        return None;
    }
    // A track or pool change doesn't move the active track's media, so leave
    // the playhead where it is (just clamped).
    if reanchors(action) {
        Some(reanchor(project, current, ti, anchor))
    } else {
        Some(current.clamp(0.0, secs_f64(&project.last_frame())))
    }
}

/// Does this action move the active track's media? If not, the playhead stays
/// where it is — an anchor taken against the old active track would otherwise
/// resolve against the wrong one (`track rm` re-indexes).
pub fn reanchors(action: &Action) -> bool {
    !matches!(
        action,
        Action::TrackAdd { .. }
            | Action::TrackMute { .. }
            | Action::TrackRm { .. }
            | Action::AssetRm { .. }
            | Action::AnnotationAdd { .. }
            | Action::AnnotationAddAt { .. }
            | Action::AnnotationRemove { .. }
            | Action::AnnotationResolve { .. }
            | Action::CropKey { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_core::Target;
    use nle_core::annotation::{Anchor as AnnotationAnchor, AnnotationKind, SourceRef};
    use nle_core::timeline::{Gap, Timeline, TrackKind};

    /// The negative list in `reanchors` is the contract, so every excluded
    /// variant must classify as "leave it put". `TrackRm` is the
    /// discriminating case: it re-indexes the track list, so an anchor taken
    /// against the old active track would resolve against the wrong one.
    /// The positive control keeps the test from passing if `reanchors` were
    /// reduced to `false`.
    #[test]
    fn non_media_actions_do_not_reanchor() {
        let excluded = [
            Action::TrackAdd {
                kind: TrackKind::Video,
            },
            Action::TrackMute { track: 0 },
            Action::TrackRm { track: 1 },
            Action::AssetRm { id: 1 },
            Action::AnnotationAdd {
                anchor: AnnotationAnchor::Source {
                    source: SourceRef::Asset { id: 1 },
                    time: Seconds::from_integer(0),
                },
                kind: AnnotationKind::Marker,
            },
            Action::AnnotationAddAt {
                track: 0,
                at: 1.0,
                to: None,
                kind: AnnotationKind::Marker,
            },
            Action::AnnotationRemove { id: 1 },
            Action::AnnotationResolve { id: 1 },
            Action::CropKey {
                track: 0,
                clip: 1,
                at: 1.0,
                x: 0.0,
                y: 0.0,
                w: 50.0,
                h: 50.0,
            },
        ];
        for action in &excluded {
            assert!(!reanchors(action), "{action:?} leaves the playhead put");
        }
        // A media edit still takes the anchor branch.
        assert!(reanchors(&Action::Delete {
            track: 0,
            target: Target::At(4.0),
            ripple: true,
        }));
    }

    fn shortened_project() -> Project {
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        p.timeline = Timeline {
            tracks: vec![Track {
                kind: TrackKind::Video,
                muted: false,
                segments: vec![Segment::Gap(Gap {
                    duration: Seconds::from_integer(2),
                })],
            }],
        };
        p
    }

    /// `land`'s own clamp: a recorded undo/redo frame past the shortened
    /// timeline's final frame lands on `last_frame`, never `end`. The
    /// end-to-end counterpart is
    /// `tui::tests::playhead::redo_restores_the_clamped_pre_edit_position`,
    /// which also sees `persist_playhead`'s `set_playhead` clamp.
    #[test]
    fn a_restored_frame_past_the_end_clamps_to_last_frame() {
        let project = shortened_project(); // 2 s
        let last = secs_f64(&project.last_frame());
        assert!(last < 6.0, "the recorded frame is past the end");
        let landed = land(
            &project,
            6.0,
            0,
            None,
            &Action::Undo,
            &Outcome {
                desc: "undo".into(),
                changed: true,
                playhead: Some(180), // 6 s at 30 fps
            },
        );
        assert_eq!(landed, Some(last), "clamped to the final frame");
    }
}
