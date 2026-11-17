//! Notes, markers and (later) expectations anchored to source media.
//!
//! A source anchor is a media time, not a timeline position: trimming,
//! splitting or rippling the timeline cannot move it. A timeline anchor is
//! clip-relative (`clip` + `source`), so edits keep it on the same footage.

use serde::{Deserialize, Serialize};

use crate::provenance::Provenance;
use crate::time::{Seconds, rational_ser};
use crate::timeline::{Clip, Segment, Track};

/// A note or marker attached to a project sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    pub id: u32,
    pub sequence: u32,
    pub kind: AnnotationKind,
    pub anchor: Anchor,
    pub provenance: Provenance,
    #[serde(default)]
    pub status: AnnotationStatus,
}

impl Annotation {
    /// The note text, if this is a note.
    pub fn text(&self) -> Option<&str> {
        match &self.kind {
            AnnotationKind::Note { text } => Some(text),
            AnnotationKind::Marker => None,
        }
    }

    /// A short label for notices and lists.
    pub fn label(&self) -> &'static str {
        match self.kind {
            AnnotationKind::Note { .. } => "note",
            AnnotationKind::Marker => "marker",
        }
    }
}

/// What the annotation says. The payload lives in the variant, so a note
/// cannot exist without text and a marker cannot carry stray text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnnotationKind {
    Note { text: String },
    Marker,
}

/// Where the annotation points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum Anchor {
    /// A time inside one source file, or inside a logical recording's group
    /// clock. Both survive timeline edits.
    Source {
        source: SourceRef,
        #[serde(with = "rational_ser")]
        time: Seconds,
    },
    /// A range on the timeline, held by the clips under its endpoints.
    /// Endpoints keep their **source** time, so a trim-in moves the note with
    /// the material instead of sliding it onto different footage. A note or
    /// marker is a point (`start == end`).
    Timeline { start: ClipPoint, end: ClipPoint },
}

/// A timeline position tied to a clip and a media time inside it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ClipPoint {
    /// Clip id (unique within the sequence).
    pub clip: u32,
    #[serde(with = "rational_ser")]
    pub source: Seconds,
}

/// Which media clock a source anchor uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "of", rename_all = "snake_case")]
pub enum SourceRef {
    Asset { id: u32 },
    Group { id: u32 },
}

/// Resolution state for directives and expectations; notes can be resolved
/// too, so deleting is not the only way to clear a review list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationStatus {
    #[default]
    Open,
    Resolved,
}

/// Re-anchor timeline annotations after an edit. `before`/`after` are the
/// sequence's full track lists; source anchors pass through untouched.
///
/// A matching clip keeps its source point (trims, speed and ripples follow
/// the material); a split re-anchors to the one new clip with the same asset
/// and source; anything else is dropped with a warning. The caller records
/// both lists in the undo change, so a drop is undoable.
pub(crate) fn reanchor_timeline(
    annotations: &[Annotation],
    before: &[Track],
    after: &[Track],
) -> (Vec<Annotation>, Vec<String>) {
    let mut kept = Vec::with_capacity(annotations.len());
    let mut warnings = Vec::new();
    for annotation in annotations {
        let Anchor::Timeline { start, end } = &annotation.anchor else {
            kept.push(annotation.clone());
            continue;
        };
        let Some(start_new) = resolve_point(start, before, after) else {
            warnings.push(format!(
                "dropped annotation #{}: its clip no longer has that source",
                annotation.id
            ));
            continue;
        };
        let Some(end_new) = resolve_point(end, before, after) else {
            warnings.push(format!(
                "dropped annotation #{}: its clip no longer has that source",
                annotation.id
            ));
            continue;
        };
        let (Some(start_pos), Some(end_pos)) = (
            point_position(&start_new, after),
            point_position(&end_new, after),
        ) else {
            warnings.push(format!(
                "dropped annotation #{}: its clip no longer has that source",
                annotation.id
            ));
            continue;
        };
        if start_pos > end_pos {
            warnings.push(format!(
                "dropped annotation #{}: its timeline range collapsed",
                annotation.id
            ));
            continue;
        }
        let mut next = annotation.clone();
        next.anchor = Anchor::Timeline {
            start: start_new,
            end: end_new,
        };
        kept.push(next);
    }
    (kept, warnings)
}

