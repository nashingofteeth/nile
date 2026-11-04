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

#[test]
fn visual_yank_pastes_at_a_cut_and_keeps_the_range_copy() {
    let mut app = visual_app();
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('v'));
    handle_key(&mut app, &fake, key('j')); // select clip + gap
    handle_key(&mut app, &fake, key('y'));
    assert_eq!(app.visual_register.len(), 2);
    let before = app.project.timeline.video_track().unwrap().segments.len();
    handle_key(&mut app, &fake, key('p'));
    assert!(matches!(app.mode, Mode::Normal), "{:?}", app.mode);
    assert_eq!(
        app.project.timeline.video_track().unwrap().segments.len(),
        before + 2,
        "the visual range is inserted at the cut"
    );
}

#[test]
fn visual_paste_refuses_a_time_inside_a_clip() {
    let mut app = visual_app();
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('v'));
    handle_key(&mut app, &fake, key('y'));
    handle_key(&mut app, &fake, key('l')); // playhead 2 is not a cut
    let before = app.project.timeline.video_track().unwrap().segments.len();
    handle_key(&mut app, &fake, key('p'));
    assert!(matches!(app.mode, Mode::Visual(_)), "stay in visual");
    assert_eq!(
        app.project.timeline.video_track().unwrap().segments.len(),
        before
    );
    assert!(app.message.contains("cut"), "{}", app.message);
}
