//! The timeline panel: one row per track plus the marker row, drawn from the
//! same quantizer as the playhead (`marker_col`).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::filmstrip::{FILMSTRIP_ROWS, FilmstripKey};
use nle_core::project::Project;
use nle_core::timeline::{Segment as TimelineSegment, Track, TrackKind};
use nle_core::track_label;
use nle_core::{Seconds, secs_f64};

use super::super::motion::{Viewport, on_boundary, truncate_label};
use super::super::{App, Mode, pool_strip_active, secs};
use super::pool::render_pool_strip;
pub(crate) fn render_timeline(f: &mut Frame, area: Rect, app: &App) {
    // While the pool is open the monitor is off the project, so the timeline
    // row is the selected asset's own strip — playing, paused or ended. The
    // project (and its insert point) returns on close.
    if pool_strip_active(app)
        && let Mode::Pool(st) = &app.mode
    {
        render_pool_strip(f, area, app, st);
        return;
    }
    let block = Block::bordered().title(" timeline ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 6 || inner.height < 2 {
        return;
    }

    let tracks = &app.project.timeline.tracks;
    let n = tracks.len();
    // A label gutter only when there is more than one track to tell apart.
    let label_w: u16 = if n > 1 { 4 } else { 0 };
    let bar_w = inner.width.saturating_sub(label_w);
    if bar_w == 0 {
        return;
    }

    // In extend mode the row rescales past the timeline end so the
    // projected range stays visible while navigating outward (the viewport
    // uses the same `view_total`, so the padded tail is not clipped).
    let in_extend = matches!(app.mode, Mode::Extend(_));
    let total = app.view_total();

    // Each visible track gets its own row; a video track can add two
    // filmstrip rows and an audio-bearing track adds a waveform row. The
    // marker row always takes one.
    let heights: Vec<usize> = tracks
        .iter()
        .map(|track| {
            1 + filmstrip_rows(app, track) + usize::from(track_has_audio(&app.project, track))
        })
        .collect();
    let capacity = inner.height.saturating_sub(1) as usize;
    let (start, count) = track_window_rows(&heights, app.active(), capacity.max(1));
    let view = app.timeline_viewport(bar_w);
    let marker_limit = inner.y + inner.height.saturating_sub(1);
    let mut y = inner.y;
    for (ti, track) in tracks.iter().enumerate().skip(start).take(count) {
        if y >= marker_limit {
            break;
        }
        let active = ti == app.active();
        if label_w > 0 {
            let style = if active {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let label = format!("{:>3} ", track_label(&app.project, ti));
            let gutter = Rect {
                x: inner.x,
                y,
                width: label_w,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(label, style))),
                gutter,
            );
        }
        let bar = Rect {
            x: inner.x + label_w,
            y,
            width: bar_w,
            height: 1,
        };
        let parts = track_parts(&app.project, track, total);
        draw_track_row(f, bar, app, &parts, in_extend && active, view, ti);
        y += 1;

        let film_rows = filmstrip_rows(app, track);
        if film_rows > 0 && y + film_rows as u16 <= marker_limit {
            if label_w > 0 {
                let mut lines = vec![Line::from(Span::styled(
                    "  ▦ ",
                    Style::default().fg(Color::DarkGray),
                ))];
                lines.extend((1..film_rows).map(|_| Line::from(Span::raw("    "))));
                f.render_widget(
                    Paragraph::new(lines),
                    Rect {
                        x: inner.x,
                        y,
                        width: label_w,
                        height: film_rows as u16,
                    },
                );
            }
            let bar = Rect {
                x: inner.x + label_w,
                y,
                width: bar_w,
                height: film_rows as u16,
            };
            draw_filmstrip_rows(f, bar, app, track, view, active);
            y += film_rows as u16;
        }

        if track_has_audio(&app.project, track) && y < marker_limit {
            if label_w > 0 {
                let gutter = Rect {
                    x: inner.x,
                    y,
                    width: label_w,
                    height: 1,
                };
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "  ~ ",
                        Style::default().fg(Color::DarkGray),
                    ))),
                    gutter,
                );
            }
            let bar = Rect {
                x: inner.x + label_w,
                y,
                width: bar_w,
                height: 1,
            };
            draw_waveform_row(f, bar, app, track, view, active);
            y += 1;
        }
    }

    if y >= inner.y + inner.height {
        return;
    }
    let bar = Rect {
        x: inner.x + label_w,
        y,
        width: bar_w,
        height: 1,
    };
    draw_marker_row(f, bar, app, view, in_extend);
}

