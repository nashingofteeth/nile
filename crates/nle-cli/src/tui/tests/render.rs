use super::super::keys::extend::plan_for_armed;
use super::super::keys::fx::form_for_effect;
use super::super::keys::handle_key;
use super::super::motion::marker_col;
use super::super::panels::fx::render_transition_form;
use super::super::panels::pool::{asset_metadata, render_pool, render_pool_strip};
use super::super::panels::timeline::{render_timeline, track_parts, track_window_rows};
use super::super::panels::{hints_for, panel_heights, render};
use super::*;
use crate::thumbnails::Thumbnail;
use crate::waveform::Waveform;
use nle_core::edit;
use nle_core::time::Seconds;
use nle_core::timeline::Effect;
use nle_core::{Armed, Project, track_label};
use ratatui::layout::Rect;
use ratatui::style::Color;

// A blend's boundary is a bare colour change (no bar), while a hard
// boundary keeps the left-aligned `▏` that lines up with the marker — a
// dotted box glyph would read as permeable but sits mid-cell.
#[test]
fn a_blend_boundary_has_no_bar() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut app = App::new(asset_project(), temp_project_path("t"));
    let batch = edit::add_transition(
        &app.project,
        0,
        3.0,
        TransitionKind::Dissolve,
        Seconds::from_integer(1),
    )
    .unwrap();
    app.project.apply(batch, 0).unwrap();
    app.playhead = 5.0; // the gap|clip cut, after the blend
    let mut term = Terminal::new(TestBackend::new(60, 6)).unwrap();
    term.draw(|f| {
        render_timeline(
            f,
            Rect {
                x: 0,
                y: 0,
                width: 60,
                height: 6,
            },
            &app,
        )
    })
    .unwrap();
    let buf = term.backend().buffer();
    let row = |y: u16| -> String {
        (0..60)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    };
    let (track, marker) = (row(1), row(2));
    assert!(!track.contains('┆'), "no dotted divider:\n{track}");
    let hard = track.find('▏').expect("a hard divider after the gap");
    let mark = marker.find('▏').expect("the playhead marker");
    assert_eq!(
        hard, mark,
        "the hard divider aligns with the marker:\n{track}\n{marker}"
    );
}

// The dump prints the model state and the grid — the diagnostic path.
#[test]
fn the_dump_prints_state_and_grid() {
    let out = dump(asset_project(), temp_project_path("t"), (40, 8), "");
    assert!(out.contains("mode: Normal"), "{out}");
    assert!(out.contains("=== grid 40x8 ==="), "{out}");
    assert!(out.contains("clip #1"), "the model summary:\n{out}");
    // Keys reach a state first.
    let out = dump(asset_project(), temp_project_path("t"), (40, 8), "ll");
    assert!(out.contains("playhead: 2 s"), "{out}");
}

// The dump is read-only: it runs on a copy, so an editing key changes
// nothing in the source project and a second dump sees the same model.
#[test]
fn the_dump_does_not_touch_the_project() {
    let path = temp_project_path("dump-ro");
    asset_project().save(&path).unwrap();
    let before = std::fs::read(&path).unwrap();
    // `lllx` creates a blend at the 3 s cut — an edit that saves.
    let first = dump(Project::load(&path).unwrap(), path.clone(), (40, 8), "lllx");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "the source project is untouched"
    );
    let second = dump(Project::load(&path).unwrap(), path.clone(), (40, 8), "lllx");
    let model = |s: &str| s.lines().nth(1).unwrap_or_default().to_string();
    assert_eq!(
        model(&first),
        model(&second),
        "the second dump starts from the same model"
    );
    assert!(
        first.starts_with(&format!("project: {}\n", path.display())),
        "the dump names what it diagnosed:\n{first}"
    );
}

#[test]
fn the_dump_does_not_migrate_a_legacy_register() {
    let path = temp_project_path("dump-legacy-register");
    asset_project().save(&path).unwrap();
    let reg = nle_core::Register {
        entries: vec![nle_core::RegisterEntry {
            segment: TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(2),
            }),
            path: None,
        }],
    };
    nle_core::save_register(&path, Some(&reg)).unwrap();
    let new = nle_core::register_path(&path);
    let legacy = path.with_extension("clipboard.json");
    std::fs::copy(&new, &legacy).unwrap();
    std::fs::remove_file(&new).unwrap();

    let _ = dump(Project::load(&path).unwrap(), path.clone(), (40, 8), "");
    assert!(
        !new.exists(),
        "the dump must not copy a legacy register into the source state dir"
    );
    assert!(legacy.exists(), "the legacy sidecar is left alone");
}

