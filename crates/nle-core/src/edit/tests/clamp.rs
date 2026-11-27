use super::super::util::split_sides;
use super::*;
use crate::time::Seconds;
use crate::timeline::{
    Clip, Gap, Segment, Track, TrackKind, Transition, TransitionKind, TransitionSource,
};

#[test]
fn split_sides_refuses_sub_frame_pieces() {
    let p = fixture(); // fps 30
    let frame = Seconds::new(1, 30);
    let dur = Seconds::from_integer(2); // 60 frames
    // A quarter frame at either edge counts as "at that edge", so no
    // sliver. (Half a frame rounds up to a frame and is kept.)
    assert_eq!(split_sides(&p, dur, frame / 4), (false, true));
    assert_eq!(split_sides(&p, dur, dur - frame / 4), (true, false));
    // A whole-frame split leaves both pieces.
    assert_eq!(split_sides(&p, dur, frame * 10), (true, true));
}

#[test]
fn purge_stumps_snaps_the_grid_and_drops_sub_frame_segments() {
    let mut p = fixture();
    p.fps = Some(Seconds::new(30, 1));
    let t = p.timeline.video_track_mut().unwrap();
    t.segments = vec![
        // 60.4 frames of clip -> snaps to 60
        Segment::Clip(Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::new(604, 300),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
        // a 0.3-frame stump -> dropped
        Segment::Clip(Clip {
            id: 2,
            asset: 1,
            asset_in: Seconds::from_integer(5),
            asset_out: Seconds::new(501, 100),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
        // 30.6 frames of gap -> snaps to 31
        Segment::Gap(Gap {
            duration: Seconds::new(51, 50),
        }),
    ];
    let (cleaned, dropped) = purge_stumps(p.timeline.video_track().unwrap(), Seconds::new(30, 1));
    assert_eq!(dropped, 1);
    assert_eq!(cleaned.len(), 2);
    let Segment::Clip(c) = &cleaned[0] else {
        panic!("expected clip")
    };
    assert_eq!(c.asset_out, Seconds::from_integer(2), "60 frames");
    let Segment::Gap(g) = &cleaned[1] else {
        panic!("expected gap")
    };
    assert_eq!(g.duration, Seconds::new(31, 30), "31 frames");
}

#[test]
fn purge_stumps_rescales_the_out_point_by_speed() {
    // A 2× clip spans half a second of timeline, which snaps to 15 frames.
    // `purge_stumps` rewrites `asset_out` from that timeline duration, so an
    // absolute assignment (6 → 1/2) or a dropped `c.speed` (6 → 11/2) would
    // both pass a 1× test and fail here.
    let fps = Seconds::from_integer(30);
    let t = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![Segment::Clip(Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(5),
            asset_out: Seconds::from_integer(6),
            speed: Seconds::from_integer(2),
            effects: Vec::new(),
        })],
    };
    let (cleaned, dropped) = purge_stumps(&t, fps);
    assert_eq!(dropped, 0);
    let Segment::Clip(c) = &cleaned[0] else {
        panic!("expected clip")
    };
    assert_eq!(c.duration(), Seconds::new(1, 2), "15 frames of timeline");
    assert_eq!(c.asset_out, Seconds::from_integer(6), "5 + (1/2 × 2)");
}

#[test]
fn clamp_to_media_trims_or_drops_past_the_end() {
    let fps = Seconds::from_integer(30);
    let mut t = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![
            Segment::Clip(Clip::new(
                1,
                1,
                Seconds::from_integer(0),
                Seconds::from_integer(3),
            )),
            Segment::Clip(Clip::new(
                2,
                1,
                Seconds::from_integer(1),
                Seconds::from_integer(4),
            )),
        ],
    };
    // Media is 2s: both out-points overrun and are clamped onto whole
    // frames inside it.
    let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(2)), fps);
    assert_eq!(notes.len(), 2, "{notes:?}");
    let Segment::Clip(c) = &t.segments[0] else {
        panic!("expected clip")
    };
    assert_eq!(c.asset_out, Seconds::from_integer(2));

    // A clip wholly past the media end is dropped.
    let mut t = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![Segment::Clip(Clip::new(
            3,
            1,
            Seconds::from_integer(5),
            Seconds::from_integer(8),
        ))],
    };
    let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(2)), fps);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(t.segments.is_empty());
}

