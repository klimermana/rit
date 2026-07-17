//! Full-screen refs browser: local branches, remotes, and tags, each
//! peeled to its commit. `Enter` re-roots the log at the selected ref.

use crate::{app::App, model::RefKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

/// Width of the ref-name column; long names are char-truncated.
const NAME_COL_WIDTH: usize = 32;
const DATE_COL_WIDTH: usize = 8;

pub fn draw_refs_view(frame: &mut Frame, app: &mut App, area: Rect) {
    let title = if app.refs.loading { " Refs [loading...] " } else { " Refs " };
    let block = crate::ui::pane_block(title.to_string(), true, crate::ui::mode_accent(app));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Single source of truth for the pane height — input-time clamping
    // in `refs_move` reads this.
    app.refs.view_height = (inner.height as usize).max(1);

    if app.refs.entries.is_empty() {
        let msg = if app.refs.loading { "Loading refs..." } else { "No branches or tags found" };
        frame.render_widget(Paragraph::new(msg).style(Style::default().fg(Color::Gray)), inner);
        return;
    }

    let height = inner.height as usize;
    let scroll = app.refs.scroll;
    let end = (scroll + height).min(app.refs.entries.len());

    let mut rows: Vec<Line> = Vec::with_capacity(height);
    for (i, entry) in app.refs.entries.get(scroll..end).unwrap_or(&[]).iter().enumerate() {
        let global_idx = scroll + i;
        let (fg, modifier) = match entry.kind {
            RefKind::Head => (Color::Cyan, Modifier::BOLD),
            RefKind::LocalBranch => (Color::Cyan, Modifier::empty()),
            RefKind::RemoteBranch => (Color::Magenta, Modifier::empty()),
            RefKind::Tag => (Color::Yellow, Modifier::empty()),
        };

        let name = truncate_chars(&entry.name, NAME_COL_WIDTH);
        let spans = vec![
            Span::styled(format!("{name:<NAME_COL_WIDTH$}"), Style::default().fg(fg).add_modifier(modifier)),
            Span::raw(" "),
            Span::styled(format!("{:<DATE_COL_WIDTH$}", entry.authored_relative), Style::default().fg(Color::Blue)),
            Span::raw(" "),
            Span::raw(entry.summary.clone()),
        ];

        let row_style = if global_idx == app.refs.selected {
            Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        rows.push(Line::from(spans).style(row_style));
    }

    frame.render_widget(Paragraph::new(rows), inner);
}

/// Char-aware truncation (multibyte-safe), mirroring the log view's.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.char_indices().nth(max_chars).and_then(|(boundary, _)| s.get(..boundary)).unwrap_or(s).to_string()
}