#[test]
fn the_dump_restores_the_active_track() {
    use super::super::view_state;
    let path = temp_project_path("dump-track");
    let mut p = asset_project();
    p.timeline.add_track(TrackKind::Audio);
    p.save(&path).unwrap();
    view_state::save(
        &path,
        view_state::SessionState {
            view: Default::default(),
            active_track: 1,
            pool_marks: Default::default(),
        },
    )
    .unwrap();

    let out = dump(Project::load(&path).unwrap(), path, (40, 8), "");
    assert!(
        out.contains("active: a1"),
        "the saved track is active:\n{out}"
    );
}

#[test]
fn the_dump_carries_the_saved_undo_history() {
    let path = temp_project_path("dump-undo");
    let mut doc = Document::new(asset_project());
    doc.edit_tracks(0, |p| {
        p.timeline.add_track(TrackKind::Audio);
        Ok("added track a1".to_string())
    })
    .unwrap();
    doc.save(&path).unwrap();
    doc.save_history(&History::sidecar_path(&path)).unwrap();

    // `u` only works if the dump loaded the sidecar: `App::new` starts with
    // an empty stack, so this is the distinguishing case.
    let out = dump(Project::load(&path).unwrap(), path, (40, 8), "u");
    assert!(out.contains("message: \"undo: added track a1\""), "{out}");
    assert!(!out.contains("  a1"), "the added track is undone:\n{out}");
}

#[test]
fn track_labels_number_within_kind() {
    let mut p = asset_project();
    p.timeline.add_track(TrackKind::Audio); // index 1
    p.timeline.add_track(TrackKind::Video); // index 2
    assert_eq!(track_label(&p, 0), "v1");
    assert_eq!(track_label(&p, 1), "a1");
    assert_eq!(track_label(&p, 2), "v2");
}

#[test]
fn track_window_rows_centre_on_the_active_track() {
    assert_eq!(track_window_rows(&[1; 2], 0, 4), (0, 2), "all fit");
    assert_eq!(track_window_rows(&[1; 10], 0, 4), (0, 4));
    assert_eq!(
        track_window_rows(&[1; 10], 9, 4),
        (6, 4),
        "clamped to the end"
    );
    assert_eq!(track_window_rows(&[1; 10], 5, 4), (3, 4), "centred");
    // Audio tracks take two rows, so fewer tracks fit in the same space.
    assert_eq!(track_window_rows(&[2, 2, 1, 1], 0, 5), (0, 3));
    assert_eq!(track_window_rows(&[2, 2, 1, 1], 2, 5), (1, 3));
}

