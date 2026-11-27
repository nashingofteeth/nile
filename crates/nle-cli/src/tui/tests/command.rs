use super::super::actions::select_sequence;
use super::super::jobs::start_job;
use super::super::keys::command::{import_command, parse_defaults, run_command, split_paths};
use super::super::keys::handle_key;
use super::super::open_import_fallback;
use super::*;
use nle_core::expect::CheckResult;
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
    let _ = std::fs::remove_file(nle_core::Histories::sidecar_path(&path));
}

#[test]
fn sequence_command_switches_undo_and_adds() {
    let path = temp_project_path("sequence-command");
    let mut p = asset_project();
    let second = p.add_sequence(
        "Vertical",
        Some(Seconds::new(30, 1)),
        Some(nle_core::VideoSettings {
            width: 1080,
            height: 1920,
        }),
    );
    p.save(&path).unwrap();
    let mut app = App::new(p, path.clone());

    assert!(cmd(&mut app, "split --at 1").contains("split"));
    assert_eq!(app.project.depth(), 1, "the edit landed on sequence 1");

    let switched = cmd(&mut app, &format!("sequence {second}"));
    assert!(switched.contains("Vertical"), "{switched}");
    assert_eq!(app.project.active_id(), second, "the active sequence moved");
    assert_eq!(app.project.depth(), 0, "and so did undo");
    assert_eq!(app.active_track, 0, "tracks are per sequence");

    let refused = cmd(&mut app, "undo");
    assert!(refused.contains("nothing to undo"), "{refused}");
    assert_eq!(
        app.project.depth(),
        0,
        "sequence 1's undo did not replay here"
    );

    assert!(cmd(&mut app, "sequence use 1").contains("Sequence 1"));
    assert_eq!(app.project.depth(), 1, "sequence 1 kept its edit");
    assert!(cmd(&mut app, "undo").contains("split"));
    assert_eq!(app.project.depth(), 0);

    let added = cmd(
        &mut app,
        "sequence add Square --width 720 --height 720 --fps 24",
    );
    assert!(added.contains("added"), "{added}");
    assert_eq!(app.project.sequences.len(), 3);
    let square = app.project.sequences.last().unwrap();
    assert_eq!(square.name, "Square");
    assert_eq!(
        square.video,
        Some(nle_core::VideoSettings {
            width: 720,
            height: 720
        })
    );
    assert_eq!(square.fps, Some(Seconds::from_integer(24)));

    let before = app.project.sequences.len();
    let refused = cmd(&mut app, "sequence add Zero --fps 0");
    assert!(refused.contains("positive"), "{refused}");
    assert_eq!(app.project.sequences.len(), before, "nothing was added");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::Histories::sidecar_path(&path));
}

#[test]
fn sequence_derive_copies_without_switching() {
    let path = temp_project_path("sequence-derive");
    let p = asset_project();
    p.save(&path).unwrap();
    let mut app = App::new(p, path.clone());
    let source_segments = app.project.timeline.tracks[0].segments.len();

    let derived = cmd(
        &mut app,
        "sequence derive --from 1 --name Copy --size 1080x1920 --fps 24",
    );
    assert!(derived.contains("derived from #1"), "{derived}");
    assert!(derived.contains("not active"), "{derived}");
    assert_eq!(app.project.sequences.len(), 2);
    assert_eq!(app.project.active_id(), 1, "the copy is not active");
    let copy = app.project.sequences.last().unwrap();
    assert_eq!(copy.id, 2);
    assert_eq!(copy.name, "Copy");
    assert_eq!(copy.fps, Some(Seconds::from_integer(24)));
    assert_eq!(
        copy.video,
        Some(nle_core::VideoSettings {
            width: 1080,
            height: 1920
        })
    );
    assert_eq!(copy.timeline.tracks[0].segments.len(), source_segments);
    assert_eq!(
        copy.playhead,
        Seconds::from_integer(0),
        "a copy starts at its own head"
    );

    // A snapshot copy, not a live derivation: the source keeps its timeline.
    assert!(cmd(&mut app, "split --at 1").contains("split"));
    assert_eq!(
        app.project.timeline.tracks[0].segments.len(),
        source_segments + 1
    );
    assert_eq!(
        app.project.sequence(2).unwrap().timeline.tracks[0]
            .segments
            .len(),
        source_segments,
        "editing the source left the copy untouched"
    );

    // And its undo is its own: the copy starts empty.
    assert!(cmd(&mut app, "sequence 2").contains("Copy"));
    assert_eq!(app.project.depth(), 0, "the copy has no inherited history");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::Histories::sidecar_path(&path));
}

