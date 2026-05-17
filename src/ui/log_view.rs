use crate::{
    app::{App, LogRow, WorkingTreeRow},
    model::{CommitRecord, RefKind},
    ui::highlight_matches_in_span,
};
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

    let search_query = app.search.query.to_lowercase();
    let has_search = !search_query.is_empty();
    let current_match_row = app.search.current_pos();

    let mut rows: Vec<Line> = Vec::with_capacity(height);

    for (i, row) in app.log.rows[scroll..end].iter().enumerate() {
        let global_idx = scroll + i;
        let is_selected = global_idx == app.log.selected;
        let is_match = has_search && app.search.matches.binary_search(&global_idx).is_ok();
        let is_current = has_search && current_match_row == Some(global_idx);

        // Same convention as the diff view: current match gets a bright
        // background, other matches get a dim one.
        let highlight = if is_current {
            Style::default().bg(Color::Yellow).fg(Color::Black)
        } else {
            Style::default().bg(Color::Rgb(100, 80, 0)).fg(Color::White)
        };
        let row_query = if is_match { search_query.as_str() } else { "" };

        let spans = match row {
            LogRow::WorkingTree(w) => working_tree_spans(w, app),
            LogRow::Commit(c) => commit_spans(c, app, row_query, highlight),
        };

        let row_style = if is_selected {
            Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        rows.push(Line::from(spans).style(row_style));
    }

    frame.render_widget(Paragraph::new(rows), inner);
}

fn commit_spans(commit: &CommitRecord, app: &App, search_query: &str, highlight_style: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if !commit.graph.is_empty() {
        spans.push(Span::styled(format!("{} ", commit.graph), Style::default().fg(Color::DarkGray)));
    }
    spans.push(Span::styled(
        commit.short_id.to_string(),
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!("{:<width$}", commit.authored_relative, width = app.date_col_width()),
        Style::default().fg(Color::Blue),
    ));
    spans.push(Span::raw(" "));

    // Truncate the full author name to the column width here at render
    // time; the underlying CommitRecord stores the full name so search
    // can find substrings past the 20-char display cap.
    let author_truncated = truncate_chars(&commit.author, app.author_col_width());
    let author_span = Span::styled(
        format!("{:<width$}", author_truncated, width = app.author_col_width()),
        Style::default().fg(Color::Green),
    );
    highlight_matches_in_span(&mut spans, author_span, search_query, highlight_style);
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

    let summary_span = Span::raw(commit.summary.clone());
    highlight_matches_in_span(&mut spans, summary_span, search_query, highlight_style);
    spans
}

/// Char-aware truncation so multibyte names don't get sliced mid-codepoint
/// when they exceed the column width.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((boundary, _)) => s[..boundary].to_string(),
        None => s.to_string(),
    }
}

fn working_tree_spans(row: &WorkingTreeRow, app: &App) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let dim = Style::default().fg(Color::DarkGray);
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
