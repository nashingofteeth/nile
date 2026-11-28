//! Deleting, splitting, pasting and gap insertion — the operations that
//! change a track's shape rather than its edges.
//!
//! A track is an append-only sequence, so every one of these is expressed as
//! Insert/Remove/Replace ops on its segment list; the ``_on`` forms take a
//! track index and the unsuffixed ones wrap the first video track.
mod delete;

pub use delete::{
    delete_clip_by_id, delete_clip_by_id_on, delete_clip_ripple_by_id, delete_clip_ripple_by_id_on,
    delete_ripple, delete_ripple_on, delete_segment, delete_segment_on, delete_under,
    delete_under_on, trim_consume_target, trim_consume_target_on,
};

use crate::project::Project;
use crate::time::{Seconds, checked_add, checked_frames_to_seconds, checked_mul, checked_sub};
use crate::timeline::{
    Clip, Gap, MAX_TIMELINE_SECONDS, Segment, Track, Transition, TransitionSource,
};

use super::extend::extend_gap_ending_at;
use super::util::{
    at_least_a_frame, clip_at, merge_pair, replace_with_many, secs_f64, segment_note, snap,
    source_span, split_sides, stamp, track_at, video_track_index,
};
use super::{Batch, EditError, EditOp};

pub fn split(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    split_on(project, video_track_index(project)?, playhead)
}

pub fn paste(project: &Project, snapshot: &Segment, playhead: f64) -> Result<Batch, EditError> {
    paste_on(project, video_track_index(project)?, snapshot, playhead)
}

pub fn insert_gap(project: &Project, playhead: f64, frames: i64) -> Result<Batch, EditError> {
    insert_gap_on(project, video_track_index(project)?, playhead, frames)
}

pub fn paste_ripple(
    project: &Project,
    snapshot: &Segment,
    playhead: f64,
) -> Result<Batch, EditError> {
    paste_ripple_on(project, video_track_index(project)?, snapshot, playhead)
}

/// `s` — split the clip under the playhead into two clips.
pub fn split_on(project: &Project, track: usize, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let (ti, si, c) = clip_at(project, track, &t)?;
    let seg_start = track_at(project, ti)?.start_of(si);
    let left_len = checked_sub(&t, &seg_start).ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
    let clip_dur = c
        .checked_duration()
        .ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
    if left_len <= Seconds::from_integer(0) || left_len >= clip_dur {
        return Err(EditError::NothingToTrim(stamp(project, &t)));
    }
    at_least_a_frame(project, &left_len)?;
    let right_len =
        checked_sub(&clip_dur, &left_len).ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
    at_least_a_frame(project, &right_len)?;

    let mut left = Clip {
        id: c.id,
        asset: c.asset,
        asset_in: c.asset_in,
        asset_out: c.asset_in + source_span(&c, left_len),
        speed: c.speed,
        effects: c.effects.clone(),
    };
    left.truncate_crop_keys(left_len, clip_dur);
    let mut right = Clip {
        id: project.next_clip_id()?,
        asset: c.asset,
        asset_in: c.asset_in + source_span(&c, t - seg_start),
        asset_out: c.asset_out,
        speed: c.speed,
        effects: c.effects.clone(),
    };
    right.rebase_crop_keys(left_len, clip_dur);
    let left = Segment::Clip(left);
    let right = Segment::Clip(right);
    Ok(Batch {
        ops: replace_with_many(ti, si, &Segment::Clip(c.clone()), vec![left, right]),
        desc: format!("split clip #{} at {}", c.id, stamp(project, &t)),
    })
}

/// The ops appending `payload` at/past the track end: the void between the
/// track's end and the playhead becomes a gap — coalesced with the payload when
/// it is itself a gap, and absorbed by the trailing gap when the track already
/// ends in one, so the track never holds two adjacent gap segments.
///
/// Takes the track rather than its length: the caller cannot pass a length that
/// disagrees with the segments the void is merged into.
pub(super) fn append_at_end(
    ti: usize,
    track: &Track,
    void: Seconds,
    payload: Segment,
) -> Result<Vec<EditOp>, EditError> {
    let len = track.segments.len();
    if let (Some(have), Some(last)) = (
        match track.segments.last() {
            Some(Segment::Gap(g)) => Some(g.duration),
            _ => None,
        },
        track.segments.last(),
    ) {
        // A trailing gap already covers the track's end: grow it by the void
        // and let a gap payload merge into it, instead of inserting a second
        // gap beside it.
        let zero = Seconds::from_integer(0);
        let span = if void > zero { void } else { zero };
        let grown = checked_add(&have, &span).ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
        return Ok(match payload {
            Segment::Gap(pg) => {
                let total = checked_add(&grown, &pg.duration)
                    .ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
                vec![EditOp::Replace {
                    track: ti,
                    index: len - 1,
                    before: last.clone(),
                    after: Segment::Gap(Gap { duration: total }),
                }]
            }
            payload if span > zero => vec![
                EditOp::Replace {
                    track: ti,
                    index: len - 1,
                    before: last.clone(),
                    after: Segment::Gap(Gap { duration: grown }),
                },
                EditOp::Insert {
                    track: ti,
                    index: len,
                    segment: payload,
                },
            ],
            payload => vec![EditOp::Insert {
                track: ti,
                index: len,
                segment: payload,
            }],
        });
    }
    if void <= Seconds::from_integer(0) {
        return Ok(vec![EditOp::Insert {
            track: ti,
            index: len,
            segment: payload,
        }]);
    }
    Ok(match payload {
        Segment::Gap(pg) => {
            let total =
                checked_add(&void, &pg.duration).ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
            vec![EditOp::Insert {
                track: ti,
                index: len,
                segment: Segment::Gap(Gap { duration: total }),
            }]
        }
        payload => vec![
            EditOp::Insert {
                track: ti,
                index: len,
                segment: Segment::Gap(Gap { duration: void }),
            },
            EditOp::Insert {
                track: ti,
                index: len + 1,
                segment: payload,
            },
        ],
    })
}

