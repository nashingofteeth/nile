//! The shared expectation-check seam: one render report in, one
//! [`CheckReport`] out.
//!
//! The CLI, the protocol server and the TUI all ask the same question — "does
//! this render satisfy the expectations on this sequence?" — so the answer is
//! computed here once and each front-end only chooses how to show it. This
//! module prints nothing and sets no exit code: it returns the value and lets
//! the caller decide, exactly like `plan_for_armed`/`apply_extend_plan` in the
//! edit path.
//!
//! [`evaluate_with`] stays the pure verdict; this seam adds only the I/O-shaped
//! parts (find the sidecar, locate the transcript store, pick the annotations).
//! The rules a check follows — tolerances, blend sides, revision staleness,
//! what counts as unverifiable — live in `nle-core/src/expect.rs`; the report
//! it reads is described in `docs/pitfalls/render-report.md`.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use nle_core::expect::{CheckReport, CheckResult, evaluate_with};
use nle_core::report::RenderReport;
use nle_core::{AnnotationKind, AnnotationStatus, Project, ProjectError, TranscriptStore};

use crate::{say, say_err};

/// The report path for either a video (`out.mp4` → `out.mp4.report.json`) or
/// its sidecar (returned unchanged). A sidecar's name already ends in
/// `.report.json`, so appending again would look for a file nobody writes.
///
/// Shared with `nile report`, whose argument is the same either-or path.
pub(crate) fn report_sidecar(path: &Path) -> PathBuf {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".report.json"))
    {
        path.to_path_buf()
    } else {
        RenderReport::sidecar_path(path)
    }
}

/// Evaluate every expectation on the render's sequence against the render
/// described by `report`.
///
/// The sequence defaults to the **report's own** (`report.sequence`), not the
/// project's active one: a report of sequence #2 answers questions about #2
/// even if the editor has since switched to #1. An explicit `Some(s)` must
/// name that same sequence — asking about a different one would evaluate the
/// wrong annotations against the wrong frame map, so it is a clear error
/// naming both. Either way the sequence has to exist in the project.
///
/// `ids` narrows the set to those annotation ids; an empty slice means "every
/// expectation on the sequence". An id that names nothing is not an error — a
/// caller may filter with a stale selection and deserves an empty report
/// rather than a crash.
///
/// `project_path` locates the project's transcript store
/// (`.nile/<stem>/analysis/transcripts`); a check that names no transcript
/// never touches it, and a missing one is an unverifiable detail rather than
/// a seam error.
pub(crate) fn check_with(
    project: &Project,
    project_path: &Path,
    report: &Path,
    sequence: Option<u32>,
    ids: &[u32],
) -> Result<CheckReport> {
    let report = RenderReport::load(&report_sidecar(report))?;
    let sequence = match sequence {
        None => report.sequence,
        Some(requested) if requested != report.sequence => bail!(
            "report rendered sequence #{}, not requested sequence #{requested}",
            report.sequence
        ),
        Some(requested) => requested,
    };
    if project.sequence(sequence).is_none() {
        return Err(ProjectError::SequenceNotFound(sequence).into());
    }
    let analysis = TranscriptStore::at(project_path);
    // `annotations_for` already scopes by sequence; `evaluate_with` still
    // compares the annotation's sequence with the report's, so a render of
    // another sequence fails there instead of being quietly excluded.
    let checks = project
        .annotations_for(sequence)
        .filter(|annotation| matches!(&annotation.kind, AnnotationKind::Expectation { .. }))
        .filter(|annotation| ids.is_empty() || ids.contains(&annotation.id))
        .map(|annotation| evaluate_with(project, &report, annotation, &analysis))
        .collect();
    Ok(CheckReport::new(&report, checks))
}

/// The human wording for a check report, one line per check plus a summary.
///
/// The TUI and the CLI print the same lines; keeping the strings here stops
/// the two front-ends drifting. Deterministic: no timings, no paths, no
/// terminal width.
pub(crate) fn format_human(report: &CheckReport) -> Vec<String> {
    let mut lines = Vec::with_capacity(report.checks().len() + 1);
    for check in report.checks() {
        let glyph = match check.result {
            CheckResult::Pass => "✓",
            CheckResult::Fail => "✗",
            CheckResult::Unverifiable => "?",
        };
        let mut line = format!("{glyph} #{} {}", check.id, check.check);
        if let Some((from, to)) = check.frames {
            line.push_str(&format!(" f{from}..{to}"));
        }
        // A detail is part of the verdict, on every result: a pass can still
        // carry a caveat (`words_complete`'s "proves audio is not cut short,
        // not that words are present"), and dropping it would let a bare ✓
        // overclaim. Checks with nothing to say leave `detail` as `None`.
        if let Some(detail) = &check.detail {
            line.push_str(" — ");
            line.push_str(detail);
        }
        // Resolving acknowledges a result, it does not hide it: the glyph and
        // any failure detail stay, and this suffix says a human has seen it.
        if check.status == AnnotationStatus::Resolved {
            line.push_str(" [resolved]");
        }
        lines.push(line);
    }
    let summary = report.summary();
    lines.push(format!(
        "{} check(s): {} pass, {} fail, {} unverifiable",
        report.checks().len(),
        summary.pass,
        summary.fail,
        summary.unverifiable
    ));
    lines
}

