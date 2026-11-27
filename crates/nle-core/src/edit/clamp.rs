//! Keeping the model inside its media: every write path that can move a
//! source in/out calls `clamp_to_media`, and `purge_stumps` drops the
//! sub-frame leftovers a grid-snapped edit can strand.

use crate::project::Project;
use crate::time::{Seconds, checked_frames_to_seconds, format_seconds};
use crate::timeline::{Clip, Gap, Segment, Track, TransitionSource};

/// Whole frames in `span` at `fps`, rounded, or `None` when that count does
/// not fit `i64`. The i128 product keeps `span * fps` exact instead of letting
/// `Rational64` cross-multiply two i64s into an overflow (a debug panic, a
/// silent wrap in release); an unrepresentable count means the grid is finer
/// than any position this timeline can encode, so callers leave the value be.
fn whole_frames(span: Seconds, fps: Seconds) -> Option<i64> {
    let num = i128::from(*span.numer()) * i128::from(*fps.numer());
    let den = i128::from(*span.denom()) * i128::from(*fps.denom());
    if den == 0 {
        return None;
    }
    // Half away from zero, like `Rational64::round`.
    let rounded = if num >= 0 {
        (num + den / 2) / den
    } else {
        (num - den / 2) / den
    };
    i64::try_from(rounded).ok()
}

/// Keep one track's clips inside their media and on the frame grid. The
/// source range is clamped to `[0, media]` (in-point ≥ 0, out-point ≤ the
/// media's duration), then the timeline duration is snapped to whole frames
/// (rounded, falling back to the floor when rounding would run past the media
/// end). A clip shorter than one frame is dropped.
/// `media` maps an asset id to its duration; `fps` is the timeline's frame
/// rate. Returns one note per adjusted or dropped clip.
///
/// Every write path needs this: a Kdenlive rate conform can round an out-point
/// a hair past the file's end, and `reconstruct` snaps a rendered file's end up
/// to the project grid. Either way a clip would read past EOF (the export falls
/// short) and leave a fractional boundary for `snap` to land on the wrong side
/// of.
pub fn clamp_to_media(
    track: &mut Track,
    media: &dyn Fn(u32) -> Option<Seconds>,
    fps: Seconds,
) -> Vec<String> {
    let mut notes = Vec::new();
    if fps <= Seconds::from_integer(0) {
        return notes;
    }
    let zero = Seconds::from_integer(0);
    // None when the frame grid is finer than `Ratio<i64>` can encode; the
    // caller keeps the media-clamped value, the closest expressible one.
    let frame_secs = |n: i64| checked_frames_to_seconds(n, fps);
    track.segments.retain_mut(|seg| match seg {
        // A non-positive speed has no sane clamp — leave it for `validate()`
        // to reject rather than dividing by zero here.
        Segment::Clip(c) if c.speed <= zero => true,
        Segment::Clip(c) => {
            let Some(d) = media(c.asset) else {
                return true;
            };
            // Keep the source range inside [0, media]: a negative in-point (an
            // EDL source TC before its offset, say) reads before the file start.
            let in_ = c.asset_in.max(zero);
            let out = c.asset_out.min(d);
            let mut frames = whole_frames((out - in_) / c.speed, fps);
            let new_out = match frames {
                Some(n) => match frame_secs(n) {
                    Some(step) => {
                        let new_out = in_ + step * c.speed;
                        if new_out > d {
                            frames = Some(n - 1);
                            frame_secs(n - 1)
                                .map(|step| in_ + step * c.speed)
                                .unwrap_or(out)
                        } else {
                            new_out
                        }
                    }
                    None => out,
                },
                // A frame below the `Rational64` grid this timeline can
                // encode: keep the media-clamped out-point, the closest
                // expressible one. An unrepresentable count from an inverted
                // span is not a grid problem — the whole-frame check below
                // drops it.
                None => out,
            };
            if !frames.map_or(out > in_, |frames| frames >= 1) {
                notes.push(format!("clip #{}: holds no whole frame — dropped", c.id));
                return false;
            }
            if in_ != c.asset_in {
                notes.push(format!(
                    "clip #{}: in {} → {}",
                    c.id,
                    format_seconds(&c.asset_in),
                    format_seconds(&in_)
                ));
            }
            if new_out != c.asset_out {
                notes.push(format!(
                    "clip #{}: out {} → {}",
                    c.id,
                    format_seconds(&c.asset_out),
                    format_seconds(&new_out)
                ));
            }
            c.asset_in = in_;
            c.asset_out = new_out;
            true
        }
        Segment::Transition(t) => {
            let id = t.id;
            // The duration lands on the frame grid; each source span stays
            // inside its media.
            match whole_frames(t.dur, fps) {
                Some(frames) if frames < 1 => {
                    notes.push(format!("transition #{id}: holds no whole frame — dropped"));
                    return false;
                }
                Some(frames) => {
                    if let Some(dur) = frame_secs(frames) {
                        if dur != t.dur {
                            notes.push(format!(
                                "transition #{id}: dur {} → {}",
                                format_seconds(&t.dur),
                                format_seconds(&dur)
                            ));
                        }
                        t.dur = dur;
                    }
                }
                // A frame below the `Rational64` grid: `t.dur` is already the
                // closest expressible duration.
                None => {}
            }
            let mut outside = false;
            let mut clamped = false;
            for side in [&mut t.a, &mut t.b] {
                let TransitionSource::Clip {
                    asset,
                    src_in,
                    src_out,
                    ..
                } = side
                else {
                    continue;
                };
                let Some(d) = media(*asset) else {
                    continue;
                };
                let in_ = (*src_in).max(zero);
                let out = (*src_out).min(d);
                if out <= in_ {
                    outside = true;
                    break;
                }
                clamped |= in_ != *src_in || out != *src_out;
                *src_in = in_;
                *src_out = out;
            }
            if outside {
                notes.push(format!(
                    "transition #{id}: source span outside the media — dropped"
                ));
                return false;
            }
            if clamped {
                notes.push(format!(
                    "transition #{id}: source span clamped inside the media"
                ));
            }
            true
        }
        Segment::Gap(_) => true,
    });
    notes
}

