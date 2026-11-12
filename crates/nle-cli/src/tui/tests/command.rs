use super::super::jobs::start_job;
use super::super::keys::command::{import_command, parse_defaults, run_command, split_paths};
use super::super::keys::handle_key;
use super::super::open_import_fallback;
use super::*;
use nle_core::time::Seconds;
use nle_core::{Action, Target};

#[test]
fn track_command_adds_mutes_and_removes() {
    let path = std::env::temp_dir().join(format!(
        "nle-track-command-test-{}.json",
        std::process::id()
    ));
    asset_project().save(&path).unwrap();
    let mut app = App::new(asset_project(), path.clone());
    assert!(cmd(&mut app, "track add audio").contains("a1"));
    assert_eq!(app.project.timeline.tracks.len(), 2);
    assert_eq!(app.active_track, 1, "the new track becomes active");
    assert!(cmd(&mut app, "track mute 2").contains("muted"));
    assert!(app.project.timeline.track(1).unwrap().muted);
    // Refuses to remove the only video track…
    assert!(cmd(&mut app, "track rm 1").contains("only video"));
    // …but removes the empty audio track.
    assert!(cmd(&mut app, "track rm 2").contains("removed"));
    assert_eq!(app.project.timeline.tracks.len(), 1);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::History::sidecar_path(&path));
}

#[test]
fn import_command_reports_usage_and_parses_quoted_paths() {
    let mut app = App::new(asset_project(), temp_project_path("import-cmd"));
    assert!(import_command(&mut app, "import").contains("usage"));
    assert!(
        import_command(&mut app, "import \"unterminated").contains("unterminated"),
        "a broken quote is refused before a job starts"
    );
    assert_eq!(
        split_paths(r#"one.mp4 "two files.mov" 'three four.mp4'"#).unwrap(),
        vec!["one.mp4", "two files.mov", "three four.mp4"]
    );
    assert_eq!(
        split_paths(r"one\ two.mp4").unwrap(),
        vec!["one two.mp4"],
        "backslash escapes spaces too"
    );
}

#[test]
fn missing_import_paths_are_reported_by_expansion() {
    let (files, errors) =
        crate::cmd::import::expand_import_paths(&[PathBuf::from("/no/such/file.xyz")]);
    assert!(files.is_empty());
    assert!(errors[0].contains("not found"), "{errors:?}");
}

#[test]
fn tab_completes_an_import_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("solo.mp4"), b"").unwrap();
    let mut app = App::new(asset_project(), temp_project_path("complete"));
    app.mode = Mode::Command(format!("import {}/so", dir.path().display()));
    let fake = FakeMpv::new();
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
    );
    let Mode::Command(line) = &app.mode else {
        panic!("still on the command line")
    };
    assert_eq!(line, &format!("import {}/solo.mp4 ", dir.path().display()));
}

#[test]
fn a_directory_import_expands_to_its_immediate_media_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.mp4"), b"").unwrap();
    std::fs::write(dir.path().join("b.txt"), b"").unwrap();
    std::fs::write(dir.path().join("c.mov"), b"").unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    let (files, errors) = crate::cmd::import::expand_import_paths(&[dir.path().to_path_buf()]);
    assert!(errors.is_empty(), "{errors:?}");
    let names: Vec<_> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["a.mp4", "c.mov"]);
}

// The `:cmd` grammar is the CLI's, flag for flag, with --at defaulting
// to the playhead — one parser, three front-ends.
#[test]
fn the_command_line_runs_catalog_actions() {
    let mut app = App::new(asset_project(), temp_project_path("cmd-grammar"));
    app.playhead = 1.0;
    let d = parse_defaults(&app);
    assert_eq!(
        action::parse("delete --at 2 --ripple", &d).unwrap(),
        Action::Delete {
            track: 0,
            target: Target::At(2.0),
            ripple: true,
        }
    );
    assert_eq!(
        action::parse("split", &d).unwrap(),
        Action::Split { track: 0, at: 1.0 }
    );
    assert_eq!(
        action::parse("speed 50", &d).unwrap(),
        Action::SetSpeed {
            track: 0,
            clip: 1,
            rate: Seconds::new(1, 2),
        }
    );
    // Unknown verbs fall through to the front-end's own commands.
    assert!(
        run_command(&mut app, &FakeMpv::new(), "nope")
            .unwrap()
            .contains("unknown")
    );
}

