pub mod blame_view;
pub mod diff_view;
pub mod help;
pub mod log_view;
pub mod refs_view;

use crate::{
    app::{App, SearchSnapshot},
    model::{DiffLine, DiffLineKind},
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};

/// Mirrors tig: prefer side-by-side panes when the aspect ratio is wide
/// enough that two stacked panes would each be very short.
fn use_vertical_split(width: u16, height: u16) -> bool {
    width > 160 || (width as f32 * 0.5) > ((height as f32 - 1.0) * 2.0)
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let size = frame.area();

    let [title_area, main_area, status_bar_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .areas(size);

    // Title bar
    let title = format!(" rit  {}  [{}] ", app.repo_name, app.branch_name);
    frame.render_widget(
        Paragraph::new(title)
            .alignment(Alignment::Center)
            .style(Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD)),
        title_area,
    );

    if app.status.open {
        draw_status_view(frame, app, main_area);
        draw_status_bar(frame, app, status_bar_area);
        if app.show_help {
            help::draw_help(frame, size);
        }
        return;
    }

    if app.refs.open {
        refs_view::draw_refs_view(frame, app, main_area);
        draw_status_bar(frame, app, status_bar_area);
        if app.show_help {
            help::draw_help(frame, size);
        }
        return;
    }

    if app.blame.open {
        blame_view::draw_blame_view(frame, app, main_area);
        draw_status_bar(frame, app, status_bar_area);
        if app.show_help {
            help::draw_help(frame, size);
        }
        return;
    }

    let (log_area, diff_area_opt) = if app.diff.open {
        if use_vertical_split(size.width, size.height) {
            let [left, right] = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(main_area);
            (left, Some(right))
        } else {
            // Stacked layout: diff takes the lion's share. When the user
            // opens the diff pane the diff is the working surface, so it
            // gets enough rows to actually read a hunk in one viewport;
            // the log keeps ~35% for selection context, which is still a
            // ~17-row column on a 50-row terminal.
            let [top, bottom] = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
                .areas(main_area);
            (top, Some(bottom))
        }
    } else {
        (main_area, None)
    };

    // Single source of truth for the log viewport size.
    app.log.view_height = (log_area.height as usize).saturating_sub(2).max(1);
    app.ensure_selected_visible();

    let is_log_focused = matches!(app.focus, crate::app::Focus::Log);
    log_view::draw_log(frame, app, log_area, is_log_focused);

    if let Some(diff_area) = diff_area_opt {
        app.diff.view_height = (diff_area.height as usize).saturating_sub(2).max(1);
        let is_diff_focused = matches!(app.focus, crate::app::Focus::Diff);
        diff_view::draw_diff(frame, app, diff_area, is_diff_focused);
    }

    draw_status_bar(frame, app, status_bar_area);

    if app.show_help {
        help::draw_help(frame, size);
    }
}

fn draw_status_view(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(if app.status.loading { " Status [loading...] " } else { " Status " })
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;
    let lines: &[DiffLine] = app.status.document.as_ref().map(|d| d.lines.as_slice()).unwrap_or(&[]);
    let total = lines.len();
    if total == 0 {
        return;
    }
    let scroll = app.status.scroll.min(total.saturating_sub(1));
    let end = (scroll + height).min(total);
    let visible: Vec<Line> = lines.get(scroll..end).unwrap_or(&[]).iter().map(diff_line_to_ratatui).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

/// Bordered block for the log/diff panes with the focus treatment:
/// thick cyan border and bold title when focused, plain dark gray
/// otherwise — so which pane receives keys is legible from the frame
/// itself, not just a border tint.
pub(crate) fn pane_block(title: String, focused: bool) -> Block<'static> {
    let (border_style, border_type, title_style) = if focused {
        (
            Style::default().fg(Color::Cyan),
            BorderType::Thick,
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )
    } else {
        (Style::default().fg(Color::DarkGray), BorderType::Plain, Style::default().fg(Color::DarkGray))
    };
    Block::default()
        .title(Span::styled(title, title_style))
        .borders(Borders::ALL)
        .border_style(border_style)
        .border_type(border_type)
}

