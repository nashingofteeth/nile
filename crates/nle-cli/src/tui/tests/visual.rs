use super::super::keys::handle_key;
use super::*;

fn visual_app() -> App {
    let mut app = App::new(asset_project(), temp_project_path("visual"));
    app.loaded = true;
    app.playhead = 1.0;
    app
}

#[test]
fn v_selects_whole_segments_and_d_deletes_them_as_one_undo() {
    let mut app = visual_app();
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('v'));
    assert!(matches!(app.mode, Mode::Visual(_)), "{:?}", app.mode);
    handle_key(&mut app, &fake, key('j')); // cut 3: select the first clip + gap
    let before = app.project.timeline.video_track().unwrap().segments.len();
    handle_key(&mut app, &fake, key('d'));
    assert!(matches!(app.mode, Mode::Normal), "{:?}", app.mode);
    assert_eq!(
        app.project.timeline.video_track().unwrap().segments.len(),
        before - 2,
        "the selected clip and gap are one deletion"
    );
    handle_key(&mut app, &fake, key('u'));
    assert_eq!(
        app.project.timeline.video_track().unwrap().segments.len(),
        before,
        "the range delete is one undo step"
    );
}

// Visual mode delegates motions (and counts) to normal mode, so a zoom key
// must consume a pending count the way normal mode's do — otherwise `3` then
// `+` leaves the 3 to multiply the next motion.
#[test]
fn a_visual_zoom_key_consumes_the_count() {
    let mut app = visual_app();
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('v'));
    assert!(matches!(app.mode, Mode::Visual(_)), "{:?}", app.mode);
    handle_key(&mut app, &fake, key('3'));
    assert_eq!(app.count, Some(3));
    handle_key(&mut app, &fake, key('+'));
    assert_eq!(app.count, None, "the zoom key took the count");
    let before = app.playhead;
    handle_key(&mut app, &fake, key('l'));
    assert_eq!(
        app.playhead,
        before + 1.0,
        "the next motion moves one second, not the stale count"
    );
}

#[test]
fn visual_delete_reports_a_merged_seam() {
    // [gap][clip #1][gap][clip #2]: deleting only clip #1 joins the gaps.
    let mut p = asset_project();
    p.timeline.video_track_mut().unwrap().segments = vec![
        TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(1),
        }),
        seg_clip(1, 2),
        TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(1),
        }),
        seg_clip(2, 2),
    ];
    let mut app = App::new(p, temp_project_path("visual-merge"));
    app.loaded = true;
    app.playhead = 1.5; // inside clip #1, after the leading gap

    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('v'));
    handle_key(&mut app, &fake, key('d'));
    assert!(
        app.message.contains("merged the new seam"),
        "the notice keeps the core batch description: {}",
        app.message
    );
    let segs = &app.project.timeline.video_track().unwrap().segments;
    assert_eq!(segs.len(), 2, "{segs:?}");
    assert!(matches!(&segs[0], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(2)));
}

#[test]
fn visual_yank_pastes_at_a_cut_and_folds_the_gap() {
    let mut app = visual_app();
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('v'));
    handle_key(&mut app, &fake, key('j')); // select clip + gap
    handle_key(&mut app, &fake, key('y'));
    assert_eq!(
        app.register.as_ref().map(|r| r.entries.len()),
        Some(2),
        "the shared register holds the range"
    );
    let before = app.project.timeline.video_track().unwrap().segments.len();
    handle_key(&mut app, &fake, key('p'));
    assert!(matches!(app.mode, Mode::Normal), "{:?}", app.mode);
    assert_eq!(
        app.project.timeline.video_track().unwrap().segments.len(),
        before + 1,
        "the copied clip is inserted and the copied gap folds into the cut gap"
    );
    let segs = &app.project.timeline.video_track().unwrap().segments;
    assert!(
        matches!(&segs[2], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(4)),
        "the copied gap folds into the gap at the cut: {segs:?}"
    );
}

#[test]
fn visual_paste_splits_a_clip_at_the_playhead() {
    let mut app = visual_app();
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('v'));
    handle_key(&mut app, &fake, key('y')); // copy clip #1
    handle_key(&mut app, &fake, key('l')); // playhead 2.0 is inside clip #1
    let before = app.project.timeline.video_track().unwrap().segments.len();
    handle_key(&mut app, &fake, key('p'));
    assert!(matches!(app.mode, Mode::Normal), "{:?}", app.mode);
    assert_eq!(
        app.project.timeline.video_track().unwrap().segments.len(),
        before + 2,
        "the target clip splits and the range lands in the middle"
    );
    assert!(app.message.contains("pasted"), "{}", app.message);
    assert!(app.message.contains("split clip #1"), "{}", app.message);

    handle_key(&mut app, &fake, key('u'));
    assert_eq!(
        app.project.timeline.video_track().unwrap().segments.len(),
        before,
        "split + insert undo as one step"
    );
}

#[test]
fn visual_paste_splits_a_gap_and_coalesces_the_edges() {
    let mut app = visual_app();
    app.playhead = 3.0; // the fixture's gap
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('v'));
    handle_key(&mut app, &fake, key('y')); // copy the 2 s gap
    handle_key(&mut app, &fake, key('l')); // playhead 4.0, still in the gap
    let end_before = app.project.timeline.end();
    handle_key(&mut app, &fake, key('p'));
    assert!(matches!(app.mode, Mode::Normal), "{:?}", app.mode);
    assert!(app.message.contains("split the gap"), "{}", app.message);
    let segs = &app.project.timeline.video_track().unwrap().segments;
    assert_eq!(segs.len(), 3, "no adjacent gaps: {segs:?}");
    assert!(matches!(&segs[1], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(4)));
    assert_eq!(
        app.project.timeline.end(),
        end_before + Seconds::from_integer(2),
        "the pasted gap lengthens the timeline"
    );
}

#[test]
fn visual_paste_inside_a_transition_is_refused() {
    let mut app = App::new(blend_pair(), temp_project_path("visual-transition"));
    app.loaded = true;
    app.playhead = 2.5; // inside the 1 s dissolve around the 3 s cut
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('v'));
    handle_key(&mut app, &fake, key('y'));
    let before = app.project.timeline.video_track().unwrap().segments.len();
    handle_key(&mut app, &fake, key('p'));
    assert!(matches!(app.mode, Mode::Visual(_)), "stay in visual");
    assert_eq!(
        app.project.timeline.video_track().unwrap().segments.len(),
        before
    );
    assert!(app.message.contains("transition"), "{}", app.message);
}