#[test]
fn clamp_to_media_floors_when_rounding_would_pass_the_media_end() {
    let fps = Seconds::from_integer(30);
    let mut t = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![Segment::Clip(Clip::new(
            1,
            1,
            Seconds::from_integer(2),
            Seconds::from_integer(4),
        ))],
    };
    // 3.49s of media: rounding the duration up to 45 frames would reach
    // 3.5s — past the file — so it floors to 44 instead.
    clamp_to_media(&mut t, &|_| Some(Seconds::new(349, 100)), fps);
    let Segment::Clip(c) = &t.segments[0] else {
        panic!("expected clip")
    };
    assert_eq!(c.asset_out - c.asset_in, Seconds::new(44, 30));
    assert!(c.asset_out <= Seconds::new(349, 100));
}

#[test]
fn clamp_to_media_clamps_a_negative_in_point() {
    let fps = Seconds::from_integer(30);
    let mut t = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![Segment::Clip(Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::new(-1, 2), // -0.5s: before the file start
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })],
    };
    let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(3)), fps);
    assert_eq!(notes.len(), 1, "{notes:?}");
    let Segment::Clip(c) = &t.segments[0] else {
        panic!("expected clip")
    };
    assert_eq!(c.asset_in, Seconds::from_integer(0));
    assert_eq!(c.asset_out, Seconds::from_integer(2));
}

#[test]
fn clamp_to_media_scales_by_speed() {
    let fps = Seconds::from_integer(30);
    // A 2× clip: 4s of source occupies 2s of timeline. Media is 3.44s.
    let mut t = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![Segment::Clip(Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(4),
            speed: Seconds::from_integer(2),
            effects: Vec::new(),
        })],
    };
    clamp_to_media(&mut t, &|_| Some(Seconds::new(344, 100)), fps);
    let Segment::Clip(c) = &t.segments[0] else {
        panic!("expected clip")
    };
    // The timeline duration must land on whole frames (51), and the
    // clamped out-point must stay inside the media.
    assert_eq!(c.duration(), Seconds::new(51, 30));
    assert_eq!(c.asset_out, Seconds::new(34, 10));
    assert!(c.asset_out <= Seconds::new(344, 100));
}

#[test]
fn clamp_to_media_ignores_nonpositive_speed() {
    let fps = Seconds::from_integer(30);
    let mut t = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![Segment::Clip(Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(3),
            speed: Seconds::from_integer(0),
            effects: Vec::new(),
        })],
    };
    // No panic, no change — `validate()` is the authority on bad speed.
    let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(2)), fps);
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(t.segments.len(), 1);
}

#[test]
fn clamp_to_media_survives_a_frame_rate_past_the_rational_grid() {
    // i64::MAX fps: one frame is finer than any `Rational64` position on a
    // multi-second span, so `span * fps` does not fit i64 (the old form
    // panicked in debug and wrapped in release).
    let fps = Seconds::new(i64::MAX, 1);
    let mut t = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![
            Segment::Clip(Clip::new(
                1,
                1,
                Seconds::from_integer(0),
                Seconds::from_integer(2),
            )),
            Segment::Transition(Transition {
                id: 2,
                dur: Seconds::from_integer(2),
                kind: TransitionKind::Dissolve,
                a: TransitionSource::Clip {
                    asset: 1,
                    src_in: Seconds::from_integer(0),
                    src_out: Seconds::from_integer(2),
                    effects: Vec::new(),
                },
                b: TransitionSource::Clip {
                    asset: 1,
                    src_in: Seconds::from_integer(2),
                    src_out: Seconds::from_integer(4),
                    effects: Vec::new(),
                },
            }),
        ],
    };
    let before = t.clone();
    let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(10)), fps);
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(
        t, before,
        "inside the media and already the closest expressible spans"
    );
}

#[test]
fn purge_stumps_survives_a_frame_rate_past_the_rational_grid() {
    let p = fixture(); // [clip 2s][gap 1s][clip 2s]
    let fps = Seconds::new(i64::MAX, 1);
    let (cleaned, dropped) = purge_stumps(p.timeline.video_track().unwrap(), fps);
    assert_eq!(dropped, 0, "no span is under an unrepresentable frame");
    assert_eq!(cleaned, p.timeline.video_track().unwrap().segments);
}

#[test]
fn clamp_to_media_drops_a_clip_whose_in_point_is_past_the_media_end() {
    // 10^18 s in-point on a 6 s file: the media-clamped span is hugely
    // negative, so the frame count is unrepresentable at i64::MAX fps too.
    // Neither arm may keep a clip with asset_out <= asset_in.
    let in_ = Seconds::new(10i64.pow(18), 1);
    let mut t = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![Segment::Clip(Clip::new(
            1,
            1,
            in_,
            in_ + Seconds::from_integer(1),
        ))],
    };
    let fps = Seconds::new(i64::MAX, 1);
    let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(6)), fps);
    assert!(
        t.segments.is_empty(),
        "no clip with asset_out <= asset_in survives"
    );
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("holds no whole frame"), "{notes:?}");
}