#[test]
fn multi_track_timeline_draws_a_row_per_track() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut p = asset_project();
    p.timeline.add_track(TrackKind::Audio); // index 1, empty
    let mut app = App::new(p, temp_project_path("t"));
    app.active_track = 1;
    let mut term = Terminal::new(TestBackend::new(60, 6)).unwrap();
    term.draw(|f| {
        render_timeline(
            f,
            Rect {
                x: 0,
                y: 0,
                width: 60,
                height: 6,
            },
            &app,
        )
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    let row = |y: u16| {
        (0..60)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect::<String>()
    };
    // With two tracks the gutter appears: row 1 is v1, row 2 is a1.
    assert!(row(1).contains("v1"), "{:?}", row(1));
    assert!(row(2).contains("a1"), "{:?}", row(2));
    assert!(!row(1).contains("a1"), "{:?}", row(1));
}

// A blend's dividers are permeable — the transition is part of both
// neighbours — so the clip reads as reaching into it.
#[test]
fn a_blend_boundary_is_permeable() {
    let mut app = App::new(asset_project(), temp_project_path("blend-divider"));
    let batch = edit::add_transition(
        &app.project,
        0,
        3.0,
        TransitionKind::Dissolve,
        Seconds::from_integer(1),
    )
    .unwrap();
    app.project.apply(batch, 0).unwrap();
    // [C1 0..2][blend 2..3)[gap 3..5)[C2 5..7)
    let parts = track_parts(&app.project, &app.project.timeline.tracks[0], 7.0);
    assert_eq!(parts.len(), 4, "{parts:?}");
    assert!(!parts[0].3, "the first part has no left divider");
    assert!(parts[1].3, "the blend's left divider is permeable");
    assert!(parts[2].3, "the part after a blend is permeable too");
    assert!(!parts[3].3, "the gap separates C2 from the blend");
}

// A track shorter than the timeline reads as a trailing gap, not blank.
#[test]
fn shorter_track_pads_with_a_trailing_gap() {
    let mut p = asset_project(); // track 0 ends at 7.0
    let a = p.media[0].id;
    let ti = p.timeline.add_track(TrackKind::Video);
    p.timeline.tracks[ti].segments = vec![TimelineSegment::Clip(nle_core::Clip {
        id: 7,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(2),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];
    // The 2 s track is padded to the 7 s timeline with a 5 s gap.
    let parts = track_parts(&p, &p.timeline.tracks[ti], 7.0);
    assert_eq!(parts.len(), 2, "{parts:?}");
    let (dur, label, _, _) = parts.last().unwrap();
    assert_eq!(label, "gap");
    assert!((dur - 5.0).abs() < 1e-9, "{dur}");
    // A track that already reaches the end gains nothing.
    assert_eq!(track_parts(&p, &p.timeline.tracks[0], 7.0).len(), 3);
}

// A clip with effects is marked in the row; the prefix survives a narrow
// cell, where a suffix would be truncated away.
#[test]
fn a_clip_with_effects_is_marked_in_the_track_row() {
    let mut p = asset_project();
    if let TimelineSegment::Clip(c) = &mut p.timeline.tracks[0].segments[0] {
        c.effects = vec![nle_core::timeline::Effect::Eq {
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0,
        }];
    }
    let parts = track_parts(&p, &p.timeline.tracks[0], 7.0);
    assert_eq!(parts[0].1, "*red.mp4", "{parts:?}");
    assert_eq!(parts[1].1, "gap", "only the clip is marked");
}

// The panel is sized so every track fits; the bottom panels are dropped
// before a track is hidden.
#[test]
fn panel_heights_show_every_track_when_they_fit() {
    let (tl, keys, msg) = panel_heights(3, 0, 24);
    assert!(tl >= 3 + 3, "3 tracks + marker + borders: {tl}");
    assert_eq!((keys, msg), (8, 3), "bottom panels still fit");
    // It grows with the track count instead of windowing.
    assert!(panel_heights(6, 0, 24).0 > panel_heights(2, 0, 24).0);
    // Too short: the bottom panels go first.
    assert_eq!(panel_heights(6, 0, 14).1, 0);
    assert_eq!(panel_heights(6, 0, 14).2, 0);
    // An active audio track reserves one more row for its waveform.
    assert_eq!(panel_heights(2, 1, 24).0, panel_heights(2, 0, 24).0 + 1);
}

// The active audio track gets a waveform row; cached peaks render as blocks.
#[test]
fn the_active_audio_track_shows_a_waveform_row() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut p = asset_project();
    p.media[0].probe.streams.push(nle_core::media::RawStream {
        index: 1,
        codec_type: Some("audio".into()),
        codec_name: Some("aac".into()),
        sample_rate: Some("48000".into()),
        channels: Some(2),
        ..Default::default()
    });
    let ti = p.timeline.add_track(TrackKind::Audio);
    p.timeline.tracks[ti].segments = vec![TimelineSegment::Clip(nle_core::Clip {
        id: 2,
        asset: 1,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(3),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];
    let mut app = App::new(p, temp_project_path("wave-render"));
    app.active_track = ti;
    app.waveforms.insert(
        1,
        Waveform {
            peaks: vec![1.0; 8],
            duration: 3.0,
        },
    );
    let mut term = Terminal::new(TestBackend::new(40, 9)).unwrap();
    term.draw(|f| {
        render_timeline(
            f,
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 9,
            },
            &app,
        )
    })
    .unwrap();
    let buf = term.backend().buffer();
    let text: String = (0..9)
        .flat_map(|y| (0..40).map(move |x| buf[(x, y)].symbol().chars().next().unwrap_or(' ')))
        .collect();
    assert!(text.contains('█'), "waveform blocks should render:\n{text}");
}

// Embedded audio in a video track is still audio: the active video track
// gets the same waveform row as an audio track.
#[test]
fn a_video_tracks_embedded_audio_shows_a_waveform_row() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut p = asset_project();
    p.media[0].probe.streams.push(nle_core::media::RawStream {
        index: 1,
        codec_type: Some("audio".into()),
        codec_name: Some("aac".into()),
        sample_rate: Some("48000".into()),
        channels: Some(2),
        ..Default::default()
    });
    let mut app = App::new(p, temp_project_path("wave-video"));
    app.active_track = 0;
    app.waveforms.insert(
        1,
        Waveform {
            peaks: vec![1.0; 8],
            duration: 10.0,
        },
    );
    let mut term = Terminal::new(TestBackend::new(40, 8)).unwrap();
    term.draw(|f| {
        render_timeline(
            f,
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 8,
            },
            &app,
        )
    })
    .unwrap();
    let buf = term.backend().buffer();
    let text: String = (0..8)
        .flat_map(|y| (0..40).map(move |x| buf[(x, y)].symbol().chars().next().unwrap_or(' ')))
        .collect();
    assert!(
        text.contains('█'),
        "video audio should render a waveform:\n{text}"
    );
}

// Every unmuted audio-bearing track gets its own waveform row; a muted
// track keeps its segment row but drops the waveform.
#[test]
fn every_audio_bearing_track_gets_a_waveform_row() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut p = asset_project();
    p.media[0].probe.streams.push(nle_core::media::RawStream {
        index: 1,
        codec_type: Some("audio".into()),
        codec_name: Some("aac".into()),
        sample_rate: Some("48000".into()),
        channels: Some(2),
        ..Default::default()
    });
    let audio = p.timeline.add_track(TrackKind::Audio);
    p.timeline.tracks[audio].segments = vec![seg_clip(2, 3)];
    let muted = p.timeline.add_track(TrackKind::Audio);
    p.timeline.tracks[muted].segments = vec![seg_clip(3, 3)];
    p.timeline.tracks[muted].muted = true;

    let mut app = App::new(p, temp_project_path("wave-all-tracks"));
    app.active_track = audio;
    app.waveforms.insert(
        1,
        Waveform {
            peaks: vec![1.0; 8],
            duration: 10.0,
        },
    );
    let mut term = Terminal::new(TestBackend::new(40, 10)).unwrap();
    term.draw(|f| {
        render_timeline(
            f,
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 10,
            },
            &app,
        )
    })
    .unwrap();
    let buf = term.backend().buffer();
    let rows: Vec<String> = (0..10)
        .map(|y| {
            (0..40)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect()
        })
        .collect();
    assert_eq!(
        rows.iter().filter(|row| row.contains('█')).count(),
        2,
        "embedded video audio + the audio track; the muted track has none:\n{}",
        rows.join("\n")
    );
    assert_eq!(
        rows.iter().filter(|row| row.contains('~')).count(),
        2,
        "one waveform gutter per unmuted audio-bearing track"
    );
}

