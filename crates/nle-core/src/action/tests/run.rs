use super::*;

#[test]
fn edits_record_undo_history() {
    let mut p = fixture();
    let mut reg = None;
    let mut h = History::default();
    let out = run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Split { track: 0, at: 2.5 },
        2.5,
    )
    .unwrap();
    assert!(out.changed && out.playhead.is_none());
    assert!(h.can_undo());
    // Undo returns to the frame the edit was made from.
    let undone = run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
    assert_eq!(undone.playhead, Some(75), "2.5 s @ 30 fps");
    assert_eq!(p.timeline.video_track().unwrap().segments.len(), 4);
}

// `--clip` finds the clip's own track: `effect` used to need an explicit
// `--track`, unlike delete/yank/speed.
#[test]
fn add_effect_finds_the_clips_own_track() {
    let mut p = fixture();
    let vi = p.timeline.add_track(TrackKind::Video);
    p.timeline.tracks[vi].segments = vec![Segment::Clip(Clip {
        id: 9,
        asset: 1,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(1),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];
    let mut reg = None;
    let out = run(
        &mut p,
        &mut reg,
        &Action::AddEffect {
            track: 0, // the default track; clip #9 lives on the second
            clip: 9,
            effect: Effect::Eq {
                brightness: 0.0,
                contrast: 1.0,
                saturation: 1.0,
            },
            index: None,
        },
    )
    .unwrap();
    assert!(out.changed, "{}", out.desc);
    match &p.timeline.tracks[vi].segments[0] {
        Segment::Clip(c) => assert_eq!(c.effects.len(), 1, "the effect landed"),
        other => panic!("expected a clip, got {other:?}"),
    }
}

#[test]
fn a_transition_action_retimes_an_existing_blend() {
    let mut p = fixture();
    let mut reg = None;
    let action = |len| Action::Transition {
        track: 0,
        at: 4.0,
        kind: TransitionKind::Dissolve,
        len,
    };
    let dur = |p: &Project| {
        p.timeline
            .video_track()
            .unwrap()
            .segments
            .iter()
            .find_map(|s| match s {
                Segment::Transition(t) => Some(t.dur),
                _ => None,
            })
            .unwrap()
    };
    run(&mut p, &mut reg, &action(TransitionLen::Frames(30))).unwrap();
    assert_eq!(dur(&p), Seconds::from_integer(1));

    // The original cut is the blend's far edge, and still addresses it.
    let out = run(&mut p, &mut reg, &action(TransitionLen::Frames(15))).unwrap();
    assert!(out.changed);
    assert!(out.desc.contains("15 frames"), "{}", out.desc);
    assert_eq!(dur(&p), Seconds::new(1, 2));

    // A rate is relative to the current length: 50% doubles it.
    run(&mut p, &mut reg, &action(TransitionLen::Percent(50.0))).unwrap();
    assert_eq!(dur(&p), Seconds::from_integer(1));

    // 100% is a no-op, not a history entry.
    let out = run(&mut p, &mut reg, &action(TransitionLen::Percent(100.0))).unwrap();
    assert!(!out.changed);
    assert!(out.desc.contains("unchanged"), "{}", out.desc);

    // A rate with no blend at the position refuses.
    let out = run(
        &mut p,
        &mut reg,
        &Action::Transition {
            track: 0,
            at: 0.5,
            kind: TransitionKind::Dissolve,
            len: TransitionLen::Percent(50.0),
        },
    );
    assert!(matches!(out, Err(EditError::NoTransition(_))));
}

// The cut a transition is added to sits in the middle of the new blend
// (the neighbours overlap there), so the playhead lands on its midpoint —
// not on either edge.
#[test]
fn a_transition_lands_on_its_midpoint() {
    // (at a seam the blend sits on the *preceding* frames: the clip
    // gives up material backward, so the blend here owns [t−dur, t).)
    let mut p = fixture(); // [Gap][Clip #1 1..4][Gap][Clip #2 6..8]
    let mut reg = None;
    let out = run(
        &mut p,
        &mut reg,
        &Action::Transition {
            track: 0,
            at: 4.0,
            kind: TransitionKind::Dissolve,
            len: TransitionLen::Frames(30),
        },
    )
    .unwrap();
    // The clip gives up 1 s, so the blend owns [3, 4): midpoint 3.5.
    assert_eq!(out.playhead, Some(105), "the middle of the blend");
    // Deleting at the blend's first frame still removes the blend and
    // hands the trimmed frames back.
    let out = run(
        &mut p,
        &mut reg,
        &Action::Delete {
            track: 0,
            target: Target::At(3.0),
            ripple: false,
        },
    )
    .unwrap();
    assert!(out.desc.contains("transition"), "{}", out.desc);
    let segs = &p.timeline.video_track().unwrap().segments;
    assert!(
        !segs.iter().any(|s| matches!(s, Segment::Transition(_))),
        "the blend is gone: {segs:?}"
    );
    match &segs[..] {
        [
            Segment::Gap(_),
            Segment::Clip(c),
            Segment::Gap(_),
            Segment::Clip(_),
        ] => {
            assert_eq!(
                (c.asset_in, c.asset_out),
                (0.into(), 3.into()),
                "the clip is whole again"
            );
        }
        other => panic!("expected the clip restored, got {other:?}"),
    }
}

// The frame-grid degenerate case: a ONE-frame blend's midpoint
// (start + half a frame) rounds up out of it — the blend owns a single
// frame, and the playhead must stay on it, not hop past it.
#[test]
fn a_one_frame_blend_lands_on_its_own_frame() {
    let mut p = fixture(); // [Gap][Clip #1 1..4][Gap][Clip #2 6..8]
    let mut reg = None;
    // The blend at a seam sits on the preceding frames (the clip gives
    // up material backward), so a one-frame blend at 4.0 owns [4−1f, 4):
    // frame 119, not 120.
    let out = run(
        &mut p,
        &mut reg,
        &Action::Transition {
            track: 0,
            at: 4.0,
            kind: TransitionKind::Dissolve,
            len: TransitionLen::Frames(1),
        },
    )
    .unwrap();
    assert_eq!(out.playhead, Some(119), "the blend's own frame");
}

#[test]
fn yank_fills_the_register_and_paste_consumes_it() {
    let mut p = fixture();
    let mut reg = None;
    let out = run(
        &mut p,
        &mut reg,
        &Action::Yank {
            track: 0,
            target: Target::Clip(1),
        },
    )
    .unwrap();
    assert!(!out.changed, "a yank writes the register, not the project");
    assert!(matches!(
        reg,
        Some(Register { ref entries })
            if matches!(
                entries.as_slice(),
                [RegisterEntry {
                    segment: Segment::Clip(Clip { id: 1, .. }),
                    ..
                }]
            )
    ));
    // Pasting without a register is an error; with one it inserts.
    assert!(matches!(
        run(
            &mut p,
            &mut None,
            &Action::Paste {
                track: 0,
                at: 7.0,
                ripple: false,
                segment: None,
            },
        )
        .unwrap_err(),
        EditError::RegisterEmpty
    ));
    run(
        &mut p,
        &mut reg,
        &Action::Paste {
            track: 0,
            at: 7.0,
            ripple: true,
            segment: None,
        },
    )
    .unwrap();
    // Pasting inside clip #2 splits it: gap, #1, gap, #2a, pasted, #2b.
    assert_eq!(p.timeline.video_track().unwrap().segments.len(), 6);
}

// A ripple insert is an insert edit for the whole timeline: a separate
// audio/overlay track gets the same gap, so it stays in sync. One undo
// restores every shifted track.
#[test]
fn a_ripple_insert_shifts_every_track() {
    let mut p = fixture();
    let ai = p.timeline.add_track(TrackKind::Audio);
    let a = p.media[0].id;
    p.timeline.track_mut(ai).unwrap().segments = vec![Segment::Clip(Clip {
        id: 99,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(2),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];
    let pasted = Segment::Clip(Clip {
        id: 0,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(1),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    let mut h = History::default();
    let mut reg = None;
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Paste {
            track: 0,
            at: 0.0,
            ripple: true,
            segment: Some(pasted),
        },
        0.0,
    )
    .unwrap();

    let audio = &p.timeline.track(ai).unwrap().segments;
    assert!(
        matches!(audio.first(), Some(Segment::Gap(g)) if g.duration == Seconds::from_integer(1)),
        "the audio track takes a 1 s gap: {audio:?}"
    );
    assert_eq!(
        p.timeline.track(ai).unwrap().start_of(1),
        Seconds::from_integer(1),
        "the audio clip moved with the insert"
    );
    assert_eq!(h.depth(), 1, "the multi-track edit is one undo entry");

    run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
    assert!(
        matches!(p.timeline.track(ai).unwrap().segments.first(), Some(Segment::Clip(c)) if c.id == 99),
        "undo restores the audio track"
    );
    assert_eq!(h.depth(), 0);
}

fn sync_pair() -> (Project, usize) {
    let mut p = Project::new();
    p.fps = Some(Seconds::from_integer(30));
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
    let clip = |id, ain: i64, aout: i64| {
        Segment::Clip(Clip {
            id,
            asset: a,
            asset_in: Seconds::from_integer(ain),
            asset_out: Seconds::from_integer(aout),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })
    };
    // Base: [0..2][2..4]; audio: one 4 s clip.
    p.timeline.video_track_mut().unwrap().segments = vec![clip(1, 0, 2), clip(2, 2, 4)];
    let ai = p.timeline.add_track(TrackKind::Audio);
    p.timeline.track_mut(ai).unwrap().segments = vec![clip(99, 0, 4)];
    (p, ai)
}

fn total(track: &crate::timeline::Track) -> Seconds {
    track.end()
}

#[test]
fn a_ripple_delete_shifts_every_track() {
    let (mut p, ai) = sync_pair();
    let mut h = History::default();
    let mut reg = None;
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Delete {
            track: 0,
            target: Target::Clip(1),
            ripple: true,
        },
        0.0,
    )
    .unwrap();

    let audio = &p.timeline.track(ai).unwrap().segments;
    assert_eq!(
        audio.len(),
        1,
        "the audio clip is cut, not shifted: {audio:?}"
    );
    assert_eq!(
        p.timeline.track(ai).unwrap().start_of(0),
        Seconds::from_integer(0)
    );
    assert_eq!(
        total(p.timeline.track(ai).unwrap()),
        Seconds::from_integer(2)
    );
    assert!(
        matches!(audio.first(), Some(Segment::Clip(c)) if c.asset_in == Seconds::from_integer(2)),
        "the surviving audio starts at its source 2 s mark: {audio:?}"
    );
    assert_eq!(h.depth(), 1, "one undo entry for both tracks");

    run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
    assert_eq!(
        total(p.timeline.track(ai).unwrap()),
        Seconds::from_integer(4),
        "undo restores the audio span"
    );
    assert_eq!(p.timeline.video_track().unwrap().segments.len(), 2);
}

#[test]
fn a_ripple_trim_shifts_every_track() {
    let (mut p, ai) = sync_pair();
    let mut h = History::default();
    let mut reg = None;
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::TrimIn {
            track: 0,
            at: 1.0,
            ripple: true,
        },
        1.0,
    )
    .unwrap();
    assert_eq!(
        total(p.timeline.track(ai).unwrap()),
        Seconds::from_integer(3)
    );
    let audio = &p.timeline.track(ai).unwrap().segments;
    assert_eq!(audio.len(), 2, "the middle second is removed: {audio:?}");
    assert_eq!(
        p.timeline.track(ai).unwrap().start_of(1),
        Seconds::from_integer(1)
    );
    assert!(
        matches!(&audio[1], Segment::Clip(c) if c.asset_in == Seconds::from_integer(2)),
        "{audio:?}"
    );
}

#[test]
fn a_speed_change_shifts_every_track() {
    let (mut p, ai) = sync_pair();
    let mut h = History::default();
    let mut reg = None;
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::SetSpeed {
            track: 0,
            clip: 1,
            rate: Seconds::from_integer(2),
        },
        0.0,
    )
    .unwrap();
    assert_eq!(
        total(p.timeline.track(ai).unwrap()),
        Seconds::from_integer(3)
    );
    let audio = &p.timeline.track(ai).unwrap().segments;
    assert_eq!(
        audio.len(),
        2,
        "the retime frees the second second: {audio:?}"
    );
}

#[test]
fn an_inserted_gap_shifts_every_track() {
    let (mut p, ai) = sync_pair();
    let mut h = History::default();
    let mut reg = None;
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::InsertGap {
            track: 0,
            at: 0.0,
            len: GapLen::Frames(30),
        },
        0.0,
    )
    .unwrap();
    let audio = &p.timeline.track(ai).unwrap().segments;
    assert_eq!(
        total(p.timeline.track(ai).unwrap()),
        Seconds::from_integer(5)
    );
    assert!(
        matches!(audio.first(), Some(Segment::Gap(g)) if g.duration == Seconds::from_integer(1)),
        "the audio starts with the inserted second: {audio:?}"
    );
    assert!(
        matches!(&audio[1], Segment::Clip(c) if c.asset_in == Seconds::from_integer(0)),
        "{audio:?}"
    );
}

#[test]
fn extend_out_shifts_at_the_old_end() {
    let (mut p, ai) = sync_pair();
    // Give the audio track material after the extend point, so the shift is
    // visible; a shorter track ending at the shift point needs no padding.
    p.timeline
        .track_mut(ai)
        .unwrap()
        .segments
        .push(Segment::Clip(Clip {
            id: 100,
            asset: 1,
            asset_in: Seconds::from_integer(4),
            asset_out: Seconds::from_integer(6),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }));
    let mut h = History::default();
    let mut reg = None;
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Extend {
            track: 0,
            target: Target::Clip(2),
            edge: Edge::Out,
            to: 5.0,
            ripple: true,
        },
        4.0,
    )
    .unwrap();
    let audio = &p.timeline.track(ai).unwrap().segments;
    assert_eq!(
        total(p.timeline.track(ai).unwrap()),
        Seconds::from_integer(7)
    );
    assert!(
        matches!(&audio[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1)),
        "the tail grows into a gap on the audio track: {audio:?}"
    );
    assert_eq!(
        p.timeline.track(ai).unwrap().start_of(2),
        Seconds::from_integer(5),
        "the audio after the old end shifted by one second"
    );
}

