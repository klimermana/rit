//! Top-level UI driver: owns the `App` struct, the channel ends to the
//! git worker + input thread, and the run loop that ties them together.
//! Mode-specific logic is split out:
//!
//!   - `state`     — plain-data state structs (LogState, DiffState, …)
//!   - `search`    — pure search helpers (`commit_matches`,
//!     `should_narrow`, `cycle`)
//!   - `input`     — keyboard dispatch (`impl App` for handle_input)
//!   - `clipboard` — platform-conditional yank helper

pub mod clipboard;
pub mod input;
pub mod search;
pub mod state;

pub use state::{
    CommitSearchState, DiffSearchState, DiffState, Focus, LogRow, LogState, SearchSnapshot, StatusState,
    WorkingTreeRow, YankFeedback,
};

use crate::{
    app::{
        clipboard::yank_to_clipboard,
        search::{commit_matches, should_narrow},
    },
    git::{GitMsg, GitReq, HistoryMsg, HistoryReq, InspectMsg, InspectReq},
    model::{DiffTarget, RepoInfo},
    ui,
};
use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, after, never, select};
use crossterm::event::Event;
use ratatui::{Terminal, backend::Backend};
use std::time::{Duration, Instant};

const YANK_FEEDBACK_DURATION: Duration = Duration::from_secs(2);
const AUTHOR_COL_WIDTH: usize = 20;
const DATE_COL_WIDTH: usize = 8;

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

    pub(crate) fn send_inspect(&self, req: InspectReq) {
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

    pub(crate) fn reload(&mut self) {
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

    pub(crate) fn move_log_down(&mut self, n: usize) {
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

    pub(crate) fn move_log_up(&mut self, n: usize) {
        let new_sel = self.log.selected.saturating_sub(n);
        if new_sel != self.log.selected {
            self.log.selected = new_sel;
            self.ensure_selected_visible();
            if self.diff.open {
                self.fetch_diff_for_selected();
            }
        }
    }

    pub(crate) fn jump_log_top(&mut self) {
        self.log.selected = 0;
        self.log.scroll = 0;
        if self.diff.open {
            self.fetch_diff_for_selected();
        }
    }

    pub(crate) fn jump_log_bottom(&mut self) {
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

    pub(crate) fn fetch_diff_for_selected(&mut self) {
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

    pub(crate) fn update_commit_matches(&mut self) {
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
        // `last_query` is the already-lowercased mirror of `query`,
        // updated by every `update_commit_matches` call (the only place
        // the user-visible query changes). Re-lowercasing on every batch
        // would be redundant — slot in the cached form instead.
        let q = &self.search.last_query;
        if q.is_empty() {
            return;
        }
        let rows = &self.log.rows;
        for i in new_range {
            if let Some(LogRow::Commit(c)) = rows.get(i)
                && commit_matches(c, q)
            {
                self.search.matches.push(i);
            }
        }
    }

    pub(crate) fn update_diff_matches(&mut self) {
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

    pub(crate) fn jump_commit_match(&mut self, delta: isize) {
        if let Some(pos) = self.search.advance(delta) {
            self.apply_commit_match_position(pos);
        }
    }

    pub(crate) fn jump_diff_match(&mut self, delta: isize) {
        if let Some(pos) = self.diff.search.advance(delta) {
            self.apply_diff_match_position(pos);
        }
    }

    pub(crate) fn commit_jump_first_at_or_after_cursor(&mut self) {
        let cursor = self.log.selected;
        let idx = self.search.matches.iter().position(|&i| i >= cursor).unwrap_or(0);
        let Some(&pos) = self.search.matches.get(idx) else { return };
        self.search.current = idx;
        self.apply_commit_match_position(pos);
    }

    pub(crate) fn diff_jump_first_at_or_after_cursor(&mut self) {
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
    pub(crate) fn cycle_focus(&mut self) {
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
    pub(crate) fn migrate_search_to_diff(&mut self) {
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
    pub(crate) fn migrate_search_to_log(&mut self) {
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

    pub(crate) fn yank_selected_hash(&mut self) {
        let Some(LogRow::Commit(commit)) = self.log.rows.get(self.log.selected) else {
            return;
        };
        let hash = commit.id.to_string();
        yank_to_clipboard(&hash);
        let preview = hash.get(..12).unwrap_or(hash.as_str());
        self.yank_message = Some(YankFeedback { text: format!("Copied: {preview}"), shown_at: Instant::now() });
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

    pub(crate) fn diff_scroll_down(&mut self, n: usize) {
        let max = self.diff.total_visible_lines().saturating_sub(self.diff.view_height);
        self.diff.scroll = (self.diff.scroll.saturating_add(n)).min(max);
    }

    pub(crate) fn diff_scroll_to_bottom(&mut self) {
        let total = self.diff.total_visible_lines();
        self.diff.scroll = total.saturating_sub(self.diff.view_height);
    }
}
