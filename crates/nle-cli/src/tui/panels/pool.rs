//! The media-pool panel and the strip that replaces the timeline while the
//! pool is open.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::super::motion::marker_col;
use crate::thumbnails::{THUMB_HEIGHT, THUMB_WIDTH, Thumbnail};
use nle_core::time::Seconds;
use nle_core::timeline::{Clip, Segment as TimelineSegment, Track, TrackKind};
use nle_core::track_label;

use super::super::motion::Viewport;
use super::super::{App, PoolMarks, PoolState, pool_sel, secs};
use super::timeline::{draw_track_row, track_parts};
/// The pool takes over the info pane: asset list on the left, probe
/// metadata for the selection on the right.
pub(crate) fn render_pool(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::{List, ListState, Paragraph, Wrap};
    let [list_area, meta_area] =
        Layout::horizontal([Constraint::Length(28), Constraint::Min(0)]).areas(area);

    // Left: the asset list. The title names the track `enter` inserts into.
    let target = track_label(&app.project, app.active());
    let list_block = Block::bordered().title(format!(" media pool → {target} "));
    let list_inner = list_block.inner(list_area);
    f.render_widget(list_block, list_area);
    let items: Vec<ratatui::widgets::ListItem> = app
        .project
        .media
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let used = app.project.asset_usage(a.id);
            let mark = if used.is_empty() { "○" } else { "●" };
            let style = if i == pool_sel(app).unwrap_or(0) {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            ratatui::widgets::ListItem::new(Line::from(vec![
                Span::styled(format!("{mark} #{:<3} ", a.id), style),
                Span::styled(asset_line(a), style),
            ]))
        })
        .collect();
    // The list scrolls to keep the selected row visible: `ListState` owns the
    // viewport offset, while the selection itself lives in the mode
    // (`pool_sel`), so the widget state is rebuilt each frame, not persisted.
    let mut list_state = ListState::default();
    list_state.select(pool_sel(app));
    f.render_stateful_widget(List::new(items), list_inner, &mut list_state);

    // Right: a preview of the selected video, plus probe metadata. The
    // thumbnail only appears when the pane is wide enough to leave the
    // metadata readable.
    let selected = app.project.media.get(pool_sel(app).unwrap_or(0));
    let show_thumb = selected.is_some_and(|a| a.probe.has_video())
        && meta_area.width >= 44
        && meta_area.height >= 5;
    let meta_area = if show_thumb {
        let [thumb_area, meta_area] =
            Layout::horizontal([Constraint::Length(20), Constraint::Min(0)]).areas(meta_area);
        let thumb_block = Block::bordered().title(" preview ");
        let inner = thumb_block.inner(thumb_area);
        f.render_widget(thumb_block, thumb_area);
        if inner.width > 0 && inner.height > 0 {
            match selected.and_then(|a| app.thumbnails.get(&a.id)) {
                Some(thumb) => render_thumbnail(f, inner, thumb),
                None => {
                    let busy = selected.is_some_and(|a| {
                        app.thumb_job.as_ref().is_some_and(|job| job.asset == a.id)
                    });
                    let msg = if busy { "loading…" } else { "no preview" };
                    f.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            msg,
                            Style::default().fg(Color::DarkGray),
                        ))),
                        inner,
                    );
                }
            }
        }
        meta_area
    } else {
        meta_area
    };

    let meta_block = Block::bordered().title(" metadata ");
    let meta_inner = meta_block.inner(meta_area);
    f.render_widget(meta_block, meta_area);
    let lines = match selected {
        Some(a) => {
            let mut lines = asset_metadata(a);
            if a.probe.has_video() {
                match app.keyframes.get(&a.id) {
                    Some(keys) => lines.push(format!("keys   {}", keys.len())),
                    None if app
                        .keyframe_job
                        .as_ref()
                        .is_some_and(|job| job.asset == a.id) =>
                    {
                        lines.push("keys   probing…".to_string())
                    }
                    None => {}
                }
            }
            let marks = app.source_marks();
            if marks.in_at.is_some() || marks.out_at.is_some() {
                let in_at = marks.in_at.unwrap_or(0.0);
                let out_at = marks
                    .out_at
                    .or_else(|| a.duration().map(|d| secs(&d)))
                    .unwrap_or(in_at);
                lines.push(format!("marks  {in_at:.3} – {out_at:.3}s"));
            }
            lines
        }
        None => vec!["(no asset selected)".to_string()],
    };
    let para = Paragraph::new(
        lines
            .into_iter()
            .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::Gray))))
            .collect::<Vec<_>>(),
    )
    .wrap(Wrap { trim: false });
    f.render_widget(para, meta_inner);
}