#[test]
fn extend_in_shifts_at_the_clip_start() {
    let (mut p, ai) = sync_pair();
    let mut h = History::default();
    let mut reg = None;
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Extend {
            track: 0,
            target: Target::Clip(2),
            edge: Edge::In,
            to: 1.0,
            ripple: true,
        },
        2.0,
    )
    .unwrap();
    assert_eq!(
        total(p.timeline.track(ai).unwrap()),
        Seconds::from_integer(5)
    );
    let audio = &p.timeline.track(ai).unwrap().segments;
    assert!(
        matches!(&audio[0], Segment::Clip(c) if c.asset_out == Seconds::from_integer(2)),
        "{audio:?}"
    );
    assert!(
        matches!(&audio[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1)),
        "the head grows into a gap at the clip start: {audio:?}"
    );
    assert!(
        matches!(&audio[2], Segment::Clip(c) if c.asset_in == Seconds::from_integer(2)),
        "{audio:?}"
    );
}

fn clip_ids(p: &Project) -> Vec<(usize, u32)> {
    p.timeline
        .tracks
        .iter()
        .enumerate()
        .flat_map(|(ti, t)| {
            t.segments.iter().filter_map(move |s| match s {
                Segment::Clip(c) => Some((ti, c.id)),
                Segment::Transition(t) => Some((ti, t.id)),
                Segment::Gap(_) => None,
            })
        })
        .collect()
}

