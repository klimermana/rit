use crate::{
    app::{App, LogRow, StagedRow, WorkingTreeRow},
    model::{CommitRecord, RefKind},
    ui::highlight_matches_in_span,
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

pub fn draw_log(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    let block = crate::ui::pane_block(" Log ".to_string(), focused, crate::ui::mode_accent(app));

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

    let search_query = app.search.state.query.to_lowercase();
    let has_search = !search_query.is_empty();
    let current_match_row = app.search.current_pos();

    let mut rows: Vec<Line> = Vec::with_capacity(height);

    for (i, row) in app.log.rows.get(scroll..end).unwrap_or(&[]).iter().enumerate() {
        let global_idx = scroll + i;
        let is_selected = global_idx == app.log.selected;
        let is_match = has_search && app.search.state.matches.binary_search(&global_idx).is_ok();
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
            LogRow::Staged(s) => staged_spans(s, app),
            LogRow::Commit(c) => commit_spans(c, app, row_query, highlight),
        };

        // The bright selection bar follows focus: when the diff pane is
        // driving, the log's cursor drops to a faint bar so exactly one
        // pane shows the "active" highlight at a time.
        let row_style = if is_selected {
            if focused {
                Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(Color::Rgb(45, 45, 45))
            }
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
    let hash = if app.display.full_hash { commit.id.to_string() } else { commit.short_id.to_string() };
    spans.push(Span::styled(hash, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    spans.push(Span::raw(" "));
    if let Some(date) = date_cell(app, commit.authored_unix_secs, &commit.authored_relative) {
        spans.push(Span::styled(date, Style::default().fg(Color::Blue)));
        spans.push(Span::raw(" "));
    }

    // Truncate the full author name to the column width here at render
    // time; the underlying CommitRecord stores the full name so search
    // can find substrings past the display cap.
    if app.author_col_width() > 0 {
        let author_truncated = truncate_chars(&commit.author, app.author_col_width());
        let author_span = Span::styled(
            format!("{:<width$}", author_truncated, width = app.author_col_width()),
            Style::default().fg(Color::Green),
        );
        highlight_matches_in_span(&mut spans, author_span, search_query, highlight_style);
        spans.push(Span::raw(" "));
    }

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
    s.char_indices().nth(max_chars).and_then(|(boundary, _)| s.get(..boundary)).unwrap_or(s).to_string()
}

/// Render the date column per the active `DateMode`: pre-formatted
/// relative text, absolute local time from the stored epoch seconds, or
/// `None` when the column is hidden. Padded to the mode's column width.
fn date_cell(app: &App, unix_secs: i64, relative: &str) -> Option<String> {
    use crate::app::DateMode;
    use chrono::{Local, TimeZone};
    let width = app.date_col_width();
    match app.display.date {
        DateMode::Relative => Some(format!("{relative:<width$}")),
        DateMode::Absolute => {
            let text = Local
                .timestamp_opt(unix_secs, 0)
                .single()
                .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "?".to_string());
            Some(format!("{text:<width$}"))
        }
        DateMode::Off => None,
    }
}

/// The hash / date / author columns shared by the pseudo rows (working
/// tree, staged): an all-zero hash, "now", and the configured author,
/// each following the same display toggles as the commit rows.
fn pseudo_row_scaffold(author: &str, app: &App) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let dim = Style::default().fg(Color::DarkGray);
    let hash = if app.display.full_hash { "0".repeat(40) } else { "0000000".to_string() };
    spans.push(Span::styled(hash, dim));
    spans.push(Span::raw(" "));
    if app.date_col_width() > 0 {
        spans.push(Span::styled(
            format!("{:<width$}", "now", width = app.date_col_width()),
            Style::default().fg(Color::Blue),
        ));
        spans.push(Span::raw(" "));
    }
    if app.author_col_width() > 0 {
        let author = truncate_chars(author, app.author_col_width());
        spans.push(Span::styled(
            format!("{:<width$}", author, width = app.author_col_width()),
            Style::default().fg(Color::Green),
        ));
        spans.push(Span::raw(" "));
    }
    spans
}

fn working_tree_spans(row: &WorkingTreeRow, app: &App) -> Vec<Span<'static>> {
    let mut spans = pseudo_row_scaffold(&row.author, app);
    let (text, style) = match row.dirty {
        Some(true) => ("Uncommitted changes", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Some(false) => ("Working tree clean", Style::default().fg(Color::Green)),
        // Pre-dirty-check fallback; matches the historical "Not Committed Yet" label.
        None => ("Not Committed Yet", Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)),
    };
    spans.push(Span::styled(text, style));
    spans
}

fn staged_spans(row: &StagedRow, app: &App) -> Vec<Span<'static>> {
    let mut spans = pseudo_row_scaffold(&row.author, app);
    // The row only exists while something is staged, so the label is
    // unconditional — green for "ready to commit", distinct from the
    // yellow "Uncommitted changes" above it.
    spans.push(Span::styled("Staged changes", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)));
    spans
}