/// The process status a check report earns: 0 when every check passed, 1 when
/// any failed or was unverifiable. Seam errors are the caller's 2.
fn check_exit_code(report: &CheckReport) -> i32 {
    let summary = report.summary();
    if summary.fail + summary.unverifiable > 0 {
        1
    } else {
        0
    }
}

/// `nile annotation check --report FILE [--sequence N] [--id N]… [--json]` —
/// evaluate the sequence's expectations against a render report.
///
/// The exit code is part of the interface (CI gates on it): 0 when every
/// check passes, 1 when any fails **or** is unverifiable (a resolved failure
/// still fails), 2 for a seam error — a missing or corrupt report, a sequence
/// the report does not name, or an `--id` that selects no expectation. clap's
/// own usage errors are already 2.
pub(crate) fn cmd_annotation_check(
    project_path: &Path,
    report: &Path,
    sequence: Option<u32>,
    ids: &[u32],
    json: bool,
) -> Result<()> {
    let project = Project::load(project_path)?;
    let check = match check_with(&project, project_path, report, sequence, ids) {
        Ok(check) => check,
        Err(e) => {
            say_err!("Error: {e}");
            std::process::exit(2);
        }
    };
    // The seam returns an empty report for a stale id so a protocol/TUI caller
    // can show "nothing selected". The CLI is a gate: an id that names no
    // expectation (a typo, a note, another sequence) must not pass silently.
    if !ids.is_empty() && check.checks().is_empty() {
        let wanted = ids
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(", ");
        say_err!(
            "Error: no expectation with id {wanted} on sequence #{} — ids come from `nile annotation ls`",
            check.sequence()
        );
        std::process::exit(2);
    }
    if json {
        say!("{}", serde_json::to_string_pretty(&check)?);
    } else {
        for line in format_human(&check) {
            say!("{line}");
        }
    }
    // `say!` tolerates a closed pipe, but the exit code *is* this command's
    // output: flush before handing the status to the OS so a redirected
    // report arrives whole.
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let code = check_exit_code(&check);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use nle_core::media::{RawFormat, RawProbe, RawStream};
    use nle_core::time::Seconds;
    use nle_core::timeline::{Clip, Segment, Track, TrackKind};
    use nle_core::{
        Actor, Anchor, AnnotationKind, ClipPoint, Expectation, Provenance, Transcript,
        TranscriptStore, VideoSettings, Word,
    };

    fn s(n: i64) -> Seconds {
        Seconds::from_integer(n)
    }

    fn probe(audio: bool) -> RawProbe {
        let mut streams = vec![RawStream {
            index: 0,
            codec_type: Some("video".into()),
            width: Some(1920),
            height: Some(1080),
            r_frame_rate: Some("30/1".into()),
            ..Default::default()
        }];
        if audio {
            streams.push(RawStream {
                index: 1,
                codec_type: Some("audio".into()),
                ..Default::default()
            });
        }
        RawProbe {
            streams,
            format: RawFormat {
                format_name: "mp4".into(),
                duration: Some("60".into()),
                ..Default::default()
            },
        }
    }

    /// One 0–10 s video clip (#1) showing `asset`, so a report built from it
    /// covers 300 frames at 30 fps.
    fn project(asset: u32) -> Project {
        project_with(asset, false)
    }

    /// As [`project`], with `audio` deciding whether the clips probe as audible
    /// — the `words_complete` checks read that, not the frame map.
    fn project_with(asset: u32, audio: bool) -> Project {
        let mut project = Project::new();
        project.fps = Some(Seconds::from_integer(30));
        project.video = Some(VideoSettings {
            width: 1920,
            height: 1080,
        });
        project.add_asset("/media/a.mp4".into(), probe(audio));
        project.add_asset("/media/b.mp4".into(), probe(audio));
        project.timeline.tracks = vec![Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![Segment::Clip(Clip::new(1, asset, s(0), s(10)))],
        }];
        project
    }

    fn cam(asset: u32) -> Expectation {
        Expectation::Cam {
            asset: Some(asset),
            group: None,
            tolerance_frames: 0,
        }
    }

    fn timeline_anchor() -> Anchor {
        Anchor::Timeline {
            start: ClipPoint {
                clip: 1,
                source: s(0),
            },
            end: ClipPoint {
                clip: 1,
                source: s(5),
            },
        }
    }

    fn add(project: &mut Project, expect: Expectation) -> u32 {
        project
            .add_annotation(
                1,
                AnnotationKind::Expectation { expect },
                timeline_anchor(),
                Provenance {
                    actor: Actor::Human,
                    at: 0,
                },
            )
            .expect("valid annotation")
    }

    fn add_note(project: &mut Project) -> u32 {
        project
            .add_annotation(
                1,
                AnnotationKind::Note {
                    text: "look at this".to_string(),
                },
                timeline_anchor(),
                Provenance {
                    actor: Actor::Human,
                    at: 0,
                },
            )
            .expect("valid annotation")
    }

    /// Write `project`'s report beside `out.mp4` in `dir`; return the video
    /// path (the sidecar sits at `sidecar_path(&out)`).
    fn write_report(project: &Project, dir: &Path) -> PathBuf {
        let out = dir.join("out.mp4");
        RenderReport::build(project, &out, 300, None)
            .save(&RenderReport::sidecar_path(&out))
            .expect("write report");
        out
    }

    /// The project file path the seam reads its state directories from. It
    /// need not exist: `state_dir` keys on the literal path until it does.
    fn project_path(dir: &Path) -> PathBuf {
        dir.join("nile.json")
    }

    #[test]
    fn a_matching_cam_passes_and_renders_a_tick() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = project(1);
        let id = add(&mut project, cam(1));
        let out = write_report(&project, dir.path());

        let report =
            check_with(&project, &project_path(dir.path()), &out, None, &[]).expect("check");
        assert_eq!(report.sequence(), 1);
        assert_eq!(report.checks().len(), 1);
        assert_eq!(report.checks()[0].id, id);
        assert_eq!(report.checks()[0].result, CheckResult::Pass);
        assert_eq!(report.checks()[0].frames, Some((0, 150)));
        assert_eq!(report.summary().pass, 1);

        let lines = format_human(&report);
        assert!(lines[0].starts_with('✓'), "{lines:?}");
        assert!(lines[0].contains("cam"), "{lines:?}");
        assert!(lines[0].contains("f0..150"), "{lines:?}");
        assert!(!lines[0].contains("[resolved]"), "open: {lines:?}");
        assert_eq!(lines[1], "1 check(s): 1 pass, 0 fail, 0 unverifiable");
    }

    #[test]
    fn a_cam_on_another_asset_fails_with_the_frame() {
        let dir = tempfile::tempdir().unwrap();
        // The render shows asset #2; the expectation wants #1.
        let rendered = project(2);
        let mut expected = project(1);
        add(&mut expected, cam(1));
        let out = write_report(&rendered, dir.path());

        let report =
            check_with(&expected, &project_path(dir.path()), &out, None, &[]).expect("check");
        let check = &report.checks()[0];
        assert_eq!(check.result, CheckResult::Fail);
        let detail = check.detail.as_deref().expect("detail");
        assert!(detail.contains("frame 0"), "{detail}");
        assert!(detail.contains("asset #2"), "{detail}");
        assert_eq!(report.summary().fail, 1);

        let lines = format_human(&report);
        assert!(lines[0].starts_with('✗'), "{lines:?}");
        assert!(lines[0].contains("frame 0"), "{lines:?}");
    }

    #[test]
    fn a_resolved_failure_still_runs_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let rendered = project(2);
        let mut expected = project(1);
        let id = add(&mut expected, cam(1));
        expected.resolve_annotation(id).expect("resolve");
        let out = write_report(&rendered, dir.path());

        let report =
            check_with(&expected, &project_path(dir.path()), &out, None, &[]).expect("check");
        let check = &report.checks()[0];
        assert_eq!(check.status, AnnotationStatus::Resolved);
        assert_eq!(check.result, CheckResult::Fail);
        assert_eq!(report.summary().fail, 1, "resolving is not a waiver");
        let line = &format_human(&report)[0];
        assert!(line.starts_with('✗'), "{line}");
        assert!(line.contains("frame 0"), "{line}");
        assert!(line.ends_with("[resolved]"), "{line}");
    }

    #[test]
    fn a_transcript_word_check_is_unverifiable() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = project(1);
        add(
            &mut project,
            Expectation::WordsComplete {
                min_coverage: 1.0,
                transcript: Some("t1-0000000000000000".to_string()),
            },
        );
        let out = write_report(&project, dir.path());

        let report =
            check_with(&project, &project_path(dir.path()), &out, None, &[]).expect("check");
        let check = &report.checks()[0];
        assert_eq!(check.result, CheckResult::Unverifiable);
        assert!(
            check
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("not found")),
            "{check:?}"
        );
        assert_eq!(report.summary().unverifiable, 1);
        assert!(format_human(&report)[0].starts_with('?'));
    }

    #[test]
    fn a_stored_transcript_reports_word_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = project_with(1, true);
        // The transcript lives under the project path the seam is given; the
        // annotation names only its content address.
        let hash = TranscriptStore::at(&project_path(dir.path()))
            .import(&Transcript::new(
                1,
                vec![Word {
                    text: "take".to_string(),
                    start: s(1),
                    end: s(2),
                }],
            ))
            .expect("import");
        add(
            &mut project,
            Expectation::WordsComplete {
                min_coverage: 1.0,
                transcript: Some(hash),
            },
        );
        let out = write_report(&project, dir.path());

        let report =
            check_with(&project, &project_path(dir.path()), &out, None, &[]).expect("check");
        assert_eq!(report.checks()[0].result, CheckResult::Pass, "{report:?}");
        assert_eq!(
            report.checks()[0].detail.as_deref(),
            Some("1/1 words complete")
        );
    }

    #[test]
    fn a_transcript_hash_with_no_store_entry_is_unverifiable() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = project_with(1, true);
        add(
            &mut project,
            Expectation::WordsComplete {
                min_coverage: 1.0,
                transcript: Some("t1-0000000000000000".to_string()),
            },
        );
        let out = write_report(&project, dir.path());

        let report =
            check_with(&project, &project_path(dir.path()), &out, None, &[]).expect("check");
        let check = &report.checks()[0];
        assert_eq!(check.result, CheckResult::Unverifiable);
        let detail = check.detail.as_deref().expect("detail");
        assert!(detail.contains("t1-0000000000000000"), "{detail}");
        assert!(detail.contains("not found"), "{detail}");
    }

    #[test]
    fn a_passing_word_check_keeps_its_caveat_on_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = project_with(1, true);
        add(
            &mut project,
            Expectation::WordsComplete {
                min_coverage: 1.0,
                transcript: None,
            },
        );
        let out = write_report(&project, dir.path());

        let report =
            check_with(&project, &project_path(dir.path()), &out, None, &[]).expect("check");
        assert_eq!(report.checks()[0].result, CheckResult::Pass);
        // A bare ✓ would claim more than the check proves: coverage is there,
        // the words are not.
        assert!(
            report.checks()[0]
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("not that words are present")),
            "{:?}",
            report.checks()[0]
        );
        let lines = format_human(&report);
        assert!(lines[0].starts_with('✓'), "{lines:?}");
        assert!(
            lines[0].contains("this proves audio is not cut short"),
            "{lines:?}"
        );
        assert!(lines[0].contains("not that words are present"), "{lines:?}");
    }

    #[test]
    fn ids_select_one_annotation_and_empty_ids_check_all() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = project(1);
        let first = add(&mut project, cam(1));
        let second = add(&mut project, cam(1));
        // A note is not an expectation and never becomes a check.
        add_note(&mut project);
        let out = write_report(&project, dir.path());

        let all = check_with(&project, &project_path(dir.path()), &out, None, &[]).expect("check");
        assert_eq!(all.checks().len(), 2);
        assert_eq!(all.checks()[0].id, first);
        assert_eq!(all.checks()[1].id, second);

        let one =
            check_with(&project, &project_path(dir.path()), &out, None, &[second]).expect("check");
        assert_eq!(one.checks().len(), 1);
        assert_eq!(one.checks()[0].id, second);
        assert_eq!(one.summary().pass, 1);

        // A stale id filters everything out rather than erroring.
        let none =
            check_with(&project, &project_path(dir.path()), &out, None, &[99]).expect("check");
        assert!(none.checks().is_empty());
        assert_eq!(none.summary().pass, 0);
    }

    #[test]
    fn an_explicit_sequence_must_match_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = project(1);
        add(&mut project, cam(1));
        let out = write_report(&project, dir.path());

        // The report is of #1; checking #2 would pair #2's annotations with
        // #1's frame map, so it is refused rather than answered differently.
        let error =
            check_with(&project, &project_path(dir.path()), &out, Some(2), &[]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("#2"), "{text}");
        assert!(text.contains("#1"), "{text}");
    }

    #[test]
    fn the_report_sequence_wins_over_the_active_one() {
        let dir = tempfile::tempdir().unwrap();
        // The render is of sequence #1 (showing asset #2); the editor has
        // since switched to an empty sequence #2.
        let rendered = project(2);
        let mut expected = project(1);
        let id = add(&mut expected, cam(1));
        let second = expected.add_sequence("Second", Some(Seconds::from_integer(30)), None);
        assert_eq!(second, 2);
        expected.select_sequence(second).expect("switch");
        let out = write_report(&rendered, dir.path());

        // `None` follows the report: #1's annotation is checked (and fails on
        // asset #2), not #2's empty set.
        let report =
            check_with(&expected, &project_path(dir.path()), &out, None, &[]).expect("check");
        assert_eq!(report.sequence(), 1, "the report's sequence, not active");
        assert_eq!(report.checks().len(), 1);
        assert_eq!(report.checks()[0].id, id);
        assert_eq!(report.checks()[0].result, CheckResult::Fail);
    }

    #[test]
    fn a_sequence_missing_from_the_project_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = project(1);
        add(&mut project, cam(1));
        let out = write_report(&project, dir.path());
        // A sidecar from another project names sequence #9, which this one
        // does not have.
        let mut moved = RenderReport::load(&RenderReport::sidecar_path(&out)).unwrap();
        moved.sequence = 9;
        moved
            .save(&RenderReport::sidecar_path(&out))
            .expect("rewrite");

        let error = check_with(&project, &project_path(dir.path()), &out, None, &[]).unwrap_err();
        assert!(error.to_string().contains("sequence #9"), "{error}");
        let error =
            check_with(&project, &project_path(dir.path()), &out, Some(9), &[]).unwrap_err();
        assert!(error.to_string().contains("sequence #9"), "{error}");
    }

    #[test]
    fn a_missing_or_corrupt_report_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let project = project(1);

        let missing = dir.path().join("missing.mp4");
        assert!(check_with(&project, &project_path(dir.path()), &missing, None, &[]).is_err());

        let out = dir.path().join("bad.mp4");
        std::fs::write(RenderReport::sidecar_path(&out), b"{ not json").unwrap();
        assert!(check_with(&project, &project_path(dir.path()), &out, None, &[]).is_err());
    }

    #[test]
    fn the_report_path_accepts_a_video_or_its_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = project(1);
        add(&mut project, cam(1));
        let out = write_report(&project, dir.path());
        let sidecar = RenderReport::sidecar_path(&out);

        assert_eq!(report_sidecar(&out), sidecar);
        assert_eq!(report_sidecar(&sidecar), sidecar);

        let from_video = check_with(&project, &project_path(dir.path()), &out, None, &[])
            .expect("from the video");
        let from_sidecar = check_with(&project, &project_path(dir.path()), &sidecar, None, &[])
            .expect("from the sidecar");
        assert_eq!(from_video, from_sidecar);
        assert_eq!(from_video.summary().pass, 1);
    }

    #[test]
    fn the_exit_code_is_zero_only_when_every_check_passes() {
        let dir = tempfile::tempdir().unwrap();
        let mut passing = project(1);
        add(&mut passing, cam(1));
        let out = write_report(&passing, dir.path());
        let report =
            check_with(&passing, &project_path(dir.path()), &out, None, &[]).expect("check");
        assert_eq!(check_exit_code(&report), 0, "{report:?}");

        // A sequence with nothing to check is a vacuous pass.
        let empty = project(1);
        let none = check_with(&empty, &project_path(dir.path()), &out, None, &[]).expect("check");
        assert!(none.checks().is_empty());
        assert_eq!(check_exit_code(&none), 0);

        // A failed cam and an unverifiable words check both gate at 1.
        let rendered = project(2);
        let mut failing = project(1);
        add(&mut failing, cam(1));
        add(
            &mut failing,
            Expectation::WordsComplete {
                min_coverage: 1.0,
                transcript: Some("t1-0000000000000000".to_string()),
            },
        );
        let rendered_dir = tempfile::tempdir().unwrap();
        let out = write_report(&rendered, rendered_dir.path());
        let report =
            check_with(&failing, &project_path(dir.path()), &out, None, &[]).expect("check");
        assert_eq!(report.summary().fail, 1);
        assert_eq!(report.summary().unverifiable, 1);
        assert_eq!(check_exit_code(&report), 1);
    }
}