fn assert_ids_unique(p: &Project) {
    let ids = clip_ids(p);
    let mut sorted = ids.iter().map(|&(_, id)| id).collect::<Vec<_>>();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        ids.len(),
        "clip ids must stay unique across tracks (a split on one track must \
         not reuse an id the base edit just handed out): {ids:?}"
    );
}

// The sync ripple plans its gap inserts against the pre-edit project, so its
// split halves must draw ids past the ones the base edit already took. With a
// shared counter a clip-addressed action (`--clip`, `x`, extend) finds the
// wrong track (`resolve_track` takes the first match).
#[test]
fn a_synced_gap_split_keeps_clip_ids_unique() {
    let (mut p, _) = sync_pair();
    let mut h = History::default();
    let mut reg = None;
    // `at` 1 s is inside the audio clip too, so both tracks split.
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::InsertGap {
            track: 0,
            at: 1.0,
            len: GapLen::Frames(30),
        },
        1.0,
    )
    .unwrap();
    assert_ids_unique(&p);
}

#[test]
fn a_synced_ripple_paste_keeps_clip_ids_unique() {
    let (mut p, _) = sync_pair();
    let mut h = History::default();
    let mut reg = None;
    let pasted = Segment::Clip(Clip {
        id: 0,
        asset: 1,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(1),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Paste {
            track: 0,
            at: 1.0,
            ripple: true,
            segment: Some(pasted),
        },
        1.0,
    )
    .unwrap();
    assert_ids_unique(&p);
}

// The audio track's split half is addressable by id: the delete lands on the
// audio track, not on the base clip that used to share its id.
#[test]
fn a_synced_split_half_deletes_on_its_own_track() {
    let (mut p, ai) = sync_pair();
    let mut h = History::default();
    let mut reg = None;
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::InsertGap {
            track: 0,
            at: 1.0,
            len: GapLen::Frames(30),
        },
        1.0,
    )
    .unwrap();
    // The audio right half is the clip whose source starts at 1 s.
    let audio_id = p
        .timeline
        .track(ai)
        .unwrap()
        .segments
        .iter()
        .find_map(|s| match s {
            Segment::Clip(c) if c.asset_in == Seconds::from_integer(1) => Some(c.id),
            _ => None,
        })
        .expect("the audio track split at 1 s");
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Delete {
            track: ai,
            target: Target::Clip(audio_id),
            ripple: false,
        },
        2.0,
    )
    .unwrap();
    assert!(
        !p.timeline
            .track(ai)
            .unwrap()
            .segments
            .iter()
            .any(|s| matches!(s, Segment::Clip(c) if c.asset_in == Seconds::from_integer(1))),
        "the delete removed the audio split half: {:?}",
        p.timeline.track(ai).unwrap().segments
    );
    // The base track still has both halves (ids 1 and the split's fresh id).
    assert_eq!(
        p.timeline.video_track().unwrap().segments.len(),
        4,
        "the base track was untouched: {:?}",
        p.timeline.video_track().unwrap().segments
    );
}