// A yank writes the register the CLI reads (`nile paste`), so a shell
// and the TUI can hand segments back and forth.
#[test]
fn a_yank_writes_the_shared_register_file() {
    let path = temp_project_path("register");
    let mut app = App::new(asset_project(), path.clone());
    app.playhead = 1.0;
    cmd(&mut app, "yank");
    assert!(matches!(
        app.register
            .as_ref()
            .and_then(|r| r.entries.first())
            .map(|e| &e.segment),
        Some(TimelineSegment::Clip(_))
    ));
    assert!(
        nle_core::register_path(&path).exists(),
        "the register sidecar is written on yank"
    );
    let _ = std::fs::remove_file(nle_core::register_path(&path));
}

// The io commands report usage up front and run one job at a time —
// a finished job lands its summary in the message line.
#[test]
fn io_commands_report_usage_and_run_one_job_at_a_time() {
    let mut app = App::new(asset_project(), temp_project_path("io"));
    assert!(cmd(&mut app, "export").contains("usage"));
    assert!(cmd(&mut app, "detect").contains("usage"));
    assert!(cmd(&mut app, "edl-import").contains("usage"));
    assert!(cmd(&mut app, "otio-import").contains("usage"));

    let tx = start_job(&mut app, "export", false).unwrap();
    assert!(
        cmd(&mut app, "export -o out.mp4").contains("already running"),
        "no second job while one is running"
    );
    tx.send(Ok("✓ out.mp4".to_string())).unwrap();
    poll_job(&mut app, &FakeMpv::new());
    assert_eq!(app.message, "✓ out.mp4");
    assert!(app.job.is_none(), "the job is done");
}

// `:cache prune` is an IO command, not an edit: it validates its
// arguments and never touches the project (so no undo entry).
#[test]
fn cache_prune_reports_usage_and_refuses_bad_arguments() {
    let mut app = App::new(asset_project(), temp_project_path("cache"));
    assert!(cmd(&mut app, "cache").contains("usage"));
    assert!(cmd(&mut app, "cache nope").contains("unknown cache command"));
    assert!(cmd(&mut app, "cache prune --days 0").contains("1 or more"));
    assert!(cmd(&mut app, "cache prune --days soon").contains("1 or more"));
    assert!(cmd(&mut app, "cache prune --week").contains("--week"));
    assert!(!app.project.can_undo(), "pruning is not an edit");
}

// `:track rm N` takes the track's clips with it (no "empty it first").
#[test]
fn track_rm_deletes_a_nonempty_track_with_its_clips() {
    let path = std::env::temp_dir().join(format!(
        "nle-track-rm-nonempty-test-{}.json",
        std::process::id()
    ));
    let mut p = asset_project();
    let ai = p.timeline.add_track(TrackKind::Audio);
    p.timeline
        .track_mut(ai)
        .unwrap()
        .segments
        .push(TimelineSegment::Clip(nle_core::Clip::new(
            9,
            1,
            Seconds::from_integer(0),
            Seconds::from_integer(2),
        )));
    p.save(&path).unwrap();
    let mut app = App::new(p, path.clone());
    let msg = cmd(&mut app, "track rm 2");
    assert!(msg.contains("removed") && msg.contains("1 clip"), "{msg}");
    assert_eq!(app.project.timeline.tracks.len(), 1);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::History::sidecar_path(&path));
}

#[test]
fn speed_command_retimes_and_ripples() {
    let mut app = App::new(asset_project(), temp_project_path("speed-cmd"));
    app.loaded = true;
    app.playhead = 0.5; // inside clip #1 [0..3)
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('r')); // -> :speed
    for c in "50".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    // clip #1 at 50%: its 3s of source now occupy 6s of timeline, so the
    // gap and clip #2 slide right (a speed change always ripples).
    let track = app.project.timeline.video_track().unwrap();
    let TimelineSegment::Clip(c) = &track.segments[0] else {
        panic!("expected clip")
    };
    assert_eq!(c.speed, Seconds::new(1, 2));
    assert_eq!(track.segments[0].duration(), Seconds::from_integer(6));
    assert_eq!(track.start_of(1), Seconds::from_integer(6));
}

