use crate::{
    app::{App, LogRow},
    git::{DiffLine, DiffLineKind, DiffStats, DiffTarget, FileStat},
    ui::{diff_line_to_ratatui, highlight_matches_in_span},
};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

pub fn draw_diff(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    let border_style = if focused { Style::default().fg(Color::Cyan) } else { Style::default().fg(Color::DarkGray) };

    let title = diff_title(app);

    let block = Block::default().title(title).borders(Borders::ALL).border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.diff.loading && app.diff.header_lines.is_none() && app.diff.body_lines.is_none() {
        let para = Paragraph::new("Loading diff...").style(Style::default().fg(Color::Gray));
        frame.render_widget(para, inner);
        return;
    }

    if app.diff.header_lines.is_none() && app.diff.body_lines.is_none() {
        let msg = if app.log.rows.is_empty() { "Select a commit to view diff" } else { "No diff available" };
        let para = Paragraph::new(msg).style(Style::default().fg(Color::Gray));
        frame.render_widget(para, inner);
        return;
    }

    // Compute section lengths without building any Line objects yet.
    let header_slice: &[DiffLine] = app.diff.header_lines.as_deref().unwrap_or(&[]);
    let body_slice: &[DiffLine] = if app.diff.show_hunks { app.diff.body_lines.as_deref().unwrap_or(&[]) } else { &[] };
    let header_len = header_slice.len();

    // Diffstat is small (one row per file, plus 3 framing lines) so build it
    // eagerly; the wins come from not materialising the body.
    let diffstat: Vec<Line<'static>> = match (&app.diff.files, &app.diff.stats) {
        (Some(files), Some(stats)) if !files.is_empty() => {
            let mut out = Vec::with_capacity(files.len() + 3);
            append_diffstat(&mut out, files, stats);
            out
        }
        _ => Vec::new(),
    };
    let diffstat_len = diffstat.len();
    let body_offset = header_len + diffstat_len;
    let total = body_offset + body_slice.len();
    if total == 0 {
        return;
    }

    let height = inner.height as usize;
    let scroll = app.diff.scroll.min(total.saturating_sub(1));
    let end = (scroll + height).min(total);

    let show_nums = app.diff.show_line_numbers;
    let search_query = app.diff.search.query.to_lowercase();
    let has_diff_search = !search_query.is_empty();
    let current_match_line = app.diff.search.current_pos();

    // Build Lines only for the visible window.
    let visible: Vec<Line> = (scroll..end)
        .map(|global_idx| {
            let line: Line<'static> = if global_idx < header_len {
                diff_line_to_ratatui(&header_slice[global_idx])
            } else if global_idx < body_offset {
                diffstat[global_idx - header_len].clone()
            } else {
                diff_line_to_ratatui(&body_slice[global_idx - body_offset])
            };

            let is_current = has_diff_search && current_match_line == Some(global_idx);
            let is_match = has_diff_search && app.diff.search.matches.binary_search(&global_idx).is_ok();
            let highlight = if is_current {
                Style::default().bg(Color::Yellow).fg(Color::Black)
            } else {
                Style::default().bg(Color::Rgb(100, 80, 0)).fg(Color::White)
            };

            let mut spans = Vec::new();
            if show_nums {
                spans.push(Span::styled(format!("{:>4} ", global_idx + 1), Style::default().fg(Color::DarkGray)));
            }
            if is_match {
                for span in line.spans {
                    highlight_matches_in_span(&mut spans, span, &search_query, highlight);
                }
            } else {
                spans.extend(line.spans);
            }
            Line::from(spans).style(line.style)
        })
        .collect();

    frame.render_widget(Paragraph::new(visible), inner);
}

fn diff_title(app: &App) -> String {
    let mode_tag = if app.diff.show_hunks { "" } else { "  [summary]" };
    let label = match app.diff.target {
        Some(DiffTarget::WorkingTree) => "Working Tree".to_string(),
        Some(DiffTarget::Commit(_)) => match app.log.rows.get(app.log.selected) {
            Some(LogRow::Commit(c)) => c.short_id.to_string(),
            _ => "commit".to_string(),
        },
        None => return " Diff ".to_string(),
    };
    if let Some(stats) = &app.diff.stats {
        format!(
            " Diff: {}  {} file{} changed  +{}  -{}{} ",
            label,
            stats.files,
            if stats.files == 1 { "" } else { "s" },
            stats.insertions,
            stats.deletions,
            mode_tag,
        )
    } else {
        format!(" Diff: {}{} ", label, mode_tag)
    }
}

fn append_diffstat(out: &mut Vec<Line<'static>>, files: &[FileStat], stats: &DiffStats) {
    out.push(diff_line_to_ratatui(&DiffLine { kind: DiffLineKind::Faint, text: "---".to_string() }));

    // Column width: longest path, capped so we leave room for the bar.
    let max_path = files.iter().map(|f| f.path.len()).max().unwrap_or(0).min(60);
    let max_changes = files.iter().map(|f| f.additions + f.deletions).max().unwrap_or(0);
    // Bar width: cap at 20 chars regardless of column space.
    let bar_cap = 20usize;

    for f in files {
        let total = f.additions + f.deletions;
        let bar_len =
            if max_changes == 0 { 0 } else { ((total as f32 / max_changes as f32) * bar_cap as f32).round() as usize };
        let bar_len = bar_len.min(bar_cap).max(if total > 0 { 1 } else { 0 });

        // Split the bar between + and - proportionally.
        let plus_len =
            if total == 0 { 0 } else { ((f.additions as f32 / total as f32) * bar_len as f32).round() as usize };
        let minus_len = bar_len.saturating_sub(plus_len);

        let text = format!(
            " {path:<path_w$} | {n:>4} {plus}{minus}",
            path = f.path,
            path_w = max_path,
            n = total,
            plus = "+".repeat(plus_len),
            minus = "-".repeat(minus_len),
        );
        out.push(diff_line_to_ratatui(&DiffLine { kind: DiffLineKind::Diffstat, text }));
    }

    let summary = format!(
        " {} file{} changed, {} insertion{}(+), {} deletion{}(-)",
        stats.files,
        if stats.files == 1 { "" } else { "s" },
        stats.insertions,
        if stats.insertions == 1 { "" } else { "s" },
        stats.deletions,
        if stats.deletions == 1 { "" } else { "s" },
    );
    out.push(diff_line_to_ratatui(&DiffLine { kind: DiffLineKind::DiffstatTotal, text: summary }));
    out.push(diff_line_to_ratatui(&DiffLine { kind: DiffLineKind::Blank, text: String::new() }));
}