#[test]
fn sequence_derive_refuses_bad_arguments() {
    let path = temp_project_path("sequence-derive-bad");
    let p = asset_project();
    p.save(&path).unwrap();
    let mut app = App::new(p, path.clone());
    let before = app.project.sequences.len();

    let bad_size = cmd(
        &mut app,
        "sequence derive --from 1 --name Copy --size 1080by1920",
    );
    assert!(bad_size.contains("invalid --size"), "{bad_size}");
    let bad_fps = cmd(&mut app, "sequence derive --from 1 --name Copy --fps 0");
    assert!(bad_fps.contains("--fps must be positive"), "{bad_fps}");
    let missing = cmd(&mut app, "sequence derive --from 9 --name Copy");
    assert!(missing.contains("sequence #9 not found"), "{missing}");
    let no_name = cmd(&mut app, "sequence derive --from 1");
    assert!(no_name.contains("usage: :sequence"), "{no_name}");
    assert_eq!(
        app.project.sequences.len(),
        before,
        "no refused derive left a sequence behind"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::Histories::sidecar_path(&path));
}

#[test]
fn sequence_audio_command_sets_prints_and_undoes() {
    let path = temp_project_path("sequence-audio");
    let p = asset_project();
    p.save(&path).unwrap();
    let mut app = App::new(p, path.clone());

    let shown = cmd(&mut app, "sequence audio");
    assert!(shown.contains("normalize off"), "{shown}");
    assert!(shown.contains("fade-in 0 s"), "{shown}");

    let set = cmd(
        &mut app,
        "sequence audio --normalize --fade-in 0.5 --fade-out 1",
    );
    assert!(set.contains("normalize on"), "{set}");
    let audio = app.project.active_sequence().audio;
    assert!(audio.normalize);
    assert_eq!(audio.fade_in, Seconds::new(1, 2));
    assert_eq!(audio.fade_out, Seconds::from_integer(1));

    let undone = cmd(&mut app, "undo");
    assert!(undone.contains("sequence audio"), "{undone}");
    assert_eq!(
        app.project.active_sequence().audio,
        nle_core::AudioSettings::default()
    );

    let refused = cmd(&mut app, "sequence audio --fade-in -1");
    assert!(refused.contains("invalid --fade-in"), "{refused}");
    let bad = cmd(&mut app, "sequence audio --normalize maybe");
    assert!(bad.contains("expects true|false"), "{bad}");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::Histories::sidecar_path(&path));
}

#[test]
fn switching_flushes_the_outgoing_playhead_and_follows_the_new_one() {
    let path = temp_project_path("sequence-switch-cursor");
    let mut p = asset_project();
    let second = p.add_sequence(
        "Vertical",
        Some(Seconds::new(30, 1)),
        Some(nle_core::VideoSettings {
            width: 1080,
            height: 1920,
        }),
    );
    p.save(&path).unwrap();
    let mut app = App::new(p, path.clone());
    app.playhead = 2.0; // moved, but not yet flushed by the idle timer
    app.timeline_view.zoom = 4.0;
    app.timeline_view.start = 5.0; // the new sequence's 0 s cursor is off-screen

    let notice = cmd(&mut app, &format!("sequence {second}"));
    assert!(notice.contains("Vertical"), "{notice}");
    let saved = Project::load(&path).unwrap();
    assert_eq!(
        saved.sequence(1).unwrap().playhead,
        Seconds::from_integer(2),
        "the sequence we left kept its live cursor"
    );
    assert_eq!(app.playhead, 0.0, "the target's stored cursor is adopted");
    assert!(
        app.timeline_view.start < 1.0,
        "the viewport followed the new playhead (start {})",
        app.timeline_view.start
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::Histories::sidecar_path(&path));
    let _ = std::fs::remove_file(nle_core::register_path(&path));
}