/// Render a cached RGB frame as truecolor half-blocks. The image is
/// letterboxed into the cell rectangle; each cell is one pixel wide and two
/// pixels tall.
fn render_thumbnail(f: &mut Frame, area: Rect, thumb: &Thumbnail) {
    let w = area.width as u32;
    let h = area.height as u32;
    if w == 0 || h == 0 {
        return;
    }
    let pixels_h = h * 2;
    let src_aspect = THUMB_WIDTH as f64 / THUMB_HEIGHT as f64;
    let dst_aspect = w as f64 / pixels_h as f64;
    let (img_w, img_h) = if dst_aspect > src_aspect {
        let img_h = pixels_h;
        let img_w = ((src_aspect * img_h as f64).round() as u32).max(1).min(w);
        (img_w, img_h)
    } else {
        let img_w = w;
        let img_h = ((img_w as f64 / src_aspect).round() as u32)
            .max(1)
            .min(pixels_h);
        (img_w, img_h)
    };
    let x_off = (w - img_w) / 2;
    let y_off = (pixels_h - img_h) / 2;
    let sample = |x: u32, y: u32| -> Color {
        if x < x_off || x >= x_off + img_w || y < y_off || y >= y_off + img_h {
            return Color::Rgb(0, 0, 0);
        }
        let sx = ((x - x_off) as f64 / img_w as f64 * THUMB_WIDTH as f64) as u32;
        let sy = ((y - y_off) as f64 / img_h as f64 * THUMB_HEIGHT as f64) as u32;
        let (r, g, b) = thumb.pixel(sx, sy);
        Color::Rgb(r, g, b)
    };
    let lines: Vec<Line> = (0..h)
        .map(|y| {
            let spans: Vec<Span> = (0..w)
                .map(|x| {
                    let py = y * 2;
                    Span::styled(
                        "▀",
                        Style::default().fg(sample(x, py)).bg(sample(x, py + 1)),
                    )
                })
                .collect();
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

/// Plain-text summary for the pool row (path basename + duration).
pub(crate) fn asset_line(a: &nle_core::Asset) -> String {
    let name = a
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| a.path.display().to_string());
    let dur = a
        .duration()
        .map(|d| format!("{}s", secs(&d)))
        .unwrap_or_default();
    format!("{name} {dur}")
}

/// Human-readable probe metadata for the metadata pane (all from the
/// stored `RawProbe` — the file is never re-probed).
pub(crate) fn asset_metadata(a: &nle_core::Asset) -> Vec<String> {
    let p = &a.probe;
    let mut out = vec![
        format!("path   {}", a.path.display()),
        format!("id     #{}", a.id),
    ];
    let fmt = &p.format;
    out.push(format!("format {}", fmt.format_name));
    if let Some(d) = a.duration() {
        out.push(format!("dur    {}s", secs(&d)));
    }
    if let Some(sz) = fmt.size.as_deref() {
        out.push(format!("size   {sz} bytes"));
    }
    if let Some(b) = fmt.bit_rate.as_deref() {
        out.push(format!("bitrate {b}"));
    }
    if let Some((w, h)) = a.video_size() {
        out.push(format!("size   {w}×{h}"));
    }
    if let Some(f) = a.fps() {
        out.push(format!("fps    {}/{}", f.numer(), f.denom()));
    }
    out.push(String::new());
    for s in &p.streams {
        let kind = s.codec_type.as_deref().unwrap_or("?");
        let codec = s.codec_name.as_deref().unwrap_or("?");
        let mut line = format!("stream {} {kind}/{codec}", s.index);
        if let Some(w) = s.width
            && let Some(h) = s.height
        {
            line += &format!(" {w}×{h}");
        }
        if let Some(r) = &s.r_frame_rate {
            line += &format!(" {r}fps");
        }
        if let Some(pf) = &s.pix_fmt {
            line += &format!(" {pf}");
        }
        if let Some(sr) = s.sample_rate.as_deref() {
            line += &format!(" {sr}Hz");
        }
        if let Some(ch) = s.channels {
            line += &format!(" {ch}ch");
        }
        out.push(line);
    }
    out
}

/// The pool previews a raw file, so the monitor is not on the project
/// timeline. Replace the timeline row with the selected asset's own strip (a
/// single clip `0..duration`) and put the marker at the preview clock.
/// `panel_heights` already sized this panel to one track + marker.
pub(crate) fn render_pool_strip(f: &mut Frame, area: Rect, app: &App, st: &PoolState) {
    let Some(asset) = app.project.media.get(st.sel) else {
        f.render_widget(Block::bordered().title(" preview "), area);
        return;
    };
    let block = Block::bordered().title(format!(" preview — {} ", asset_line(asset)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 6 || inner.height < 2 {
        return;
    }
    let Some(dur) = asset.duration() else {
        // No known duration means no scale to draw against: name only.
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                asset_line(asset),
                Style::default().fg(Color::DarkGray),
            ))),
            inner,
        );
        return;
    };
    let total = secs(&dur).max(0.001);
    // Reuse the project row renderer so the name truncates and the edges
    // quantize through the same `marker_col` as the playhead marker.
    let track = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![TimelineSegment::Clip(Clip::new(
            0,
            asset.id,
            Seconds::from_integer(0),
            dur,
        ))],
    };
    let row = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
    };
    let parts = track_parts(&app.project, &track, total);
    draw_track_row(
        f,
        row,
        app,
        &parts,
        false,
        Viewport::fit(total),
        app.active(),
    );
    let marker = Rect {
        x: inner.x,
        y: inner.y + 1,
        width: inner.width,
        height: 1,
    };
    let keys = app
        .keyframes
        .get(&asset.id)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    draw_pool_marker_row(f, marker, total, st.pos, app.eof, keys, app.source_marks());
}