// An overlay track is not the master sequence: a ripple insert there must
// not push the base video track.
#[test]
fn a_ripple_insert_on_an_overlay_stays_local() {
    let mut p = fixture();
    let vi = p.timeline.add_track(TrackKind::Video);
    let a = p.media[0].id;
    let base_before = p.timeline.track(0).unwrap().segments.clone();
    let pasted = Segment::Clip(Clip {
        id: 0,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(1),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    let mut h = History::default();
    let mut reg = None;
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Paste {
            track: vi,
            at: 0.0,
            ripple: true,
            segment: Some(pasted),
        },
        0.0,
    )
    .unwrap();
    assert_eq!(
        p.timeline.track(0).unwrap().segments,
        base_before,
        "the base track stays put"
    );
    assert_eq!(p.timeline.track(vi).unwrap().segments.len(), 1);
}

// An overwrite paste is deliberately track-local: it does not shift the
// timeline, so separate tracks must keep their positions.
#[test]
fn an_overwrite_paste_leaves_other_tracks_alone() {
    let mut p = fixture();
    let ai = p.timeline.add_track(TrackKind::Audio);
    let a = p.media[0].id;
    let audio_before = vec![Segment::Clip(Clip {
        id: 99,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(2),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];
    p.timeline.track_mut(ai).unwrap().segments = audio_before.clone();
    let pasted = Segment::Clip(Clip {
        id: 0,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(1),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    let mut h = History::default();
    let mut reg = None;
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Paste {
            track: 0,
            at: 0.0,
            ripple: false,
            segment: Some(pasted),
        },
        0.0,
    )
    .unwrap();
    assert_eq!(p.timeline.track(ai).unwrap().segments, audio_before);
}

#[test]
fn track_edits_are_undoable() {
    let mut p = fixture();
    let mut reg = None;
    let mut h = History::default();

    // Add → undo → redo.
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::TrackAdd {
            kind: TrackKind::Audio,
        },
        0.0,
    )
    .unwrap();
    assert_eq!(p.timeline.tracks.len(), 2);
    run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
    assert_eq!(p.timeline.tracks.len(), 1, "the add undoes");
    run_action(&mut p, &mut h, &mut reg, &Action::Redo, 0.0).unwrap();
    assert_eq!(p.timeline.tracks.len(), 2, "and redoes");

    // Mute → undo restores the flag.
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::TrackMute { track: 1 },
        0.0,
    )
    .unwrap();
    assert!(p.timeline.track(1).unwrap().muted);
    run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
    assert!(!p.timeline.track(1).unwrap().muted, "the mute undoes");

    // Put a clip on the new track through the catalog (so the stack
    // stays in step), then remove the track: undo brings it back with
    // its clip.
    let a = p.media[0].id;
    let clip = Segment::Clip(Clip {
        id: 0,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(2),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Paste {
            track: 1,
            at: 0.0,
            ripple: true,
            segment: Some(clip),
        },
        0.0,
    )
    .unwrap();
    assert_eq!(p.timeline.track(1).unwrap().segments.len(), 1);
    run_action(&mut p, &mut h, &mut reg, &Action::TrackRm { track: 1 }, 0.0).unwrap();
    assert_eq!(p.timeline.tracks.len(), 1);
    run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
    assert_eq!(p.timeline.tracks.len(), 2, "the track is back");
    assert_eq!(
        p.timeline.track(1).unwrap().segments.len(),
        1,
        "with its clip"
    );
}

#[test]
fn a_refused_track_edit_leaves_the_project_alone() {
    let mut p = fixture();
    let mut h = History::default();
    // Removing the only video track fails validation — and the snapshot
    // is rolled back, not left half-applied.
    let err = h.edit_tracks(&mut p, 0, |p| {
        p.timeline.tracks.remove(0);
        Ok("removed the only video track".to_string())
    });
    assert!(err.is_err(), "a project needs a video track");
    assert_eq!(p.timeline.tracks.len(), 1, "the edit rolled back");
    assert!(!h.can_undo(), "nothing was recorded");
}

#[test]
fn a_track_list_changed_outside_the_stack_refuses_to_undo() {
    let mut p = fixture();
    let mut reg = None;
    let mut h = History::default();
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::TrackAdd {
            kind: TrackKind::Audio,
        },
        0.0,
    )
    .unwrap();
    // Something (a stale sidecar, an out-of-band write) moved the tracks.
    p.timeline.add_track(TrackKind::Video);
    assert!(matches!(
        run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0),
        Err(EditError::History(_))
    ));
}

#[test]
fn purge_is_undoable() {
    let mut p = fixture();
    // Drift clip #1 to 60.4 frames at 30 fps: purge snaps it to 60.
    if let Segment::Clip(c) = &mut p.timeline.tracks[0].segments[1] {
        c.asset_out = c.asset_in + Seconds::new(604, 300);
    }
    let mut reg = None;
    let mut h = History::default();
    let out = run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::PurgeStumps { track: None },
        0.0,
    )
    .unwrap();
    assert!(out.desc.contains("purged"), "{}", out.desc);
    let span = |p: &Project| match &p.timeline.tracks[0].segments[1] {
        Segment::Clip(c) => c.asset_out - c.asset_in,
        other => panic!("expected a clip, got {other:?}"),
    };
    assert_eq!(span(&p), Seconds::from_integer(2), "snapped to 60 frames");
    run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
    assert_eq!(span(&p), Seconds::new(604, 300), "the drift is back");
}