/// The status bar's mode label and its chip color. Branch order mirrors
/// `App::handle_input`'s dispatch exactly, so the chip always names the
/// mode whose keymap is live.
fn mode_label(app: &App) -> (&'static str, Color) {
    if app.show_help {
        ("HELP", Color::White)
    } else if app.diff.search.active || app.search.state.active {
        ("SEARCH", Color::Yellow)
    } else if app.status.open {
        ("STATUS", Color::Gray)
    } else if app.refs.open {
        ("REFS", Color::Green)
    } else if app.blame.open {
        ("BLAME", Color::Magenta)
    } else if matches!(app.focus, crate::app::Focus::Diff) {
        ("DIFF", Color::Cyan)
    } else {
        ("LOG", Color::Blue)
    }
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    // Diff search takes precedence over log search when both have a query —
    // the diff pane is the foreground context whenever it's open.
    let text = if let Some(line) = search_status_line(app.diff.search.snapshot(), "diff search") {
        line
    } else if let Some(line) = search_status_line(
        app.search.snapshot(),
        if app.diff.open { "search  Tab:switch  q/Esc:close-diff" } else { "search  Enter:open-diff" },
    ) {
        line
    } else if let Some(y) = &app.yank_message {
        Line::from(Span::styled(format!(" ✓ {}", y.text), Style::default().fg(Color::Green)))
    } else if app.status.open {
        Line::from(Span::raw(" q/Esc/s:close-status  j/k:scroll  g/G:top/bottom"))
    } else if app.refs.open {
        Line::from(Span::raw(" q/Esc/r:close-refs  j/k:nav  Enter:open-log-at-ref"))
    } else if app.blame.open {
        Line::from(Span::raw(
            " q/Esc:close-blame  j/k:nav  Enter:open-commit  ,:reblame-parent  Backspace:back  y:yank",
        ))
    } else if app.diff.open {
        Line::from(Span::raw(
            " q/Esc:close-diff  j/k:nav  ]/[:file  Tab:switch  /:search-diff  v:hunks  y:yank  ?:help",
        ))
    } else {
        let count = app.commits_len();
        let count_str = if app.walk_done { format!("{}", count) } else { format!("{}+", count) };
        Line::from(Span::raw(format!(
            " {} commits  q:quit  j/k:nav  Enter:diff  /:search  y:yank  r:refs  s:status  ?:help  R:reload",
            count_str,
        )))
    };

    // Prepend the mode chip — one fixed spot that names the active
    // keymap, vim-style.
    let (label, color) = mode_label(app);
    let chip =
        Span::styled(format!(" {label} "), Style::default().bg(color).fg(Color::Black).add_modifier(Modifier::BOLD));
    let mut spans = vec![chip];
    spans.extend(text.spans);

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray).fg(Color::Gray)),
        area,
    );
}

