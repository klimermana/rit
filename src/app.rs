use crate::{
    git::{GitMsg, GitReq, HistoryMsg, HistoryReq, InspectMsg, InspectReq},
    model::{CommitRecord, DiffDocument, DiffTarget, RepoInfo, StatusDocument},
    ui,
};
use anyhow::Result;
use compact_str::CompactString;
use crossbeam_channel::{Receiver, Sender, after, never, select};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::Backend};
use std::time::{Duration, Instant};

const YANK_FEEDBACK_DURATION: Duration = Duration::from_secs(2);
const HALF_PAGE: usize = 10;
const AUTHOR_COL_WIDTH: usize = 20;
const DATE_COL_WIDTH: usize = 8;

pub struct WorkingTreeRow {
    pub author: CompactString,
    /// `None` before the first dirty check completes; the renderer falls
    /// back to a neutral label in that case. `Some(true)` if anything
    /// (staged / unstaged / untracked) differs from HEAD.
    pub dirty: Option<bool>,
}

pub enum LogRow {
    WorkingTree(WorkingTreeRow),
    Commit(CommitRecord),
}

pub enum Focus {
    Log,
    Diff,
}

pub struct LogState {
    pub rows: Vec<LogRow>,
    pub selected: usize,
    pub scroll: usize,
    /// Inner height of the log pane, updated by the renderer each frame.
    pub view_height: usize,
}

/// Search state for the log pane. `matches` holds indices into
/// `LogState.rows`. The `last_query` / `last_generation` pair drives the
/// incremental-narrowing logic in `update_commit_matches` — when the user
/// extends a query within the same walk generation, we filter the prior
/// match set instead of rescanning every row.
pub struct CommitSearchState {
    pub active: bool,
    pub query: String,
    pub matches: Vec<usize>,
    pub current: usize,
    pub last_query: String,
    pub last_generation: u64,
}

/// Search state for the diff pane. `matches` holds virtual line indices
/// (header + diffstat + body) so the renderer can binary-search visible
/// indices directly.
pub struct DiffSearchState {
    pub active: bool,
    pub query: String,
    pub matches: Vec<usize>,
    pub current: usize,
}

/// Read-only view of either search's pageable bits, used by the status-bar
/// renderer so it doesn't depend on the concrete state type.
pub struct SearchSnapshot<'a> {
    pub active: bool,
    pub query: &'a str,
    pub matches_len: usize,
    pub display_index: usize,
}

impl Default for CommitSearchState {
    fn default() -> Self {
        Self::new()
    }
}

impl CommitSearchState {
    pub fn new() -> Self {
        Self {
            active: false,
            query: String::new(),
            matches: Vec::new(),
            current: 0,
            last_query: String::new(),
            last_generation: 0,
        }
    }

    pub fn clear(&mut self) {
        self.active = false;
        self.query.clear();
        self.matches.clear();
        self.current = 0;
        // Reset the narrowing cursor so the next type does a full rescan
        // instead of attempting to narrow a now-empty matches set.
        self.last_query.clear();
    }

    pub fn advance(&mut self, delta: isize) -> Option<usize> {
        cycle(&self.matches, &mut self.current, delta)
    }

    pub fn current_pos(&self) -> Option<usize> {
        self.matches.get(self.current).copied()
    }

    pub fn display_index(&self) -> usize {
        if self.matches.is_empty() { 0 } else { self.current + 1 }
    }

    pub fn snapshot(&self) -> SearchSnapshot<'_> {
        SearchSnapshot {
            active: self.active,
            query: &self.query,
            matches_len: self.matches.len(),
            display_index: self.display_index(),
        }
    }
}

impl Default for DiffSearchState {
    fn default() -> Self {
        Self::new()
    }
}

impl DiffSearchState {
    pub fn new() -> Self {
        Self { active: false, query: String::new(), matches: Vec::new(), current: 0 }
    }

    pub fn clear(&mut self) {
        self.active = false;
        self.query.clear();
        self.matches.clear();
        self.current = 0;
    }