/// Clip ids are unique across tracks: a clip-addressed action finds its
/// own track, so `nile speed ID` needs no `--track` (same as
/// delete/yank/extend).
#[test]
fn a_clip_addressed_action_finds_its_own_track() {
    let mut p = fixture();
    let a = p.media[0].id;
    let ai = p.timeline.add_track(TrackKind::Audio);
    let clip = Segment::Clip(Clip {
        id: 0,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(2),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    let mut h = History::default();
    let mut reg = None;
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Paste {
            track: ai,
            at: 0.0,
            ripple: true,
            segment: Some(clip),
        },
        0.0,
    )
    .unwrap();
    let id = match &p.timeline.track(ai).unwrap().segments[0] {
        Segment::Clip(c) => c.id,
        other => panic!("expected a clip, got {other:?}"),
    };
    // The clip lives on the audio track; `track: 0` must still find it.
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::SetSpeed {
            track: 0,
            clip: id,
            rate: Seconds::from_integer(2),
        },
        0.0,
    )
    .unwrap();
    assert_eq!(
        p.timeline.track(ai).unwrap().segments[0].duration(),
        Seconds::from_integer(1),
        "the audio-track clip was retimed"
    );
}

#[test]
fn extend_moves_an_edge_under_the_same_rules_as_the_tui() {
    let mut p = fixture();
    let mut reg = None;
    run(
        &mut p,
        &mut reg,
        &Action::Extend {
            track: 0,
            target: Target::Clip(1),
            edge: Edge::Out,
            to: 5.0,
            ripple: false,
        },
    )
    .unwrap();
    // The tail grew into the gap; the gap shrank to keep the length.
    let segs = &p.timeline.video_track().unwrap().segments;
    assert!(matches!(segs[1], Segment::Clip(Clip { id: 1, .. })));
    assert_eq!(
        crate::extend::clip_span(&p, 1, 0),
        Some((1.0, 5.0)),
        "{segs:?}"
    );
}