// A zoomed viewport draws only the visible time window.
#[test]
fn a_zoomed_timeline_draws_only_the_visible_window() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut p = asset_project();
    let (blue, _) = p.add_asset("/media/blue.mp4".into(), p.asset(1).unwrap().probe.clone());
    if let TimelineSegment::Clip(c) = &mut p.timeline.tracks[0].segments[2] {
        c.asset = blue;
    }
    let mut app = App::new(p, temp_project_path("zoom-render"));
    app.loaded = true;
    app.playhead = 5.5;
    app.timeline_view.zoom = 4.0;
    app.timeline_view.start = 5.0; // visible 5.0..6.75 of the 7 s timeline

    let mut term = Terminal::new(TestBackend::new(40, 6)).unwrap();
    term.draw(|f| {
        render_timeline(
            f,
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 6,
            },
            &app,
        )
    })
    .unwrap();
    let buf = term.backend().buffer();
    let text: String = (0..6)
        .flat_map(|y| (0..40).map(move |x| buf[(x, y)].symbol().chars().next().unwrap_or(' ')))
        .collect();
    assert!(
        text.contains("blue.mp4"),
        "the visible clip is drawn:\n{text}"
    );
    assert!(
        !text.contains("red.mp4"),
        "the off-screen first clip is not drawn:\n{text}"
    );
}