#[test]
fn a_foreign_write_is_not_hidden_by_a_switch_notice() {
    let path = temp_project_path("sequence-switch-guard");
    let mut p = asset_project();
    let second = p.add_sequence("Vertical", None, None);
    p.save(&path).unwrap();
    let mut app = App::new(p, path.clone());
    app.playhead = 1.0;

    // The shell writes first; our switch must adopt, not clobber.
    let mut foreign = Project::load(&path).unwrap();
    foreign.set_playhead(3.0);
    foreign.save(&path).unwrap();

    let notice = cmd(&mut app, &format!("sequence {second}"));
    assert!(notice.contains("not saved"), "{notice}");
    assert!(
        !notice.contains("sequence #2"),
        "a failed switch must not claim success: {notice}"
    );
    assert_eq!(app.project.active_id(), 1, "the reload undid the switch");
    assert_eq!(app.playhead, 3.0, "and adopted the shell's cursor");

    // The add path is honest too: another shell write, then the reload
    // discards the in-memory add.
    let mut foreign = Project::load(&path).unwrap();
    foreign.set_playhead(0.5);
    foreign.save(&path).unwrap();
    let notice = cmd(&mut app, "sequence add Third");
    assert!(notice.contains("not saved"), "{notice}");
    assert!(
        !notice.contains("Third added"),
        "a failed add must not claim success: {notice}"
    );
    assert_eq!(app.project.sequences.len(), 2);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::Histories::sidecar_path(&path));
    let _ = std::fs::remove_file(nle_core::register_path(&path));
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
        app.project
            .register()
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
    let _ = std::fs::remove_file(nle_core::Histories::sidecar_path(&path));
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
    for c in "effect crop --clip 1 --x 10 --w 90".chars() {
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
            x: 10.0,
            y: 0.0,
            w: 100.0 - 10.0 - 0.0,
            h: 100.0 - 0.0 - 0.0,
            keys: Vec::new(),
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

#[test]
fn ctrl_t_toggles_the_filmstrip_rows() {
    let mut app = App::new(asset_project(), temp_project_path("filmstrip-key"));
    let fake = FakeMpv::new();
    assert!(!app.filmstrip_enabled);
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
    );
    assert!(app.filmstrip_enabled);
    assert_eq!(app.message, "filmstrips on");
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
    );
    assert!(!app.filmstrip_enabled);
    assert_eq!(app.message, "filmstrips off");
}

#[test]
fn note_anchors_the_clip_under_the_playhead() {
    let path = temp_project_path("note-command");
    asset_project().save(&path).unwrap();
    let mut app = App::new(asset_project(), path.clone());
    app.playhead = 1.0; // clip #1 starts at 0, asset_in 1 → source 2 s

    let notice = cmd(&mut app, "note check the cut");
    assert!(notice.contains("added annotation #1"), "{notice}");
    let annotation = &app.project.annotations[0];
    match &annotation.anchor {
        nle_core::Anchor::Source {
            source: nle_core::SourceRef::Asset { id },
            time,
        } => {
            assert_eq!(*id, 1);
            assert_eq!(*time, Seconds::from_integer(2));
        }
        other => panic!("unexpected anchor {other:?}"),
    }
    assert_eq!(app.project.depth(), 1, "one undo step");

    // A source anchor is a media time: splitting under it keeps it put.
    cmd(&mut app, "split --at 2");
    assert_eq!(app.project.annotations.len(), 1);
    assert!(cmd(&mut app, "annotation ls").contains("1 annotation(s)"));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::Histories::sidecar_path(&path));
}

#[test]
fn annotation_ls_reads_the_requested_sequence() {
    let path = temp_project_path("annotation-ls-sequence");
    let mut p = asset_project();
    let second = p.add_sequence("Second", Some(Seconds::new(30, 1)), None);
    p.add_annotation(
        second,
        nle_core::AnnotationKind::Note {
            text: "on the vertical".into(),
        },
        nle_core::Anchor::Source {
            source: nle_core::SourceRef::Asset { id: 1 },
            time: Seconds::from_integer(0),
        },
        nle_core::Provenance {
            actor: nle_core::Actor::Human,
            at: 0,
        },
    )
    .unwrap();
    p.save(&path).unwrap();
    let mut app = App::new(p, path.clone());

    assert!(cmd(&mut app, "annotation ls").contains("no annotations"));
    assert!(cmd(&mut app, "annotation ls --sequence 2").contains("1 annotation(s)"));
    assert!(cmd(&mut app, "annotation ls --sequence 99").contains("not found"));
    assert!(cmd(&mut app, "annotation ls --sequence nope").contains("expects a number"));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::Histories::sidecar_path(&path));
}