#[test]
fn range_delete_and_paste_are_catalog_actions() {
    let mut p = fixture(); // [clip #1][gap][clip #2]
    let mut reg = None;

    let out = run(
        &mut p,
        &mut reg,
        &Action::DeleteRange {
            track: 0,
            start: 0,
            end: 1,
        },
    )
    .unwrap();
    assert!(out.changed);
    assert_eq!(out.playhead, Some(0), "the range start is the landing");
    assert_eq!(p.timeline.video_track().unwrap().segments.len(), 2);

    // Yank [clip #2, gap] into the shared register (the TUI's visual range).
    let out = run(
        &mut p,
        &mut reg,
        &Action::YankRange {
            track: 0,
            start: 0,
            end: 1,
        },
    )
    .unwrap();
    assert!(!out.changed);
    assert_eq!(reg.as_ref().map(|r| r.entries.len()), Some(2));

    // The range paste is ripple-insert; a normal paste must not silently
    // drop the second entry.
    assert!(
        run(&mut p, &mut reg, &Action::PasteRange { track: 0, at: 10.0 },)
            .unwrap()
            .changed
    );
    assert!(matches!(
        run(
            &mut p,
            &mut reg,
            &Action::Paste {
                track: 0,
                at: 0.0,
                ripple: true,
                segment: None,
            },
        )
        .unwrap_err(),
        EditError::RangeRegister
    ));
}

