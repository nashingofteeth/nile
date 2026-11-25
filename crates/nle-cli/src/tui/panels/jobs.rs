//! The job-queue panel: ids, states, progress, and the report hand-off.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use nle_core::jobs::JobState;

use super::super::App;
use super::super::keys::jobs::jobs_sel;

pub(crate) fn render_jobs(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" jobs ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if let Some(error) = &app.jobs_error {
        let line = Line::from(Span::styled(
            format!("jobs need nile serve: {error}"),
            Style::default().fg(Color::Red),
        ));
        f.render_widget(Paragraph::new(line), inner);
        return;
    }
    if app.jobs.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no jobs — `nile job submit -o FILE`",
                Style::default().fg(Color::DarkGray),
            ))),
            inner,
        );
        return;
    }
    let sel = jobs_sel(app).unwrap_or(0);
    let rows: Vec<Line> = app
        .jobs
        .iter()
        .enumerate()
        .map(|(i, job)| {
            let pct = (job.progress.fraction() * 100.0).round() as u32;
            let detail = match job.state {
                JobState::Running => format!("{pct:>3}%"),
                JobState::Queued => "  — ".to_string(),
                JobState::Done => match &job.report {
                    Some(_) => "report".to_string(),
                    None => " done ".to_string(),
                },
                _ => job.state.label().to_string(),
            };
            let mut text = format!(
                " #{:<3} {:<11} {:<6} seq {:<3} {}",
                job.id,
                job.state.label(),
                detail,
                job.sequence,
                short(&job.output.display().to_string(), 40)
            );
            if let Some(error) = &job.error
                && job.state != JobState::Running
            {
                text.push_str(&format!(" — {error}"));
            }
            let style = if i == sel {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(Span::styled(text, style))
        })
        .collect();
    f.render_widget(Paragraph::new(rows), inner);
}

fn short(text: &str, max: usize) -> String {
    let mut s: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        s.push('…');
    }
    s
}