pub(crate) fn track_has_audio(project: &Project, track: &Track) -> bool {
    !track.muted
        && track.segments.iter().any(|seg| match seg {
            TimelineSegment::Clip(c) => project.asset(c.asset).is_some_and(|a| a.probe.has_audio()),
            TimelineSegment::Transition(t) => [&t.a, &t.b].into_iter().any(|side| {
                side.asset()
                    .and_then(|id| project.asset(id))
                    .is_some_and(|a| a.probe.has_audio())
            }),
            TimelineSegment::Gap(_) => false,
        })
}

/// Whether a video track has picture to show in a filmstrip row. Muted video
/// tracks still count; only the audio waveform row respects mute.
pub(crate) fn track_has_video(project: &Project, track: &Track) -> bool {
    track.kind == TrackKind::Video
        && track.segments.iter().any(|seg| match seg {
            TimelineSegment::Clip(c) => project.asset(c.asset).is_some_and(|a| a.probe.has_video()),
            TimelineSegment::Transition(t) => [&t.a, &t.b].into_iter().any(|side| {
                side.asset()
                    .and_then(|id| project.asset(id))
                    .is_some_and(|a| a.probe.has_video())
            }),
            TimelineSegment::Gap(_) => false,
        })
}

pub(crate) fn filmstrip_rows(app: &App, track: &Track) -> usize {
    if app.filmstrip_enabled && track_has_video(&app.project, track) {
        FILMSTRIP_ROWS as usize
    } else {
        0
    }
}