/// A marker row for the pool strip: keyframe ticks, then red `▏` / `▕` at
/// the preview clock or file end.
pub(crate) fn draw_pool_marker_row(
    f: &mut Frame,
    bar: Rect,
    total: f64,
    pos: f64,
    eof: bool,
    keyframes: &[f64],
    marks: PoolMarks,
) {
    if bar.width == 0 {
        return;
    }
    let mut cells: Vec<(char, Style)> = vec![(' ', Style::default()); bar.width as usize];
    for &key in keyframes {
        if key >= 0.0 && key <= total {
            let x = marker_col(key, total, bar.width) as usize;
            cells[x] = ('·', Style::default().fg(Color::DarkGray));
        }
    }
    let mark_style = Style::default().fg(Color::Yellow);
    if let Some(at) = marks.in_at.filter(|at| *at >= 0.0 && *at <= total) {
        cells[marker_col(at, total, bar.width) as usize] = ('[', mark_style);
    }
    if let Some(at) = marks.out_at.filter(|at| *at >= 0.0 && *at <= total) {
        cells[marker_col(at, total, bar.width) as usize] = (']', mark_style);
    }
    let ph = marker_col(pos, total, bar.width) as usize;
    let ch = if eof { '▕' } else { '▏' };
    cells[ph] = (ch, Style::default().fg(Color::Red));

    let mut line = Line::default();
    let mut run = String::new();
    let mut style = cells[0].1;
    for (ch, cell_style) in cells {
        if cell_style != style {
            line.push_span(Span::styled(std::mem::take(&mut run), style));
            style = cell_style;
        }
        run.push(ch);
    }
    line.push_span(Span::styled(run, style));
    f.render_widget(Paragraph::new(line), bar);
}