/// The pasted piece for a clipboard segment: a clip takes a fresh id, a
/// gap just carries its duration.
pub(super) fn pasted_piece(snapshot: &Segment, new_id: u32) -> Segment {
    match snapshot {
        Segment::Clip(c) => Segment::Clip(Clip {
            id: new_id,
            asset: c.asset,
            asset_in: c.asset_in,
            asset_out: c.asset_out,
            speed: c.speed,
            effects: c.effects.clone(),
        }),
        Segment::Gap(g) => Segment::Gap(Gap {
            duration: g.duration,
        }),
        Segment::Transition(t) => Segment::Transition(Transition {
            id: new_id,
            ..t.clone()
        }),
    }
}

/// A lazily seeded cursor for a paste's fresh clip/transition ids.
///
/// A gap payload consumes no id, so the cursor stays unseeded and a pure-gap
/// paste works even when the clip id space is full. The first real request
/// seeds it from [`Project::next_clip_id`]; [`Project::take_clip_id`] then
/// enforces the reserved `u32::MAX`.
struct FreshIds<'a> {
    project: &'a Project,
    cursor: Option<Option<u32>>,
}

impl<'a> FreshIds<'a> {
    fn new(project: &'a Project) -> Self {
        Self {
            project,
            cursor: None,
        }
    }

    fn take(&mut self) -> Result<u32, EditError> {
        if self.cursor.is_none() {
            self.cursor = Some(Some(self.project.next_clip_id()?));
        }
        let cursor = self.cursor.as_mut().expect("the cursor was just seeded");
        Ok(Project::take_clip_id(cursor)?)
    }
}

/// Paste the snapshot, allocating only when its kind carries an id. Returns
/// the piece and the id it consumed (a gap consumes none).
fn pasted_with_id(
    ids: &mut FreshIds<'_>,
    snapshot: &Segment,
) -> Result<(Segment, Option<u32>), EditError> {
    match snapshot {
        Segment::Gap(_) => Ok((snapshot.clone(), None)),
        Segment::Clip(_) | Segment::Transition(_) => {
            let id = ids.take()?;
            Ok((pasted_piece(snapshot, id), Some(id)))
        }
    }
}

/// The `desc` label for a ripple paste, using the id actually spent.
fn inserted_label(snapshot: &Segment, payload_id: Option<u32>) -> String {
    match snapshot {
        Segment::Clip(_) => payload_id.map_or_else(
            || "clip inserted".to_string(),
            |id| format!("clip #{id} inserted"),
        ),
        Segment::Gap(_) => "gap inserted".to_string(),
        Segment::Transition(_) => "transition inserted".to_string(),
    }
}

