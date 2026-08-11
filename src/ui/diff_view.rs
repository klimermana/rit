use crate::{
    app::{App, DiffSelection, LogRow},
    model::{DiffLine, DiffLineKind, DiffStats, DiffTarget, FileStat},
    ui::{diff_line_to_ratatui, highlight_matches_in_span},
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

/// Background of the mouse drag-selection bar. Bluish so it can't be
/// confused with the yellow search highlights or the gray cursor bars.
const SELECTION_BG: Color = Color::Rgb(40, 65, 110);

pub fn draw_diff(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    let block = crate::ui::pane_block(diff_title(app), focused, crate::ui::mode_accent(app));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.diff.loading && app.diff.document.is_none() {
        let para = Paragraph::new("Loading diff...").style(Style::default().fg(Color::Gray));
        frame.render_widget(para, inner);
        return;
    }

    let Some(document) = app.diff.document.as_ref() else {
        let msg = if app.log.rows.is_empty() { "Select a commit to view diff" } else { "No diff available" };
        let para = Paragraph::new(msg).style(Style::default().fg(Color::Gray));
        frame.render_widget(para, inner);
        return;
    };

    // Compute section lengths without building any Line objects yet.
    let header_slice: &[DiffLine] = &document.header;
    let body_slice: &[DiffLine] = if app.diff.show_hunks { &document.body } else { &[] };
    let header_len = header_slice.len();

    // Diffstat is small (one row per file, plus 3 framing lines) so build it
    // eagerly; the wins come from not materialising the body.
    let diffstat: Vec<Line<'static>> = if !document.files.is_empty() {
        let picker_matches = (app.diff.file_picker.is_some() && !app.diff.picker_filter.query.is_empty())
            .then_some(app.diff.picker_filter.matches.as_slice());
        let mut out = Vec::with_capacity(document.files.len() + 3);
        append_diffstat(&mut out, &document.files, &document.stats, app.diff.file_picker, picker_matches);
        out
    } else {
        Vec::new()
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
    let selection = app.diff.selection.as_ref().map(DiffSelection::range);

    // Build Lines only for the visible window.
    let visible: Vec<Line> = (scroll..end)
        .map(|global_idx| {
            // Branch is chosen from `global_idx` against the section
            // lengths computed above, so each `.get` should always
            // succeed; the fallback empty line is a defensive default
            // rather than expected behavior.
            let line: Line<'static> = if global_idx < header_len {
                header_slice.get(global_idx).map(diff_line_to_ratatui).unwrap_or_default()
            } else if global_idx < body_offset {
                diffstat.get(global_idx - header_len).cloned().unwrap_or_default()
            } else {
                body_slice.get(global_idx - body_offset).map(diff_line_to_ratatui).unwrap_or_default()
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
            // Mouse selection: a full-width background bar over the
            // selected rows. Patched at the line level so span
            // foregrounds (and the search highlight's own bg) still
            // read through.
            let mut style = line.style;
            if selection.is_some_and(|(lo, hi)| (lo..=hi).contains(&global_idx)) {
                style = style.patch(Style::default().bg(SELECTION_BG));
            }
            Line::from(spans).style(style)
        })
        .collect();

    // Horizontal scroll is applied via Paragraph::scroll. The whole pane
    // (line numbers, +/- prefix, content) shifts together — simplest
    // behavior, and `#` is available to hide the gutter when the user
    // wants every column for content.
    let hscroll = u16::try_from(app.diff.horizontal_scroll).unwrap_or(u16::MAX);
    frame.render_widget(Paragraph::new(visible).scroll((0, hscroll)), inner);
}

fn diff_title(app: &App) -> String {
    let mode_tag = if app.diff.show_hunks { "" } else { "  [summary]" };
    // Scope tag only applies to commit diffs — the worker never filters
    // the working-tree document.
    let scope_tag = match (&app.diff.target, &app.path_filter) {
        (Some(DiffTarget::Commit(_)), Some(spec)) if app.diff.scoped => format!("  [only {spec}]"),
        _ => String::new(),
    };
    let label = match app.diff.target {
        Some(DiffTarget::WorkingTree) => "Working Tree".to_string(),
        Some(DiffTarget::Commit(_)) => match app.log.rows.get(app.log.selected) {
            Some(LogRow::Commit(c)) => c.short_id.to_string(),
            _ => "commit".to_string(),
        },
        None => return " Diff ".to_string(),
    };
    let Some(doc) = app.diff.document.as_ref() else {
        return format!(" Diff: {}{}{} ", label, mode_tag, scope_tag);
    };
    let stats = &doc.stats;
    let trunc_tag = truncation_tag(&doc.flags);
    // Single-file takeover view (`o`): path-centric title.
    if app.diff.file_view_return.is_some()
        && let Some(f) = doc.files.first()
    {
        return format!(
            " Diff: {}  —  {}  +{}  -{}  [file]{} ",
            label, f.path, stats.insertions, stats.deletions, trunc_tag,
        );
    }
    format!(
        " Diff: {}  {} file{} changed  +{}  -{}{}{}{} ",
        label,
        stats.files,
        if stats.files == 1 { "" } else { "s" },
        stats.insertions,
        stats.deletions,
        mode_tag,
        scope_tag,
        trunc_tag,
    )
}

/// Suffix appended to the diff title when any guardrail kicked in. Empty
/// string when `flags.truncated` is false.
fn truncation_tag(flags: &crate::model::DiffFlags) -> String {
    if !flags.truncated {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    if flags.skipped_binary_files > 0 {
        parts.push(format!("{} binary", flags.skipped_binary_files));
    }
    if flags.skipped_large_files > 0 {
        parts.push(format!("{} large", flags.skipped_large_files));
    }
    if parts.is_empty() { "  [truncated]".to_string() } else { format!("  [truncated: {}]", parts.join(", ")) }
}

pub(crate) fn append_diffstat(
    out: &mut Vec<Line<'static>>,
    files: &[FileStat],
    stats: &DiffStats,
    selected: Option<usize>,
    // Picker filter matches (sorted file indices); rows outside the set
    // render faint. `None` = no filter, all rows normal.
    filter_matches: Option<&[usize]>,
) {
    out.push(diff_line_to_ratatui(&DiffLine { kind: DiffLineKind::Faint, text: "---".to_string() }));

    // Column width: longest path, capped so we leave room for the bar.
    let max_path = files.iter().map(|f| f.path.len()).max().unwrap_or(0).min(60);
    let max_changes = files.iter().map(|f| f.additions + f.deletions).max().unwrap_or(0);
    // Bar width: cap at 20 chars regardless of column space.
    let bar_cap = 20usize;

    for (i, f) in files.iter().enumerate() {
        let total = f.additions + f.deletions;
        let bar_len = bar_len_for(total, max_changes, bar_cap).min(bar_cap).max(usize::from(total > 0));

        // Split the bar between + and - proportionally.
        let plus_len = bar_len_for(f.additions, total, bar_len);
        let minus_len = bar_len.saturating_sub(plus_len);

        let text = format!(
            " {path:<path_w$} | {n:>4} {plus}{minus}",
            path = f.path,
            path_w = max_path,
            n = total,
            plus = "+".repeat(plus_len),
            minus = "-".repeat(minus_len),
        );
        // Rows the picker filter excludes render faint; the span's own
        // fg would win over a line-level restyle, so pick the kind here.
        let kind = if filter_matches.is_some_and(|m| m.binary_search(&i).is_err()) {
            DiffLineKind::Faint
        } else {
            DiffLineKind::Diffstat
        };
        let mut line = diff_line_to_ratatui(&DiffLine { kind, text });
        // File-picker cursor row (`t` mode).
        if selected == Some(i) {
            line.style = Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD);
        }
        out.push(line);
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

/// Scale `part` against `whole`, rounding to the nearest integer in `0..=cap`.
/// Returns 0 when `whole` is 0. Integer-only so the f32 cast lint stays out
/// of the diffstat hot path.
fn bar_len_for(part: usize, whole: usize, cap: usize) -> usize {
    if whole == 0 {
        return 0;
    }
    // Round half up: (part * cap + whole/2) / whole.
    let scaled = part.saturating_mul(cap).saturating_add(whole / 2) / whole;
    scaled.min(cap)
}