// A two-clip track must map each clip's peaks from its own timeline start,
// not from the whole-track x coordinate (the old bug truncated the row).
#[test]
fn a_second_clip_gets_its_own_waveform_region() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut p = asset_project();
    p.media[0].probe.streams.push(nle_core::media::RawStream {
        index: 1,
        codec_type: Some("audio".into()),
        codec_name: Some("aac".into()),
        sample_rate: Some("48000".into()),
        channels: Some(2),
        ..Default::default()
    });
    let mut app = App::new(p, temp_project_path("wave-second"));
    app.active_track = 0;
    app.waveforms.insert(
        1,
        Waveform {
            // Only the first half of a source has signal. If the second
            // clip is mapped from its own start, it has blocks; the old
            // global-x mapping lands in the silent half.
            peaks: vec![1.0; 4].into_iter().chain(vec![0.0; 4]).collect(),
            duration: 10.0,
        },
    );
    let mut term = Terminal::new(TestBackend::new(40, 8)).unwrap();
    term.draw(|f| {
        render_timeline(
            f,
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 8,
            },
            &app,
        )
    })
    .unwrap();
    let buf = term.backend().buffer();
    let row: String = (0..40)
        .map(|x| buf[(x, 2)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert!(
        row.chars().filter(|c| "▁▂▃▄▅▆▇█".contains(*c)).count() > 8,
        "both clips need waveform blocks:\n{row}"
    );
    assert!(row.contains('▏'), "audio cuts need dividers:\n{row}");
}

// The full render shows every track's gutter on a normal terminal.
#[test]
fn all_tracks_are_visible_in_the_timeline_panel() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut p = asset_project();
    p.timeline.add_track(TrackKind::Video); // v2
    p.timeline.add_track(TrackKind::Audio); // a1
    let app = App::new(p, temp_project_path("t"));
    let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
    term.draw(|f| render(f, &app)).unwrap();
    let buf = term.backend().buffer().clone();
    let row = |y: u16| {
        (0..60)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect::<String>()
    };
    // status at y=0, timeline border at y=1, tracks at y=2.., marker last.
    assert!(row(2).contains("v1"), "{:?}", row(2));
    assert!(row(3).contains("v2"), "{:?}", row(3));
    assert!(row(4).contains("a1"), "{:?}", row(4));
}

// The pool's asset list scrolls its viewport so the selected row stays
// visible once there are more assets than fit (a stateless `List` always
// draws from row 0 and lets the highlight walk off the bottom).
#[test]
fn pool_list_scrolls_to_keep_the_selection_visible() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut p = asset_project();
    let probe = p.asset(1).unwrap().probe.clone();
    for i in 2..=20 {
        p.add_asset(format!("/media/a{i}.mp4").into(), probe.clone());
    }
    let mut app = App::new(p, temp_project_path("t"));
    open_pool_mode(&mut app, 19); // the last asset, far past a short pane
    let mut term = Terminal::new(TestBackend::new(60, 8)).unwrap();
    term.draw(|f| render_pool(f, f.area(), &app)).unwrap();
    let buf = term.backend().buffer().clone();
    let text: String = (0..8u16)
        .map(|y| (0..60u16).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("a20.mp4"),
        "selected row must be visible:\n{text}"
    );
    assert!(
        !text.contains("a2.mp4"),
        "list scrolled, early rows gone:\n{text}"
    );
}

// The keys panel reserves six inner rows (panel_heights gives it 8 with
// borders) and wraps at the inner width (78 at 80 columns) — one row of
// margin, the repo's "≲76 columns" rule. A sheet that outgrows the rows
// is silently clipped: the normal sheet had seven lines for six rows,
// and one line wrapped past the width. Count *rendered* rows, so wrapped
// prose (the effects form's description) is allowed when it still fits.
#[test]
fn every_mode_hints_fit_the_keys_panel() {
    let cases: Vec<(&str, App)> = vec![
        ("normal", App::new(asset_project(), temp_project_path("t"))),
        ("command", {
            let mut a = App::new(asset_project(), temp_project_path("t"));
            a.mode = Mode::Command("x".into());
            a
        }),
        ("extend", {
            let mut a = App::new(asset_project(), temp_project_path("t"));
            a.mode = Mode::Extend(ExtendState {
                track: 0,
                armed: Armed::Clip { id: 1, head: false },
                edge_pos: 3.0,
                ripple: false,
                lo: 0.0,
                hi: Some(9.0),
                mat: None,
                preview_off: -1.0,
            });
            a
        }),
        ("pool", {
            let mut a = App::new(asset_project(), temp_project_path("t"));
            open_pool_mode(&mut a, 0);
            a
        }),
        ("transition", {
            let mut a = App::new(asset_project(), temp_project_path("t"));
            a.mode = Mode::Transition(TransitionForm {
                id: 1,
                start: 2.0,
                kind: TransitionKind::Dissolve,
                frames: String::new(),
                current: 30,
            });
            a
        }),
        ("fx list", {
            let mut a = App::new(asset_project(), temp_project_path("t"));
            a.mode = Mode::Fx(FxState {
                clip: 1,
                sel: 0,
                form: None,
            });
            a
        }),
        ("fx form", {
            let mut a = App::new(asset_project(), temp_project_path("t"));
            let effect = Effect::Crop {
                left: 0.0,
                right: 0.0,
                top: 0.0,
                bottom: 0.0,
            };
            a.mode = Mode::Fx(FxState {
                clip: 1,
                sel: 0,
                form: Some(form_for_effect(&effect, 0)),
            });
            a
        }),
    ];
    for (name, app) in &cases {
        let rows: usize = hints_for(app)
            .iter()
            .map(|l| l.chars().count().div_ceil(76))
            .sum();
        assert!(rows <= 6, "[{name}] {rows} rendered rows");
    }
    // At the default 24-row terminal the panel really is six inner rows:
    // if this changes, the ceiling above must follow.
    let (_, keys_h, _) = panel_heights(3, 0, 24);
    assert_eq!(keys_h, 8, "six inner rows for the hint sheet");
}

