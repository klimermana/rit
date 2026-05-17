use crate::{
    git::{DiffLine, DiffStats, DiffTarget, FileStat, GitMsg, GitReq, RepoInfo},
    ui,
};
use anyhow::Result;
use compact_str::CompactString;
use crossbeam_channel::{after, never, select, Receiver, Sender};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::Backend, Terminal};
use std::time::{Duration, Instant};

const YANK_FEEDBACK_DURATION: Duration = Duration::from_secs(2);
const LOAD_PAGE: usize = 256;
const INITIAL_LOAD: usize = 64;
const PREFETCH_THRESHOLD: usize = 32;
const HALF_PAGE: usize = 10;
const AUTHOR_COL_WIDTH: usize = 20;
const DATE_COL_WIDTH: usize = 8;

pub struct CommitInfo {
    pub id: gix::ObjectId,
    pub short_id: CompactString,
    pub author: CompactString,
    pub author_lower: CompactString,
    pub date: CompactString,
    pub summary: String,
    pub summary_lower: String,
    pub refs: Vec<RefLabel>,
    pub graph: CompactString,
}

pub struct WorkingTreeRow {
    pub author: CompactString,
}

pub enum LogRow {
    WorkingTree(WorkingTreeRow),
    Commit(CommitInfo),
}

#[derive(Clone)]
pub struct RefLabel {
    pub name: CompactString,
    pub kind: RefKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Head,
    LocalBranch,
    RemoteBranch,
    Tag,
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

/// Shared search state used by both the log view and the in-diff search.
/// `matches` holds positions in whatever index space the view uses (log row
/// indices, or diff virtual line indices). `current` is an index into
/// `matches` and is 0 when there are no matches.
pub struct SearchState {
    pub active: bool,
    pub query: String,
    pub matches: Vec<usize>,
    pub current: usize,
}

#[derive(Clone, Copy)]
pub enum SearchKind {
    Log,
    Diff,
}

impl SearchState {
    pub fn new() -> Self {
        Self { active: false, query: String::new(), matches: Vec::new(), current: 0 }
    }

    pub fn clear(&mut self) {
        self.active = false;
        self.query.clear();
        self.matches.clear();
        self.current = 0;
    }

    /// Cyclically advance `current` by `delta`. Returns the new match
    /// position, or `None` if there are no matches.
    pub fn advance(&mut self, delta: isize) -> Option<usize> {
        let len = self.matches.len();
        if len == 0 {
            return None;
        }
        let len_i = len as isize;
        let cur = self.current as isize;
        let next = ((cur + delta) % len_i + len_i) % len_i;
        self.current = next as usize;
        self.matches.get(self.current).copied()
    }

    pub fn current_pos(&self) -> Option<usize> {
        self.matches.get(self.current).copied()
    }

