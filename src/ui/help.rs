use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

pub fn draw_help(frame: &mut Frame, area: Rect) {
    let width = 52u16.min(area.width.saturating_sub(4));
    // 53 content rows + 2 border rows; clamped to the terminal height.
    let height = 55u16.min(area.height.saturating_sub(4));

    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;

    let popup_area = Rect { x, y, width, height };

    frame.render_widget(Clear, popup_area);

    // White matches the HELP mode chip — the popup and the chip carry
    // the same accent, like every other mode.
    let block = Block::default()
        .title(Span::styled(" Help ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::White))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let h = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let k = Style::default().fg(Color::Yellow);
    let d = Style::default().fg(Color::White);

    macro_rules! row {
        ($key:expr_2021, $desc:expr_2021) => {
            Line::from(vec![Span::styled($key, k), Span::styled($desc, d)])
        };
    }

    let lines: Vec<Line> = vec![
        Line::from(Span::styled("Navigation", h)),
        row!("  j / ↓          ", "Move down"),
        row!("  k / ↑          ", "Move up"),
        row!("  g / G          ", "Top / Bottom"),
        row!("  Ctrl+D / Ctrl+U", "Half-page down / up"),
        row!("  PageDown / PageUp", "Full-page down / up"),
        Line::from(""),
        Line::from(Span::styled("Diff View", h)),
        row!("  Enter          ", "Open diff pane for selected commit"),
        row!("  Tab            ", "Switch focus: log ↔ diff"),
        row!("  h / l / ← / →  ", "Scroll left / right (4 cols)"),
        row!("  0              ", "Reset horizontal scroll"),
        row!("  ] / [          ", "Jump to next / previous file in diff"),
        row!("  t              ", "File picker: j/k select, / filter, Enter, o"),
        row!("  o              ", "Full-file diff of the file at the top (q backs out)"),
        row!("  b              ", "Blame the file at the top of the diff pane"),
        Line::from(""),
        Line::from(Span::styled("Blame View", h)),
        row!("  Enter          ", "Open the selected line's commit"),
        row!("  ,              ", "Re-blame at that commit's parent"),
        row!("  Backspace      ", "Back to the previous blame"),
        row!("  q / Esc        ", "Close diff pane (or quit from log)"),
        Line::from(""),
        Line::from(Span::styled("Search", h)),
        row!("  /              ", "Start search (message, author, branch, or tag)"),
        row!("  n / N          ", "Next / previous match"),
        row!("  Esc            ", "Clear search"),
        Line::from(""),
        Line::from(Span::styled("Actions", h)),
        row!("  y              ", "Yank (copy) commit hash to clipboard"),
        row!("  s              ", "Open working tree status view"),
        row!("  r              ", "Browse branches & tags (Enter opens log at ref)"),
        row!("  R              ", "Reload"),
        Line::from(""),
        Line::from(Span::styled("Display", h)),
        row!("  #              ", "Toggle line numbers in diff"),
        row!("  v              ", "Toggle patch hunks (summary view when off)"),
        row!("  f              ", "Toggle limiting the diff to the CLI pathspec"),
        row!("  D              ", "Cycle date column: relative / absolute / off"),
        row!("  A              ", "Cycle author column: full / abbreviated / off"),
        row!("  X              ", "Toggle full 40-char commit hash"),
        Line::from(""),
        Line::from(Span::styled("Mouse", h)),
        row!("  wheel          ", "Scroll the pane under the pointer"),
        row!("  click          ", "Focus pane / select commit"),
        row!("  drag in diff   ", "Select text; copied on release"),
        row!("  double-click   ", "Select the word under the cursor"),
        row!("  triple-click   ", "Select the whole line"),
        row!("  Opt/Alt+drag   ", "Terminal's native text selection"),
        Line::from(""),
        Line::from(Span::styled("CLI", h)),
        row!("  rit <pathspec> ", "Limit log to commits matching pathspec (git log -- semantics)"),
        row!("  rit blame <path>", "Launch straight into the blame view"),
        Line::from(""),
        row!("  ? / q / Esc    ", "Close this help"),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}
