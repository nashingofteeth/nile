use super::super::jobs::{ImportJob, ImportMsg, ImportOutcome, poll_import};
use super::*;
use nle_core::media::RawProbe;

fn probe() -> RawProbe {
    asset_project().media[0].probe.clone()
}

// The worker only reports; the main thread applies the batch. It must land
// as one undo entry, and the pool should jump to the last new asset.
#[test]
fn a_probed_batch_is_one_undo_entry_and_selects_the_last_asset() {
    let mut app = App::new(asset_project(), temp_project_path("import-batch"));
    app.loaded = true;
    open_pool_mode(&mut app, 0);
    // Keep the keyframe worker from probing a fixture path that does not
    // exist; the selection logic is what this test exercises.
    app.keyframes.insert(3, Vec::new());
    let fake = FakeMpv::new();
    let (tx, rx) = std::sync::mpsc::channel();
    app.import_job = Some(ImportJob { rx });
    tx.send(ImportMsg::Progress {
        done: 1,
        total: 2,
        path: "/media/one.mp4".into(),
    })
    .unwrap();
    tx.send(ImportMsg::Done {
        outcomes: vec![
            ImportOutcome {
                path: "/media/one.mp4".into(),
                probe: Ok(probe()),
            },
            ImportOutcome {
                path: "/media/two.mp4".into(),
                probe: Ok(probe()),
            },
        ],
        errors: Vec::new(),
    })
    .unwrap();
    poll_import(&mut app, &fake);

    assert_eq!(app.project.media.len(), 3, "two assets added");
    assert_eq!(app.project.depth(), 1, "the batch is one undo entry");
    assert_eq!(app.message, "imported 2 new");
    assert_eq!(pool_sel(&app), Some(2), "the last new row is selected");
    assert!(
        fake.calls().iter().any(|c| matches!(c, Call::Load(_, _))),
        "the pool preview reloaded on the new asset"
    );

    app.project.undo().unwrap();
    assert_eq!(app.project.media.len(), 1, "one undo removes both");
}

// A bad file doesn't abort the batch; the good files import and the failure
// is reported on the message line.
#[test]
fn a_failed_probe_does_not_sink_the_batch() {
    let mut app = App::new(asset_project(), temp_project_path("import-partial"));
    app.loaded = true;
    let fake = FakeMpv::new();
    let (tx, rx) = std::sync::mpsc::channel();
    app.import_job = Some(ImportJob { rx });
    tx.send(ImportMsg::Done {
        outcomes: vec![
            ImportOutcome {
                path: "/media/new.mp4".into(),
                probe: Ok(probe()),
            },
            ImportOutcome {
                path: "/media/bad.mp4".into(),
                probe: Err("not media".to_string()),
            },
        ],
        errors: Vec::new(),
    })
    .unwrap();
    poll_import(&mut app, &fake);

    assert_eq!(app.project.media.len(), 2);
    assert!(app.message.contains("imported 1 new"), "{}", app.message);
    assert!(app.message.contains("bad.mp4"), "{}", app.message);
}

// Importing a file already in the pool is not a new asset and creates no
// undo entry.
#[test]
fn a_duplicate_import_is_reported_not_added() {
    let mut app = App::new(asset_project(), temp_project_path("import-dupe"));
    app.loaded = true;
    let fake = FakeMpv::new();
    let (tx, rx) = std::sync::mpsc::channel();
    app.import_job = Some(ImportJob { rx });
    tx.send(ImportMsg::Done {
        outcomes: vec![ImportOutcome {
            path: "/media/red.mp4".into(),
            probe: Ok(probe()),
        }],
        errors: Vec::new(),
    })
    .unwrap();
    poll_import(&mut app, &fake);

    assert_eq!(app.project.media.len(), 1);
    assert_eq!(app.project.depth(), 0);
    assert!(app.message.contains("already pooled"), "{}", app.message);
}
