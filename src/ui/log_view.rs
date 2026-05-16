use crate::app::{App, CommitInfo, LogRow, RefKind, WorkingTreeRow};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

pub fn draw_log(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    let border_style = if focused { Style::default().fg(Color::Cyan) } else { Style::default().fg(Color::DarkGray) };

    let block = Block::default().title(" Log ").borders(Borders::ALL).border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if let Some(err) = &app.error {
        let para = Paragraph::new(err.as_str()).style(Style::default().fg(Color::Red));
        frame.render_widget(para, inner);
        return;
    }

    if app.log.rows.is_empty() {
        let msg = if !app.walk_done { "Loading commits..." } else { "No commits found" };
        let para = Paragraph::new(msg).style(Style::default().fg(Color::Gray));
        frame.render_widget(para, inner);
        return;
    }

    let height = inner.height as usize;
    let scroll = app.log.scroll;
    let end = (scroll + height).min(app.log.rows.len());

    let mut rows: Vec<Line> = Vec::with_capacity(height);

    for (i, row) in app.log.rows[scroll..end].iter().enumerate() {
        let global_idx = scroll + i;
        let is_selected = global_idx == app.log.selected;
        let is_match = !app.search.query.is_empty() && app.search.matches.binary_search(&global_idx).is_ok();

        let spans = match row {
            LogRow::WorkingTree(w) => working_tree_spans(w, app),
            LogRow::Commit(c) => commit_spans(c, app),
        };

        let row_style = if is_selected {
            Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD)
        } else if is_match {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        rows.push(Line::from(spans).style(row_style));
    }

    frame.render_widget(Paragraph::new(rows), inner);
}

fn commit_spans<'a>(commit: &'a CommitInfo, app: &App) -> Vec<Span<'a>> {
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::styled(format!("{} ", commit.graph), Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled(commit.short_id.as_str(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!("{:<width$}", commit.date, width = app.date_col_width()),
        Style::default().fg(Color::Blue),
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!("{:<width$}", commit.author, width = app.author_col_width()),
        Style::default().fg(Color::Green),
    ));
    spans.push(Span::raw(" "));

    for label in &commit.refs {
        let (fg, modifier) = match label.kind {
            RefKind::Head => (Color::Cyan, Modifier::BOLD),
            RefKind::LocalBranch => (Color::Cyan, Modifier::empty()),
            RefKind::RemoteBranch => (Color::Magenta, Modifier::empty()),
            RefKind::Tag => (Color::Yellow, Modifier::empty()),
        };
        spans.push(Span::styled(format!("[{}]", label.name), Style::default().fg(fg).add_modifier(modifier)));
        spans.push(Span::raw(" "));
    }

    spans.push(Span::raw(commit.summary.as_str()));
    spans
}

fn working_tree_spans<'a>(row: &'a WorkingTreeRow, app: &App) -> Vec<Span<'a>> {
    let mut spans: Vec<Span> = Vec::new();
    let dim = Style::default().fg(Color::DarkGray);
    // Graph column left blank to line up with the commit rows.
    spans.push(Span::raw("  "));
    spans.push(Span::styled("0000000", dim));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!("{:<width$}", "now", width = app.date_col_width()),
        Style::default().fg(Color::Blue),
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!("{:<width$}", row.author, width = app.author_col_width()),
        Style::default().fg(Color::Green),
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::styled("Not Committed Yet", Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)));
    spans
}