/// `p` — replace-paste: drop the clipboard at the playhead, overwriting
/// whatever its span covers. Covered gaps vanish, wholly-covered clips
/// are removed, and a straddled follower is trimmed at its near edge;
/// past the track end the timeline simply grows. The pasted clip keeps
/// the yanked source range (splitting a clip hands the surviving right
/// half a fresh id). A yanked GAP pastes empty space of the same length.
pub fn paste_on(
    project: &Project,
    track: usize,
    snapshot: &Segment,
    playhead: f64,
) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = track;
    let track = track_at(project, ti)?;
    // A gap pasted at the end of a gap extends it (replace: the covered
    // span is overwritten, followers do not shift).
    if let Segment::Gap(pg) = snapshot
        && let Some(batch) = extend_gap_ending_at(project, ti, playhead, pg.duration, false)?
    {
        return Ok(batch);
    }
    let dur = snapshot.duration();
    let mut ids = FreshIds::new(project);
    let (pasted, _) = pasted_with_id(&mut ids, snapshot)?;

    // At/past the end (or an empty track): nothing to overwrite — append,
    // bridging the void with a gap when there's distance to cover.
    if track.segments.is_empty() || t >= track.end() {
        let void = checked_sub(&t, &track.end()).ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
        return Ok(Batch {
            ops: append_at_end(ti, track, void, pasted.clone())?,
            desc: format!("paste at {}", stamp(project, &t)),
        });
    }

    let si = track.index_at(t).ok_or(EditError::NoSegment)?;
    let seg = track.segments[si].clone();
    let left_len =
        checked_sub(&t, &track.start_of(si)).ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
    // A left remainder under one frame is treated as "at the edge" so it
    // can't strand a sub-frame sliver.
    let left_ok =
        left_len > Seconds::from_integer(0) && at_least_a_frame(project, &left_len).is_ok();
    let target = checked_add(&t, &dur).ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;

    // How far the overwrite reaches (`remove_upto`): the straddled
    // segment containing the paste's end, or the whole tail when the
    // paste runs off the track.
    let mut notes: Vec<String> = Vec::new();
    let straddle = if target < track.end() {
        Some((track.index_at(target).ok_or(EditError::NoSegment)?, target))
    } else {
        None
    };
    let remove_upto = straddle
        .map(|(ei, _)| ei + 1)
        .unwrap_or(track.segments.len());

    // Wholly-covered segments between the paste and the end boundary go
    // (the straddled segment is re-created as the right piece, so it has
    // its own note instead).
    let covered_upto = straddle.map(|(ei, _)| ei).unwrap_or(track.segments.len());
    for segment in track.segments.iter().take(covered_upto).skip(si + 1) {
        notes.push(format!("replaced {}", segment_note(segment)));
    }

    // What survives past the paste: the straddled segment's tail (none
    // when the paste ends exactly on a boundary).
    let right = match straddle {
        Some((ei, at)) => {
            let off = checked_sub(&at, &track.start_of(ei))
                .ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
            let rem = checked_sub(&track.segments[ei].duration(), &off)
                .ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
            if rem > Seconds::from_integer(0) && at_least_a_frame(project, &rem).is_ok() {
                match &track.segments[ei] {
                    // A transition is opaque: the paste consumes it whole.
                    Segment::Transition(_) => None,
                    Segment::Gap(_) => Some(Segment::Gap(Gap { duration: rem })),
                    Segment::Clip(c) => {
                        // A split keeps the head's id on the left, so the
                        // tail needs a fresh id only when the same clip
                        // supplied both pieces.
                        let id = if ei == si && left_ok {
                            ids.take()?
                        } else {
                            c.id
                        };
                        if off > Seconds::from_integer(0) {
                            notes.push(format!("trimmed clip #{}", c.id));
                        }
                        let mut tail = Clip {
                            id,
                            asset: c.asset,
                            asset_in: c.asset_in + source_span(c, off),
                            asset_out: c.asset_out,
                            speed: c.speed,
                            effects: c.effects.clone(),
                        };
                        tail.rebase_crop_keys(off, c.duration());
                        Some(Segment::Clip(tail))
                    }
                }
            } else {
                None
            }
        }
        None => None,
    };

    // Compose the replacement for [t, t+dur): left remainder, paste,
    // right remainder.
    let mut pieces: Vec<Segment> = Vec::new();
    if left_ok {
        match &seg {
            // A transition is opaque: the paste consumes it whole.
            Segment::Transition(_) => {}
            Segment::Gap(_) => pieces.push(Segment::Gap(Gap { duration: left_len })),
            Segment::Clip(c) => pieces.push(Segment::Clip(Clip {
                asset_out: c.asset_in + source_span(c, left_len),
                ..c.clone()
            })),
        }
    }
    pieces.push(pasted.clone());
    if let Some(r) = right {
        pieces.push(r);
    }

    // Coalesce adjacent gaps the paste produced (a pasted gap butting an
    // existing one).
    let pieces: Vec<Segment> = {
        let mut merged: Vec<Segment> = Vec::with_capacity(pieces.len());
        for piece in pieces {
            if let Segment::Gap(g) = &piece
                && let Some(Segment::Gap(prev)) = merged.last_mut()
            {
                let total = prev.duration + g.duration;
                prev.duration = total;
                continue;
            }
            merged.push(piece);
        }
        merged
    };

    // Remove wholly-covered followers first (descending, so indices above
    // `si` stay valid), then swap `si` for the piece list.
    let mut ops: Vec<EditOp> = Vec::new();
    for k in (si + 1..remove_upto).rev() {
        ops.push(EditOp::Remove {
            track: ti,
            index: k,
            segment: track.segments[k].clone(),
        });
    }
    ops.extend(replace_with_many(ti, si, &seg, pieces));

    let mut desc = format!("paste at {}", stamp(project, &t));
    if !notes.is_empty() {
        desc += &format!(" ({})", notes.join(", "));
    }
    Ok(Batch { ops, desc })
}

