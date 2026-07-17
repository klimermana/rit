//! Full-screen blame view: every line of a file annotated with the
//! commit / author / age that introduced it. Line-cursor based —
//! `Enter` opens the selected line's commit diff, `,` re-blames at
//! that commit's parent, Backspace walks back.

use crate::app::App;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

const AUTHOR_COL_WIDTH: usize = 12;
const DATE_COL_WIDTH: usize = 8;

pub fn draw_blame_view(frame: &mut Frame, app: &mut App, area: Rect) {
    let title = match app.blame.document.as_ref() {
        Some(doc) => {
            let rev = doc.at.to_hex_with_len(7);
            if app.blame.loading {
                format!(" Blame {} @ {rev} [loading...] ", doc.path)
            } else {
                format!(" Blame {} @ {rev} ", doc.path)
            }
        }
        None => " Blame [loading...] ".to_string(),
    };
    let block = crate::ui::pane_block(title, true, crate::ui::mode_accent(app));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Single source of truth for the pane height — input-time cursor
    // clamping in `blame_move` reads this.
    app.blame.view_height = (inner.height as usize).max(1);

    if let Some(err) = &app.blame.error {
        let para = Paragraph::new(format!("Error: {err}")).style(Style::default().fg(Color::Red));
        frame.render_widget(para, inner);
        return;
    }

    let Some(doc) = app.blame.document.as_ref() else {
        frame.render_widget(Paragraph::new("Annotating...").style(Style::default().fg(Color::Gray)), inner);
        return;
    };
    if doc.lines.is_empty() {
        frame.render_widget(Paragraph::new("(empty file)").style(Style::default().fg(Color::DarkGray)), inner);
        return;
    }

    let height = inner.height as usize;
    let scroll = app.blame.scroll;
    let end = (scroll + height).min(doc.lines.len());
    // Line-number gutter sized for the largest visible number.
    let no_width = doc.lines.len().to_string().len();

    let mut rows: Vec<Line> = Vec::with_capacity(height);
    for (i, line) in doc.lines.get(scroll..end).unwrap_or(&[]).iter().enumerate() {
        let global_idx = scroll + i;
        let author = truncate_chars(&line.author, AUTHOR_COL_WIDTH);
        let spans = vec![
            Span::styled(line.commit_short.to_string(), Style::default().fg(Color::Yellow)),
            Span::raw(" "),
            Span::styled(format!("{:<DATE_COL_WIDTH$}", line.authored_relative), Style::default().fg(Color::Blue)),
            Span::raw(" "),
            Span::styled(format!("{author:<AUTHOR_COL_WIDTH$}"), Style::default().fg(Color::Green)),
            Span::raw(" "),
            Span::styled(format!("{:>no_width$} │ ", line.line_no), Style::default().fg(Color::DarkGray)),
            Span::raw(line.text.clone()),
        ];

        let row_style = if global_idx == app.blame.selected {
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