#[test]
fn annotation_check_reports_verdicts_and_keeps_them_per_sequence() {
    let path = temp_project_path("annotation-check");
    let dir = tempfile::tempdir().unwrap();
    let mut p = asset_project();
    let second = p.add_sequence("Second", Some(Seconds::new(30, 1)), None);
    // Both spans sit on clip #1, which the report shows: a `cam` for asset 1
    // passes, an `absent` for the same asset fails.
    let anchor = || nle_core::Anchor::Timeline {
        start: nle_core::ClipPoint {
            clip: 1,
            source: Seconds::from_integer(1),
        },
        end: nle_core::ClipPoint {
            clip: 1,
            source: Seconds::from_integer(2),
        },
    };
    let provenance = || nle_core::Provenance {
        actor: nle_core::Actor::Human,
        at: 0,
    };
    let pass = p
        .add_annotation(
            1,
            nle_core::AnnotationKind::Expectation {
                expect: nle_core::Expectation::Cam {
                    asset: Some(1),
                    group: None,
                    tolerance_frames: 0,
                },
            },
            anchor(),
            provenance(),
        )
        .unwrap();
    let fail = p
        .add_annotation(
            1,
            nle_core::AnnotationKind::Expectation {
                expect: nle_core::Expectation::Absent {
                    asset: Some(1),
                    group: None,
                    black: false,
                    tolerance_frames: 0,
                },
            },
            anchor(),
            provenance(),
        )
        .unwrap();
    p.save(&path).unwrap();
    let out = dir.path().join("out.mp4");
    let report = nle_core::report::RenderReport::build(&p, &out, 210, None);
    report
        .save(&nle_core::report::RenderReport::sidecar_path(&out))
        .unwrap();
    let mut app = App::new(p, path.clone());
    let check = |app: &mut App, extra: &str| {
        cmd(
            app,
            &format!("annotation check --report {}{extra}", out.display()),
        )
    };

    // The message is `format_human`'s own summary, so log and message agree.
    let notice = check(&mut app, "");
    assert_eq!(notice, "2 check(s): 1 pass, 1 fail, 0 unverifiable");
    assert_eq!(app.project.depth(), 0, "a query is not an undo entry");
    // Both verdicts are kept under the report's sequence, with the start of
    // the anchor they mark.
    let marks = app.checks.get(&1).expect("the report's sequence");
    assert_eq!(marks.len(), 2);
    assert_eq!(marks[&pass].result, CheckResult::Pass);
    assert_eq!(marks[&pass].start, 0.0);
    assert_eq!(marks[&fail].result, CheckResult::Fail);
    assert_eq!(marks[&fail].start, 0.0);

    // A report belongs to its sequence: switching away and back keeps it,
    // and the sequence we switched to has none of sequence #1's marks.
    assert!(cmd(&mut app, &format!("sequence {second}")).contains("Second"));
    assert_eq!(app.project.active_id(), second);
    assert!(
        app.checks.contains_key(&1),
        "the left sequence keeps its map"
    );
    assert!(!app.checks.contains_key(&second));
    assert!(cmd(&mut app, "sequence 1").contains("#1"));
    assert_eq!(app.project.active_id(), 1);
    assert_eq!(
        app.checks[&1].len(),
        2,
        "the marks return with the sequence"
    );

    // `--id` is repeatable like the CLI's ids and narrows the run; a bad id
    // is a message that leaves the last run intact.
    let narrowed = check(&mut app, &format!(" --id {pass}"));
    assert_eq!(narrowed, "1 check(s): 1 pass, 0 fail, 0 unverifiable");
    assert_eq!(app.checks[&1].len(), 1);
    assert!(app.checks[&1].contains_key(&pass));
    let bad = check(&mut app, " --id nope");
    assert!(bad.contains("--id expects a number"), "{bad}");
    assert_eq!(app.checks[&1].len(), 1, "a bad id keeps the last run");

    // `--report` is required; a missing file is a message, not a panic, and
    // neither a typo nor a mismatched sequence wipes the marks.
    assert!(cmd(&mut app, "annotation check").contains("usage"));
    let missing = dir.path().join("missing.mp4");
    let message = cmd(
        &mut app,
        &format!("annotation check --report {}", missing.display()),
    );
    assert!(message.starts_with("check failed:"), "{message}");
    let wrong = check(&mut app, " --sequence 2");
    assert!(wrong.contains("#1") && wrong.contains("#2"), "{wrong}");
    assert_eq!(app.checks[&1].len(), 1, "the previous verdicts survive");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::Histories::sidecar_path(&path));
}