/// `_` — insert `frames` of gap at the playhead. Over a clip it splits
/// the clip and inserts the gap between the halves (the tail slides
/// right); over a gap it lengthens that gap; past the end the void plus
/// the new gap. Everything grows by `frames`.
pub fn insert_gap_on(
    project: &Project,
    track: usize,
    playhead: f64,
    frames: i64,
) -> Result<Batch, EditError> {
    if frames < 1 {
        return Err(EditError::TooShort);
    }
    let t = snap(project, playhead)?;
    let fps = project.fps.ok_or(EditError::NoFps)?;
    // Bound the length *before* converting frames to a duration: `frames`
    // near i64::MAX would overflow the conversion itself.
    let limit = Seconds::from_integer(MAX_TIMELINE_SECONDS);
    // `limit * fps` overflows an absurd (but positive) rate; refuse it rather
    // than panicking inside `Ratio`. A tiny rate yields zero representable
    // frames, so every positive `frames` is correctly too long.
    let max_frames = checked_mul(&limit, &fps)
        .ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?
        .to_integer();
    if frames > max_frames {
        return Err(EditError::TooLong(MAX_TIMELINE_SECONDS));
    }
    let gap = Segment::Gap(Gap {
        duration: checked_frames_to_seconds(frames, fps)
            .ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?,
    });
    let gap_end =
        checked_add(&t, &gap.duration()).ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
    if gap_end > limit {
        return Err(EditError::TooLong(MAX_TIMELINE_SECONDS));
    }
    let ti = track;
    let track = track_at(project, ti)?;

    // A gap ending exactly at the playhead absorbs the insert rather than
    // spawning a second, adjacent gap (the rule `paste` already uses). This
    // also covers a trailing gap at the track end. Ripple growth = followers
    // shift right, which is what inserting frames means.
    if let Some(mut batch) = extend_gap_ending_at(project, ti, playhead, gap.duration(), true)? {
        batch.desc = format!("insert gap at {}", stamp(project, &t));
        return Ok(batch);
    }

    // At/past the end: bridge the void (if any), coalesced with the new gap.
    if track.segments.is_empty() || t >= track.end() {
        let void = checked_sub(&t, &track.end()).ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
        return Ok(Batch {
            ops: append_at_end(ti, track, void, gap)?,
            desc: format!("insert gap at {}", stamp(project, &t)),
        });
    }

    let si = track.index_at(t).ok_or(EditError::NoSegment)?;
    let seg = track.segments[si].clone();
    let seg_start = track.start_of(si);
    let off = checked_sub(&t, &seg_start).ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;

    match &seg {
        // `_` on a transition would split it; refuse (phase 1).
        Segment::Transition(_) => Err(EditError::Transition),
        // Over a gap: add to it.
        Segment::Gap(g) => {
            let grown = checked_add(&g.duration, &gap.duration())
                .ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
            Ok(Batch {
                ops: replace_with_many(ti, si, &seg, vec![Segment::Gap(Gap { duration: grown })]),
                desc: format!("insert gap at {}", stamp(project, &t)),
            })
        }
        // Over a clip: split it at the playhead, gap between the halves.
        Segment::Clip(c) => {
            // A split within a frame of either edge is that edge: no slivers.
            let (head_ok, tail_ok) = split_sides(project, c.duration(), off);
            // Only a split (head present) gives the tail a fresh id; an
            // insert-before keeps the original id. Keep the cursor lazy so a
            // full id space does not refuse a legal no-id edit.
            let mut next = project.next_clip_id().ok();
            let mut pieces: Vec<Segment> = Vec::new();
            if head_ok {
                pieces.push(Segment::Clip(Clip {
                    id: c.id,
                    asset: c.asset,
                    asset_in: c.asset_in,
                    asset_out: c.asset_in + source_span(c, off),
                    speed: c.speed,
                    effects: c.effects.clone(),
                }));
            }
            pieces.push(gap);
            if tail_ok {
                // The tail keeps the id when the head is absent (insert-before),
                // else gets a fresh one (a split).
                let id = if head_ok {
                    Project::take_clip_id(&mut next)?
                } else {
                    c.id
                };
                let mut tail = Clip {
                    id,
                    asset: c.asset,
                    asset_in: c.asset_in + source_span(c, off),
                    asset_out: c.asset_out,
                    speed: c.speed,
                    effects: c.effects.clone(),
                };
                tail.rebase_crop_keys(off, c.duration());
                pieces.push(Segment::Clip(tail));
            }
            Ok(Batch {
                ops: replace_with_many(ti, si, &seg, pieces),
                desc: format!("insert gap at {}", stamp(project, &t)),
            })
        }
    }
}