// While the pool is open the timeline panel is the asset's own strip and
// marker (the preview clock) — playing, paused or ended. The project and
// its insert cursor return only on close.
#[test]
fn pool_strip_replaces_the_timeline_while_the_pool_is_open() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut p = asset_project();
    let probe = p.asset(1).unwrap().probe.clone(); // 10 s, 30 fps
    p.add_asset("/media/pooled.mp4".into(), probe);
    let mut app = App::new(p, temp_project_path("t"));
    open_pool_mode(&mut app, 1); // index 1, id 2 (not the first asset)
    set_pool_pos(&mut app, 5.0); // halfway through the 10 s asset
    app.paused = true;

    let draw = |app: &App| {
        let mut term = Terminal::new(TestBackend::new(40, 6)).unwrap();
        term.draw(|f| render_timeline(f, f.area(), app)).unwrap();
        let buf = term.backend().buffer().clone();
        let text = (0..6u16)
            .map(|y| (0..40u16).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        (text, buf)
    };

    // Paused: the asset strip, so the pool preview and its clock are
    // what the panel describes.
    let (paused, buf) = draw(&app);
    assert!(
        paused.contains("pooled.mp4"),
        "paused shows the asset:\n{paused}"
    );
    assert!(
        !paused.contains("red.mp4"),
        "the project is replaced:\n{paused}"
    );
    // Inner width 38 (40 − borders); 5/10 of it → column 1 + 19, row 2.
    let want = 1 + marker_col(5.0, 10.0, 38) as usize;
    assert_eq!(
        buf[(want as u16, 2u16)].symbol(),
        "▏",
        "marker follows the preview clock:\n{paused}"
    );

    // Playing and at EOF it stays the asset strip.
    app.paused = false;
    let (playing, _) = draw(&app);
    assert!(
        playing.contains("pooled.mp4"),
        "playing shows the asset:\n{playing}"
    );
    app.eof = true;
    let (ended, _) = draw(&app);
    assert!(
        ended.contains("pooled.mp4"),
        "EOF keeps the asset:\n{ended}"
    );
    assert!(
        !ended.contains("red.mp4"),
        "the project waits for close:\n{ended}"
    );
}

#[test]
fn pool_draws_keyframe_ticks_and_a_thumbnail() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut app = App::new(asset_project(), temp_project_path("pool-preview"));
    app.loaded = true;
    open_pool_mode(&mut app, 0);
    app.keyframes.insert(1, vec![2.0, 4.0]);
    app.pool_marks.insert(
        1,
        PoolMarks {
            in_at: Some(1.5),
            out_at: Some(5.0),
        },
    );
    app.thumbnails.insert(
        1,
        Thumbnail::new(vec![
            255;
            (crate::thumbnails::THUMB_WIDTH * crate::thumbnails::THUMB_HEIGHT * 3)
                as usize
        ])
        .unwrap(),
    );

    // The strip marker row shows both keyframe ticks.
    let st = PoolState {
        sel: 0,
        pos: 1.0,
        resume: 0.0,
    };
    let mut term = Terminal::new(TestBackend::new(60, 4)).unwrap();
    term.draw(|f| render_pool_strip(f, f.area(), &app, &st))
        .unwrap();
    let buf = term.backend().buffer();
    let strip: String = (0..4)
        .flat_map(|y| (0..60).map(move |x| buf[(x, y)].symbol()))
        .collect();
    assert_eq!(strip.matches('·').count(), 2, "{strip}");
    assert!(strip.contains('[') && strip.contains(']'), "{strip}");

    // The metadata pane renders the cached frame as half-blocks.
    let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
    term.draw(|f| render_pool(f, f.area(), &app)).unwrap();
    let buf = term.backend().buffer();
    let text: String = (0..20)
        .flat_map(|y| (0..100).map(move |x| buf[(x, y)].symbol()))
        .collect();
    assert!(text.contains('▀'), "thumbnail half-blocks missing:\n{text}");
    assert!(text.contains("keys   2"), "keyframe count missing:\n{text}");
    assert!(
        text.contains("marks  1.500 – 5.000s"),
        "source range missing:\n{text}"
    );
}