fn resolve_point(point: &ClipPoint, before: &[Track], after: &[Track]) -> Option<ClipPoint> {
    let (track_index, segment_index, old) = find_clip(before, point.clip)?;
    if point.source < old.asset_in || point.source >= old.asset_out {
        return None;
    }
    if let Some((_, _, new)) = find_clip(after, point.clip)
        && new.asset == old.asset
        && point.source >= new.asset_in
        && point.source < new.asset_out
    {
        return Some(ClipPoint {
            clip: new.id,
            source: point.source,
        });
    }

    // A split replaces one clip with two on the same track, inside the old
    // clip's timeline span and sharing one source boundary. Restricting the
    // fallback this far keeps a duplicate take elsewhere (another track, or
    // later on the same track) from silently stealing the note.
    let old_start = before[track_index].start_of(segment_index);
    let old_end = old_start + old.duration();
    let track = after.get(track_index)?;
    let mut candidates = track
        .segments
        .iter()
        .enumerate()
        .filter_map(|(index, segment)| match segment {
            Segment::Clip(clip)
                if clip.id != old.id
                    && clip.asset == old.asset
                    && point.source >= clip.asset_in
                    && point.source < clip.asset_out
                    && (clip.asset_in == old.asset_in || clip.asset_out == old.asset_out) =>
            {
                let start = track.start_of(index);
                (start >= old_start && start + clip.duration() <= old_end).then_some(clip)
            }
            _ => None,
        });
    let first = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    Some(ClipPoint {
        clip: first.id,
        source: point.source,
    })
}

fn find_clip(tracks: &[Track], id: u32) -> Option<(usize, usize, &Clip)> {
    for (track_index, track) in tracks.iter().enumerate() {
        for (segment_index, segment) in track.segments.iter().enumerate() {
            if let Segment::Clip(clip) = segment
                && clip.id == id
            {
                return Some((track_index, segment_index, clip));
            }
        }
    }
    None
}