/// `P` — ripple paste: insert the clipboard at the playhead, splitting
/// whatever is there (clip or gap) and growing the timeline by the
/// payload's duration. A clipboard gap inserts empty space; a clipboard
/// clip inserts the clip. At a cut the insert lands before the clip
/// starting there.
pub fn paste_ripple_on(
    project: &Project,
    track: usize,
    snapshot: &Segment,
    playhead: f64,
) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = track;
    let track = track_at(project, ti)?;
    // A gap pasted at the end of a gap extends it and pushes followers
    // right, rather than making a second adjacent gap segment.
    if let Segment::Gap(pg) = snapshot
        && let Some(batch) = extend_gap_ending_at(project, ti, playhead, pg.duration, true)?
    {
        return Ok(batch);
    }
    // At or past the track end: the void becomes a gap (coalesced with the
    // payload when it is itself a gap), then the payload lands on the playhead.
    if t >= track.end() {
        let void = checked_sub(&t, &track.end()).ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
        let mut ids = FreshIds::new(project);
        let (pasted, payload_id) = pasted_with_id(&mut ids, snapshot)?;
        return Ok(Batch {
            ops: append_at_end(ti, track, void, pasted)?,
            desc: format!(
                "ripple paste at {} — {}",
                stamp(project, &t),
                inserted_label(snapshot, payload_id)
            ),
        });
    }
    let si = track.index_at(t).ok_or(EditError::NoSegment)?;
    let seg = track.segments[si].clone();
    let seg_start = track.start_of(si);

    // A gap pasted into a gap just lengthens it.
    if let (Segment::Gap(g), Segment::Gap(pg)) = (&seg, snapshot) {
        let grown = checked_add(&g.duration, &pg.duration)
            .ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
        return Ok(Batch {
            ops: replace_with_many(ti, si, &seg, vec![Segment::Gap(Gap { duration: grown })]),
            desc: format!("ripple paste at {} — gap inserted", stamp(project, &t)),
        });
    }

    // The containing segment splits at t; the paste rides between the
    // pieces. Inserting shifts every follower automatically.
    let mut ids = FreshIds::new(project);
    let (pieces, payload_id) = match &seg {
        // A transition owns its neighbours' trimmed spans and can't be split,
        // so an insert at/over one is refused (as `_` does) rather than
        // consuming it — a ripple paste would have to cut it in two.
        Segment::Transition(_) => return Err(EditError::Transition),
        Segment::Clip(c) => {
            let head_len =
                checked_sub(&t, &seg_start).ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
            let (head_ok, tail_ok) = split_sides(project, c.duration(), head_len);
            let mut pieces = Vec::new();
            if head_ok {
                pieces.push(Segment::Clip(Clip {
                    id: c.id,
                    asset: c.asset,
                    asset_in: c.asset_in,
                    asset_out: c.asset_in + source_span(c, head_len),
                    speed: c.speed,
                    effects: c.effects.clone(),
                }));
            }
            let (pasted, payload_id) = pasted_with_id(&mut ids, snapshot)?;
            pieces.push(pasted);
            if tail_ok {
                // The right half is a new clip on a split, or keeps the
                // original's identity on an insert-before.
                let id = if head_ok { ids.take()? } else { c.id };
                let mut tail = Clip {
                    id,
                    asset: c.asset,
                    asset_in: c.asset_in + source_span(c, head_len),
                    asset_out: c.asset_out,
                    speed: c.speed,
                    effects: c.effects.clone(),
                };
                tail.rebase_crop_keys(head_len, c.duration());
                pieces.push(Segment::Clip(tail));
            }
            (pieces, payload_id)
        }
        Segment::Gap(g) => {
            // Guard the remainders like `paste_on` does: an off-grid gap start
            // could otherwise strand a sub-frame sliver.
            let mut pieces = Vec::new();
            let head =
                checked_sub(&t, &seg_start).ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
            let tail =
                checked_sub(&g.duration, &head).ok_or(EditError::TooLong(MAX_TIMELINE_SECONDS))?;
            if head > Seconds::from_integer(0) && at_least_a_frame(project, &head).is_ok() {
                pieces.push(Segment::Gap(Gap { duration: head }));
            }
            let (pasted, payload_id) = pasted_with_id(&mut ids, snapshot)?;
            pieces.push(pasted);
            if tail > Seconds::from_integer(0) && at_least_a_frame(project, &tail).is_ok() {
                pieces.push(Segment::Gap(Gap { duration: tail }));
            }
            (pieces, payload_id)
        }
    };
    Ok(Batch {
        ops: replace_with_many(ti, si, &seg, pieces),
        desc: format!(
            "ripple paste at {} — {}",
            stamp(project, &t),
            inserted_label(snapshot, payload_id)
        ),
    })
}

/// Remove a contiguous inclusive range of whole segments in one undo step.
/// A transition inside the range is removed with its neighbours: the first
/// blend gives its left handle back to the surviving left segment, the last
/// blend gives its right handle back, and interior blends disappear with the
/// range. A range that is exactly one transition keeps the `dd` semantics
/// (gap spans are preserved when there is no clip handle to restore).
pub fn delete_range_on(
    project: &Project,
    track: usize,
    start: usize,
    end: usize,
) -> Result<Batch, EditError> {
    let tr = track_at(project, track)?;
    if start > end || end >= tr.segments.len() {
        return Err(EditError::SegmentNotFound(start.min(end)));
    }
    // A segment beside a transition is one of its sides: selecting it also
    // selects the blend, so the other side gets its borrowed length back.
    let mut start = start;
    let mut end = end;
    if start > 0 && matches!(tr.segments[start - 1], Segment::Transition(_)) {
        start -= 1;
    }
    if end + 1 < tr.segments.len() && matches!(tr.segments[end + 1], Segment::Transition(_)) {
        end += 1;
    }
    if start == end
        && let Segment::Transition(t) = tr.segments[start].clone()
    {
        return delete::delete_transition_on(project, track, start, t);
    }
    if tr.segments[start..=end]
        .iter()
        .any(|s| matches!(s, Segment::Transition(_)))
    {
        return delete_range_through_transitions(project, track, start, end);
    }
    let mut ops = Vec::with_capacity(end - start + 1);
    for index in (start..=end).rev() {
        ops.push(EditOp::Remove {
            track,
            index,
            segment: tr.segments[index].clone(),
        });
    }
    // The removal joins the two survivors: fold them when seamless, exactly
    // as a one-segment delete does — two adjacent gaps (or two halves of one
    // clip) must not be left behind. The descending removals put the right
    // neighbour at `start`.
    let seam = (start > 0 && end + 1 < tr.segments.len())
        .then(|| merge_pair(&tr.segments[start - 1], &tr.segments[end + 1]))
        .flatten();
    let merged = seam.is_some();
    if let Some(joined) = seam {
        ops.push(EditOp::Replace {
            track,
            index: start - 1,
            before: tr.segments[start - 1].clone(),
            after: joined,
        });
        ops.push(EditOp::Remove {
            track,
            index: start,
            segment: tr.segments[end + 1].clone(),
        });
    }
    let mut desc = format!("deleted {} segment(s)", end - start + 1);
    if merged {
        desc += " (merged the new seam)";
    }
    Ok(Batch { ops, desc })
}