#[test]
fn check_verdicts_survive_a_switch_and_drop_on_an_edit() {
    let path = temp_project_path("annotation-check-clear");
    let mut p = asset_project();
    let second = p.add_sequence("Second", Some(Seconds::new(30, 1)), None);
    p.save(&path).unwrap();
    let mut app = App::new(p, path.clone());
    let fake = FakeMpv::new();
    let mark = |result, start| CheckMark { result, start };
    let seed = |app: &mut App, sequence: u32, id: u32| {
        app.checks
            .entry(sequence)
            .or_default()
            .insert(id, mark(CheckResult::Pass, 0.0));
    };

    seed(&mut app, 1, 10);
    seed(&mut app, second, 11);
    assert!(select_sequence(&mut app, &fake, second).is_some());
    assert!(
        app.checks.contains_key(&1),
        "the sequence we left keeps its map"
    );
    assert!(select_sequence(&mut app, &fake, 1).is_some());
    assert!(app.checks[&1].contains_key(&10), "it is there on return");
    assert!(app.checks[&second].contains_key(&11));

    // A move is not an edit: the timeline the report describes is unchanged.
    handle_key(&mut app, &fake, key('l'));
    assert_eq!(
        app.checks[&1].len(),
        1,
        "a playhead move keeps the verdicts"
    );

    // A changed edit invalidates only the sequence it changed.
    assert!(cmd(&mut app, "split --at 1").contains("split"));
    assert!(
        !app.checks.contains_key(&1) || app.checks[&1].is_empty(),
        "the edited sequence's marks drop"
    );
    assert!(
        app.checks[&second].contains_key(&11),
        "the other sequence did not change"
    );

    // A reload replaces the project outright: nobody's report survives.
    seed(&mut app, 1, 12);
    super::super::reload_project(&mut app, &fake, false).unwrap();
    assert!(app.checks.is_empty());

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::Histories::sidecar_path(&path));
}

#[test]
fn annotation_add_timeline_pins_the_clip_under_the_position() {
    let path = temp_project_path("annotation-timeline");
    asset_project().save(&path).unwrap();
    let mut app = App::new(asset_project(), path.clone());

    let notice = cmd(&mut app, "annotation add pinned --timeline --at 2.5");
    assert!(notice.contains("added annotation #1"), "{notice}");
    let annotation = &app.project.annotations[0];
    let nle_core::Anchor::Timeline { start, end } = &annotation.anchor else {
        panic!("expected a timeline anchor");
    };
    assert_eq!(start.clip, 1);
    assert_eq!(
        start.source,
        Seconds::from_integer(7) / Seconds::from_integer(2)
    );
    assert_eq!(start, end, "a note is a point");
    assert_eq!(
        app.project.annotation_span(annotation),
        Some((
            Seconds::from_integer(5) / Seconds::from_integer(2),
            Seconds::from_integer(5) / Seconds::from_integer(2)
        ))
    );

    // Split at 2 s: the note moves to the split-off piece, same material.
    cmd(&mut app, "split --at 2");
    let annotation = &app.project.annotations[0];
    let nle_core::Anchor::Timeline { start, .. } = &annotation.anchor else {
        panic!("still a timeline anchor");
    };
    assert_ne!(start.clip, 1);
    let piece = start.clip;
    assert_eq!(
        app.project.annotation_span(annotation),
        Some((
            Seconds::from_integer(5) / Seconds::from_integer(2),
            Seconds::from_integer(5) / Seconds::from_integer(2)
        ))
    );

    // Deleting the clip drops the note and says so in the message line.
    let dropped = cmd(&mut app, &format!("delete --clip {piece}"));
    assert!(dropped.contains("dropped annotation"), "{dropped}");
    assert!(app.project.annotations.is_empty());

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::Histories::sidecar_path(&path));
}

#[test]
fn annotation_add_timeline_range_resolves_both_ends() {
    let path = temp_project_path("annotation-range");
    asset_project().save(&path).unwrap();
    let mut app = App::new(asset_project(), path.clone());

    let notice = cmd(&mut app, "annotation add span --timeline --at 0.5 --to 2.5");
    assert!(notice.contains("added annotation #1"), "{notice}");
    let annotation = &app.project.annotations[0];
    let nle_core::Anchor::Timeline { start, end } = &annotation.anchor else {
        panic!("expected a timeline anchor");
    };
    assert_eq!(start.clip, 1);
    assert_eq!(end.clip, 1);
    assert_eq!(
        end.source,
        Seconds::from_integer(7) / Seconds::from_integer(2)
    );
    assert_eq!(
        app.project.annotation_span(annotation),
        Some((
            Seconds::from_integer(1) / Seconds::from_integer(2),
            Seconds::from_integer(5) / Seconds::from_integer(2)
        ))
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::Histories::sidecar_path(&path));
}