fn point_position(point: &ClipPoint, tracks: &[Track]) -> Option<Seconds> {
    crate::timeline::clip_position(tracks, point.clip, point.source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provenance::{Actor, Provenance};
    use crate::timeline::{Clip, TrackKind};

    fn s(n: i64) -> Seconds {
        Seconds::from_integer(n)
    }

    fn clip(id: u32, asset: u32, asset_in: i64, asset_out: i64) -> Segment {
        Segment::Clip(Clip::new(id, asset, s(asset_in), s(asset_out)))
    }

    fn track(segments: Vec<Segment>) -> Track {
        Track {
            kind: TrackKind::Video,
            muted: false,
            segments,
        }
    }

    fn note(id: u32, start: (u32, i64), end: (u32, i64)) -> Annotation {
        Annotation {
            id,
            sequence: 1,
            kind: AnnotationKind::Note {
                text: format!("note {id}"),
            },
            anchor: Anchor::Timeline {
                start: ClipPoint {
                    clip: start.0,
                    source: s(start.1),
                },
                end: ClipPoint {
                    clip: end.0,
                    source: s(end.1),
                },
            },
            provenance: Provenance {
                actor: Actor::Human,
                at: 0,
            },
            status: AnnotationStatus::Open,
        }
    }

    #[test]
    fn a_timeline_point_follows_its_material_and_a_source_point_ignores_edits() {
        let before = vec![track(vec![clip(1, 1, 0, 4)])];
        // The clip heads at 0 with asset_in 0, so source 2 is timeline 2.
        let source = Annotation {
            id: 9,
            sequence: 1,
            kind: AnnotationKind::Marker,
            anchor: Anchor::Source {
                source: SourceRef::Asset { id: 1 },
                time: s(2),
            },
            provenance: Provenance {
                actor: Actor::Human,
                at: 0,
            },
            status: AnnotationStatus::Open,
        };
        // A ripple moves the clip to 3..7; the point moves with it.
        let after = vec![track(vec![
            Segment::Gap(crate::timeline::Gap { duration: s(3) }),
            clip(1, 1, 0, 4),
        ])];
        let (kept, warnings) =
            reanchor_timeline(&[note(1, (1, 2), (1, 2)), source], &before, &after);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(kept.len(), 2);
        assert_eq!(
            kept[0].anchor,
            Anchor::Timeline {
                start: ClipPoint {
                    clip: 1,
                    source: s(2)
                },
                end: ClipPoint {
                    clip: 1,
                    source: s(2)
                }
            }
        );
        assert!(matches!(kept[1].anchor, Anchor::Source { .. }));
    }

    #[test]
    fn a_split_reanchors_to_the_piece_that_holds_the_source() {
        let before = vec![track(vec![clip(1, 1, 0, 10)])];
        // Split at source 5: left keeps id 1, right is id 2.
        let after = vec![track(vec![clip(1, 1, 0, 5), clip(2, 1, 5, 10)])];
        let (kept, warnings) = reanchor_timeline(&[note(1, (1, 7), (1, 7))], &before, &after);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(kept.len(), 1);
        assert!(matches!(
            kept[0].anchor,
            Anchor::Timeline {
                start: ClipPoint { clip: 2, .. },
                ..
            }
        ));
    }

    #[test]
    fn a_removed_clip_drops_its_note_with_a_warning() {
        let before = vec![track(vec![clip(1, 1, 0, 4)])];
        let after = vec![track(Vec::new())];
        let (kept, warnings) = reanchor_timeline(&[note(1, (1, 1), (1, 2))], &before, &after);
        assert!(kept.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("dropped annotation #1"),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_trimmed_away_point_is_dropped() {
        let before = vec![track(vec![clip(1, 1, 0, 10)])];
        // Trim the head to source 3: source 1 is gone.
        let after = vec![track(vec![clip(1, 1, 3, 10)])];
        let (kept, warnings) = reanchor_timeline(&[note(1, (1, 1), (1, 1))], &before, &after);
        assert!(kept.is_empty());
        assert!(
            warnings[0].contains("dropped annotation #1"),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_duplicate_take_on_another_track_does_not_steal_a_note() {
        let before = vec![
            track(vec![clip(1, 1, 0, 10)]),
            track(vec![clip(2, 1, 0, 10)]),
        ];
        // The owning clip is gone; the same footage on another track is not
        // its split sibling.
        let after = vec![track(Vec::new()), track(vec![clip(2, 1, 0, 10)])];
        let (kept, warnings) = reanchor_timeline(&[note(1, (1, 5), (1, 5))], &before, &after);
        assert!(kept.is_empty(), "the note did not teleport to track 2");
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn a_copy_outside_the_old_span_does_not_steal_a_note() {
        let before = vec![track(vec![clip(1, 1, 0, 10), clip(3, 1, 0, 10)])];
        // Deleting clip #1 leaves a gap; the later copy starts after the old
        // span, so it is not a split sibling.
        let after = vec![track(vec![
            Segment::Gap(crate::timeline::Gap { duration: s(10) }),
            clip(3, 1, 0, 10),
        ])];
        let (kept, warnings) = reanchor_timeline(&[note(1, (1, 5), (1, 5))], &before, &after);
        assert!(kept.is_empty(), "the note did not move to the later copy");
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn an_ambiguous_reused_asset_is_dropped() {
        let before = vec![track(vec![clip(1, 1, 0, 20)])];
        // Two new pieces inside the old span both read that source; neither
        // is evidence for which one owns the note.
        let after = vec![track(vec![clip(2, 1, 0, 10), clip(3, 1, 0, 10)])];
        let (kept, warnings) = reanchor_timeline(&[note(1, (1, 5), (1, 5))], &before, &after);
        assert!(kept.is_empty(), "two candidates are not evidence");
        assert_eq!(warnings.len(), 1);
    }
}