    pub fn advance(&mut self, delta: isize) -> Option<usize> {
        cycle(&self.matches, &mut self.current, delta)
    }

    pub fn current_pos(&self) -> Option<usize> {
        self.matches.get(self.current).copied()
    }

    pub fn display_index(&self) -> usize {
        if self.matches.is_empty() { 0 } else { self.current + 1 }
    }

    pub fn snapshot(&self) -> SearchSnapshot<'_> {
        SearchSnapshot {
            active: self.active,
            query: &self.query,
            matches_len: self.matches.len(),
            display_index: self.display_index(),
        }
    }
}

/// Substring-match a commit against a pre-lowercased query. The summary
/// check tends to hit first in practice, so it goes first.
fn commit_matches(c: &CommitRecord, q: &str) -> bool {
    c.search.summary_lower.contains(q) || c.search.author_lower.contains(q)
}

/// Decide whether the current commit-search update can be served by
/// narrowing the previous match set instead of rescanning the whole
/// index. Two conditions: the walk hasn't been reloaded under us
/// (generations match), and the new query is a strict extension of the
/// previous non-empty one.
fn should_narrow(prev_query: &str, prev_generation: u64, query: &str, generation: u64) -> bool {
    generation == prev_generation && !prev_query.is_empty() && query.starts_with(prev_query)
}

/// Cyclically advance `*current` by `delta`. Returns the new match position
/// or `None` if `matches` is empty.
fn cycle(matches: &[usize], current: &mut usize, delta: isize) -> Option<usize> {
    let len_i = isize::try_from(matches.len()).ok().filter(|&n| n > 0)?;
    let cur_i = isize::try_from(*current).ok()?;
    // rem_euclid on a positive divisor yields a non-negative result, so
    // `unsigned_abs` is a lossless conversion back to usize.
    let next_i = (cur_i.saturating_add(delta)).rem_euclid(len_i);
    *current = next_i.unsigned_abs();
    matches.get(*current).copied()
}

pub struct DiffState {
    pub open: bool,
    pub target: Option<DiffTarget>,
    pub document: Option<DiffDocument>,
    pub scroll: usize,
    pub loading: bool,
    pub show_line_numbers: bool,
    pub show_hunks: bool,
    /// Inner height of the diff pane, updated by the renderer each frame.
    pub view_height: usize,
    /// In-diff search state (`/` while the diff pane is focused). Matches are
    /// virtual line indices into header + diffstat + body.
    pub search: DiffSearchState,
    /// Lazily-built lowercased mirrors of `document.header` / `document.body`,
    /// keyed by the same indices. Populated on first search so a 50k-line
    /// body isn't re-lowercased on every keystroke. Cleared whenever the
    /// underlying diff content changes.
    pub header_lower: Option<Vec<String>>,
    pub body_lower: Option<Vec<String>>,
}

pub struct StatusState {
    pub open: bool,
    pub document: Option<StatusDocument>,
    pub scroll: usize,
    pub loading: bool,
}

pub struct YankFeedback {
    pub text: String,
    pub shown_at: Instant,
}

pub struct App {
    pub log: LogState,
    pub search: CommitSearchState,
    pub diff: DiffState,
    pub status: StatusState,
    pub focus: Focus,
    pub show_help: bool,
    pub yank_message: Option<YankFeedback>,
    pub error: Option<String>,
    pub repo_name: String,
    pub branch_name: String,
    /// True once the worker has reported it walked the whole history.
    pub walk_done: bool,
    /// Current walk generation. Bumped on reload; the worker tags
    /// Commits/WalkDone with its own generation and stale messages are
    /// dropped.
    pub walk_gen: u64,
    pub should_quit: bool,
    pub tx: Sender<GitReq>,
    pub rx: Receiver<GitMsg>,
    pub input_rx: Receiver<Event>,
}