// Metadata pane formats the stored probe (no re-probe).
#[test]
fn asset_metadata_lists_streams() {
    let p = asset_project();
    let a = p.asset(1).unwrap();
    let md = asset_metadata(a).join("\n");
    assert!(md.contains("red.mp4"), "{md}");
    assert!(md.contains("320×240"), "{md}");
    assert!(md.contains("h264"), "{md}");
    assert!(md.contains("30/1"), "{md}");
}

// The pool renders list + metadata without panicking.
#[test]
fn media_pool_renders() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut p = asset_project();
    // A second asset so list index (1) differs from asset id (2).
    p.add_asset("/media/blue.mp4".into(), p.asset(1).unwrap().probe.clone());
    let mut app = App::new(p, temp_project_path("t"));
    open_pool_mode(&mut app, 1); // second row

    let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    term.draw(|f| render(f, &app)).unwrap();
    let buf = term.backend().buffer().clone();
    let text: String = (0..20)
        .map(|y| {
            (0..60)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("media pool"), "{text}");
    assert!(text.contains("red.mp4"), "{text}");

    // The highlighted (cyan-bg) POOL row must hold the SECOND asset
    // (blue), not the first — index-based, not id-based. Find the pool
    // block by its title row first.
    let title_y = (0..20u16)
        .find(|&y| {
            let row: String = (0..60).map(|x| buf[(x, y)].symbol().to_string()).collect();
            row.contains("media pool")
        })
        .expect("pool title present");
    let cyan_pool_row = (title_y..20u16).find(|&y| (0..60).any(|x| buf[(x, y)].bg == Color::Cyan));
    let row: String = cyan_pool_row
        .map(|y| (0..60).map(|x| buf[(x, y)].symbol().to_string()).collect())
        .unwrap_or_default();
    assert!(
        row.contains("blue.mp4"),
        "highlight on the selected row: {row:?}"
    );
    assert!(!row.contains("red.mp4"), "not the first row: {row:?}");
}

