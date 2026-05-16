use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

pub fn draw_help(frame: &mut Frame, area: Rect) {
    let width = 52u16.min(area.width.saturating_sub(4));
    let height = 32u16.min(area.height.saturating_sub(4));

    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;

    let popup_area = Rect { x, y, width, height };

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let h = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let k = Style::default().fg(Color::Yellow);
    let d = Style::default().fg(Color::White);

    macro_rules! row {
        ($key:expr, $desc:expr) => {
            Line::from(vec![Span::styled($key, k), Span::styled($desc, d)])
        };
    }

    let lines: Vec<Line> = vec![
        Line::from(Span::styled("Navigation", h)),
        row!("  j / ↓          ", "Move down"),
        row!("  k / ↑          ", "Move up"),
        row!("  g / G          ", "Top / Bottom"),
        row!("  Ctrl+D / Ctrl+U", "Half-page down / up"),
        Line::from(""),
        Line::from(Span::styled("Diff View", h)),
        row!("  Enter          ", "Open diff pane for selected commit"),
        row!("  Tab            ", "Switch focus: log ↔ diff"),
        row!("  q / Esc        ", "Close diff pane (or quit from log)"),
        Line::from(""),
        Line::from(Span::styled("Search", h)),
        row!("  /              ", "Start search (message or author)"),
        row!("  n / N          ", "Next / previous match"),
        row!("  Esc            ", "Clear search"),
        Line::from(""),
        Line::from(Span::styled("Actions", h)),
        row!("  y              ", "Yank (copy) commit hash to clipboard"),
        row!("  s              ", "Open working tree status view"),
        row!("  R              ", "Reload from HEAD"),
        Line::from(""),
        Line::from(Span::styled("Display", h)),
        row!("  #              ", "Toggle line numbers in diff"),
        Line::from(""),
        Line::from(Span::styled("CLI", h)),
        row!("  rit <path>     ", "Limit log to commits touching <path>"),
        Line::from(""),
        row!("  ? / q / Esc    ", "Close this help"),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}
