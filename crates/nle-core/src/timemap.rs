//! Exact piecewise-linear maps between timeline and source time.
//!
//! A `TimeMap` is an ordered set of non-overlapping spans. Each span maps a
//! half-open timeline range to a half-open source range; a gap between spans
//! means "no media". All arithmetic is rational, so a cut or a speed change
//! does not drift after repeated round-trips.

use thiserror::Error;

use crate::time::{Seconds, checked_add, checked_div, checked_mul, checked_sub};
use crate::timeline::{Segment, Track};

#[derive(Debug, Error, PartialEq)]
pub enum TimeMapError {
    #[error("time-map span is empty or reversed")]
    EmptySpan,
    #[error("time-map spans overlap or are out of order")]
    Overlap,
    #[error("time-map arithmetic overflow")]
    Overflow,
}

/// One half-open timeline→source span.
#[derive(Debug, Clone, PartialEq)]
pub struct TimeSpan {
    timeline_start: Seconds,
    timeline_end: Seconds,
    source_start: Seconds,
    source_end: Seconds,
}

impl TimeSpan {
    pub fn new(
        timeline_start: Seconds,
        timeline_end: Seconds,
        source_start: Seconds,
        source_end: Seconds,
    ) -> Result<Self, TimeMapError> {
        if timeline_start >= timeline_end || source_start >= source_end {
            return Err(TimeMapError::EmptySpan);
        }
        Ok(Self {
            timeline_start,
            timeline_end,
            source_start,
            source_end,
        })
    }

    pub fn timeline(&self) -> (Seconds, Seconds) {
        (self.timeline_start, self.timeline_end)
    }

    pub fn source(&self) -> (Seconds, Seconds) {
        (self.source_start, self.source_end)
    }

    fn timeline_len(&self) -> Option<Seconds> {
        checked_sub(&self.timeline_end, &self.timeline_start)
    }

    fn source_len(&self) -> Option<Seconds> {
        checked_sub(&self.source_end, &self.source_start)
    }

    fn scale(&self) -> Option<Seconds> {
        checked_div(&self.source_len()?, &self.timeline_len()?)
    }

    fn map_endpoint(&self, timeline: &Seconds) -> Option<Seconds> {
        let dt = checked_sub(timeline, &self.timeline_start)?;
        let offset = checked_mul(&dt, &self.scale()?)?;
        checked_add(&self.source_start, &offset)
    }

    fn inverse_endpoint(&self, source: &Seconds) -> Option<Seconds> {
        let ds = checked_sub(source, &self.source_start)?;
        let offset = checked_div(&ds, &self.scale()?)?;
        checked_add(&self.timeline_start, &offset)
    }

    fn contains_timeline(&self, t: &Seconds) -> bool {
        self.timeline_start <= *t && *t < self.timeline_end
    }

    fn contains_source(&self, s: &Seconds) -> bool {
        self.source_start <= *s && *s < self.source_end
    }
}

/// An ordered, non-overlapping set of timeline→source spans.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TimeMap {
    spans: Vec<TimeSpan>,
}

impl TimeMap {
    pub fn new(spans: Vec<TimeSpan>) -> Result<Self, TimeMapError> {
        for pair in spans.windows(2) {
            if pair[0].timeline_end > pair[1].timeline_start {
                return Err(TimeMapError::Overlap);
            }
        }
        Ok(Self { spans })
    }

