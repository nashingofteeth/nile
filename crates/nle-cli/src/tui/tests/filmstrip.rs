use super::super::enable_live_monitor;
use super::super::jobs::start_filmstrips;
use super::*;
use crate::filmstrip::FilmstripKey;

// The worker is keyed by the clip's source span, not the whole asset: a
// trim gets a different strip.
#[test]
fn the_worker_starts_with_the_clip_source_span() {
    let mut app = App::new(asset_project(), temp_project_path("filmstrip-scan"));
    app.preview_enabled = true;
    app.filmstrip_enabled = true;
    start_filmstrips(&mut app);
    let job = app.filmstrip_job.as_ref().expect("a filmstrip job");
    assert_eq!(
        job.key,
        FilmstripKey::new(1, &Seconds::from_integer(1), &Seconds::from_integer(4))
    );
}

// `--dump` records the preference without an extractor; the live monitor's
// start is where a waiting "on" becomes real.
#[test]
fn enabling_the_preview_starts_a_waiting_preference() {
    let mut app = App::new(asset_project(), temp_project_path("filmstrip-waiting"));
    app.filmstrip_enabled = true;
    assert!(app.filmstrip_job.is_none(), "no worker without a preview");
    enable_live_monitor(&mut app);
    assert!(app.preview_enabled);
    assert!(
        app.filmstrip_job.is_some(),
        "the waiting preference starts extracting"
    );
}
