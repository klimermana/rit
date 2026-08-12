//! Mouse input dispatch. Capture is enabled at startup, so the terminal
//! hands us wheel/click/drag events instead of doing its own screen-wide
//! text selection (hold Option/Alt for the terminal's native selection).
//!
//! What the mouse does:
//!   - wheel: scrolls whichever pane (or full-screen view) is under the
//!     pointer
//!   - click: focuses the pane; in the log it also selects the clicked
//!     commit
//!   - drag in the diff pane: character-precise text selection —
//!     confined to the pane, so a side-by-side layout never bleeds the
//!     log into the copy — and yanks the gutter-stripped text to the
//!     clipboard on release
//!   - double-click in the diff pane selects the word under the cell,
//!     triple-click the whole line

use crate::{
    app::{
        App, YankFeedback,
        clipboard::yank_to_clipboard,
        state::{DiffSelection, Focus, cell_in_selection},
    },
    model::DiffLineKind,
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::time::Instant;
use unicode_width::UnicodeWidthChar;

/// Lines per wheel tick. Matches the common terminal-emulator default.
const WHEEL_STEP: usize = 3;

/// Consecutive presses on the same cell, each within this window of the
/// previous one, count up a multi-click: 2 selects the word, 3 (or
/// more) the whole line.
const MULTI_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);

