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