/// A range that contains a transition can change both boundary neighbours;
/// build the resulting track once and swap it in as one history batch.
fn delete_range_through_transitions(
    project: &Project,
    track: usize,
    start: usize,
    end: usize,
) -> Result<Batch, EditError> {
    let before = track_at(project, track)?.segments.clone();
    let mut after = before[..start].to_vec();
    let mut suffix = before[end + 1..].to_vec();

    // A transition at the range's left edge still owns the surviving left
    // neighbour's tail. If that neighbour is a gap, its span stays a gap
    // (the `dd` rule when no clip handle can be restored).
    if let Segment::Transition(t) = &before[start]
        && let Some(left) = after.last_mut()
        && !restore_left(left, t)
        && let Segment::Gap(gap) = left
    {
        gap.duration += t.dur;
    }

    // The symmetric right edge.
    if let Segment::Transition(t) = &before[end]
        && let Some(right) = suffix.first_mut()
        && !restore_right(right, t)
        && let Segment::Gap(gap) = right
    {
        gap.duration += t.dur;
    }

    // The removal joins two gaps: fold them so the track never holds two
    // adjacent Gap segments.
    if let (Some(Segment::Gap(left)), Some(Segment::Gap(right))) =
        (after.last_mut(), suffix.first())
    {
        left.duration += right.duration;
        suffix.remove(0);
    }
    after.extend(suffix);

    Ok(replace_track_batch(
        track,
        &before,
        after,
        format!(
            "deleted {} segment(s) (through transitions)",
            end - start + 1
        ),
    ))
}

/// Swap a whole track in one batch. Transition-aware deletes can change both
/// boundary neighbours, so a full before/after snapshot is simpler and less
/// error-prone than trying to splice every handle in place.
pub(super) fn replace_track_batch(
    track: usize,
    before: &[Segment],
    after: Vec<Segment>,
    desc: String,
) -> Batch {
    let mut ops = Vec::with_capacity(before.len() + after.len());
    for index in (0..before.len()).rev() {
        ops.push(EditOp::Remove {
            track,
            index,
            segment: before[index].clone(),
        });
    }
    for (index, segment) in after.into_iter().enumerate() {
        ops.push(EditOp::Insert {
            track,
            index,
            segment,
        });
    }
    Batch { ops, desc }
}

fn restore_left(segment: &mut Segment, transition: &Transition) -> bool {
    let (
        Segment::Clip(left),
        TransitionSource::Clip {
            asset,
            src_in,
            src_out,
            ..
        },
    ) = (segment, &transition.a)
    else {
        return false;
    };
    if left.asset != *asset || left.asset_out != *src_in {
        return false;
    }
    left.asset_out = *src_out;
    true
}

fn restore_right(segment: &mut Segment, transition: &Transition) -> bool {
    let (
        Segment::Clip(right),
        TransitionSource::Clip {
            asset,
            src_in,
            src_out,
            ..
        },
    ) = (segment, &transition.b)
    else {
        return false;
    };
    if right.asset != *asset || right.asset_in != *src_out {
        return false;
    }
    right.asset_in = *src_in;
    true
}

/// Insert a gap of `dur` on every track except `except`, so a ripple insert
/// keeps the timeline in sync. A track with nothing at or after `at` has
/// nothing to shift; a blend at `at` takes the gap at its start rather than
/// being split. `next` is the shared id counter (see
/// `insert_segments_on_with`): every split half takes a fresh id, so callers
/// that planned other inserts against the same project must seed it past them.
pub fn ripple_other_tracks(
    project: &Project,
    except: usize,
    at: f64,
    dur: Seconds,
    next: &mut Option<u32>,
) -> Result<Vec<EditOp>, EditError> {
    if dur <= Seconds::from_integer(0) {
        return Ok(Vec::new());
    }
    let at = snap(project, at)?;
    let gap = Segment::Gap(Gap { duration: dur });
    let mut ops = Vec::new();
    for (ti, tr) in project.timeline.tracks.iter().enumerate() {
        if ti == except || tr.segments.is_empty() || tr.end() <= at {
            continue;
        }
        let insert_at = match tr.index_at(at) {
            Some(si) if matches!(tr.segments[si], Segment::Transition(_)) => tr.start_of(si),
            _ => at,
        };
        let batch = insert_segments_on_with(
            project,
            ti,
            secs_f64(&insert_at),
            std::slice::from_ref(&gap),
            next,
        )?;
        ops.extend(batch.ops);
    }
    Ok(ops)
}