/// Chars that glue a word together beyond alphanumerics — iTerm's
/// default "characters considered part of a word", so double-clicking
/// a path (`src/app/mouse.rs`), a flag (`--baseline`), or a version
/// (`0.2.2`) grabs the whole thing.
const WORD_JOINERS: &str = "_./-+~\\";

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
            K::Drag(MouseButton::Left) => self.mouse_drag(ev.column, ev.row),
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
            let pos = (self.diff_line_at(inner, row, total), self.diff_cell_at(inner, column));
            let now = Instant::now();
            let count = match self.last_diff_click.take() {
                Some((when, prev, n)) if prev == pos && now.duration_since(when) <= MULTI_CLICK_WINDOW => n + 1,
                _ => 1,
            };
            self.last_diff_click = Some((now, pos, count));
            // Double-click selects the word under the cell, triple the
            // whole line. `moved` starts true so the release copies
            // even though no drag happened; dragging stays live so the
            // selection can still be extended before release.
            let span = match count {
                1 => None,
                2 => self.diff_word_cells(pos.0, pos.1),
                _ => self.diff_line_cells(pos.0),
            };
            self.diff.selection = Some(match span {
                Some((start, end)) => {
                    DiffSelection { anchor: (pos.0, start), cursor: (pos.0, end), dragging: true, moved: true }
                }
                None => DiffSelection { anchor: pos, cursor: pos, dragging: true, moved: false },
            });
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

    fn mouse_drag(&mut self, column: u16, row: u16) {
        let Some(inner) = self.panes.diff else { return };
        let total = self.diff.total_visible_lines();
        if inner.width == 0
            || inner.height == 0
            || total == 0
            || !self.diff.selection.as_ref().is_some_and(|s| s.dragging)
        {
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
        let clamped_col = column.clamp(inner.x, inner.x + inner.width - 1);
        let cursor = (self.diff_line_at(inner, clamped_row, total), self.diff_cell_at(inner, clamped_col));
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

    /// Screen column → display cell of the *unscrolled* rendered row:
    /// pane-relative x plus the horizontal scroll the renderer applies,
    /// so the selection stays glued to content when the view shifts.
    fn diff_cell_at(&self, inner: Rect, column: u16) -> usize {
        self.diff.horizontal_scroll + usize::from(column.saturating_sub(inner.x))
    }

    /// Cells the gutter occupies at the start of a rendered row —
    /// mirrors the renderer's `format!("{:>4} ", idx + 1)`.
    fn diff_gutter_cells(&self, idx: usize) -> usize {
        if !self.diff.show_line_numbers {
            return 0;
        }
        let digits = idx.saturating_add(1).ilog10() as usize + 1;
        digits.max(4) + 1
    }

    /// Rendered-content text of virtual line `idx`, plus the display
    /// cell where it starts (after the gutter and any `+`/`-` prefix).
    fn diff_line_text(&self, idx: usize) -> Option<(String, usize)> {
        let doc = self.diff.document.as_ref()?;
        let header_len = doc.header.len();
        let body_offset = header_len + self.diff.diffstat_line_count();
        let (text, kind) = if idx < header_len {
            let l = doc.header.get(idx)?;
            (l.text.clone(), Some(l.kind))
        } else if idx < body_offset {
            let mut lines = Vec::new();
            crate::ui::diff_view::append_diffstat(&mut lines, &doc.files, &doc.stats, None, None);
            let l = lines.get(idx - header_len)?;
            (l.spans.iter().map(|s| s.content.as_ref()).collect::<String>().trim_end().to_string(), None)
        } else {
            let l = doc.body.get(idx - body_offset)?;
            (l.text.clone(), Some(l.kind))
        };
        let prefix = usize::from(matches!(kind, Some(DiffLineKind::Add | DiffLineKind::Del | DiffLineKind::Context)));
        Some((text, self.diff_gutter_cells(idx) + prefix))
    }

    /// The word under `cell` on virtual line `idx`, as an inclusive
    /// display-cell span. `None` when the cell is in the gutter, past
    /// the end of the line, or the line doesn't exist.
    fn diff_word_cells(&self, idx: usize, cell: usize) -> Option<(usize, usize)> {
        let (text, offset) = self.diff_line_text(idx)?;
        word_cells_at(&text, offset, cell)
    }

    /// All of virtual line `idx` as an inclusive display-cell span,
    /// from the left edge (cell 0, so the highlight covers the gutter
    /// like a cross-line drag does) to the last content cell. `None`
    /// only when the line doesn't exist; copying an empty line yields
    /// empty text, which the copy path already drops.
    fn diff_line_cells(&self, idx: usize) -> Option<(usize, usize)> {
        let (text, offset) = self.diff_line_text(idx)?;
        let width: usize = text.chars().map(|c| c.width().unwrap_or(0)).sum();
        Some((0, (offset + width).saturating_sub(1)))
    }

    fn copy_diff_selection(&mut self) {
        let Some(sel) = self.diff.selection.as_ref() else { return };
        let (lo, hi) = sel.range();
        let text = self.diff_selection_text(lo, hi);
        if text.is_empty() {
            return;
        }
        let feedback = if lo.0 == hi.0 {
            let count = text.chars().count();
            format!("Copied {count} char{}", if count == 1 { "" } else { "s" })
        } else {
            let count = text.lines().count();
            format!("Copied {count} line{}", if count == 1 { "" } else { "s" })
        };
        yank_to_clipboard(&text);
        self.yank_message = Some(YankFeedback { text: feedback, shown_at: Instant::now() });
    }

    /// Plain text of the selection from `lo` to `hi` (`(line, cell)`
    /// pairs, `lo <= hi`), in the same header → diffstat → body order
    /// the renderer draws. Fully-covered lines come back whole and
    /// gutter-stripped — no line numbers, no `+`/`-`/space prefix
    /// (`DiffLine.text` already stores body lines unprefixed); the
    /// first and last line are cut to the selected cells. Lines are
    /// newline-separated with no trailing newline, like a terminal's
    /// own selection.
    pub(crate) fn diff_selection_text(&self, lo: (usize, usize), hi: (usize, usize)) -> String {
        let Some(doc) = self.diff.document.as_ref() else { return String::new() };
        let total = self.diff.total_visible_lines();
        if total == 0 {
            return String::new();
        }
        let hi = if hi.0 < total { hi } else { (total - 1, usize::MAX - 1) };
        if lo > hi {
            return String::new();
        }
        // Borrow the per-line cell spans from a selection shaped like
        // the (ordered) input, so copy and highlight can't disagree.
        let sel = DiffSelection { anchor: lo, cursor: hi, dragging: false, moved: true };
        let header_len = doc.header.len();
        let body_offset = header_len + self.diff.diffstat_line_count();

        // Diffstat rows only exist as render-time Lines; rebuild them
        // once and flatten the spans, trimming the column padding.
        let diffstat: Vec<String> = if lo.0 < body_offset && hi.0 >= header_len && !doc.files.is_empty() {
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
        for idx in lo.0..=hi.0 {
            let (line, kind): (&str, Option<DiffLineKind>) = if idx < header_len {
                let l = doc.header.get(idx);
                (l.map(|l| l.text.as_str()).unwrap_or(""), l.map(|l| l.kind))
            } else if idx < body_offset {
                (diffstat.get(idx - header_len).map(String::as_str).unwrap_or(""), None)
            } else {
                let l = doc.body.get(idx - body_offset);
                (l.map(|l| l.text.as_str()).unwrap_or(""), l.map(|l| l.kind))
            };
            let prefix =
                usize::from(matches!(kind, Some(DiffLineKind::Add | DiffLineKind::Del | DiffLineKind::Context)));
            let offset = self.diff_gutter_cells(idx) + prefix;
            if idx > lo.0 {
                out.push('\n');
            }
            if let Some((start, end)) = sel.cells_on_line(idx) {
                out.push_str(slice_by_cells(line, offset, start, end));
            }
        }
        out
    }
}

/// The run of like-classed chars covering display cell `cell`, as an
/// inclusive cell span. Chars fall into three classes — word
/// (alphanumeric or [`WORD_JOINERS`]), whitespace, and other — and a
/// double-click grabs the whole run of whichever class it lands on,
/// like iTerm. `text`'s first char sits at cell `offset`; a `cell`
/// outside the text yields `None`.
fn word_cells_at(text: &str, offset: usize, cell: usize) -> Option<(usize, usize)> {
    #[derive(Clone, Copy, PartialEq)]
    enum Class {
        Word,
        Space,
        Other,
    }
    fn classify(c: char) -> Class {
        if c.is_alphanumeric() || WORD_JOINERS.contains(c) {
            Class::Word
        } else if c.is_whitespace() {
            Class::Space
        } else {
            Class::Other
        }
    }

    // Per-char (start cell, width, class), remembering which char the
    // clicked cell landed on (zero-width chars can't be clicked).
    let mut acc = offset;
    let mut chars: Vec<(usize, usize, Class)> = Vec::new();
    let mut hit = None;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if w > 0 && (acc..acc + w).contains(&cell) {
            hit = Some(chars.len());
        }
        chars.push((acc, w, classify(c)));
        acc += w;
    }
    let i = hit?;
    let class = chars.get(i)?.2;
    let mut lo = i;
    while lo > 0 && chars.get(lo - 1).is_some_and(|c| c.2 == class) {
        lo -= 1;
    }
    let mut hi = i;
    while chars.get(hi + 1).is_some_and(|c| c.2 == class) {
        hi += 1;
    }
    let start = chars.get(lo)?.0;
    let (end_start, end_w, _) = *chars.get(hi)?;
    Some((start, end_start + end_w.saturating_sub(1)))
}

/// Substring of `text` covering the selected display cells
/// `start..end` (`end: None` = to end of line), where `text`'s first
/// char sits at cell `offset` of the rendered row — i.e. the gutter
/// and `+`/`-` prefix occupy `0..offset` and are never part of the
/// result.
fn slice_by_cells(text: &str, offset: usize, start: usize, end: Option<usize>) -> &str {
    let mut acc = offset;
    let mut from: Option<usize> = None;
    let mut to = 0;
    for (byte, ch) in text.char_indices() {
        let w = ch.width().unwrap_or(0);
        if cell_in_selection(acc, w, start, end) {
            from.get_or_insert(byte);
            to = byte + ch.len_utf8();
        }
        acc += w;
    }
    from.and_then(|f| text.get(f..to)).unwrap_or("")
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
        let text = app.diff_selection_text((0, 0), (9, usize::MAX - 1));
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
        // Press on the pane's first row (virtual line 0), drag down two
        // rows and right past every line's text.
        app.handle_mouse(mouse(K::Down(MouseButton::Left), 45, 2));
        app.handle_mouse(mouse(K::Drag(MouseButton::Left), 78, 4));
        assert!(matches!(app.focus, Focus::Diff));
        let sel = app.diff.selection.as_ref().expect("selection active");
        assert_eq!(sel.range(), ((0, 4), (2, 37)));
        app.handle_mouse(mouse(K::Up(MouseButton::Left), 78, 4));
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
        assert_eq!(app.diff.selection.as_ref().unwrap().anchor, (5, 4));
        // Dragging way past the last document line clamps to total-1 (9).
        app.handle_mouse(mouse(K::Drag(MouseButton::Left), 45, 15));
        assert_eq!(app.diff.selection.as_ref().unwrap().line_range(), (5, 9));
    }

    #[test]
    fn drag_within_line_selects_a_subsection() {
        let (mut app, _ends) = app_with_diff();
        use MouseEventKind as K;
        // Virtual line 7 ("old line", Del) draws at row 9; its content
        // starts at cell 6 (5-cell number gutter + the `-` prefix), so
        // cells 6..=8 cover exactly "old". Pane x = 41 → columns 47..=49.
        app.handle_mouse(mouse(K::Down(MouseButton::Left), 47, 9));
        app.handle_mouse(mouse(K::Drag(MouseButton::Left), 49, 9));
        app.handle_mouse(mouse(K::Up(MouseButton::Left), 49, 9));
        assert_eq!(app.diff_selection_text((7, 6), (7, 8)), "old");
        assert_eq!(app.yank_message.as_ref().map(|y| y.text.as_str()), Some("Copied 3 chars"));
    }

    #[test]
    fn leftward_drag_orders_cells_like_a_terminal() {
        let (mut app, _ends) = app_with_diff();
        use MouseEventKind as K;
        app.handle_mouse(mouse(K::Down(MouseButton::Left), 49, 9));
        app.handle_mouse(mouse(K::Drag(MouseButton::Left), 47, 9));
        assert_eq!(app.diff.selection.as_ref().unwrap().range(), ((7, 6), (7, 8)));
    }

    #[test]
    fn multi_line_selection_cuts_only_the_end_lines() {
        let (app, _ends) = app_with_diff();
        // From 'd' of "old line" (line 7, cell 8) to 'w' of "new line"
        // (line 8, cell 8): partial first and last line.
        assert_eq!(app.diff_selection_text((7, 8), (8, 8)), "d line\nnew");
    }

    #[test]
    fn click_maps_cells_through_horizontal_scroll() {
        let (mut app, _ends) = app_with_diff();
        app.diff.horizontal_scroll = 4;
        use MouseEventKind as K;
        app.handle_mouse(mouse(K::Down(MouseButton::Left), 43, 9));
        assert_eq!(app.diff.selection.as_ref().unwrap().anchor, (7, 6));
    }

    #[test]
    fn double_click_selects_word_and_copies_it() {
        let (mut app, _ends) = app_with_diff();
        use MouseEventKind as K;
        // "old line" (virtual 7, row 9): "old" spans cells 6..=8.
        // Click twice on the 'l' (cell 7 → column 48).
        app.handle_mouse(mouse(K::Down(MouseButton::Left), 48, 9));
        app.handle_mouse(mouse(K::Up(MouseButton::Left), 48, 9));
        assert!(app.diff.selection.is_none(), "first click is a plain click");
        app.handle_mouse(mouse(K::Down(MouseButton::Left), 48, 9));
        let sel = app.diff.selection.as_ref().expect("double-click selects the word");
        assert_eq!(sel.range(), ((7, 6), (7, 8)));
        app.handle_mouse(mouse(K::Up(MouseButton::Left), 48, 9));
        assert_eq!(app.yank_message.as_ref().map(|y| y.text.as_str()), Some("Copied 3 chars"));
    }

    #[test]
    fn triple_click_selects_the_whole_line() {
        let (mut app, _ends) = app_with_diff();
        use MouseEventKind as K;
        // Three clicks on the 'l' of "old line" (virtual 7, row 9):
        // the line spans cells 0..=13 (offset 6 + 8 content cells).
        for _ in 0..2 {
            app.handle_mouse(mouse(K::Down(MouseButton::Left), 48, 9));
            app.handle_mouse(mouse(K::Up(MouseButton::Left), 48, 9));
        }
        app.handle_mouse(mouse(K::Down(MouseButton::Left), 48, 9));
        let sel = app.diff.selection.as_ref().expect("triple-click selects the line");
        assert_eq!(sel.range(), ((7, 0), (7, 13)));
        app.handle_mouse(mouse(K::Up(MouseButton::Left), 48, 9));
        assert_eq!(app.diff_selection_text((7, 0), (7, 13)), "old line");
        assert_eq!(app.yank_message.as_ref().map(|y| y.text.as_str()), Some("Copied 8 chars"));
    }

    #[test]
    fn double_click_in_the_gutter_stays_a_plain_click() {
        let (mut app, _ends) = app_with_diff();
        use MouseEventKind as K;
        // Column 43 → cell 2, inside the line-number gutter.
        for _ in 0..2 {
            app.handle_mouse(mouse(K::Down(MouseButton::Left), 43, 9));
            app.handle_mouse(mouse(K::Up(MouseButton::Left), 43, 9));
        }
        assert!(app.diff.selection.is_none());
        assert!(app.yank_message.is_none());
    }

    #[test]
    fn word_cells_at_classifies_runs() {
        // "old line" rendered at offset 6: word, space, word.
        assert_eq!(word_cells_at("old line", 6, 7), Some((6, 8)));
        assert_eq!(word_cells_at("old line", 6, 9), Some((9, 9)));
        assert_eq!(word_cells_at("old line", 6, 10), Some((10, 13)));
        assert_eq!(word_cells_at("old line", 6, 5), None, "gutter cell");
        assert_eq!(word_cells_at("old line", 6, 14), None, "past end of line");
        // Joiners glue paths together; punctuation is its own run.
        assert_eq!(word_cells_at("see src/app/mouse.rs now", 0, 8), Some((4, 19)));
        assert_eq!(word_cells_at("a — b", 0, 2), Some((2, 2)));
        // A click on either half of a wide char selects its whole run.
        assert_eq!(word_cells_at("日本 語", 0, 3), Some((0, 3)));
    }

    #[test]
    fn slice_by_cells_handles_offset_wide_and_zero_width() {
        // Gutter offset: content starts at cell 6.
        assert_eq!(slice_by_cells("abc", 6, 0, Some(7)), "a");
        assert_eq!(slice_by_cells("abc", 6, 0, None), "abc");
        assert_eq!(slice_by_cells("abc", 0, 5, None), "");
        // A boundary through the middle of a wide char selects it whole.
        assert_eq!(slice_by_cells("日本語", 0, 1, Some(3)), "日本");
        assert_eq!(slice_by_cells("日本語", 0, 2, Some(4)), "本");
        // Combining marks travel with their base char.
        assert_eq!(slice_by_cells("e\u{301}x", 0, 0, Some(1)), "e\u{301}");
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
