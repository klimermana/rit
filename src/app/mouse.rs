//! Mouse input dispatch. Capture is enabled at startup, so the terminal
//! hands us wheel/click/drag events instead of doing its own screen-wide
//! text selection (hold Option/Alt for the terminal's native selection).
//!
//! What the mouse does:
//!   - wheel: scrolls whichever pane (or full-screen view) is under the
//!     pointer
//!   - click: focuses the pane; in the log it also selects the clicked
//!     commit
//!   - drag in the diff pane: selects whole lines — confined to the
//!     pane, so a side-by-side layout never bleeds the log into the
//!     copy — and yanks the gutter-stripped text to the clipboard on
//!     release

use crate::app::{
    App, YankFeedback,
    clipboard::yank_to_clipboard,
    state::{DiffSelection, Focus},
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::time::Instant;

/// Lines per wheel tick. Matches the common terminal-emulator default.
const WHEEL_STEP: usize = 3;

fn hit(rect: Option<Rect>, column: u16, row: u16) -> bool {
    rect.is_some_and(|r| column >= r.x && column < r.x + r.width && row >= r.y && row < r.y + r.height)
}

impl App {
    pub fn handle_mouse(&mut self, ev: MouseEvent) {
        use MouseEventKind as K;
        // Full-screen views: the wheel scrolls them, everything else is
        // ignored — there is no selection surface there (yet).
        if self.status.open {
            match ev.kind {
                K::ScrollDown => self.status.scroll = self.status.scroll.saturating_add(WHEEL_STEP),
                K::ScrollUp => self.status.scroll = self.status.scroll.saturating_sub(WHEEL_STEP),
                _ => {}
            }
            return;
        }
        if self.refs.open {
            match ev.kind {
                K::ScrollDown => self.refs_move(WHEEL_STEP as isize),
                K::ScrollUp => self.refs_move(-(WHEEL_STEP as isize)),
                _ => {}
            }
            return;
        }
        if self.blame.open {
            match ev.kind {
                K::ScrollDown => self.blame_move(WHEEL_STEP as isize),
                K::ScrollUp => self.blame_move(-(WHEEL_STEP as isize)),
                _ => {}
            }
            return;
        }

        match ev.kind {
            K::ScrollDown | K::ScrollUp => self.wheel(ev),
            K::Down(MouseButton::Left) => self.mouse_down(ev.column, ev.row),
            K::Drag(MouseButton::Left) => self.mouse_drag(ev.row),
            K::Up(MouseButton::Left) => self.mouse_up(),
            _ => {}
        }
    }

    fn wheel(&mut self, ev: MouseEvent) {
        let down = matches!(ev.kind, MouseEventKind::ScrollDown);
        if hit(self.panes.diff, ev.column, ev.row) {
            if down {
                self.diff_scroll_down(WHEEL_STEP);
            } else {
                self.diff.scroll = self.diff.scroll.saturating_sub(WHEEL_STEP);
            }
        } else if hit(self.panes.log, ev.column, ev.row) {
            // The log is cursor-driven, so the wheel moves the selection
            // like j/k does (and refetches the diff when the pane is open).
            if down {
                self.move_log_down(WHEEL_STEP);
            } else {
                self.move_log_up(WHEEL_STEP);
            }
        }
    }

    fn mouse_down(&mut self, column: u16, row: u16) {
        // Any press drops the previous (finished) selection highlight.
        self.diff.selection = None;
        if hit(self.panes.diff, column, row) {
            // panes.diff is only Some while the diff pane is drawn, so
            // Focus::Diff is always a legal state here.
            self.focus = Focus::Diff;
            let total = self.diff.total_visible_lines();
            if total == 0 {
                return;
            }
            let inner = self.panes.diff.unwrap_or_default();
            let idx = self.diff_line_at(inner, row, total);
            self.diff.selection = Some(DiffSelection { anchor: idx, cursor: idx, dragging: true, moved: false });
        } else if hit(self.panes.log, column, row) {
            self.focus = Focus::Log;
            let inner = self.panes.log.unwrap_or_default();
            let idx = self.log.scroll + usize::from(row.saturating_sub(inner.y));
            if idx < self.log.rows.len() && idx != self.log.selected {
                self.log.selected = idx;
                if self.diff.open {
                    self.fetch_diff_for_selected();
                }
            }
        }
    }

    fn mouse_drag(&mut self, row: u16) {
        let Some(inner) = self.panes.diff else { return };
        let total = self.diff.total_visible_lines();
        if inner.height == 0 || total == 0 || !self.diff.selection.as_ref().is_some_and(|s| s.dragging) {
            return;
        }
        // Dragging past the pane edge auto-scrolls one line per event,
        // so long selections don't require pre-scrolling the view.
        if row < inner.y {
            self.diff.scroll = self.diff.scroll.saturating_sub(1);
        } else if row >= inner.y + inner.height {
            self.diff_scroll_down(1);
        }
        let clamped_row = row.clamp(inner.y, inner.y + inner.height - 1);
        let cursor = self.diff_line_at(inner, clamped_row, total);
        if let Some(sel) = self.diff.selection.as_mut() {
            sel.cursor = cursor;
            sel.moved = true;
        }
    }

    fn mouse_up(&mut self) {
        let Some(sel) = self.diff.selection.as_mut() else { return };
        if !sel.dragging {
            return;
        }
        sel.dragging = false;
        if !sel.moved {
            // A plain click: focus change only — no copy, no lingering
            // one-line highlight.
            self.diff.selection = None;
            return;
        }
        self.copy_diff_selection();
    }

    /// Screen row → virtual line index, mirroring the renderer's scroll
    /// clamp. Caller guarantees `total > 0`.
    fn diff_line_at(&self, inner: Rect, row: u16, total: usize) -> usize {
        let scroll = self.diff.scroll.min(total - 1);
        (scroll + usize::from(row.saturating_sub(inner.y))).min(total - 1)
    }

    fn copy_diff_selection(&mut self) {
        let Some(sel) = self.diff.selection.as_ref() else { return };
        let (lo, hi) = sel.range();
        let text = self.diff_selection_text(lo, hi);
        if text.is_empty() {
            return;
        }
        let count = text.lines().count();
        yank_to_clipboard(&text);
        self.yank_message = Some(YankFeedback {
            text: format!("Copied {count} line{}", if count == 1 { "" } else { "s" }),
            shown_at: Instant::now(),
        });
    }

    /// Plain-text content of virtual lines `lo..=hi`, in the same
    /// header → diffstat → body order the renderer draws — minus the
    /// gutter: no line numbers and no `+`/`-`/space prefix
    /// (`DiffLine.text` already stores body lines unprefixed).
    pub(crate) fn diff_selection_text(&self, lo: usize, hi: usize) -> String {
        let Some(doc) = self.diff.document.as_ref() else { return String::new() };
        let total = self.diff.total_visible_lines();
        if total == 0 {
            return String::new();
        }
        let hi = hi.min(total - 1);
        let header_len = doc.header.len();
        let body_offset = header_len + self.diff.diffstat_line_count();

        // Diffstat rows only exist as render-time Lines; rebuild them
        // once and flatten the spans, trimming the column padding.
        let diffstat: Vec<String> = if lo < body_offset && hi >= header_len && !doc.files.is_empty() {
            let mut lines = Vec::new();
            crate::ui::diff_view::append_diffstat(&mut lines, &doc.files, &doc.stats, None, None);
            lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>().trim_end().to_string())
                .collect()
        } else {
            Vec::new()
        };

        let mut out = String::new();
        for idx in lo..=hi {
            let line: &str = if idx < header_len {
                doc.header.get(idx).map(|l| l.text.as_str()).unwrap_or("")
            } else if idx < body_offset {
                diffstat.get(idx - header_len).map(String::as_str).unwrap_or("")
            } else {
                doc.body.get(idx - body_offset).map(|l| l.text.as_str()).unwrap_or("")
            };
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        git::{GitMsg, GitReq},
        model::{DiffDocument, DiffFlags, DiffLine, DiffLineKind, DiffStats, DiffTarget, FileStat},
    };
    use crossbeam_channel::{Receiver, Sender};
    use crossterm::event::{Event, KeyModifiers};

    struct WorkerEnds {
        _req_rx: Receiver<GitReq>,
        _msg_tx: Sender<GitMsg>,
        _input_tx: Sender<Event>,
    }

    fn test_app() -> (App, WorkerEnds) {
        let (req_tx, req_rx) = crossbeam_channel::unbounded();
        let (msg_tx, msg_rx) = crossbeam_channel::unbounded();
        let (input_tx, input_rx) = crossbeam_channel::unbounded();
        (App::new(req_tx, msg_rx, input_rx), WorkerEnds { _req_rx: req_rx, _msg_tx: msg_tx, _input_tx: input_tx })
    }

    /// Header 1 line, two files → diffstat renders as 5 lines
    /// (separator + 2 rows + totals + blank), body offset = 6.
    fn doc() -> DiffDocument {
        DiffDocument {
            target: DiffTarget::WorkingTree,
            header: vec![DiffLine::new(DiffLineKind::CommitHeader, "Working tree")],
            body: vec![
                DiffLine::new(DiffLineKind::HunkHeader, "@@ -1,2 +1,2 @@"),
                DiffLine::new(DiffLineKind::Del, "old line"),
                DiffLine::new(DiffLineKind::Add, "new line"),
                DiffLine::new(DiffLineKind::Context, "context line"),
            ],
            files: vec![
                FileStat { path: "a.rs".into(), additions: 1, deletions: 1 },
                FileStat { path: "b.rs".into(), additions: 0, deletions: 0 },
            ],
            stats: DiffStats { files: 2, insertions: 1, deletions: 1 },
            flags: DiffFlags::default(),
            untracked_anchor: None,
            sections: Vec::new(),
        }
    }

    /// App with the diff pane open on `doc()` and both pane rects laid
    /// out side by side: log columns 0..40, diff columns 40..80.
    fn app_with_diff() -> (App, WorkerEnds) {
        let (mut app, ends) = test_app();
        app.diff.open = true;
        app.diff.target = Some(DiffTarget::WorkingTree);
        app.diff.document = Some(doc());
        app.panes.log = Some(Rect::new(1, 2, 38, 20));
        app.panes.diff = Some(Rect::new(41, 2, 38, 20));
        (app, ends)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
    }

    #[test]
    fn selection_text_strips_gutter_and_rebuilds_diffstat() {
        let (app, _ends) = app_with_diff();
        // Whole document: header line, 5 diffstat lines, 4 body lines.
        let text = app.diff_selection_text(0, 9);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[0], "Working tree");
        assert_eq!(lines[1], "---");
        assert!(lines[2].starts_with(" a.rs"), "diffstat row: {:?}", lines[2]);
        // Body lines come back without the +/-/space prefix.
        assert_eq!(lines[7], "old line");
        assert_eq!(lines[8], "new line");
        assert_eq!(lines[9], "context line");
    }

    #[test]
    fn drag_in_diff_pane_selects_lines_and_release_reports_copy() {
        let (mut app, _ends) = app_with_diff();
        use MouseEventKind as K;
        // Press on the pane's first row (virtual line 0), drag down two rows.
        app.handle_mouse(mouse(K::Down(MouseButton::Left), 45, 2));
        app.handle_mouse(mouse(K::Drag(MouseButton::Left), 45, 4));
        assert!(matches!(app.focus, Focus::Diff));
        let sel = app.diff.selection.as_ref().expect("selection active");
        assert_eq!(sel.range(), (0, 2));
        app.handle_mouse(mouse(K::Up(MouseButton::Left), 45, 4));
        let sel = app.diff.selection.as_ref().expect("selection survives release");
        assert!(!sel.dragging);
        assert_eq!(app.yank_message.as_ref().map(|y| y.text.as_str()), Some("Copied 3 lines"));
    }

    #[test]
    fn drag_maps_rows_through_scroll_and_clamps_to_document() {
        let (mut app, _ends) = app_with_diff();
        app.diff.scroll = 4;
        use MouseEventKind as K;
        app.handle_mouse(mouse(K::Down(MouseButton::Left), 45, 3));
        // Row 3 is one below the pane top → virtual line 4 + 1 = 5.
        assert_eq!(app.diff.selection.as_ref().unwrap().anchor, 5);
        // Dragging way past the last document line clamps to total-1 (9).
        app.handle_mouse(mouse(K::Drag(MouseButton::Left), 45, 15));
        assert_eq!(app.diff.selection.as_ref().unwrap().range(), (5, 9));
    }

    #[test]
    fn plain_click_focuses_diff_without_copying() {
        let (mut app, _ends) = app_with_diff();
        use MouseEventKind as K;
        app.handle_mouse(mouse(K::Down(MouseButton::Left), 45, 2));
        app.handle_mouse(mouse(K::Up(MouseButton::Left), 45, 2));
        assert!(matches!(app.focus, Focus::Diff));
        assert!(app.diff.selection.is_none(), "click alone must not leave a selection");
        assert!(app.yank_message.is_none(), "click alone must not copy");
    }

    #[test]
    fn click_in_log_pane_selects_the_clicked_row() {
        let (mut app, _ends) = app_with_diff();
        app.focus = Focus::Diff;
        // Rows beyond the working-tree row so a click can land on index 2.
        app.log
            .rows
            .push(crate::app::LogRow::WorkingTree(crate::app::WorkingTreeRow { author: "a".into(), dirty: None }));
        app.log
            .rows
            .push(crate::app::LogRow::WorkingTree(crate::app::WorkingTreeRow { author: "b".into(), dirty: None }));
        use MouseEventKind as K;
        app.handle_mouse(mouse(K::Down(MouseButton::Left), 5, 4));
        assert!(matches!(app.focus, Focus::Log));
        assert_eq!(app.log.selected, 2);
    }

    #[test]
    fn wheel_scrolls_the_pane_under_the_pointer() {
        let (mut app, _ends) = app_with_diff();
        use MouseEventKind as K;
        app.handle_mouse(mouse(K::ScrollDown, 45, 5));
        assert_eq!(app.diff.scroll, WHEEL_STEP);
        app.handle_mouse(mouse(K::ScrollUp, 45, 5));
        assert_eq!(app.diff.scroll, 0);
        // Outside both panes: nothing moves.
        app.handle_mouse(mouse(K::ScrollDown, 45, 40));
        assert_eq!(app.diff.scroll, 0);
    }
}
