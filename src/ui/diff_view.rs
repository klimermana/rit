use crate::app::App;
use crate::ui::diff_line_to_ratatui;
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

pub fn draw_diff(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let title = if let Some(commit) = app.log.commits.get(app.log.selected) {
        if let Some(stats) = &app.diff.stats {
            format!(
                " Diff: {}  {} file{} changed  +{}  -{} ",
                commit.short_id,
                stats.files,
                if stats.files == 1 { "" } else { "s" },
                stats.insertions,
                stats.deletions,
            )
        } else {
            format!(" Diff: {} ", commit.short_id)
        }
    } else {
        " Diff ".to_string()
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.diff.loading && app.diff.lines.is_none() {
        let para = Paragraph::new("Loading diff...").style(Style::default().fg(Color::Gray));
        frame.render_widget(para, inner);
        return;
    }

    let Some(diff_lines) = &app.diff.lines else {
        let msg = if app.log.commits.is_empty() {
            "Select a commit to view diff"
        } else {
            "No diff available"
        };
        let para = Paragraph::new(msg).style(Style::default().fg(Color::Gray));
        frame.render_widget(para, inner);
        return;
    };

    let total = diff_lines.len();
    if total == 0 { return; }
    let height = inner.height as usize;
    let scroll = app.diff.scroll.min(total.saturating_sub(1));
    let end = (scroll + height).min(total);

    let show_nums = app.diff.show_line_numbers;
    let visible: Vec<Line> = diff_lines[scroll..end]
        .iter()
        .enumerate()
        .map(|(i, raw)| {
            let line = diff_line_to_ratatui(raw);
            if show_nums {
                let num = Span::styled(
                    format!("{:>4} ", scroll + i + 1),
                    Style::default().fg(Color::DarkGray),
                );
                let mut spans = Vec::with_capacity(line.spans.len() + 1);
                spans.push(num);
                spans.extend(line.spans);
                Line::from(spans).style(line.style)
            } else {
                line
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(visible), inner);
}
