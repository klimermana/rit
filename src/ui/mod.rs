pub mod diff_view;
pub mod help;
pub mod log_view;

use crate::app::App;
use crate::git::{DiffLine, DiffLineKind};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

/// Mirrors tig: prefer side-by-side panes when the aspect ratio is wide
/// enough that two stacked panes would each be very short.
fn use_vertical_split(width: u16, height: u16) -> bool {
    width > 160 || (width as f32 * 0.5) > ((height as f32 - 1.0) * 2.0)
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let size = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(size);

    // Title bar
    let title = format!(" rit  {}  [{}] ", app.repo_name, app.branch_name);
    frame.render_widget(
        Paragraph::new(title).style(
            Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        chunks[0],
    );

    let main_area = chunks[1];

    if app.status.open {
        draw_status_view(frame, app, main_area);
        draw_status_bar(frame, app, chunks[2]);
        if app.show_help { help::draw_help(frame, size); }
        return;
    }

    let (log_area, diff_area_opt) = if app.diff.open {
        if use_vertical_split(size.width, size.height) {
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(main_area);
            (panes[0], Some(panes[1]))
        } else {
            let panes = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                .split(main_area);
            (panes[0], Some(panes[1]))
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
        let is_diff_focused = matches!(app.focus, crate::app::Focus::Diff);
        diff_view::draw_diff(frame, app, diff_area, is_diff_focused);
    }

    draw_status_bar(frame, app, chunks[2]);

    if app.show_help { help::draw_help(frame, size); }
}

fn draw_status_view(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(if app.status.loading { " Status [loading...] " } else { " Status " })
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;
    let total = app.status.lines.len();
    if total == 0 { return; }
    let scroll = app.status.scroll.min(total.saturating_sub(1));
    let end = (scroll + height).min(total);
    let visible: Vec<Line> = app.status.lines[scroll..end].iter().map(diff_line_to_ratatui).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let text = if app.search.active {
        Line::from(vec![
            Span::raw(" /"),
            Span::styled(app.search.query.clone(), Style::default().fg(Color::Yellow)),
            Span::raw("█"),
        ])
    } else if !app.search.query.is_empty() {
        let pos = app.search.matches.binary_search(&app.log.selected).map(|i| i + 1).unwrap_or(0);
        Line::from(Span::styled(
            format!(
                " [{}/{}] n:next  N:prev  Esc:clear  {}",
                pos,
                app.search.matches.len(),
                if app.diff.open { "Tab:switch  q/Esc:close-diff" } else { "Enter:open-diff" }
            ),
            Style::default().fg(Color::Yellow),
        ))
    } else if let Some(y) = &app.yank_message {
        Line::from(Span::styled(format!(" ✓ {}", y.text), Style::default().fg(Color::Green)))
    } else if app.status.open {
        Line::from(Span::raw(" q/Esc/s:close-status  j/k:scroll  g/G:top/bottom"))
    } else if app.diff.open {
        Line::from(Span::raw(
            " q/Esc:close-diff  j/k:nav  Tab:switch  /:search  y:yank  s:status  ?:help",
        ))
    } else {
        let loading = if !app.walk_done { " [loading…]" } else { "" };
        Line::from(Span::raw(format!(
            " {} commits{}  q:quit  j/k:nav  Enter:diff  /:search  y:yank  s:status  ?:help  R:reload",
            app.log.commits.len(),
            loading
        )))
    };

    frame.render_widget(
        Paragraph::new(text).style(Style::default().bg(Color::DarkGray).fg(Color::Gray)),
        area,
    );
}

pub fn diff_line_to_ratatui(line: &DiffLine) -> Line<'_> {
    let style = style_for(line.kind);
    Line::from(Span::styled(line.text.as_str(), style))
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
        DiffLineKind::SectionTitle => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        DiffLineKind::SectionStaged => Style::default().fg(Color::Green),
        DiffLineKind::SectionUnstaged => Style::default().fg(Color::Yellow),
        DiffLineKind::Faint => Style::default().fg(Color::DarkGray),
        DiffLineKind::Good => Style::default().fg(Color::Green),
        DiffLineKind::StatusOurs => Style::default().fg(Color::Green),
        DiffLineKind::StatusTheirs => Style::default().fg(Color::Red),
    }
}