/// Snap a track's segment boundaries back onto the project frame grid and drop
/// any segment that collapses to under one frame (a "stump"), coalescing the
/// gaps a drop leaves adjacent. Boundaries snap *cumulatively*, so each cut
/// stays within half a frame of where it was rather than drifting with the sum
/// of per-segment rounding. Returns the cleaned segments and how many were
/// dropped.
///
/// Repairs a project that accumulated sub-frame drift (an older lossy undo
/// `Atom` could do this; fractional boundaries then made `snap` land edits on
/// the wrong segment). `nile purge-stumps` / `:purge`.
pub fn purge_stumps(track: &Track, fps: Seconds) -> (Vec<Segment>, usize) {
    if fps <= Seconds::from_integer(0) {
        return (track.segments.clone(), 0);
    }
    let frame = Seconds::from_integer(1) / fps;
    let snap = |t: Seconds| match whole_frames(t, fps) {
        Some(frames) => checked_frames_to_seconds(frames, fps).unwrap_or(t),
        // A frame below the `Rational64` grid: the position is already the
        // closest expressible one.
        None => t,
    };
    let mut pos = Seconds::from_integer(0);
    let mut out: Vec<Segment> = Vec::with_capacity(track.segments.len());
    let mut dropped = 0usize;
    for seg in &track.segments {
        let end = snap(pos + seg.duration());
        let dur = end - pos;
        if dur < frame {
            dropped += 1;
            continue;
        }
        let cleaned = match seg {
            Segment::Clip(c) => Segment::Clip(Clip {
                id: c.id,
                asset: c.asset,
                asset_in: c.asset_in,
                asset_out: c.asset_in + dur * c.speed,
                speed: c.speed,
                effects: c.effects.clone(),
            }),
            Segment::Gap(_) => Segment::Gap(Gap { duration: dur }),
            Segment::Transition(t) => {
                let mut t = t.clone();
                t.dur = dur;
                Segment::Transition(t)
            }
        };
        // Dropping a stump can leave two gaps adjacent; fold them.
        if let (Segment::Gap(g), Some(Segment::Gap(prev))) = (&cleaned, out.last_mut()) {
            prev.duration += g.duration;
        } else {
            out.push(cleaned);
        }
        pos = end;
    }
    (out, dropped)
}

/// Clamp every track of `project` to its media and frame grid. The one entry
/// point the write paths share: imports and `purge-stumps` call it before
/// saving, and `History::apply` calls it after every batch, so a recorded
/// change can never leave a clip reading past EOF.
pub fn clamp_project(project: &mut Project) -> Vec<String> {
    let Some(fps) = project.fps else {
        return Vec::new();
    };
    let durations: Vec<(u32, Seconds)> = project
        .media
        .iter()
        .filter_map(|asset| asset.duration().map(|duration| (asset.id, duration)))
        .collect();
    let duration = |id: u32| {
        durations
            .iter()
            .find_map(|(asset, duration)| (*asset == id).then_some(*duration))
    };
    project
        .timeline
        .tracks
        .iter_mut()
        .flat_map(|t| clamp_to_media(t, &duration, fps))
        .collect()
}

/// Clamp every sequence, not only the active one, and restore the cursor.
/// `Document::save` uses this so a switch cannot leave another timeline with
/// a clip reading past EOF (its stored state is still written to the file).
/// Each sequence's stored playhead is clamped to its own `last_frame` too:
/// the cursor is session state, so that repair adds no note and never bumps
/// the revision on its own.
pub fn clamp_project_all(project: &mut Project) -> Vec<String> {
    let active = project.active_id();
    let ids: Vec<u32> = project.sequences.iter().map(|s| s.id).collect();
    let mut notes = Vec::new();
    for id in ids {
        project
            .select_sequence(id)
            .expect("id came from the sequence list");
        notes.extend(clamp_project(project));
        let last = project.last_frame();
        if project.playhead > last {
            project.playhead = last;
        }
    }
    project
        .select_sequence(active)
        .expect("the active sequence still exists");
    notes
}