    /// 1-based index for the status bar; 0 when there are no matches.
    pub fn display_index(&self) -> usize {
        if self.matches.is_empty() {
            0
        } else {
            self.current + 1
        }
    }
}

pub struct DiffState {
    pub open: bool,
    pub target: Option<DiffTarget>,
    pub header_lines: Option<Vec<DiffLine>>,
    pub body_lines: Option<Vec<DiffLine>>,
    pub files: Option<Vec<FileStat>>,
    pub stats: Option<DiffStats>,
    pub scroll: usize,
    pub loading: bool,
    pub show_line_numbers: bool,
    pub show_hunks: bool,
    /// Inner height of the diff pane, updated by the renderer each frame.
    pub view_height: usize,
    /// In-diff search state (`/` while the diff pane is focused). Matches are
    /// virtual line indices into header + diffstat + body.
    pub search: SearchState,
}

pub struct StatusState {
    pub open: bool,
    pub lines: Vec<DiffLine>,
    pub scroll: usize,
    pub loading: bool,
}

pub struct YankFeedback {
    pub text: String,
    pub shown_at: Instant,
}

pub struct App {
    pub log: LogState,
    pub search: SearchState,
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
        let rows = vec![LogRow::WorkingTree(WorkingTreeRow { author: "you".into() })];
        Self {
            log: LogState { rows, selected: 0, scroll: 0, view_height: 1 },
            search: SearchState::new(),
            diff: DiffState {
                open: false,
                target: None,
                header_lines: None,
                body_lines: None,
                files: None,
                stats: None,
                scroll: 0,
                loading: false,
                show_line_numbers: true,
                show_hunks: true,
                view_height: 1,
                search: SearchState::new(),
            },
            status: StatusState { open: false, lines: Vec::new(), scroll: 0, loading: false },
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

    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B::Error: Send + Sync + 'static,
    {
        // Kick off the initial walk.
        let _ = self.tx.send(GitReq::LoadMore(INITIAL_LOAD));

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
        if let Some(y) = &self.yank_message {
            if y.shown_at.elapsed() >= YANK_FEEDBACK_DURATION {
                self.yank_message = None;
            }
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
            GitMsg::RepoInfo(RepoInfo { name, branch }) => {
                self.repo_name = name;
                self.branch_name = branch;
            }
            GitMsg::Commits { gen, commits } => {
                if gen != self.walk_gen {
                    return;
                }
                self.log.rows.extend(commits.into_iter().map(LogRow::Commit));
                if !self.search.query.is_empty() {
                    self.update_matches(SearchKind::Log);
                }
            }
            GitMsg::Diff { target, header_lines, body_lines, stats, files } => {
                if self.diff.target == Some(target) {
                    self.diff.loading = false;
                    self.diff.header_lines = Some(header_lines);
                    self.diff.body_lines = Some(body_lines);
                    self.diff.files = Some(files);
                    self.diff.stats = Some(stats);
                    self.diff.scroll = 0;
                    // Re-run diff search against new content.
                    self.update_matches(SearchKind::Diff);
                }
            }
            GitMsg::Status(lines) => {
                self.status.loading = false;
                self.status.lines = lines;
            }
            GitMsg::WorkingTreeMeta { author } => {
                if let Some(LogRow::WorkingTree(w)) = self.log.rows.first_mut() {
                    w.author = author.into();
                }
            }
            GitMsg::WalkDone { gen } => {
                if gen == self.walk_gen {
                    self.walk_done = true;
                }
            }
            GitMsg::Error(e) => {
                self.error = Some(e);
                self.walk_done = true;
            }
        }
    }

    pub fn handle_input(&mut self, key: KeyEvent) {
        if self.show_help {
            self.handle_help_key(key);
            return;
        }
        if self.diff.search.active {
            self.handle_search_key(key, SearchKind::Diff);
            return;
        }
        if self.search.active {
            self.handle_search_key(key, SearchKind::Log);
            return;
        }
        if self.status.open {
            self.handle_status_key(key);
            return;
        }
        self.handle_main_key(key);
    }

    fn search_state_mut(&mut self, kind: SearchKind) -> &mut SearchState {
        match kind {
            SearchKind::Log => &mut self.search,
            SearchKind::Diff => &mut self.diff.search,
        }
    }

    fn handle_help_key(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Esc) {
            self.show_help = false;
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent, kind: SearchKind) {
        use KeyCode::*;
        use KeyModifiers as Mod;
        match (key.code, key.modifiers) {
            (Esc, _) => self.search_state_mut(kind).clear(),
            (Enter, _) => {
                self.search_state_mut(kind).active = false;
                self.jump_match(kind, 1);
            }
            (Backspace, _) => {
                self.search_state_mut(kind).query.pop();
                self.update_matches(kind);
            }
            (Char(c), Mod::NONE) | (Char(c), Mod::SHIFT) => {
                self.search_state_mut(kind).query.push(c);
                self.update_matches(kind);
                self.jump_first_at_or_after_cursor(kind);
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
            (Char('G'), Mod::NONE) => self.status.scroll = self.status.lines.len().saturating_sub(1),
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
            (Focus::Log, Char('n'), Mod::NONE) => self.jump_match(SearchKind::Log, 1),
            // `N` is typically reported with `Mod::SHIFT` since shift produced
            // the uppercase letter; accept both to be safe across terminals.
            (Focus::Log, Char('N'), Mod::NONE | Mod::SHIFT) => self.jump_match(SearchKind::Log, -1),
            (Focus::Diff, Char('n'), Mod::NONE) => self.jump_match(SearchKind::Diff, 1),
            (Focus::Diff, Char('N'), Mod::NONE | Mod::SHIFT) => self.jump_match(SearchKind::Diff, -1),
            (Focus::Log, Char('s'), Mod::NONE) => {
                self.status.open = true;
                self.status.scroll = 0;
                if self.status.lines.is_empty() && !self.status.loading {
                    self.status.loading = true;
                    let _ = self.tx.send(GitReq::FetchStatus);
                }
            }
            (_, Char('R'), Mod::NONE) => self.reload(),
            (_, Tab, Mod::NONE) if self.diff.open => {
                self.focus = match self.focus {
                    Focus::Log => Focus::Diff,
                    Focus::Diff => Focus::Log,
                };
            }
            (Focus::Log, Enter, Mod::NONE) => {
                self.diff.open = true;
                self.focus = Focus::Diff;
                self.diff.scroll = 0;
                self.fetch_diff_for_selected();
            }
            // q/Esc pops the diff pane if open; otherwise quits from log.
            (_, Char('q') | Esc, Mod::NONE) if self.diff.open => {
                self.diff.open = false;
                self.focus = Focus::Log;
                self.diff.search.clear();
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
            (Focus::Diff, Char('j') | Down | Enter, Mod::NONE) => self.diff_scroll_down(1),
            (Focus::Diff, Char('k') | Up | Backspace, Mod::NONE) => {
                self.diff.scroll = self.diff.scroll.saturating_sub(1);
            }
            (Focus::Diff, Char('g'), Mod::NONE) => self.diff.scroll = 0,
            (Focus::Diff, Char('G'), Mod::NONE) => self.diff_scroll_to_bottom(),
            (Focus::Diff, Char('d'), Mod::CONTROL) => self.diff_scroll_down(HALF_PAGE),
            (Focus::Diff, Char('u'), Mod::CONTROL) => self.diff.scroll = self.diff.scroll.saturating_sub(HALF_PAGE),
            _ => {}
        }
    }

    fn reload(&mut self) {
        let author = match self.log.rows.first() {
            Some(LogRow::WorkingTree(w)) => w.author.clone(),
            _ => "you".into(),
        };
        self.log.rows = vec![LogRow::WorkingTree(WorkingTreeRow { author })];
        self.log.selected = 0;
        self.log.scroll = 0;
        self.diff.header_lines = None;
        self.diff.body_lines = None;
        self.diff.files = None;
        self.diff.target = None;
        self.diff.stats = None;
        self.diff.loading = false;
        self.status.lines.clear();
        self.status.loading = false;
        self.search.clear();
        self.diff.search.clear();
        self.walk_done = false;
        self.walk_gen = self.walk_gen.wrapping_add(1);
        self.error = None;
        let _ = self.tx.send(GitReq::Reload);
        let _ = self.tx.send(GitReq::CheckWorkingTree);
        let _ = self.tx.send(GitReq::LoadMore(INITIAL_LOAD));
    }

    fn move_log_down(&mut self, n: usize) {
        if self.log.rows.is_empty() {
            return;
        }
        let new_sel = (self.log.selected + n).min(self.log.rows.len() - 1);
        if new_sel != self.log.selected {
            self.log.selected = new_sel;
            self.maybe_prefetch();
            self.ensure_selected_visible();
            if self.diff.open {
                self.fetch_diff_for_selected();
            }
        } else if new_sel + 1 == self.log.rows.len() {
            self.maybe_prefetch();
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
        self.log.selected = self.log.rows.len() - 1;
        if !self.walk_done {
            let _ = self.tx.send(GitReq::LoadMore(LOAD_PAGE));
        }
        self.ensure_selected_visible();
        if self.diff.open {
            self.fetch_diff_for_selected();
        }
    }

    fn maybe_prefetch(&self) {
        if !self.walk_done && self.log.selected + PREFETCH_THRESHOLD >= self.log.rows.len() {
            let _ = self.tx.send(GitReq::LoadMore(LOAD_PAGE));
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
            self.diff.header_lines = None;
            self.diff.body_lines = None;
            self.diff.files = None;
            self.diff.stats = None;
            self.diff.loading = true;
            let _ = self.tx.send(GitReq::FetchDiff(target));
        }
    }

    fn update_matches(&mut self, kind: SearchKind) {
        match kind {
            SearchKind::Log => self.update_log_matches(),
            SearchKind::Diff => self.update_diff_matches(),
        }
    }

    fn update_log_matches(&mut self) {
        let q = self.search.query.to_lowercase();
        if q.is_empty() {
            self.search.matches.clear();
            self.search.current = 0;
            return;
        }
        // Trigger continued loading so the search eventually covers all commits.
        if !self.walk_done {
            let _ = self.tx.send(GitReq::LoadMore(LOAD_PAGE));
        }
        self.search.matches = self
            .log
            .rows
            .iter()
            .enumerate()
            .filter_map(|(i, row)| match row {
                LogRow::Commit(c) if c.summary_lower.contains(&q) || c.author_lower.contains(q.as_str()) => Some(i),
                _ => None,
            })
            .collect();
        self.search.current = 0;
    }

    fn update_diff_matches(&mut self) {
        let q = self.diff.search.query.to_lowercase();
        if q.is_empty() {
            self.diff.search.matches.clear();
            self.diff.search.current = 0;
            return;
        }
        let header_len = self.diff.header_lines.as_ref().map(|v| v.len()).unwrap_or(0);
        let body_offset = header_len + self.diff.diffstat_line_count();
        let mut matches = Vec::new();

        if let Some(header) = &self.diff.header_lines {
            for (i, line) in header.iter().enumerate() {
                if line.text.to_lowercase().contains(&q) {
                    matches.push(i);
                }
            }
        }
        if let Some(body) = &self.diff.body_lines {
            for (i, line) in body.iter().enumerate() {
                if line.text.to_lowercase().contains(&q) {
                    matches.push(body_offset + i);
                }
            }
        }
        self.diff.search.matches = matches;
        self.diff.search.current = 0;
    }

    fn jump_match(&mut self, kind: SearchKind, delta: isize) {
        if let Some(pos) = self.search_state_mut(kind).advance(delta) {
            self.apply_match_position(kind, pos);
        }
    }

    fn jump_first_at_or_after_cursor(&mut self, kind: SearchKind) {
        let cursor = match kind {
            SearchKind::Log => self.log.selected,
            SearchKind::Diff => self.diff.scroll,
        };
        let state = self.search_state_mut(kind);
        if state.matches.is_empty() {
            return;
        }
        let idx = state.matches.iter().position(|&i| i >= cursor).unwrap_or(0);
        state.current = idx;
        let pos = state.matches[idx];
        self.apply_match_position(kind, pos);
    }

    fn apply_match_position(&mut self, kind: SearchKind, pos: usize) {
        match kind {
            SearchKind::Log => {
                self.log.selected = pos;
                self.ensure_selected_visible();
                if self.diff.open {
                    self.fetch_diff_for_selected();
                }
            }
            SearchKind::Diff => {
                // Keep at least 5 lines of context above the matched line.
                let max = self.diff.total_visible_lines().saturating_sub(self.diff.view_height);
                self.diff.scroll = pos.saturating_sub(5).min(max);
            }
        }
    }

    fn yank_selected_hash(&mut self) {
        let Some(LogRow::Commit(commit)) = self.log.rows.get(self.log.selected) else {
            return;
        };
        let hash = commit.id.to_string();
        yank_to_clipboard(&hash);
        // hash is hex ASCII, so 12-byte prefix is safe.
        let preview = if hash.len() >= 12 { &hash[..12] } else { hash.as_str() };
        self.yank_message = Some(YankFeedback { text: format!("Copied: {}", preview), shown_at: Instant::now() });
    }

    pub fn author_col_width(&self) -> usize {
        AUTHOR_COL_WIDTH
    }
    pub fn date_col_width(&self) -> usize {
        DATE_COL_WIDTH
    }

    /// Number of real commits in the log (excludes the pseudo "Not Committed
    /// Yet" row).
    pub fn commits_len(&self) -> usize {
        self.log.rows.iter().filter(|r| matches!(r, LogRow::Commit(_))).count()
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
        let header = self.header_lines.as_ref().map(|v| v.len()).unwrap_or(0);
        let diffstat = self.diffstat_line_count();
        let body = if self.show_hunks { self.body_lines.as_ref().map(|v| v.len()).unwrap_or(0) } else { 0 };
        header + diffstat + body
    }

    pub fn diffstat_line_count(&self) -> usize {
        match &self.files {
            Some(files) if !files.is_empty() => {
                // separator line `---`, one row per file, blank, totals line
                files.len() + 3
            }
            _ => 0,
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
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }
    #[cfg(target_os = "linux")]
    {
        let mut done = false;
        if let Ok(mut child) = Command::new("xclip").args(["-selection", "clipboard"]).stdin(Stdio::piped()).spawn() {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().is_ok() {
                done = true;
            }
        }
        if !done {
            if let Ok(mut child) = Command::new("xsel").args(["--clipboard", "--input"]).stdin(Stdio::piped()).spawn() {
                if let Some(stdin) = child.stdin.as_mut() {
                    let _ = stdin.write_all(text.as_bytes());
                }
                let _ = child.wait();
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn yank_to_clipboard(_text: &str) {}