#[test]
fn a_length_change_on_an_overlay_stays_local() {
    let (mut p, ai) = sync_pair();
    let vi = p.timeline.add_track(TrackKind::Video);
    p.timeline.track_mut(vi).unwrap().segments = vec![Segment::Clip(Clip {
        id: 200,
        asset: 1,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(2),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];
    let base_before = p.timeline.track(0).unwrap().segments.clone();
    let audio_before = p.timeline.track(ai).unwrap().segments.clone();
    let mut h = History::default();
    let mut reg = None;
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Delete {
            track: vi,
            target: Target::At(0.5),
            ripple: true,
        },
        0.5,
    )
    .unwrap();
    assert_eq!(p.timeline.track(0).unwrap().segments, base_before);
    assert_eq!(p.timeline.track(ai).unwrap().segments, audio_before);
}

// A transition boundary on another track cannot be cut without guessing at
// its handles. The base edit still lands; the notice says sync was skipped.
#[test]
fn a_transition_boundary_skips_sync_with_a_notice() {
    let mut p = Project::new();
    p.fps = Some(Seconds::from_integer(30));
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
    let clip = |id, ain: i64, aout: i64| {
        Segment::Clip(Clip {
            id,
            asset: a,
            asset_in: Seconds::from_integer(ain),
            asset_out: Seconds::from_integer(aout),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })
    };
    // The base clip ends one frame inside the overlay's 1 s dissolve.
    p.timeline.video_track_mut().unwrap().segments = vec![clip(1, 0, 2)];
    let vi = p.timeline.add_track(TrackKind::Video);
    p.timeline.track_mut(vi).unwrap().segments = vec![clip(200, 0, 2), clip(201, 2, 4)];
    let batch = edit::add_transition(
        &p,
        vi,
        2.0,
        TransitionKind::Dissolve,
        Seconds::from_integer(1),
    )
    .unwrap();
    let mut th = History::default();
    th.apply(&mut p, batch, 0).unwrap();
    let overlay_before = p.timeline.track(vi).unwrap().segments.clone();
    let ts = p
        .timeline
        .track(vi)
        .unwrap()
        .segments
        .iter()
        .position(|s| matches!(s, Segment::Transition(_)))
        .map(|i| p.timeline.track(vi).unwrap().start_of(i))
        .unwrap();
    let base_end = ts + Seconds::new(1, 30);
    p.timeline.track_mut(0).unwrap().segments = vec![Segment::Clip(Clip {
        id: 1,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: base_end,
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];

    let mut h = History::default();
    let mut reg = None;
    let out = run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Delete {
            track: 0,
            target: Target::At(0.0),
            ripple: true,
        },
        0.0,
    )
    .unwrap();
    assert!(
        out.desc.contains("sync skipped"),
        "the notice names the skipped sync: {}",
        out.desc
    );
    assert_eq!(
        p.timeline.track(vi).unwrap().segments,
        overlay_before,
        "the overlay with the transition stayed put"
    );
    assert!(p.timeline.video_track().unwrap().segments.is_empty());
}