/// Remove the timeline span `[start, end)` from `track`, leaving everything
/// outside it untouched. This is the other-track half of a base-track sync
/// ripple: a hard time removal, not a blend-aware delete, so a transition
/// wholly inside the span is dropped without restoring its handles.
pub fn remove_span_on(
    project: &Project,
    track: usize,
    start: Seconds,
    end: Seconds,
    next_id: &mut Option<u32>,
) -> Result<Batch, EditError> {
    let tr = track_at(project, track)?;
    let total = tr.end();
    let zero = Seconds::from_integer(0);
    let start = if start < zero { zero } else { start };
    let end = if end > total { total } else { end };
    if end <= start {
        return Ok(Batch {
            ops: Vec::new(),
            desc: "sync ripple: nothing to remove".to_string(),
        });
    }
    let before = tr.segments.clone();
    let si = tr.index_at(start).ok_or(EditError::NoSegment)?;
    let si_start = tr.start_of(si);
    if matches!(before[si], Segment::Transition(_)) && start > si_start {
        return Err(EditError::Transition);
    }
    let ei = if end >= total {
        before.len()
    } else {
        tr.index_at(end).ok_or(EditError::NoSegment)?
    };
    if ei < before.len() {
        let ei_start = tr.start_of(ei);
        if matches!(before[ei], Segment::Transition(_)) && end > ei_start {
            return Err(EditError::Transition);
        }
    }

    let mut after = before[..si].to_vec();
    if start > si_start {
        let (left, _) = split_segment_at(project, &before[si], start - si_start, next_id)?;
        after.push(left);
    }
    let mut suffix = Vec::new();
    if end < total && end > tr.start_of(ei) {
        let (_, right) = split_segment_at(project, &before[ei], end - tr.start_of(ei), next_id)?;
        suffix.push(right);
        suffix.extend_from_slice(&before[ei + 1..]);
    } else {
        suffix.extend_from_slice(&before[ei..]);
    }
    if let (Some(left), Some(right)) = (after.last(), suffix.first())
        && let Some(merged) = merge_pair(left, right)
    {
        after.pop();
        after.push(merged);
        suffix.remove(0);
    }
    after.extend(suffix);
    // A track cannot start or end with a blend: the removed span may have
    // taken away the clip the transition borrowed its picture from.
    if matches!(after.first(), Some(Segment::Transition(_)))
        || matches!(after.last(), Some(Segment::Transition(_)))
    {
        return Err(EditError::Transition);
    }
    Ok(replace_track_batch(
        track,
        &before,
        after,
        format!(
            "sync ripple (removed {} to {})",
            stamp(project, &start),
            stamp(project, &end)
        ),
    ))
}

/// Take the next segment id from a shared counter.
///
/// Delegates to [`Project::take_clip_id`]: `u32::MAX` is reserved, so spending
/// `u32::MAX - 1` leaves `None` and a second allocation refuses instead of
/// aliasing the last id. The batch is refused before anything is applied.
fn take_id(next: &mut Option<u32>) -> Result<u32, EditError> {
    Ok(Project::take_clip_id(next)?)
}

/// Split one segment at a timeline offset. Transitions cannot be split; the
/// caller guards those boundaries before calling.
fn split_segment_at(
    project: &Project,
    seg: &Segment,
    offset: Seconds,
    next_id: &mut Option<u32>,
) -> Result<(Segment, Segment), EditError> {
    match seg {
        Segment::Clip(c) => {
            at_least_a_frame(project, &offset)?;
            at_least_a_frame(project, &(c.duration() - offset))?;
            let split = c.asset_in + source_span(c, offset);
            let mut left = Clip {
                id: c.id,
                asset: c.asset,
                asset_in: c.asset_in,
                asset_out: split,
                speed: c.speed,
                effects: c.effects.clone(),
            };
            left.truncate_crop_keys(offset, c.duration());
            let mut right = Clip {
                id: take_id(next_id)?,
                asset: c.asset,
                asset_in: split,
                asset_out: c.asset_out,
                speed: c.speed,
                effects: c.effects.clone(),
            };
            right.rebase_crop_keys(offset, c.duration());
            Ok((Segment::Clip(left), Segment::Clip(right)))
        }
        Segment::Gap(g) => {
            let (left, right) = (offset, g.duration - offset);
            at_least_a_frame(project, &left)?;
            at_least_a_frame(project, &right)?;
            Ok((
                Segment::Gap(Gap { duration: left }),
                Segment::Gap(Gap { duration: right }),
            ))
        }
        Segment::Transition(_) => Err(EditError::Transition),
    }
}

/// Insert whole segments at any playhead, giving copied clips/transitions
/// fresh ids. A playhead inside a clip or gap splits that segment first; a
/// playhead inside a transition is refused because a blend owns both sides.
/// No overwrite: this is the `P`/ripple-insert shape. Visual paste uses it.
pub fn insert_segments_on(
    project: &Project,
    track: usize,
    at: f64,
    segments: &[Segment],
) -> Result<Batch, EditError> {
    // A gap-only payload (or an insert with no split) takes no id; keep the
    // cursor lazy so a full space only refuses when a payload/split needs one.
    let mut next = project.next_clip_id().ok();
    insert_segments_on_with(project, track, at, segments, &mut next)
}