#[test]
fn purge_command_snaps_a_drifted_clip() {
    // Give clip #1 a fractional (drifted) duration before it becomes a
    // Document: 60.4 frames at 30fps.
    let mut p = asset_project();
    if let TimelineSegment::Clip(c) = &mut p.timeline.video_track_mut().unwrap().segments[0] {
        c.asset_out = c.asset_in + Seconds::new(604, 300);
    }
    let mut app = App::new(p, temp_project_path("purge"));
    let msg = cmd(&mut app, "purge");
    assert!(msg.contains("purged"), "{msg}");
    let TimelineSegment::Clip(c) = &app.project.timeline.video_track().unwrap().segments[0] else {
        panic!("expected clip")
    };
    assert_eq!(
        c.asset_out - c.asset_in,
        Seconds::from_integer(2),
        "snapped to 60 frames"
    );
}

// `:help` names the available commands.
#[test]
fn help_command_lists_commands() {
    let mut app = App::new(asset_project(), temp_project_path("help"));
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key(':'));
    for c in "help".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(app.message.contains(":import"), "{}", app.message);
    assert!(app.message.contains(":track"), "{}", app.message);
}

// A command line opened from the pool (`:`) puts the timeline back on the
// monitor when it closes, or the monitor keeps playing the pooled file
// while the UI shows the timeline.
#[test]
fn a_command_from_the_pool_restores_the_timeline() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), temp_project_path("pool-cmd"));
    app.loaded = true;
    handle_key(&mut app, &fake, key('m')); // open the pool
    handle_key(&mut app, &fake, key(':')); // …and the command line
    assert!(matches!(app.mode, Mode::Command(_)));
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    );
    assert!(matches!(app.mode, Mode::Normal));
    assert!(
        app.reload_requested,
        "the timeline goes back on the monitor"
    );

    // The no-yazi fallback opens the pre-filled import line and remembers
    // that it came from the pool.
    let mut app = App::new(asset_project(), temp_project_path("pool-cmd2"));
    app.loaded = true;
    handle_key(&mut app, &fake, key('m'));
    handle_key(&mut app, &fake, key('a'));
    assert!(app.pending_import_picker);
    open_import_fallback(&mut app);
    assert_eq!(app.mode, Mode::Command("import ".to_string()));
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    );
    assert!(app.reload_requested);

    // A `:` line opened from Normal does not force a reload.
    let mut app = App::new(asset_project(), temp_project_path("pool-cmd3"));
    app.loaded = true;
    handle_key(&mut app, &fake, key(':'));
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    );
    assert!(!app.reload_requested);
}

// `:transition fade` picks the fade-through-black kind.
#[test]
fn transition_command_adds_a_fade() {
    let mut app = App::new(asset_project(), temp_project_path("trans-cmd"));
    app.loaded = true;
    app.playhead = 3.0;
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key(':'));
    for c in "transition fade".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    let track = app.project.timeline.video_track().unwrap();
    assert!(
        track.segments.iter().any(
            |s| matches!(s, TimelineSegment::Transition(t) if t.kind == TransitionKind::FadeBlack)
        ),
        "a fade was added: {:?}",
        track.segments
    );
}

// `:add --track 0` is refused like every other command (tracks are 1-based);
// it used to silently mean track 1.
#[test]
fn colon_add_refuses_track_zero() {
    let mut app = App::new(asset_project(), temp_project_path("add-track0"));
    app.loaded = true;
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key(':'));
    for c in "add red.mp4 --track 0".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(app.message.contains("1-based"), "{}", app.message);
}

// `:effect crop` appends an effect to a clip.
#[test]
fn effect_command_adds_an_effect() {
    let mut app = App::new(asset_project(), temp_project_path("effect-cmd"));
    app.loaded = true;
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key(':'));
    for c in "effect crop --clip 1 --left 10".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    let track = app.project.timeline.video_track().unwrap();
    let TimelineSegment::Clip(c) = &track.segments[0] else {
        panic!("expected a clip")
    };
    assert_eq!(
        c.effects,
        vec![nle_core::timeline::Effect::Crop {
            left: 10.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0
        }]
    );
}

#[test]
fn filmstrip_command_toggles_the_timeline_rows() {
    let mut app = App::new(asset_project(), temp_project_path("filmstrip-cmd"));
    assert!(!app.filmstrip_enabled);
    assert_eq!(cmd(&mut app, "filmstrip"), "filmstrips on");
    assert!(app.filmstrip_enabled);
    assert_eq!(cmd(&mut app, "filmstrip off"), "filmstrips off");
    assert!(!app.filmstrip_enabled);
    assert!(cmd(&mut app, "filmstrip maybe").contains("on|off"));
}