// The bottom panels: a keys cheat-sheet and a dedicated notice panel.
#[test]
fn hints_and_notice_panels_render() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let render_text = |app: &App| {
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| render(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..24)
            .map(|y| {
                (0..80)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Normal mode: the cheat-sheet and a separate notice panel.
    let mut app = App::new(asset_project(), temp_project_path("t"));
    app.set_message("extend tail of clip #1 (absorbed gap)");
    let text = render_text(&app);
    assert!(text.contains("keys"), "keys panel title: {text}");
    assert!(text.contains("message"), "message panel title: {text}");
    assert!(text.contains("edit"), "cheat-sheet present: {text}");
    assert!(text.contains("absorbed gap"), "notice shown: {text}");

    // Pool mode: the pool hint line replaces the timeline sheet.
    open_pool_mode(&mut app, 0);
    let text = render_text(&app);
    assert!(text.contains("media pool"), "{text}");
    assert!(text.contains("enter insert"), "{text}");
}

// Render test: extend mode overlays the project timeline — the track
// row is the clips (not a source strip) with the playhead notched, and
// the marker row carries the dim material extent, the yellow workable
// span, and the playhead.
#[test]
fn extend_overlays_the_timeline() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let p = asset_project(); // C1 [0..3) src1..4, gap [3..5), C2 [5..7) src0..2
    let plan = plan_for_armed(&p, Armed::Clip { id: 1, head: false }, 3.0, 0).unwrap();
    assert_eq!((plan.lo, plan.hi), (0.0, Some(9.0)));
    assert_eq!(plan.mat, Some((-1.0, 9.0)));

    let mut app = App::new(p, temp_project_path("t"));
    app.playhead = 1.0; // roamed off the armed edge (3.0)
    app.mode = Mode::Extend(ExtendState {
        track: 0,
        armed: plan.armed,
        edge_pos: plan.edge_pos,
        ripple: false,
        lo: plan.lo,
        hi: plan.hi,
        mat: plan.mat,
        preview_off: 2.0,
    });

    let mut term = Terminal::new(TestBackend::new(60, 4)).unwrap();
    let area = Rect {
        x: 0,
        y: 0,
        width: 60,
        height: 4,
    };
    term.draw(|f| render_timeline(f, area, &app)).unwrap();
    let buf = term.backend().buffer().clone();
    let sym = |x: u16, y: u16| -> (String, Color, Color) {
        let c = &buf[(x, y)];
        (c.symbol().to_string(), c.fg, c.bg)
    };

    // Track row is the project timeline: cyan clip #1 (label) then the
    // dim gap, with the roamed playhead notched in yellow.
    let (_, _, b) = sym(3, 1);
    assert_eq!(b, Color::Cyan, "clip cells");
    let (_, _, b) = sym(31, 1);
    assert_eq!(b, Color::DarkGray, "gap cells");
    let (s, f, _) = sym(9, 1);
    assert_eq!((s.as_str(), f), ("▏", Color::Yellow), "roaming notch");

    // Marker row: dim material extent under a yellow workable span
    // (the span [0, 9] covers it, so yellow wins); playhead notch
    // inside. The row rescales to fit the span past the timeline end.
    let (s, f, _) = sym(1, 2);
    assert_eq!((s.as_str(), f), ("─", Color::Yellow), "workable span");
    let (s, f, _) = sym(9, 2);
    assert_eq!((s.as_str(), f), ("▏", Color::Yellow), "playhead");
    let (s, f, _) = sym(58, 2);
    assert_eq!(
        (s.as_str(), f),
        ("─", Color::Yellow),
        "span reaches the far end"
    );
}

// `X` opens the panel on the clip under the playhead; a gap or an audio
// clip is refused with a message instead of an empty panel.
#[test]
fn x_opens_the_effects_panel_on_the_clip_under_the_playhead() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), temp_project_path("fx-open"));
    app.loaded = true;
    app.playhead = 1.0;
    handle_key(&mut app, &fake, key('x'));
    assert!(is_fx(&app));
    assert_eq!(
        fx_state(&app),
        Some(&FxState {
            clip: 1,
            sel: 0,
            form: None
        }),
        "clip #1 is under the playhead"
    );
    assert_eq!(app.message, "effects");

    let mut app = App::new(asset_project(), temp_project_path("fx-gap"));
    app.loaded = true;
    app.playhead = 4.0; // the gap between the clips
    handle_key(&mut app, &fake, key('x'));
    assert!(!is_fx(&app));
    assert!(app.message.contains("onto a clip"), "{}", app.message);

    let mut p = asset_project();
    let ai = p.timeline.add_track(TrackKind::Audio);
    p.timeline.tracks[ai].segments = vec![TimelineSegment::Clip(nle_core::Clip::new(
        9,
        1,
        Seconds::from_integer(0),
        Seconds::from_integer(2),
    ))];
    let mut app = App::new(p, temp_project_path("fx-audio"));
    app.loaded = true;
    app.active_track = ai;
    app.playhead = 0.5;
    handle_key(&mut app, &fake, key('x'));
    assert!(!is_fx(&app));
    assert!(app.message.contains("video clips"), "{}", app.message);
}

// The form shows the current length beside the blank input.
#[test]
fn the_transition_form_renders_current_frames() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut app = App::new(asset_project(), temp_project_path("t"));
    app.mode = Mode::Transition(TransitionForm {
        id: 4,
        start: 2.0,
        kind: TransitionKind::Dissolve,
        frames: "15".into(),
        current: 30,
    });
    let mut term = Terminal::new(TestBackend::new(60, 8)).unwrap();
    term.draw(|f| render_transition_form(f, f.area(), &app))
        .unwrap();
    let buf = term.backend().buffer().clone();
    let text: String = (0..8u16)
        .map(|y| (0..60u16).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("transition #4"), "{text}");
    assert!(text.contains("frames"), "{text}");
    assert!(text.contains("was 30"), "{text}");
}