impl App {
    pub fn new(tx: Sender<GitReq>, rx: Receiver<GitMsg>, input_rx: Receiver<Event>) -> Self {
        let rows = vec![LogRow::WorkingTree(WorkingTreeRow { author: "you".into(), dirty: None })];
        Self {
            log: LogState { rows, selected: 0, scroll: 0, view_height: 1 },
            search: CommitSearchState::new(),
            diff: DiffState {
                open: false,
                target: None,
                document: None,
                scroll: 0,
                loading: false,
                show_line_numbers: true,
                show_hunks: true,
                view_height: 1,
                search: DiffSearchState::new(),
                header_lower: None,
                body_lower: None,
            },
            status: StatusState { open: false, document: None, scroll: 0, loading: false },
            focus: Focus::Log,
            show_help: false,
            yank_message: None,
            error: None,
            repo_name: "unknown".to_string(),
            branch_name: "HEAD".to_string(),
            walk_done: false,
            walk_gen: 0,
            should_quit: false,
            tx,
            rx,
            input_rx,
        }
    }

    fn send_history(&self, req: HistoryReq) {
        _ = self.tx.send(GitReq::History(req));
    }

    fn send_inspect(&self, req: InspectReq) {
        _ = self.tx.send(GitReq::Inspect(req));
    }

    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B::Error: Send + Sync + 'static,
    {
        // The worker self-paces — it streams batches automatically until
        // walk_done, so the app no longer kicks off a LoadMore.
        loop {
            self.poll_git_msgs();
            self.expire_yank();

            terminal.draw(|frame| {
                ui::draw(frame, self);
            })?;

            if self.should_quit {
                return Ok(());
            }

            // Clone the receivers so the borrows in `select!` don't conflict
            // with mutable use of `self` inside the arms.
            let input_rx = self.input_rx.clone();
            let msg_rx = self.rx.clone();
            let yank_wait = self.yank_wakeup();

            select! {
                recv(input_rx) -> evt => match evt {
                    Ok(Event::Key(key)) => self.handle_input(key),
                    // Resize / Mouse / etc. just wake the loop so we redraw.
                    Ok(_) => {}
                    Err(_) => self.should_quit = true,
                },
                recv(msg_rx) -> msg => match msg {
                    Ok(m) => self.apply_msg(m),
                    Err(_) => {
                        if self.error.is_none() {
                            self.error = Some("git worker disconnected".to_string());
                        }
                    }
                },
                recv(yank_wait) -> _ => {
                    // Yank feedback timer; expire_yank handles it next iter.
                }
            }
        }
    }

    fn yank_wakeup(&self) -> Receiver<Instant> {
        match &self.yank_message {
            Some(y) => {
                let elapsed = y.shown_at.elapsed();
                let remaining = YANK_FEEDBACK_DURATION.checked_sub(elapsed).unwrap_or_default();
                after(remaining)
            }
            None => never(),
        }
    }

    fn expire_yank(&mut self) {
        if let Some(y) = &self.yank_message
            && y.shown_at.elapsed() >= YANK_FEEDBACK_DURATION
        {
            self.yank_message = None;
        }
    }

