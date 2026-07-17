//! Keyboard input dispatch. `handle_input` is the entry from `App::run`'s
//! select! arm; it routes by mode (help / search / status / main) and
//! the per-mode handlers fan out to the rest of `App`.

use crate::{
    app::{App, state::Focus},
    git::InspectReq,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Half-page increment for Ctrl+D/U. Matches tig's default.
const HALF_PAGE: usize = 10;

/// Column step for horizontal scroll in the diff pane. One "tab" worth so
/// repeated taps cover ground quickly; precision past that is rarely
/// needed in a diff viewer.
const HORIZONTAL_STEP: usize = 4;

impl App {
    pub fn handle_input(&mut self, key: KeyEvent) {
        // Ctrl+C quits from every mode — the per-mode handlers only
        // match unmodified/shifted chars, so without this it would fall
        // through to a no-op in search, status, and help modes.
        if matches!((key.code, key.modifiers), (KeyCode::Char('c'), KeyModifiers::CONTROL)) {
            self.should_quit = true;
            return;
        }
        if self.show_help {
            self.handle_help_key(key);
            return;
        }
        if self.diff.search.active {
            self.handle_diff_search_key(key);
            return;
        }
        if self.search.state.active {
            self.handle_commit_search_key(key);
            return;
        }
        if self.status.open {
            self.handle_status_key(key);
            return;
        }
        if self.refs.open {
            self.handle_refs_key(key);
            return;
        }
        if self.blame.open {
            self.handle_blame_key(key);
            return;
        }
        self.handle_main_key(key);
    }

    fn handle_help_key(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Esc) {
            self.show_help = false;
        }
    }

    fn handle_commit_search_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        use KeyModifiers as Mod;
        match (key.code, key.modifiers) {
            (Esc, _) => self.search.clear(),
            (Enter, _) => {
                self.search.state.active = false;
                self.jump_commit_match(1);
            }
            (Backspace, _) => {
                self.search.state.query.pop();
                self.update_commit_matches();
            }
            (Char(c), Mod::NONE) | (Char(c), Mod::SHIFT) => {
                self.search.state.query.push(c);
                self.update_commit_matches();
                self.commit_jump_first_at_or_after_cursor();
            }
            _ => {}
        }
    }

    fn handle_diff_search_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        use KeyModifiers as Mod;
        match (key.code, key.modifiers) {
            (Esc, _) => self.diff.search.clear(),
            (Enter, _) => {
                self.diff.search.active = false;
                self.jump_diff_match(1);
            }
            (Backspace, _) => {
                self.diff.search.query.pop();
                self.update_diff_matches();
            }
            (Char(c), Mod::NONE) | (Char(c), Mod::SHIFT) => {
                self.diff.search.query.push(c);
                self.update_diff_matches();
                self.diff_jump_first_at_or_after_cursor();
            }
            _ => {}
        }
    }

    fn handle_status_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        use KeyModifiers as Mod;
        match (key.code, key.modifiers) {
            (Char('q') | Esc | Char('s'), Mod::NONE) => self.status.open = false,
            (Char('j') | Down, Mod::NONE) => self.status.scroll = self.status.scroll.saturating_add(1),
            (Char('k') | Up, Mod::NONE) => self.status.scroll = self.status.scroll.saturating_sub(1),
            (Char('g'), Mod::NONE) => self.status.scroll = 0,
            (Char('G'), Mod::NONE | Mod::SHIFT) => {
                self.status.scroll = self.status.document.as_ref().map_or(0, |d| d.lines.len()).saturating_sub(1);
            }
            (Char('d'), Mod::CONTROL) => self.status.scroll = self.status.scroll.saturating_add(HALF_PAGE),
            (Char('u'), Mod::CONTROL) => self.status.scroll = self.status.scroll.saturating_sub(HALF_PAGE),
            _ => {}
        }
    }

    fn handle_refs_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        use KeyModifiers as Mod;
        match (key.code, key.modifiers) {
            (Char('q') | Esc | Char('r'), Mod::NONE) => self.refs.open = false,
            (Enter, Mod::NONE) => self.refs_activate_selected(),
            (Char('j') | Down, Mod::NONE) => self.refs_move(1),
            (Char('k') | Up, Mod::NONE) => self.refs_move(-1),
            (Char('g'), Mod::NONE) => {
                self.refs.selected = 0;
                self.refs.scroll = 0;
            }
            (Char('G'), Mod::NONE | Mod::SHIFT) => self.refs_move(isize::MAX),
            (Char('d'), Mod::CONTROL) => self.refs_move(HALF_PAGE as isize),
            (Char('u'), Mod::CONTROL) => self.refs_move(-(HALF_PAGE as isize)),
            (PageDown, _) => self.refs_move(self.refs.view_height.max(1) as isize),
            (PageUp, _) => self.refs_move(-(self.refs.view_height.max(1) as isize)),
            _ => {}
        }
    }

    fn handle_blame_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        use KeyModifiers as Mod;
        match (key.code, key.modifiers) {
            (Char('q') | Esc, Mod::NONE) => self.blame.open = false,
            (Enter, Mod::NONE) => self.blame_open_commit(),
            (Char(','), Mod::NONE) => self.blame_reblame_at_parent(),
            (Backspace, Mod::NONE) => self.blame_back(),
            (Char('j') | Down, Mod::NONE) => self.blame_move(1),
            (Char('k') | Up, Mod::NONE) => self.blame_move(-1),
            (Char('g'), Mod::NONE) => {
                self.blame.selected = 0;
                self.blame.scroll = 0;
            }
            (Char('G'), Mod::NONE | Mod::SHIFT) => self.blame_move(isize::MAX),
            (Char('d'), Mod::CONTROL) => self.blame_move(HALF_PAGE as isize),
            (Char('u'), Mod::CONTROL) => self.blame_move(-(HALF_PAGE as isize)),
            (PageDown, _) => self.blame_move(self.blame.view_height.max(1) as isize),
            (PageUp, _) => self.blame_move(-(self.blame.view_height.max(1) as isize)),
            (Char('y'), Mod::NONE) => self.yank_blame_hash(),
            _ => {}
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        use KeyModifiers as Mod;
        match (&self.focus, key.code, key.modifiers) {
            (_, Char('?'), Mod::NONE) => self.show_help = true,
            (_, Char('#'), Mod::NONE) => self.diff.show_line_numbers = !self.diff.show_line_numbers,
            (_, Char('v'), Mod::NONE) if self.diff.open => {
                self.diff.show_hunks = !self.diff.show_hunks;
                self.diff.scroll = 0;
            }
            (_, Char('f'), Mod::NONE) if self.diff.open => self.toggle_diff_scope(),
            (_, Char('y'), Mod::NONE) => self.yank_selected_hash(),
            (Focus::Log, Char('/'), Mod::NONE) => {
                self.search.clear();
                self.search.state.active = true;
            }
            (Focus::Diff, Char('/'), Mod::NONE) => {
                self.diff.search.clear();
                self.diff.search.active = true;
            }
            (Focus::Log, Char('n'), Mod::NONE) => self.jump_commit_match(1),
            // `N` is typically reported with `Mod::SHIFT` since shift produced
            // the uppercase letter; accept both to be safe across terminals.
            (Focus::Log, Char('N'), Mod::NONE | Mod::SHIFT) => self.jump_commit_match(-1),
            (Focus::Diff, Char('n'), Mod::NONE) => self.jump_diff_match(1),
            (Focus::Diff, Char('N'), Mod::NONE | Mod::SHIFT) => self.jump_diff_match(-1),
            (Focus::Log, Char('r'), Mod::NONE) => self.open_refs_view(),
            (Focus::Log, Char('s'), Mod::NONE) => {
                self.status.open = true;
                self.status.scroll = 0;
                if self.status.document.is_none() && !self.status.loading {
                    self.status.loading = true;
                    self.send_inspect(InspectReq::LoadStatus);
                }
            }
            (_, Char('R'), Mod::NONE) => self.reload(),
            // Display toggles (tig-style). Uppercase letters typically
            // arrive with Mod::SHIFT; accept both, like `G`/`N` above.
            (_, Char('D'), Mod::NONE | Mod::SHIFT) => self.display.date = self.display.date.next(),
            (_, Char('A'), Mod::NONE | Mod::SHIFT) => self.display.author = self.display.author.next(),
            (_, Char('X'), Mod::NONE | Mod::SHIFT) => self.display.full_hash = !self.display.full_hash,
            (_, Tab, Mod::NONE) if self.diff.open => self.cycle_focus(),
            (Focus::Log, Enter, Mod::NONE) => {
                self.diff.open = true;
                self.focus = Focus::Diff;
                self.diff.scroll = 0;
                // Carry an active commit search over so the query keeps
                // working in the diff pane. update_diff_matches will run
                // again once the diff payload arrives.
                self.migrate_search_to_diff();
                self.fetch_diff_for_selected();
            }
            // q/Esc pops the diff pane if open; otherwise quits from log.
            (_, Char('q') | Esc, Mod::NONE) if self.diff.open => {
                self.diff.open = false;
                self.focus = Focus::Log;
                // Carry an active diff search back to the log so the query
                // keeps working there. No-op if diff search is empty.
                self.migrate_search_to_log();
            }
            // Esc clears an active search result set (when diff is not open).
            (_, Esc, Mod::NONE) if !self.search.state.query.is_empty() => {
                self.search.clear();
            }
            (Focus::Log, Char('q'), Mod::NONE) => self.should_quit = true,
            (Focus::Log, Char('j') | Down, Mod::NONE) => self.move_log_down(1),
            (Focus::Log, Char('k') | Up, Mod::NONE) => self.move_log_up(1),
            (Focus::Log, Char('g'), Mod::NONE) => self.jump_log_top(),
            (Focus::Log, Char('G'), Mod::NONE | Mod::SHIFT) => self.jump_log_bottom(),
            (Focus::Log, Char('d'), Mod::CONTROL) => self.move_log_down(HALF_PAGE),
            (Focus::Log, Char('u'), Mod::CONTROL) => self.move_log_up(HALF_PAGE),
            // Full-page nav. Modifiers ignored — PageUp/PageDown is dedicated.
            (Focus::Log, PageDown, _) => self.move_log_down(self.log.view_height.max(1)),
            (Focus::Log, PageUp, _) => self.move_log_up(self.log.view_height.max(1)),
            (Focus::Diff, Char('j') | Down | Enter, Mod::NONE) => self.diff_scroll_down(1),
            (Focus::Diff, Char('k') | Up | Backspace, Mod::NONE) => {
                self.diff.scroll = self.diff.scroll.saturating_sub(1);
            }
            (Focus::Diff, Char('g'), Mod::NONE) => self.diff.scroll = 0,
            (Focus::Diff, Char('G'), Mod::NONE | Mod::SHIFT) => self.diff_scroll_to_bottom(),
            (Focus::Diff, Char('d'), Mod::CONTROL) => self.diff_scroll_down(HALF_PAGE),
            (Focus::Diff, Char('u'), Mod::CONTROL) => self.diff.scroll = self.diff.scroll.saturating_sub(HALF_PAGE),
            (Focus::Diff, PageDown, _) => self.diff_scroll_down(self.diff.view_height.max(1)),
            (Focus::Diff, PageUp, _) => {
                self.diff.scroll = self.diff.scroll.saturating_sub(self.diff.view_height.max(1));
            }
            (Focus::Diff, Char('h') | Left, Mod::NONE) => {
                self.diff.horizontal_scroll = self.diff.horizontal_scroll.saturating_sub(HORIZONTAL_STEP);
            }
            (Focus::Diff, Char('l') | Right, Mod::NONE) => {
                self.diff.horizontal_scroll = self.diff.horizontal_scroll.saturating_add(HORIZONTAL_STEP);
            }
            (Focus::Diff, Char('0'), Mod::NONE) => self.diff.horizontal_scroll = 0,
            (Focus::Diff, Char(']'), Mod::NONE) => self.diff_jump_file(1),
            (Focus::Diff, Char('['), Mod::NONE) => self.diff_jump_file(-1),
            (Focus::Diff, Char('b'), Mod::NONE) => self.blame_from_diff_pane(),
            _ => {}
        }
    }
}