/// Draw the two-row truecolor filmstrip under a video track's clip row. The
/// strip is sampled from the clip's source span; gaps and transitions keep
/// the same bands as the label row. A strip that has not loaded yet falls
/// back to a dark cyan placeholder.
fn draw_filmstrip_rows(
    f: &mut Frame,
    bar: Rect,
    app: &App,
    track: &Track,
    view: Viewport,
    active: bool,
) {
    if bar.width == 0 || bar.height == 0 {
        return;
    }
    let rows = usize::from(bar.height.min(FILMSTRIP_ROWS as u16));
    let cell = view.span / f64::from(bar.width);
    let view_end = view.start + view.span;
    let gap_style = Style::default().fg(Color::Black).bg(Color::DarkGray);
    let trans_style = Style::default().fg(Color::Black).bg(Color::Magenta);
    let placeholder = Style::default().bg(Color::Rgb(0, 64, 64));
    let mut lines: Vec<Vec<(char, Style)>> = (0..rows)
        .map(|_| vec![(' ', gap_style); bar.width as usize])
        .collect();

    let mut cum = 0.0;
    for seg in &track.segments {
        let start = cum;
        cum += secs(&seg.duration());
        if start >= view_end - 1e-9 {
            continue;
        }
        let x0 = view.col(start, bar.width);
        let x1 = if cum >= view_end - 1e-9 {
            bar.width
        } else {
            view.col(cum, bar.width)
        };
        match seg {
            TimelineSegment::Gap(_) => {
                for x in x0..x1 {
                    for line in &mut lines {
                        line[x as usize] = (' ', gap_style);
                    }
                }
            }
            TimelineSegment::Transition(_) => {
                for x in x0..x1 {
                    for line in &mut lines {
                        line[x as usize] = (' ', trans_style);
                    }
                }
            }
            TimelineSegment::Clip(c) => {
                let key = FilmstripKey::new(c.asset, &c.asset_in, &c.asset_out);
                let src_in = secs(&c.asset_in);
                let span = secs(&c.asset_out) - src_in;
                for x in x0..x1 {
                    let Some(strip) = app.filmstrips.get(&key) else {
                        for line in &mut lines {
                            line[x as usize] = (' ', placeholder);
                        }
                        continue;
                    };
                    if span <= 0.0 {
                        for line in &mut lines {
                            line[x as usize] = (' ', placeholder);
                        }
                        continue;
                    }
                    let t = view.start + (f64::from(x) + 0.5) * cell;
                    let offset = (t - start).max(0.0);
                    let source =
                        c.source_at(Seconds::new((offset * 1_000_000.0) as i64, 1_000_000));
                    let pos = ((secs_f64(&source) - src_in) / span).clamp(0.0, 0.999_999);
                    let col = (pos * strip.cols as f64).floor() as u32;
                    for (r, line) in lines.iter_mut().enumerate() {
                        let top = strip.pixel(col, (r * 2) as u32);
                        let bottom = strip.pixel(col, (r * 2 + 1) as u32);
                        let (fg, bg) = if active {
                            (rgb_color(top), rgb_color(bottom))
                        } else {
                            (dim_color(top), dim_color(bottom))
                        };
                        line[x as usize] = ('▀', Style::default().fg(fg).bg(bg));
                    }
                }
            }
        }
    }

    // Mirror the label row's hard cut dividers. A transition boundary is
    // soft; the blend band itself already carries the boundary colour.
    let mut cum = 0.0;
    for (i, seg) in track.segments.iter().enumerate() {
        let start = cum;
        cum += secs(&seg.duration());
        if i > 0 && start >= view.start - 1e-9 && start < view_end - 1e-9 {
            let soft = matches!(seg, TimelineSegment::Transition(_))
                || track
                    .segments
                    .get(i - 1)
                    .is_some_and(|s| matches!(s, TimelineSegment::Transition(_)));
            if !soft {
                let x = view.col(start, bar.width) as usize;
                for line in &mut lines {
                    line[x] = ('▏', Style::default().fg(Color::DarkGray).bg(Color::Black));
                }
            }
        }
    }

    let lines: Vec<Line> = lines
        .into_iter()
        .map(|cells| {
            Line::from(
                cells
                    .into_iter()
                    .map(|(ch, style)| Span::styled(ch.to_string(), style))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    f.render_widget(Paragraph::new(lines), bar);
}

fn rgb_color((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

fn dim_color((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r / 2, g / 2, b / 2)
}

const WAVE_BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Draw one row of audio peaks for an audio-bearing track. The band uses the
/// same visual grammar as the video row: green clips, dark-gray gaps, and
/// black cut dividers. The active track keeps the brighter peaks; the others
/// are dimmer so the focus still reads.
fn draw_waveform_row(
    f: &mut Frame,
    bar: Rect,
    app: &App,
    track: &Track,
    view: Viewport,
    active: bool,
) {
    if bar.width == 0 {
        return;
    }
    let cell = view.span / f64::from(bar.width);
    let gap_style = Style::default().fg(Color::Black).bg(Color::DarkGray);
    // A dark green band under bright green peaks keeps the row green while
    // making the waveform readable; plain Green on LightGreen was too close.
    let (band, peaks) = if active {
        (Color::Rgb(0, 72, 0), Color::LightGreen)
    } else {
        (Color::Rgb(0, 48, 0), Color::Rgb(112, 176, 112))
    };
    let band_style = Style::default().bg(band);
    let wave_style = Style::default().fg(peaks).bg(band);
    let divider_style = Style::default().fg(Color::Black).bg(band);
    let trans_style = Style::default().fg(Color::Black).bg(Color::Magenta);
    let mut cells: Vec<(char, Style)> = vec![(' ', gap_style); bar.width as usize];
    let view_end = view.start + view.span;
    let mut cum = 0.0f64;
    for seg in &track.segments {
        let len = secs(&seg.duration());
        let start = cum;
        cum += len;
        if start >= view_end - 1e-9 {
            continue; // starts at or past the right edge
        }
        let x0 = view.col(start, bar.width);
        let x1 = if cum >= view_end - 1e-9 {
            bar.width
        } else {
            view.col(cum, bar.width)
        };
        match seg {
            TimelineSegment::Gap(_) => {
                for cell in cells.iter_mut().take(x1 as usize).skip(x0 as usize) {
                    *cell = (' ', gap_style);
                }
            }
            TimelineSegment::Transition(_) => {
                for cell in cells.iter_mut().take(x1 as usize).skip(x0 as usize) {
                    *cell = ('~', trans_style);
                }
            }
            TimelineSegment::Clip(c) => {
                let wave = app.waveforms.get(&c.asset);
                for x in x0..x1 {
                    let mut ch = ' ';
                    if let Some(wave) = wave
                        && wave.duration > 0.0
                        && !wave.peaks.is_empty()
                    {
                        // Sample the column's centre — the instant
                        // `Viewport::col` assigns to it — so a peak aligns
                        // with the cut dividers drawn from the same view.
                        let t = view.start + f64::from(x) * cell;
                        let offset = (t - start).max(0.0);
                        let source =
                            c.source_at(Seconds::new((offset * 1_000_000.0) as i64, 1_000_000));
                        let pos = secs_f64(&source) / wave.duration;
                        let idx = (pos * wave.peaks.len() as f64).floor() as usize;
                        let peak = wave
                            .peaks
                            .get(idx.min(wave.peaks.len() - 1))
                            .copied()
                            .unwrap_or(0.0);
                        let level = (peak.sqrt() * 8.0).round() as usize;
                        ch = WAVE_BLOCKS[level.min(WAVE_BLOCKS.len() - 1)];
                    }
                    cells[x as usize] = (ch, if ch == ' ' { band_style } else { wave_style });
                }
            }
        }
    }

    // Mirror the label row's cut dividers. A transition boundary is soft;
    // everything else gets the target segment's background plus a black
    // `▏`, exactly like the video track.
    cum = 0.0;
    for (i, seg) in track.segments.iter().enumerate() {
        let start = cum;
        cum += secs(&seg.duration());
        if i > 0 && start >= view.start - 1e-9 && start < view_end - 1e-9 {
            let soft = matches!(seg, TimelineSegment::Transition(_))
                || track
                    .segments
                    .get(i - 1)
                    .is_some_and(|s| matches!(s, TimelineSegment::Transition(_)));
            if !soft {
                let x = view.col(start, bar.width);
                let style = match seg {
                    TimelineSegment::Gap(_) => gap_style,
                    TimelineSegment::Transition(_) => trans_style,
                    TimelineSegment::Clip(_) => divider_style,
                };
                cells[x as usize] = ('▏', style);
            }
        }
    }

    let mut spans: Vec<Span> = Vec::new();
    let mut i = 0usize;
    while i < cells.len() {
        let style = cells[i].1;
        let mut text = String::new();
        while i < cells.len() && cells[i].1 == style {
            text.push(cells[i].0);
            i += 1;
        }
        spans.push(Span::styled(text, style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), bar);
}

/// Which tracks to show: all of them when their rows fit, else the largest
/// window centred on the active track. `heights` holds each track's row count
/// (a track plus its waveform row, when it has audio), so waveform rows are
/// never counted as free space.
pub(crate) fn track_window_rows(
    heights: &[usize],
    active: usize,
    capacity: usize,
) -> (usize, usize) {
    let n = heights.len();
    if n == 0 {
        return (0, 0);
    }
    let active = active.min(n - 1);
    if heights.iter().sum::<usize>() <= capacity {
        return (0, n);
    }
    // Largest window that fits; prefer the centred one, then any other
    // valid start (uneven row counts can make the centre too wide).
    for count in (1..=n).rev() {
        let lo = active.saturating_sub(count - 1).min(n - count);
        let hi = active.min(n - count);
        let preferred = active
            .saturating_sub(count / 2)
            .min(n - count)
            .clamp(lo, hi);
        let starts =
            std::iter::once(preferred).chain((lo..=hi).filter(|start| *start != preferred));
        for start in starts {
            let end = start + count;
            if heights[start..end].iter().sum::<usize>() <= capacity {
                return (start, count);
            }
        }
    }
    (active, 1)
}

/// One track's segments as (duration, label, style) parts, plus a trailing
/// gap for any span short of the timeline end — a track shorter than the
/// timeline reads as gap rather than blank.
/// One row's parts: `(duration, label, style, permeable_left)`. The flag is
/// true when the boundary on this part's left is a blend — a transition is
/// part of both neighbours, so that divider is drawn permeable rather than
/// hard.
pub(crate) fn track_parts(
    project: &Project,
    track: &Track,
    total: f64,
) -> Vec<(f64, String, Style, bool)> {
    let gap_style = Style::default().bg(Color::DarkGray).fg(Color::Black);
    let is_trans = |s: &TimelineSegment| matches!(s, TimelineSegment::Transition(_));
    let mut parts: Vec<(f64, String, Style, bool)> = track
        .segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let soft = is_trans(s) || i > 0 && track.segments.get(i - 1).is_some_and(is_trans);
            match s {
                TimelineSegment::Clip(c) => {
                    let name = project
                        .asset(c.asset)
                        .and_then(|a| a.path.file_name().map(|n| n.to_string_lossy().into_owned()))
                        .unwrap_or_else(|| format!("#{}", c.id));
                    // `*` marks a clip with effects — a suffix would be cut
                    // off on a narrow cell, a prefix survives.
                    let name = if c.effects.is_empty() {
                        name
                    } else {
                        format!("*{name}")
                    };
                    (
                        secs(&c.duration()),
                        name,
                        Style::default().bg(Color::Cyan).fg(Color::Black),
                        soft,
                    )
                }
                // A transition gets its own band; the real dissolve is a
                // proxy-render thing (M5), so it's labelled, not blended.
                TimelineSegment::Transition(t) => (
                    secs(&t.dur),
                    "trans".to_string(),
                    Style::default().bg(Color::Magenta).fg(Color::Black),
                    soft,
                ),
                TimelineSegment::Gap(g) => (secs(&g.duration), "gap".to_string(), gap_style, soft),
            }
        })
        .collect();
    let tail = total - secs(&track.end());
    if tail > 1e-9 {
        let soft = track.segments.last().is_some_and(is_trans);
        parts.push((tail, "gap".to_string(), gap_style, soft));
    }
    parts
}

/// Draw one track's segment row. `notch` overlays the extend-mode playhead
/// marker (only the armed track gets it). Boundaries come from the SAME
/// quantizer as the marker row (`view`), so drawn clip edges and the
/// playhead/cut indicator can never disagree.
pub(crate) fn draw_track_row(
    f: &mut Frame,
    bar: Rect,
    app: &App,
    parts: &[(f64, String, Style, bool)],
    notch: bool,
    view: Viewport,
    ti: usize,
) {
    let view_end = view.start + view.span;
    let selected = match &app.mode {
        Mode::Visual(st) if st.track == ti => {
            Some((st.anchor.min(st.cursor), st.anchor.max(st.cursor)))
        }
        _ => None,
    };
    let mut cum: f64 = 0.0;
    let mut cells: Vec<(u16, String, Style)> = Vec::new();
    for (i, (dur, label, style, soft)) in parts.iter().enumerate() {
        let seg_start = cum;
        cum += dur;
        if seg_start >= view_end - 1e-9 {
            continue; // starts at or past the right edge
        }
        let start_x = if i == 0 {
            0
        } else {
            view.col(seg_start, bar.width)
        };
        let end_x = if cum >= view_end - 1e-9 {
            bar.width
        } else {
            view.col(cum, bar.width)
        };
        let width = end_x.saturating_sub(start_x);
        if width == 0 {
            continue; // rounds away — no cell, no divider
        }
        let text = if i == 0 || start_x == 0 {
            truncate_label(label, width)
        } else {
            // A blend is part of both neighbours: its boundary is a bare
            // colour change, not a bar. (A dotted box glyph would read as
            // permeable but sits mid-cell, so it never lines up with the
            // left-aligned `▏` dividers or the playhead marker.)
            let bar = if *soft { ' ' } else { '▏' };
            format!("{bar}{}", truncate_label(label, width.saturating_sub(1)))
        };
        let style = if selected.is_some_and(|(a, b)| i >= a && i <= b) {
            Style::default().fg(Color::Black).bg(Color::Yellow)
        } else {
            *style
        };
        cells.push((start_x, text, style));
    }

    let mut spans: Vec<Span> = Vec::new();
    // The marker row hides an off-screen playhead; the notch must agree or it
    // paints at the clamped edge column while the marker row shows nothing.
    let playhead_on_screen =
        app.playhead >= view.start - 1e-9 && app.playhead <= view.start + view.span + 1e-9;
    if notch && playhead_on_screen {
        // Split the containing part's text around the playhead marker.
        let ph = view.col(app.playhead, bar.width);
        let edge = match &app.mode {
            Mode::Extend(st) => st.edge_pos,
            _ => app.playhead,
        };
        let at_edge = (app.playhead - edge).abs() < 1e-9;
        let notch_style = Style::default().fg(if at_edge { Color::White } else { Color::Yellow });
        for (x0, text, style) in &cells {
            let chars: Vec<char> = text.chars().collect();
            let off = (ph as usize).saturating_sub(*x0 as usize);
            if ph >= *x0 && off < chars.len() {
                let before: String = chars[..off].iter().collect();
                let after: String = chars[off + 1..].iter().collect();
                if !before.is_empty() {
                    spans.push(Span::styled(before, *style));
                }
                spans.push(Span::styled("▏".to_string(), notch_style));
                if !after.is_empty() {
                    spans.push(Span::styled(after, *style));
                }
            } else {
                spans.push(Span::styled(text.clone(), *style));
            }
        }
    } else {
        for (_, text, style) in &cells {
            spans.push(Span::styled(text.clone(), *style));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), bar);
}

/// Draw the playhead marker row: red ▏ normally, WHITE exactly on a cut
/// frame, ▕ at EOF. In extend mode it also carries the workable span
/// (yellow) and the material extent (dim), with a yellow playhead.
pub(crate) fn draw_marker_row(
    f: &mut Frame,
    bar: Rect,
    app: &App,
    view: Viewport,
    in_extend: bool,
) {
    let ph = view.col(app.playhead, bar.width);
    let visible =
        app.playhead >= view.start - 1e-9 && app.playhead <= view.start + view.span + 1e-9;
    let at_end = app.eof;
    let mut marker_cells: Vec<(char, Style)> = vec![(' ', Style::default()); bar.width as usize];
    if let Mode::Extend(st) = &app.mode {
        let white_here = (app.playhead - st.edge_pos).abs() < 1e-9;
        let color = if white_here {
            Color::White
        } else {
            Color::Yellow
        };
        if let Some((m0, m1)) = st.mat {
            paint_run(
                &mut marker_cells,
                view,
                bar.width,
                m0,
                m1,
                '·',
                Style::default().fg(Color::DarkGray),
            );
        }
        let hi_edge = st.hi.unwrap_or(f64::INFINITY);
        paint_run(
            &mut marker_cells,
            view,
            bar.width,
            st.lo,
            hi_edge,
            '─',
            Style::default().fg(Color::Yellow),
        );
        if visible {
            marker_cells[ph as usize] =
                (if at_end { '▕' } else { '▏' }, Style::default().fg(color));
        }
    } else {
        let _ = in_extend;
        let color = if on_boundary(&app.project, app.playhead, app.active()) {
            Color::White
        } else {
            Color::Red
        };
        if visible {
            marker_cells[ph as usize] =
                (if at_end { '▕' } else { '▏' }, Style::default().fg(color));
        }
    }
    let mut marker: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for (ch, style) in marker_cells {
        if run_style.as_ref() != Some(&style) {
            if let Some(s) = run_style {
                marker.push(Span::styled(std::mem::take(&mut run), s));
            }
            run_style = Some(style);
        }
        run.push(ch);
    }
    if let Some(s) = run_style {
        marker.push(Span::styled(run, s));
    }
    f.render_widget(Paragraph::new(Line::from(marker)), bar);
}

/// Paint `glyph` with `style` over the cells spanning `[a, b)` in
/// timeline seconds (`b` may exceed the row — it clamps).
pub(crate) fn paint_run(
    cells: &mut [(char, Style)],
    view: Viewport,
    width: u16,
    a: f64,
    b: f64,
    glyph: char,
    style: Style,
) {
    let view_end = view.start + view.span;
    if width == 0 || b <= a || b <= view.start || a >= view_end {
        return;
    }
    let col_a = view.col(a.max(view.start), width) as usize;
    let mut col_b = view.col(b.min(view_end), width) as usize;
    if b >= view_end - 1e-9 {
        col_b = cells.len();
    }
    for cell in cells.iter_mut().take(col_b).skip(col_a) {
        *cell = (glyph, style);
    }
}