    pub fn poll_git_msgs(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(msg) => self.apply_msg(msg),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    if self.error.is_none() {
                        self.error = Some("git worker disconnected".to_string());
                    }
                    break;
                }
            }
        }
    }

    fn apply_msg(&mut self, msg: GitMsg) {
        match msg {
            GitMsg::History(m) => self.apply_history_msg(m),
            GitMsg::Inspect(m) => self.apply_inspect_msg(m),
        }
    }

    fn apply_history_msg(&mut self, msg: HistoryMsg) {
        match msg {
            HistoryMsg::RepoInfo(RepoInfo { name, branch }) => {
                self.repo_name = name;
                self.branch_name = branch;
            }
            HistoryMsg::Commits { generation, commits } => {
                if generation != self.walk_gen {
                    return;
                }
                let before = self.log.rows.len();
                self.log.rows.extend(commits.into_iter().map(LogRow::Commit));
                // Incrementally widen the match set with just the new rows
                // rather than rescanning the whole index every batch.
                self.extend_commit_matches(before..self.log.rows.len());
            }
            HistoryMsg::WalkDone { generation } => {
                if generation == self.walk_gen {
                    self.walk_done = true;
                }
            }
            HistoryMsg::RefsLoaded { generation, refs_map, first_batch_rows } => {
                if generation == self.walk_gen {
                    // Backfill ref labels only on commits that arrived before
                    // refs were live. The working-tree row is at index 0
                    // (not a Commit), so the prefix runs `[1..=first_batch_rows]`.
                    // Subsequent batches were built with the refs already in
                    // hand and don't need touching.
                    let backfill_end = (first_batch_rows + 1).min(self.log.rows.len());
                    for row in self.log.rows.iter_mut().take(backfill_end) {
                        if let LogRow::Commit(c) = row
                            && let Some(labels) = refs_map.get(&c.id)
                        {
                            c.refs = labels.clone();
                        }
                    }
                }
            }
            HistoryMsg::Error(e) => {
                self.error = Some(e);
                self.walk_done = true;
            }
        }
    }

    fn apply_inspect_msg(&mut self, msg: InspectMsg) {
        match msg {
            InspectMsg::DiffLoaded(document) => {
                if self.diff.target == Some(document.target) {
                    self.diff.loading = false;
                    self.diff.document = Some(document);
                    self.diff.scroll = 0;
                    // New content invalidates the lowercased mirrors.
                    self.diff.header_lower = None;
                    self.diff.body_lower = None;
                    // Re-run diff search against new content.
                    self.update_diff_matches();
                }
            }
            InspectMsg::StatusLoaded(document) => {
                self.status.loading = false;
                self.status.document = Some(document);
            }
            InspectMsg::WorkingTreeMeta { author, dirty } => {
                if let Some(LogRow::WorkingTree(w)) = self.log.rows.first_mut() {
                    w.author = author.into();
                    // Only update the dirty bit when the worker actually
                    // produced one; preserve the previous indicator on
                    // status-query errors.
                    if dirty.is_some() {
                        w.dirty = dirty;
                    }
                }
            }
        }
    }

    pub fn handle_input(&mut self, key: KeyEvent) {
        if self.show_help {
            self.handle_help_key(key);
            return;
        }
        if self.diff.search.active {
            self.handle_diff_search_key(key);
            return;
        }
        if self.search.active {
            self.handle_commit_search_key(key);
            return;
        }
        if self.status.open {
            self.handle_status_key(key);
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
                self.search.active = false;
                self.jump_commit_match(1);
            }
            (Backspace, _) => {
                self.search.query.pop();
                self.update_commit_matches();
            }
            (Char(c), Mod::NONE) | (Char(c), Mod::SHIFT) => {
                self.search.query.push(c);
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
            (Char('G'), Mod::NONE) => {
                self.status.scroll = self.status.document.as_ref().map_or(0, |d| d.lines.len()).saturating_sub(1);
            }
            (Char('d'), Mod::CONTROL) => self.status.scroll = self.status.scroll.saturating_add(HALF_PAGE),
            (Char('u'), Mod::CONTROL) => self.status.scroll = self.status.scroll.saturating_sub(HALF_PAGE),
            _ => {}
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        use KeyModifiers as Mod;
        match (&self.focus, key.code, key.modifiers) {
            (_, Char('c'), Mod::CONTROL) => self.should_quit = true,
            (_, Char('?'), Mod::NONE) => self.show_help = true,
            (_, Char('#'), Mod::NONE) => self.diff.show_line_numbers = !self.diff.show_line_numbers,
            (_, Char('v'), Mod::NONE) if self.diff.open => {
                self.diff.show_hunks = !self.diff.show_hunks;
                self.diff.scroll = 0;
            }
            (_, Char('y'), Mod::NONE) => self.yank_selected_hash(),
            (Focus::Log, Char('/'), Mod::NONE) => {
                self.search.clear();
                self.search.active = true;
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
            (Focus::Log, Char('s'), Mod::NONE) => {
                self.status.open = true;
                self.status.scroll = 0;
                if self.status.document.is_none() && !self.status.loading {
                    self.status.loading = true;
                    self.send_inspect(InspectReq::LoadStatus);
                }
            }
            (_, Char('R'), Mod::NONE) => self.reload(),
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
            (_, Esc, Mod::NONE) if !self.search.query.is_empty() => {
                self.search.clear();
            }
            (Focus::Log, Char('q'), Mod::NONE) => self.should_quit = true,
            (Focus::Log, Char('j') | Down, Mod::NONE) => self.move_log_down(1),
            (Focus::Log, Char('k') | Up, Mod::NONE) => self.move_log_up(1),
            (Focus::Log, Char('g'), Mod::NONE) => self.jump_log_top(),
            (Focus::Log, Char('G'), Mod::NONE) => self.jump_log_bottom(),
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
            (Focus::Diff, Char('G'), Mod::NONE) => self.diff_scroll_to_bottom(),
            (Focus::Diff, Char('d'), Mod::CONTROL) => self.diff_scroll_down(HALF_PAGE),
            (Focus::Diff, Char('u'), Mod::CONTROL) => self.diff.scroll = self.diff.scroll.saturating_sub(HALF_PAGE),
            (Focus::Diff, PageDown, _) => self.diff_scroll_down(self.diff.view_height.max(1)),
            (Focus::Diff, PageUp, _) => {
                self.diff.scroll = self.diff.scroll.saturating_sub(self.diff.view_height.max(1));
            }
            _ => {}
        }
    }

    fn reload(&mut self) {
        let author = match self.log.rows.first() {
            Some(LogRow::WorkingTree(w)) => w.author.clone(),
            _ => "you".into(),
        };
        self.log.rows = vec![LogRow::WorkingTree(WorkingTreeRow { author, dirty: None })];
        self.log.selected = 0;
        self.log.scroll = 0;
        self.diff.document = None;
        self.diff.header_lower = None;
        self.diff.body_lower = None;
        self.diff.target = None;
        self.diff.loading = false;
        self.status.document = None;
        self.status.loading = false;
        self.search.clear();
        self.diff.search.clear();
        self.walk_done = false;
        self.walk_gen = self.walk_gen.wrapping_add(1);
        self.error = None;
        // Reload restarts the worker's continuous walk; no explicit
        // LoadMore needed, the worker streams batches on its own.
        self.send_history(HistoryReq::Reload);
        self.send_inspect(InspectReq::RefreshWorkingTreeMeta);
    }

    fn move_log_down(&mut self, n: usize) {
        if self.log.rows.is_empty() {
            return;
        }
        let new_sel = (self.log.selected + n).min(self.log.rows.len() - 1);
        if new_sel != self.log.selected {
            self.log.selected = new_sel;
            self.ensure_selected_visible();
            if self.diff.open {
                self.fetch_diff_for_selected();
            }
        }
    }

    fn move_log_up(&mut self, n: usize) {
        let new_sel = self.log.selected.saturating_sub(n);
        if new_sel != self.log.selected {
            self.log.selected = new_sel;
            self.ensure_selected_visible();
            if self.diff.open {
                self.fetch_diff_for_selected();
            }
        }
    }

    fn jump_log_top(&mut self) {
        self.log.selected = 0;
        self.log.scroll = 0;
        if self.diff.open {
            self.fetch_diff_for_selected();
        }
    }

    fn jump_log_bottom(&mut self) {
        if self.log.rows.is_empty() {
            return;
        }
        // G jumps to whatever is currently indexed. The worker is already
        // streaming in the background, so the "bottom" advances on its own
        // as more arrives.
        self.log.selected = self.log.rows.len() - 1;
        self.ensure_selected_visible();
        if self.diff.open {
            self.fetch_diff_for_selected();
        }
    }

    pub fn ensure_selected_visible(&mut self) {
        let h = self.log.view_height.max(1);
        if self.log.selected < self.log.scroll {
            self.log.scroll = self.log.selected;
        } else if self.log.selected >= self.log.scroll + h {
            self.log.scroll = self.log.selected.saturating_sub(h - 1);
        }
    }

    fn fetch_diff_for_selected(&mut self) {
        let Some(row) = self.log.rows.get(self.log.selected) else {
            return;
        };
        let target = match row {
            LogRow::Commit(c) => DiffTarget::Commit(c.id),
            LogRow::WorkingTree(_) => DiffTarget::WorkingTree,
        };
        if self.diff.target != Some(target) {
            self.diff.target = Some(target);
            self.diff.document = None;
            self.diff.header_lower = None;
            self.diff.body_lower = None;
            self.diff.loading = true;
            self.send_inspect(InspectReq::LoadDiff(target));
        }
    }

    fn update_commit_matches(&mut self) {
        let q = self.search.query.to_lowercase();
        if q.is_empty() {
            self.search.matches.clear();
            self.search.current = 0;
            self.search.last_query.clear();
            return;
        }

        // Incremental narrowing: if the new query is a strict extension of
        // the previous one *and* the index hasn't been reloaded under us,
        // we can filter the prior match set instead of rescanning every
        // row. On a 100k-commit index that takes typing latency from
        // O(commits) to O(matches).
        if should_narrow(&self.search.last_query, self.search.last_generation, &q, self.walk_gen) {
            let rows = &self.log.rows;
            self.search.matches.retain(|&i| match rows.get(i) {
                Some(LogRow::Commit(c)) => commit_matches(c, &q),
                _ => false,
            });
        } else {
            self.search.matches = self
                .log
                .rows
                .iter()
                .enumerate()
                .filter_map(|(i, row)| match row {
                    LogRow::Commit(c) if commit_matches(c, &q) => Some(i),
                    _ => None,
                })
                .collect();
        }

        self.search.current = 0;
        self.search.last_query = q;
        self.search.last_generation = self.walk_gen;
    }

    /// Scan only rows in `new_range`, appending any that match the current
    /// query. Called when the worker streams in a new batch so existing
    /// matches don't get re-tested.
    fn extend_commit_matches(&mut self, new_range: std::ops::Range<usize>) {
        if self.search.query.is_empty() {
            return;
        }
        let q = self.search.query.to_lowercase();
        let rows = &self.log.rows;
        for i in new_range {
            if let Some(LogRow::Commit(c)) = rows.get(i)
                && commit_matches(c, &q)
            {
                self.search.matches.push(i);
            }
        }
    }

    fn update_diff_matches(&mut self) {
        let q = self.diff.search.query.to_lowercase();
        if q.is_empty() {
            self.diff.search.matches.clear();
            self.diff.search.current = 0;
            return;
        }
        // Build the lowercase mirrors on first search so we don't re-lowercase
        // the entire diff body on every keystroke.
        self.diff.ensure_lower_cache();

        let header_len = self.diff.document.as_ref().map(|d| d.header.len()).unwrap_or(0);
        let body_offset = header_len + self.diff.diffstat_line_count();
        let mut matches = Vec::new();

        if let Some(header) = &self.diff.header_lower {
            for (i, text) in header.iter().enumerate() {
                if text.contains(&q) {
                    matches.push(i);
                }
            }
        }
        if let Some(body) = &self.diff.body_lower {
            for (i, text) in body.iter().enumerate() {
                if text.contains(&q) {
                    matches.push(body_offset + i);
                }
            }
        }
        self.diff.search.matches = matches;
        self.diff.search.current = 0;
    }

    fn jump_commit_match(&mut self, delta: isize) {
        if let Some(pos) = self.search.advance(delta) {
            self.apply_commit_match_position(pos);
        }
    }

    fn jump_diff_match(&mut self, delta: isize) {
        if let Some(pos) = self.diff.search.advance(delta) {
            self.apply_diff_match_position(pos);
        }
    }

    fn commit_jump_first_at_or_after_cursor(&mut self) {
        let cursor = self.log.selected;
        let idx = self.search.matches.iter().position(|&i| i >= cursor).unwrap_or(0);
        let Some(&pos) = self.search.matches.get(idx) else { return };
        self.search.current = idx;
        self.apply_commit_match_position(pos);
    }

    fn diff_jump_first_at_or_after_cursor(&mut self) {
        let cursor = self.diff.scroll;
        let idx = self.diff.search.matches.iter().position(|&i| i >= cursor).unwrap_or(0);
        let Some(&pos) = self.diff.search.matches.get(idx) else { return };
        self.diff.search.current = idx;
        self.apply_diff_match_position(pos);
    }

    fn apply_commit_match_position(&mut self, pos: usize) {
        self.log.selected = pos;
        self.ensure_selected_visible();
        if self.diff.open {
            self.fetch_diff_for_selected();
        }
    }

    fn apply_diff_match_position(&mut self, pos: usize) {
        // Keep at least 5 lines of context above the matched line.
        let max = self.diff.total_visible_lines().saturating_sub(self.diff.view_height);
        self.diff.scroll = pos.saturating_sub(5).min(max);
    }

    /// Tab between panes. Searches "follow focus": only one pane has an
    /// active query at a time, and the query migrates with focus so the
    /// user keeps searching the same thing across panes.
    fn cycle_focus(&mut self) {
        match self.focus {
            Focus::Log => {
                self.focus = Focus::Diff;
                self.migrate_search_to_diff();
            }
            Focus::Diff => {
                self.focus = Focus::Log;
                self.migrate_search_to_log();
            }
        }
    }

    /// Move an active commit-search query onto the diff pane and run it
    /// against the current diff content. No-op when the commit search is
    /// empty (so callers can fire this unconditionally on focus change).
    fn migrate_search_to_diff(&mut self) {
        if self.search.query.is_empty() {
            return;
        }
        let q = std::mem::take(&mut self.search.query);
        // Drain leftover commit-search state so n/N on a future return to
        // log doesn't try to narrow against a now-empty match set.
        self.search.active = false;
        self.search.matches.clear();
        self.search.current = 0;
        self.search.last_query.clear();
        self.search.last_generation = 0;

        self.diff.search.query = q;
        self.update_diff_matches();
        self.diff_jump_first_at_or_after_cursor();
    }

    /// Symmetric: move an active diff-search query back onto the log pane.
    fn migrate_search_to_log(&mut self) {
        if self.diff.search.query.is_empty() {
            return;
        }
        let q = std::mem::take(&mut self.diff.search.query);
        self.diff.search.active = false;
        self.diff.search.matches.clear();
        self.diff.search.current = 0;

        self.search.query = q;
        self.update_commit_matches();
        self.commit_jump_first_at_or_after_cursor();
    }

    fn yank_selected_hash(&mut self) {
        let Some(LogRow::Commit(commit)) = self.log.rows.get(self.log.selected) else {
            return;
        };
        let hash = commit.id.to_string();
        yank_to_clipboard(&hash);
        let preview = hash.get(..12).unwrap_or(hash.as_str());
        self.yank_message = Some(YankFeedback { text: format!("Copied: {}", preview), shown_at: Instant::now() });
    }

    pub fn author_col_width(&self) -> usize {
        AUTHOR_COL_WIDTH
    }
    pub fn date_col_width(&self) -> usize {
        DATE_COL_WIDTH
    }

    /// Number of real commits in the log (excludes the pseudo "Not Committed
    /// Yet" row at index 0). The working-tree row is created in `App::new`
    /// and replaced 1-for-1 in `reload`, never removed — so a direct
    /// `rows.len() - 1` is exact and O(1), important because this runs on
    /// every redraw via the status-bar renderer.
    pub fn commits_len(&self) -> usize {
        self.log.rows.len().saturating_sub(1)
    }

    fn diff_scroll_down(&mut self, n: usize) {
        let max = self.diff.total_visible_lines().saturating_sub(self.diff.view_height);
        self.diff.scroll = (self.diff.scroll.saturating_add(n)).min(max);
    }

    fn diff_scroll_to_bottom(&mut self) {
        let total = self.diff.total_visible_lines();
        self.diff.scroll = total.saturating_sub(self.diff.view_height);
    }
}

impl DiffState {
    /// Total number of rendered lines: header + synthesised diffstat block +
    /// (when `show_hunks`) body.
    pub fn total_visible_lines(&self) -> usize {
        let header = self.document.as_ref().map(|d| d.header.len()).unwrap_or(0);
        let diffstat = self.diffstat_line_count();
        let body = if self.show_hunks { self.document.as_ref().map(|d| d.body.len()).unwrap_or(0) } else { 0 };
        header + diffstat + body
    }

    pub fn diffstat_line_count(&self) -> usize {
        match self.document.as_ref().map(|d| &d.files) {
            Some(files) if !files.is_empty() => {
                // separator line `---`, one row per file, blank, totals line
                files.len() + 3
            }
            _ => 0,
        }
    }

    /// Populate `header_lower` / `body_lower` from the current diff content.
    /// No-op once populated; cleared by `apply_msg` whenever new diff content
    /// arrives.
    pub fn ensure_lower_cache(&mut self) {
        let Some(doc) = self.document.as_ref() else { return };
        if self.header_lower.is_none() {
            self.header_lower = Some(doc.header.iter().map(|l| l.text.to_lowercase()).collect());
        }
        if self.body_lower.is_none() {
            self.body_lower = Some(doc.body.iter().map(|l| l.text.to_lowercase()).collect());
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn yank_to_clipboard(text: &str) {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    #[cfg(target_os = "macos")]
    {
        if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
            if let Some(stdin) = child.stdin.as_mut() {
                _ = stdin.write_all(text.as_bytes());
            }
            _ = child.wait();
        }
    }
    #[cfg(target_os = "linux")]
    {
        let mut done = false;
        if let Ok(mut child) = Command::new("xclip").args(["-selection", "clipboard"]).stdin(Stdio::piped()).spawn() {
            if let Some(stdin) = child.stdin.as_mut() {
                _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().is_ok() {
                done = true;
            }
        }
        if !done {
            if let Ok(mut child) = Command::new("xsel").args(["--clipboard", "--input"]).stdin(Stdio::piped()).spawn() {
                if let Some(stdin) = child.stdin.as_mut() {
                    _ = stdin.write_all(text.as_bytes());
                }
                _ = child.wait();
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn yank_to_clipboard(_text: &str) {}

#[cfg(test)]
mod tests {
    use super::should_narrow;

    #[test]
    fn narrow_when_query_extends_within_same_generation() {
        assert!(should_narrow("foo", 1, "foob", 1));
        assert!(should_narrow("a", 5, "abc", 5));
    }

    #[test]
    fn rescan_when_query_shrinks() {
        assert!(!should_narrow("foobar", 1, "foo", 1));
        assert!(!should_narrow("ab", 1, "a", 1));
    }

    #[test]
    fn rescan_when_query_changes_completely() {
        assert!(!should_narrow("foo", 1, "bar", 1));
    }

    #[test]
    fn rescan_after_reload_changes_generation() {
        // Even though the new query strictly extends the old, a reload
        // means the row indices the previous match set referenced may not
        // line up anymore.
        assert!(!should_narrow("foo", 1, "foobar", 2));
    }

    #[test]
    fn rescan_when_no_previous_query() {
        // An empty previous query means there's no prior match set to
        // narrow; the new query must scan the whole index.
        assert!(!should_narrow("", 1, "f", 1));
    }
}