/// [`insert_segments_on`] drawing fresh ids from `next` instead of the
/// project's own counter. A caller planning several inserts against the same
/// pre-edit project (the base-track sync ripple) must share one counter, or
/// two tracks end up with the same split-half id.
pub(super) fn insert_segments_on_with(
    project: &Project,
    track: usize,
    at: f64,
    segments: &[Segment],
    next: &mut Option<u32>,
) -> Result<Batch, EditError> {
    if segments.is_empty() {
        return Err(EditError::NoSegment);
    }
    let t = snap(project, at)?;
    let tr = track_at(project, track)?;
    let payload = fresh_payload(segments, next)?;
    let desc = format!(
        "pasted {} segment(s) at {}",
        payload.len(),
        stamp(project, &t)
    );

    if t <= Seconds::from_integer(0) {
        return Ok(plain_insert(track, tr, 0, payload, &desc));
    }
    if t >= tr.end() {
        return Ok(plain_insert(track, tr, tr.segments.len(), payload, &desc));
    }
    let si = tr.index_at(t).ok_or(EditError::NoSegment)?;
    let seg = tr.segments[si].clone();
    let offset = t - tr.start_of(si);
    if offset <= Seconds::from_integer(0) {
        return Ok(plain_insert(track, tr, si, payload, &desc));
    }
    match &seg {
        Segment::Transition(_) => Err(EditError::PasteTransition),
        Segment::Gap(g) => {
            let (head_ok, tail_ok) = split_sides(project, g.duration, offset);
            if !head_ok {
                return Ok(plain_insert(track, tr, si, payload, &desc));
            }
            if !tail_ok {
                return Ok(plain_insert(track, tr, si + 1, payload, &desc));
            }
            let mut pieces = vec![Segment::Gap(Gap { duration: offset })];
            pieces.extend(payload);
            pieces.push(Segment::Gap(Gap {
                duration: g.duration - offset,
            }));
            Ok(Batch {
                ops: replace_with_many(track, si, &seg, coalesce_gaps(pieces)),
                desc: format!("{desc} (split the gap)"),
            })
        }
        Segment::Clip(c) => {
            let (head_ok, tail_ok) = split_sides(project, c.duration(), offset);
            if !head_ok {
                return Ok(plain_insert(track, tr, si, payload, &desc));
            }
            if !tail_ok {
                return Ok(plain_insert(track, tr, si + 1, payload, &desc));
            }
            let split_at = c.asset_in + source_span(c, offset);
            let left = Segment::Clip(Clip {
                id: c.id,
                asset: c.asset,
                asset_in: c.asset_in,
                asset_out: split_at,
                speed: c.speed,
                effects: c.effects.clone(),
            });
            let right = Segment::Clip(Clip {
                id: take_id(next)?,
                asset: c.asset,
                asset_in: split_at,
                asset_out: c.asset_out,
                speed: c.speed,
                effects: c.effects.clone(),
            });
            let mut pieces = vec![left];
            pieces.extend(payload);
            pieces.push(right);
            Ok(Batch {
                ops: replace_with_many(track, si, &seg, pieces),
                desc: format!("{desc} (split clip #{})", c.id),
            })
        }
    }
}

/// Insert a payload between existing segments, folding a leading/trailing
/// gap into a neighbouring gap so a cut paste never leaves two adjacent gaps.
fn plain_insert(
    track: usize,
    tr: &Track,
    index: usize,
    mut payload: Vec<Segment>,
    desc: &str,
) -> Batch {
    let mut ops = Vec::new();
    let prev_gap = index > 0 && matches!(tr.segments.get(index - 1), Some(Segment::Gap(_)));
    let next_gap =
        index < tr.segments.len() && matches!(tr.segments.get(index), Some(Segment::Gap(_)));

    // One gap pasted between two gaps: fuse all three in one step.
    if prev_gap
        && next_gap
        && payload.len() == 1
        && matches!(payload.first(), Some(Segment::Gap(_)))
    {
        let previous = tr.segments[index - 1].clone();
        let next = tr.segments[index].clone();
        ops.push(EditOp::Replace {
            track,
            index: index - 1,
            before: previous.clone(),
            after: Segment::Gap(Gap {
                duration: previous.duration() + payload[0].duration() + next.duration(),
            }),
        });
        ops.push(EditOp::Remove {
            track,
            index,
            segment: next,
        });
        return Batch {
            ops,
            desc: desc.to_string(),
        };
    }

    if prev_gap && matches!(payload.first(), Some(Segment::Gap(_))) {
        let previous = tr.segments[index - 1].clone();
        let mut merged = previous.clone();
        let add = payload.remove(0).duration();
        if let Segment::Gap(gap) = &mut merged {
            gap.duration += add;
        }
        ops.push(EditOp::Replace {
            track,
            index: index - 1,
            before: previous,
            after: merged,
        });
    }

    let mut next_merged = None;
    if next_gap
        && matches!(payload.last(), Some(Segment::Gap(_)))
        && let Some(last) = payload.pop()
    {
        let next = tr.segments[index].clone();
        let mut merged = next.clone();
        if let Segment::Gap(gap) = &mut merged {
            gap.duration += last.duration();
        }
        next_merged = Some((next, merged));
    }

    let inserted = payload.len();
    for (i, segment) in payload.into_iter().enumerate() {
        ops.push(EditOp::Insert {
            track,
            index: index + i,
            segment,
        });
    }
    if let Some((before, after)) = next_merged {
        ops.push(EditOp::Replace {
            track,
            index: index + inserted,
            before,
            after,
        });
    }
    Batch {
        ops,
        desc: desc.to_string(),
    }
}

fn fresh_payload(segments: &[Segment], next: &mut Option<u32>) -> Result<Vec<Segment>, EditError> {
    let mut out = Vec::with_capacity(segments.len());
    for segment in segments {
        let mut segment = segment.clone();
        match &mut segment {
            Segment::Clip(c) => c.id = take_id(next)?,
            Segment::Transition(t) => t.id = take_id(next)?,
            Segment::Gap(_) => {}
        }
        out.push(segment);
    }
    Ok(out)
}

/// Keep the "never two adjacent gaps" invariant after splitting a gap and
/// pasting a range whose edge is also a gap.
fn coalesce_gaps(pieces: Vec<Segment>) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        if let Some(Segment::Gap(previous)) = out.last_mut()
            && let Segment::Gap(next) = &piece
        {
            previous.duration += next.duration;
            continue;
        }
        out.push(piece);
    }
    out
}