/// Renders the status-bar line for an active or pending search.
/// Returns `None` when the search has neither input mode active nor a stored
/// query — i.e. nothing to show.
fn search_status_line(snap: SearchSnapshot<'_>, idle_hint: &str) -> Option<Line<'static>> {
    if snap.active {
        Some(Line::from(vec![
            Span::raw(format!(" [{}/{}] /", snap.display_index, snap.matches_len)),
            Span::styled(snap.query.to_string(), Style::default().fg(Color::Yellow)),
            Span::raw("█"),
        ]))
    } else if !snap.query.is_empty() {
        Some(Line::from(Span::styled(
            format!(" [{}/{}] n:next  N:prev  Esc:clear {}", snap.display_index, snap.matches_len, idle_hint),
            Style::default().fg(Color::Yellow),
        )))
    } else {
        None
    }
}

/// Splits `span` into sub-spans, wrapping each occurrence of `query`
/// (already lowercased) with `highlight_style`. Non-matching portions keep
/// the original span's style. Shared between the log and diff views.
///
/// Unicode-safe: walks the original text on char boundaries and folds case
/// via `char::to_lowercase` so non-ASCII input (accents, dotted-I, etc.)
/// can never produce a sub-char byte slice the way the old implementation
/// did when it offsetted into the original with positions from a
/// `text.to_lowercase()` copy.
pub fn highlight_matches_in_span(
    out: &mut Vec<Span<'static>>,
    span: Span<'static>,
    query: &str,
    highlight_style: Style,
) {
    if query.is_empty() {
        out.push(span);
        return;
    }
    let text: &str = &span.content;
    let base_style = span.style;
    let mut search_from = 0usize;
    let mut emit_from = 0usize;

    while search_from < text.len() {
        if let Some(match_len) = match_prefix_len(text, search_from, query) {
            if search_from > emit_from
                && let Some(slice) = text.get(emit_from..search_from)
            {
                out.push(Span::styled(slice.to_string(), base_style));
            }
            if let Some(slice) = text.get(search_from..search_from + match_len) {
                out.push(Span::styled(slice.to_string(), highlight_style));
            }
            search_from += match_len;
            emit_from = search_from;
        } else {
            // Skip to the next char boundary so we never slice mid-codepoint.
            let step = text.get(search_from..).and_then(|t| t.chars().next()).map(|c| c.len_utf8()).unwrap_or(1);
            search_from += step;
        }
    }

    if emit_from < text.len()
        && let Some(slice) = text.get(emit_from..)
    {
        out.push(Span::styled(slice.to_string(), base_style));
    }
}

/// Returns the byte-length of the prefix of `text[start..]` whose lowercase
/// folding equals `query` (which must already be lowercased). Returns
/// `None` if no such prefix exists. Operates strictly on char boundaries
/// so the caller can safely slice the original `text` by the returned
/// length.
fn match_prefix_len(text: &str, start: usize, query: &str) -> Option<usize> {
    let mut q_chars = query.chars();
    let suffix = text.get(start..)?;
    let mut consumed = 0usize;

    for (rel_byte, ch) in suffix.char_indices() {
        for lc in ch.to_lowercase() {
            match q_chars.next() {
                Some(qc) if qc == lc => continue,
                // Mismatch in the middle of folding this char, or this char
                // would produce extra lowercase output beyond the query.
                // Treat both as "no match here" — the original char is one
                // grapheme; we cannot highlight a partial slice of it.
                _ => return None,
            }
        }
        consumed = rel_byte + ch.len_utf8();
        if q_chars.clone().next().is_none() {
            return Some(consumed);
        }
    }
    // Ran out of text before query was fully consumed.
    if q_chars.next().is_none() { Some(consumed) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(spans: &[Span<'static>]) -> Vec<(String, Option<Color>)> {
        spans.iter().map(|s| (s.content.clone().into_owned(), s.style.bg)).collect()
    }

    fn highlight(text: &str, query: &str) -> Vec<(String, Option<Color>)> {
        let hl = Style::default().bg(Color::Yellow);
        let span = Span::styled(text.to_string(), Style::default());
        let mut out = Vec::new();
        highlight_matches_in_span(&mut out, span, &query.to_lowercase(), hl);
        join(&out)
    }

    #[test]
    fn empty_query_passes_span_through() {
        let hl = Style::default().bg(Color::Yellow);
        let mut out = Vec::new();
        highlight_matches_in_span(&mut out, Span::raw("hello"), "", hl);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content, "hello");
        assert_eq!(out[0].style.bg, None);
    }

    #[test]
    fn ascii_case_insensitive_match() {
        let parts = highlight("Hello World", "world");
        assert_eq!(parts, vec![("Hello ".into(), None), ("World".into(), Some(Color::Yellow)),]);
    }

    #[test]
    fn accented_chars_match_and_preserve_case() {
        // 'é' lowercases to 'é' (single char each); query is "renée" so this
        // should highlight the whole word in its original case.
        let parts = highlight("Renée Dupont", "renée");
        assert_eq!(parts, vec![("Renée".into(), Some(Color::Yellow)), (" Dupont".into(), None),]);
    }

    #[test]
    fn multibyte_chars_dont_panic_or_corrupt() {
        // Test the historical failure mode: the old impl would compute match
        // offsets from text.to_lowercase() and slice the original. With
        // non-ASCII content surrounding the match, that produced corrupt
        // byte slices and could panic. Just exercising it without panic is
        // the win; we also verify the match lands correctly.
        let parts = highlight("café CAFÉ café", "café");
        let highlighted: Vec<&str> =
            parts.iter().filter(|(_, bg)| *bg == Some(Color::Yellow)).map(|(s, _)| s.as_str()).collect();
        assert_eq!(highlighted, vec!["café", "CAFÉ", "café"]);
    }

    #[test]
    fn match_after_multibyte_prefix() {
        // The old impl miscomputed positions when multibyte chars preceded
        // the match. Verify we land on the right substring.
        let parts = highlight("🦀 hello world", "world");
        let highlighted: Vec<&str> =
            parts.iter().filter(|(_, bg)| *bg == Some(Color::Yellow)).map(|(s, _)| s.as_str()).collect();
        assert_eq!(highlighted, vec!["world"]);
    }

    #[test]
    fn no_match_in_unicode_returns_text_unchanged() {
        let parts = highlight("Renée", "smith");
        assert_eq!(parts.iter().filter(|(_, bg)| bg.is_some()).count(), 0);
        let recombined: String = parts.iter().map(|(s, _)| s.clone()).collect();
        assert_eq!(recombined, "Renée");
    }

    /// The chip must always name the mode whose keymap is live, so its
    /// precedence has to match `App::handle_input`'s dispatch order —
    /// stack the modes up and peel them back down.
    #[test]
    fn mode_label_mirrors_input_dispatch_order() {
        let (req_tx, _req_rx) = crossbeam_channel::unbounded();
        let (_msg_tx, msg_rx) = crossbeam_channel::unbounded();
        let (_input_tx, input_rx) = crossbeam_channel::unbounded();
        let mut app = App::new(req_tx, msg_rx, input_rx);

        assert_eq!(mode_label(&app).0, "LOG");
        app.diff.open = true;
        app.focus = crate::app::Focus::Diff;
        assert_eq!(mode_label(&app).0, "DIFF");
        app.blame.open = true;
        assert_eq!(mode_label(&app).0, "BLAME");
        app.refs.open = true;
        assert_eq!(mode_label(&app).0, "REFS");
        app.status.open = true;
        assert_eq!(mode_label(&app).0, "STATUS");
        app.search.state.active = true;
        assert_eq!(mode_label(&app).0, "SEARCH");
        app.show_help = true;
        assert_eq!(mode_label(&app).0, "HELP");
    }

    #[test]
    fn turkish_dotted_i_does_not_panic() {
        // "İ" (U+0130) lowercases to "i\u{0307}" — two chars from one. The
        // old impl could attempt to slice the original at a position
        // computed against the longer lowercase form. Just verify no panic
        // and the original content round-trips (matches are conservative
        // for chars whose case-fold differs in length).
        let parts = highlight("İstanbul", "istanbul");
        let recombined: String = parts.iter().map(|(s, _)| s.clone()).collect();
        assert_eq!(recombined, "İstanbul");
    }
}

pub fn diff_line_to_ratatui(line: &DiffLine) -> Line<'static> {
    let style = style_for(line.kind);
    // Add / Del / Context store their text *without* the unified-diff
    // prefix character — the renderer prepends it here based on `kind`.
    // This drops one per-line `format!` allocation from the producer
    // side (worth thousands on a large diff).
    let prefix = match line.kind {
        DiffLineKind::Add => "+",
        DiffLineKind::Del => "-",
        DiffLineKind::Context => " ",
        _ => "",
    };
    if prefix.is_empty() {
        Line::from(Span::styled(line.text.clone(), style))
    } else {
        Line::from(vec![Span::styled(prefix, style), Span::styled(line.text.clone(), style)])
    }
}

fn style_for(kind: DiffLineKind) -> Style {
    match kind {
        DiffLineKind::CommitHeader => Style::default().fg(Color::Yellow),
        DiffLineKind::Meta => Style::default(),
        DiffLineKind::Message => Style::default(),
        DiffLineKind::Blank => Style::default(),
        DiffLineKind::FileHeader => Style::default().fg(Color::Cyan),
        DiffLineKind::FileMeta => Style::default().fg(Color::Cyan),
        DiffLineKind::OldMarker => Style::default().fg(Color::Red),
        DiffLineKind::NewMarker => Style::default().fg(Color::Green),
        DiffLineKind::HunkHeader => Style::default().fg(Color::Cyan),
        DiffLineKind::Add => Style::default().fg(Color::Green),
        DiffLineKind::Del => Style::default().fg(Color::Red),
        DiffLineKind::Context => Style::default(),
        DiffLineKind::Diffstat => Style::default().fg(Color::Cyan),
        DiffLineKind::DiffstatTotal => Style::default().fg(Color::DarkGray),
        DiffLineKind::SectionTitle => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        DiffLineKind::SectionStaged => Style::default().fg(Color::Green),
        DiffLineKind::SectionUnstaged => Style::default().fg(Color::Yellow),
        DiffLineKind::Faint => Style::default().fg(Color::DarkGray),
        DiffLineKind::Good => Style::default().fg(Color::Green),
        DiffLineKind::StatusOurs => Style::default().fg(Color::Green),
        DiffLineKind::StatusTheirs => Style::default().fg(Color::Red),
    }
}