    pub fn spans(&self) -> &[TimeSpan] {
        &self.spans
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Timeline time → source time, or `None` in a gap.
    pub fn map(&self, timeline: &Seconds) -> Option<Seconds> {
        self.spans
            .iter()
            .find(|span| span.contains_timeline(timeline))
            .and_then(|span| span.map_endpoint(timeline))
    }

    /// Source time → timeline time, or `None` when no span reads it.
    pub fn inverse(&self, source: &Seconds) -> Option<Seconds> {
        self.spans
            .iter()
            .find(|span| span.contains_source(source))
            .and_then(|span| span.inverse_endpoint(source))
    }

    /// Clip the map to a timeline range. Gaps stay gaps.
    pub fn trim(&self, start: &Seconds, end: &Seconds) -> Result<Self, TimeMapError> {
        if start >= end {
            return Ok(Self::default());
        }
        let mut spans = Vec::new();
        for span in &self.spans {
            let lo = span.timeline_start.max(*start);
            let hi = span.timeline_end.min(*end);
            if lo >= hi {
                continue;
            }
            let source_lo = span.map_endpoint(&lo).ok_or(TimeMapError::Overflow)?;
            let source_hi = span.map_endpoint(&hi).ok_or(TimeMapError::Overflow)?;
            spans.push(TimeSpan::new(lo, hi, source_lo, source_hi)?);
        }
        Self::new(spans)
    }

    /// Map a half-open timeline range to one or more source ranges. A range
    /// that crosses a gap yields the pieces on either side; a range wholly in
    /// a gap yields an empty vector.
    pub fn map_range(&self, start: &Seconds, end: &Seconds) -> Vec<(Seconds, Seconds)> {
        if start >= end {
            return Vec::new();
        }
        let mut ranges = Vec::new();
        for span in &self.spans {
            let lo = span.timeline_start.max(*start);
            let hi = span.timeline_end.min(*end);
            if lo >= hi {
                continue;
            }
            let (Some(source_lo), Some(source_hi)) =
                (span.map_endpoint(&lo), span.map_endpoint(&hi))
            else {
                continue;
            };
            ranges.push((source_lo, source_hi));
        }
        ranges
    }

    /// `self` then `outer`: timeline of `self` → source of `outer`'s timeline
    /// → final source. Gaps in either map stay gaps.
    pub fn compose(&self, outer: &TimeMap) -> Result<Self, TimeMapError> {
        let mut spans = Vec::new();
        for inner in &self.spans {
            for outer_span in &outer.spans {
                let lo = inner.source_start.max(outer_span.timeline_start);
                let hi = inner.source_end.min(outer_span.timeline_end);
                if lo >= hi {
                    continue;
                }
                let timeline_lo = inner.inverse_endpoint(&lo).ok_or(TimeMapError::Overflow)?;
                let timeline_hi = inner.inverse_endpoint(&hi).ok_or(TimeMapError::Overflow)?;
                let source_lo = outer_span.map_endpoint(&lo).ok_or(TimeMapError::Overflow)?;
                let source_hi = outer_span.map_endpoint(&hi).ok_or(TimeMapError::Overflow)?;
                spans.push(TimeSpan::new(
                    timeline_lo,
                    timeline_hi,
                    source_lo,
                    source_hi,
                )?);
            }
        }
        Self::new(spans)
    }

    /// Build a map for one track. Transitions cover two sources at once, so
    /// they are holes here until a multi-source map exists; gaps are holes by
    /// definition.
    pub fn from_track(track: &Track) -> Result<Self, TimeMapError> {
        let mut cursor = Seconds::from_integer(0);
        let mut spans = Vec::new();
        for segment in &track.segments {
            let duration = segment.duration();
            if duration <= Seconds::from_integer(0) {
                return Err(TimeMapError::EmptySpan);
            }
            let end = checked_add(&cursor, &duration).ok_or(TimeMapError::Overflow)?;
            match segment {
                Segment::Clip(clip) => {
                    spans.push(TimeSpan::new(cursor, end, clip.asset_in, clip.asset_out)?);
                }
                Segment::Gap(_) | Segment::Transition(_) => {}
            }
            cursor = end;
        }
        Self::new(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::{Clip, Gap, Track, TrackKind};

    fn s(n: i64) -> Seconds {
        Seconds::from_integer(n)
    }

    fn half(seconds: i64) -> Seconds {
        Seconds::new(seconds, 2)
    }

    fn span(t0: i64, t1: i64, s0: i64, s1: i64) -> TimeSpan {
        TimeSpan::new(s(t0), s(t1), s(s0), s(s1)).unwrap()
    }

    #[test]
    fn boundaries_are_half_open_and_exact() {
        let map = TimeMap::new(vec![span(1, 3, 10, 14)]).unwrap();
        assert_eq!(map.map(&s(0)), None);
        assert_eq!(map.map(&s(1)), Some(s(10)));
        assert_eq!(map.map(&s(2)), Some(s(12)));
        assert_eq!(map.map(&s(3)), None, "the end is exclusive");
        assert_eq!(map.inverse(&s(10)), Some(s(1)));
        assert_eq!(map.inverse(&s(13)), Some(half(5)));
        assert_eq!(map.inverse(&s(14)), None);
    }

    #[test]
    fn speed_is_the_source_per_timeline_ratio() {
        // 2 s of timeline reads 4 s of source (2x speed).
        let map = TimeMap::new(vec![span(5, 7, 10, 14)]).unwrap();
        assert_eq!(map.map(&s(6)), Some(s(12)));
        assert_eq!(map.inverse(&s(12)), Some(s(6)));
    }

    #[test]
    fn gaps_split_ranges_and_trim_keeps_the_overlap() {
        let map = TimeMap::new(vec![span(0, 2, 10, 12), span(4, 6, 20, 22)]).unwrap();
        assert_eq!(map.map(&s(2)), None);
        assert_eq!(map.map(&s(5)), Some(s(21)));
        assert_eq!(
            map.map_range(&s(1), &s(5)),
            vec![(s(11), s(12)), (s(20), s(21))]
        );
        let trimmed = map.trim(&s(1), &s(5)).unwrap();
        assert_eq!(trimmed.spans(), &[span(1, 2, 11, 12), span(4, 5, 20, 21),]);
    }

    #[test]
    fn composition_maps_through_a_source_interval() {
        let inner = TimeMap::new(vec![span(0, 2, 10, 12)]).unwrap();
        let outer = TimeMap::new(vec![span(10, 12, 100, 104)]).unwrap();
        let composed = inner.compose(&outer).unwrap();
        assert_eq!(composed.map(&s(1)), Some(s(102)));
        assert_eq!(composed.inverse(&s(103)), Some(half(3)));
    }

    #[test]
    fn a_track_builds_clip_spans_and_leaves_gaps_holes() {
        let track = Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![
                Segment::Clip(Clip::new(1, 1, s(10), s(12))),
                Segment::Gap(Gap { duration: s(1) }),
                Segment::Clip(Clip::new(2, 2, s(20), s(24))),
            ],
        };
        let map = TimeMap::from_track(&track).unwrap();
        assert_eq!(map.map(&s(0)), Some(s(10)));
        assert_eq!(map.map(&s(1)), Some(s(11)));
        assert_eq!(map.map(&s(2)), None);
        assert_eq!(map.map(&s(3)), Some(s(20)));
        assert_eq!(map.map(&s(6)), Some(s(23)));
        assert_eq!(map.map(&s(7)), None, "the end is exclusive");
        assert_eq!(map.spans().len(), 2);
    }

    #[test]
    fn mapping_round_trips_exactly_for_speed_and_offsets() {
        let map = TimeMap::new(vec![span(3, 9, 100, 118)]).unwrap();
        for t in [3, 4, 5, 6, 7, 8] {
            let source = map.map(&s(t)).expect("inside the span");
            assert_eq!(map.inverse(&source), Some(s(t)), "t = {t}");
        }
        assert_eq!(map.map_range(&s(2), &s(2)), Vec::new());
        assert_eq!(map.map_range(&s(10), &s(12)), Vec::new());
    }

    #[test]
    fn invalid_spans_are_refused() {
        assert_eq!(
            TimeMap::new(vec![span(1, 3, 10, 12), span(2, 4, 20, 22)]),
            Err(TimeMapError::Overlap)
        );
        assert_eq!(
            TimeSpan::new(s(1), s(1), s(0), s(1)),
            Err(TimeMapError::EmptySpan)
        );
    }
}
