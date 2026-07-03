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
    AuthorMode, CommitSearchState, DateMode, DiffSearchState, DiffState, DisplayOptions, Focus, LogRow, LogState,
    SearchSnapshot, StatusState, WorkingTreeRow, YankFeedback,
};

use crate::{
    app::{
        clipboard::yank_to_clipboard,
        search::{commit_matches, jump_first_at_or_after, should_narrow},
    },
    git::{GitMsg, GitReq, HistoryMsg, HistoryReq, InspectMsg, InspectReq, walk::refs_lower_from_refs},
    model::{DiffLine, DiffLineKind, DiffTarget, RepoInfo},
    ui,
};
use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, after, never, select};
use crossterm::event::Event;
use ratatui::{Terminal, backend::Backend};
use std::time::{Duration, Instant};

const YANK_FEEDBACK_DURATION: Duration = Duration::from_secs(2);

pub struct App {
    pub log: LogState,
    pub search: CommitSearchState,
    pub diff: DiffState,
    pub status: StatusState,
    pub focus: Focus,
    /// Render-time display toggles (`D` date, `A` author, `X` hash).
    pub display: DisplayOptions,
    pub show_help: bool,
    pub yank_message: Option<YankFeedback>,
    pub error: Option<String>,
    pub repo_name: String,
    pub branch_name: String,
    /// True once the worker has reported it walked the whole history.
    pub walk_done: bool,
    /// Current walk generation. Bumped on reload and carried in the
    /// `Reload` request, so the worker tags Commits/WalkDone with the
    /// app's own number and stale messages are dropped.
    pub walk_gen: u64,
    /// Monotonically increasing diff-request counter. Carried on every
    /// `LoadDiff` and echoed back on `DiffLoaded` /
    /// `UntrackedFilesUpdate`; replies whose seq is not the latest
    /// issued are stale and dropped.
    pub diff_seq: u64,
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
                horizontal_scroll: 0,
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
            display: DisplayOptions::default(),
            show_help: false,
            yank_message: None,
            error: None,
            repo_name: "unknown".to_string(),
            branch_name: "HEAD".to_string(),
            walk_done: false,
            walk_gen: 0,
            diff_seq: 0,
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
                            // Refresh the searchable ref projection too — without
                            // this, ref search wouldn't find these prefix commits
                            // because their original build saw no refs.
                            c.search.refs_lower = refs_lower_from_refs(&c.refs);
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
            InspectMsg::DiffLoaded { seq, document } => {
                // Seq gate: only the reply to the newest LoadDiff counts.
                // The target check stays as defense in depth (reload
                // clears `target` without bumping the seq).
                if seq == self.diff_seq && self.diff.target == Some(document.target) {
                    self.diff.loading = false;
                    self.diff.document = Some(document);
                    self.diff.scroll = 0;
                    self.diff.horizontal_scroll = 0;
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
            InspectMsg::UntrackedFilesUpdate { target, seq, paths } => {
                self.apply_untracked_update(target, seq, paths);
            }
        }
    }

    /// Splice the side-thread untracked-files result into the current
    /// working-tree `DiffDocument`, in place of the placeholder line.
    ///
    /// Gate checks (any failure = silently drop, the update is stale):
    /// - `seq` matches the latest LoadDiff issued — a scan spawned by an
    ///   *earlier* working-tree request must not consume the anchor of a
    ///   newer document (which would make the fresh scan's own result a
    ///   no-op and leave the placeholder resolved with stale paths)
    /// - the diff pane is still showing this `target`
    /// - the document has an `untracked_anchor` (consuming it makes
    ///   repeat follow-ups no-ops)
    fn apply_untracked_update(&mut self, target: DiffTarget, seq: u64, paths: Vec<String>) {
        if seq != self.diff_seq {
            return;
        }
        if self.diff.target != Some(target) {
            return;
        }
        let Some(doc) = self.diff.document.as_mut() else { return };
        if doc.target != target {
            return;
        }
        let Some(anchor) = doc.untracked_anchor.take() else { return };

        // Pick what replaces the placeholder. When the working tree has
        // no staged/unstaged changes and no untracked files either, this
        // is the moment we emit the "clean" message that the diff-time
        // renderer suppressed pending the walk result.
        let replacement: Vec<DiffLine> = if !paths.is_empty() {
            paths.into_iter().map(|p| DiffLine::new(DiffLineKind::StatusTheirs, format!("?? {p}"))).collect()
        } else if doc.stats.files == 0 {
            vec![DiffLine::new(DiffLineKind::Good, "Nothing to commit, working tree clean")]
        } else {
            // Staged or unstaged changes exist but no untracked entries —
            // splice the placeholder out without leaving a stand-in line.
            Vec::new()
        };

        let delta = replacement.len() as isize - 1;
        doc.body.splice(anchor..anchor + 1, replacement);

        // The splice sits before the staged/unstaged sections, so every
        // file-section start past the anchor shifted by `delta`.
        for section in doc.sections.iter_mut() {
            if section.body_start > anchor {
                section.body_start = section.body_start.saturating_add_signed(delta);
            }
        }

        // Body indices shifted, so the lowercased mirrors and any
        // active diff-search match set are stale. Mirror the
        // invalidation path `DiffLoaded` already does so the next
        // search keystroke sees fresh content.
        self.diff.header_lower = None;
        self.diff.body_lower = None;
        self.update_diff_matches();
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
        // `root: None` keeps the worker's current walk root.
        self.send_history(HistoryReq::Reload { generation: self.walk_gen, root: None });
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
            self.diff_seq = self.diff_seq.wrapping_add(1);
            self.send_inspect(InspectReq::LoadDiff { target, seq: self.diff_seq });
        }
    }

    pub(crate) fn update_commit_matches(&mut self) {
        let q = self.search.state.query.to_lowercase();
        if q.is_empty() {
            self.search.state.matches.clear();
            self.search.state.current = 0;
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
            self.search.state.matches.retain(|&i| match rows.get(i) {
                Some(LogRow::Commit(c)) => commit_matches(c, &q),
                _ => false,
            });
        } else {
            self.search.state.matches = self
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

        self.search.state.current = 0;
        self.search.last_query = q;
        self.search.last_generation = self.walk_gen;
    }

    /// Scan only rows in `new_range`, appending any that match the current
    /// query. Called when the worker streams in a new batch so existing
    /// matches don't get re-tested.
    fn extend_commit_matches(&mut self, new_range: std::ops::Range<usize>) {
        if self.search.state.query.is_empty() {
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
                self.search.state.matches.push(i);
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
        let s = &mut self.search.state;
        if let Some(pos) = jump_first_at_or_after(&s.matches, &mut s.current, cursor) {
            self.apply_commit_match_position(pos);
        }
    }

    pub(crate) fn diff_jump_first_at_or_after_cursor(&mut self) {
        let cursor = self.diff.scroll;
        let s = &mut self.diff.search;
        if let Some(pos) = jump_first_at_or_after(&s.matches, &mut s.current, cursor) {
            self.apply_diff_match_position(pos);
        }
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
        if self.search.state.query.is_empty() {
            return;
        }
        let q = std::mem::take(&mut self.search.state.query);
        // Drain leftover commit-search state so n/N on a future return to
        // log doesn't try to narrow against a now-empty match set.
        self.search.state.active = false;
        self.search.state.matches.clear();
        self.search.state.current = 0;
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

        self.search.state.query = q;
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
        self.display.author.col_width()
    }
    pub fn date_col_width(&self) -> usize {
        self.display.date.col_width()
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

    /// `]` / `[`: jump the diff scroll to the next / previous file
    /// section start. Section starts are body indices; convert to
    /// virtual lines by adding the header + diffstat extent. No-op in
    /// summary mode (`v`), where the body isn't rendered.
    pub(crate) fn diff_jump_file(&mut self, dir: isize) {
        if !self.diff.show_hunks {
            return;
        }
        let Some(doc) = self.diff.document.as_ref() else { return };
        let offset = doc.header.len() + self.diff.diffstat_line_count();
        let cur = self.diff.scroll;
        let target = if dir > 0 {
            doc.sections.iter().map(|s| offset + s.body_start).find(|&v| v > cur)
        } else {
            doc.sections.iter().map(|s| offset + s.body_start).rev().find(|&v| v < cur)
        };
        if let Some(pos) = target {
            let max = self.diff.total_visible_lines().saturating_sub(self.diff.view_height);
            self.diff.scroll = pos.min(max);
        } else if dir < 0 {
            // No section above the cursor — go to the top (commit header).
            self.diff.scroll = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        git::GitReq,
        model::{CommitRecord, CommitSearchText, DiffDocument, DiffFlags, DiffStats, DiffTarget, RefKind, RefLabel},
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::{collections::HashMap, sync::Arc};

    /// Counterparty channel ends for a test `App`. Tests assert on
    /// `req_rx` (what the app sent to the worker); the two senders are
    /// only held so the app's receivers stay connected.
    struct WorkerEnds {
        req_rx: Receiver<GitReq>,
        _msg_tx: Sender<GitMsg>,
        _input_tx: Sender<Event>,
    }

    /// App wired to loopback channels.
    fn test_app() -> (App, WorkerEnds) {
        let (req_tx, req_rx) = crossbeam_channel::unbounded();
        let (msg_tx, msg_rx) = crossbeam_channel::unbounded();
        let (input_tx, input_rx) = crossbeam_channel::unbounded();
        (App::new(req_tx, msg_rx, input_rx), WorkerEnds { req_rx, _msg_tx: msg_tx, _input_tx: input_tx })
    }

    fn oid(n: u8) -> gix::ObjectId {
        let mut raw = [0u8; 20];
        raw[0] = n;
        gix::ObjectId::from_bytes_or_panic(&raw)
    }

    fn commit_record(n: u8, summary: &str, author: &str) -> CommitRecord {
        CommitRecord {
            id: oid(n),
            short_id: format!("{n:07x}").into(),
            authored_unix_secs: 0,
            authored_relative: "1d ago".into(),
            author: author.into(),
            summary: summary.to_string(),
            refs: Vec::new(),
            graph: "".into(),
            search: CommitSearchText {
                author_lower: author.to_lowercase().into(),
                summary_lower: summary.to_lowercase(),
                refs_lower: "".into(),
            },
        }
    }

    fn working_tree_doc_with_anchor() -> DiffDocument {
        DiffDocument {
            target: DiffTarget::WorkingTree,
            header: vec![DiffLine::new(DiffLineKind::CommitHeader, "Working tree")],
            body: vec![
                DiffLine::new(DiffLineKind::SectionTitle, "Untracked files"),
                DiffLine::new(DiffLineKind::Faint, "(scanning for untracked files…)"),
            ],
            files: Vec::new(),
            stats: DiffStats { files: 0, insertions: 0, deletions: 0 },
            flags: DiffFlags::default(),
            untracked_anchor: Some(1),
            sections: Vec::new(),
        }
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    // --- generation gating ---

    #[test]
    fn commits_with_stale_generation_are_dropped() {
        let (mut app, _ends) = test_app();
        assert_eq!(app.walk_gen, 0);
        app.apply_history_msg(HistoryMsg::Commits { generation: 1, commits: vec![commit_record(1, "x", "a")] });
        assert_eq!(app.log.rows.len(), 1, "stale batch must not append rows");

        app.apply_history_msg(HistoryMsg::Commits { generation: 0, commits: vec![commit_record(1, "x", "a")] });
        assert_eq!(app.log.rows.len(), 2, "current-generation batch appends");
    }

    #[test]
    fn walk_done_with_stale_generation_is_dropped() {
        let (mut app, _ends) = test_app();
        app.apply_history_msg(HistoryMsg::WalkDone { generation: 3 });
        assert!(!app.walk_done);
        app.apply_history_msg(HistoryMsg::WalkDone { generation: 0 });
        assert!(app.walk_done);
    }

    #[test]
    fn reload_carries_bumped_generation_to_worker() {
        let (mut app, ends) = test_app();
        app.reload();
        assert_eq!(app.walk_gen, 1);
        let mut saw_reload = false;
        while let Ok(req) = ends.req_rx.try_recv() {
            if let GitReq::History(HistoryReq::Reload { generation, root }) = req {
                assert_eq!(generation, app.walk_gen);
                assert!(root.is_none(), "plain reload keeps the walk root");
                saw_reload = true;
            }
        }
        assert!(saw_reload);
    }

    // --- RefsLoaded backfill ---

    #[test]
    fn refs_loaded_backfills_only_the_first_batch_prefix() {
        let (mut app, _ends) = test_app();
        // Two commits: rows [WT, c1, c2]; only c1 predates refs.
        app.apply_history_msg(HistoryMsg::Commits {
            generation: 0,
            commits: vec![commit_record(1, "first", "a"), commit_record(2, "second", "b")],
        });

        let mut map: HashMap<gix::ObjectId, Vec<RefLabel>> = HashMap::new();
        map.insert(oid(1), vec![RefLabel { name: "v1.0".into(), kind: RefKind::Tag }]);
        map.insert(oid(2), vec![RefLabel { name: "main".into(), kind: RefKind::LocalBranch }]);

        app.apply_history_msg(HistoryMsg::RefsLoaded { generation: 0, refs_map: Arc::new(map), first_batch_rows: 1 });

        let LogRow::Commit(c1) = &app.log.rows[1] else { panic!("row 1 should be a commit") };
        assert_eq!(c1.refs.len(), 1, "prefix commit gets its labels backfilled");
        assert!(c1.search.refs_lower.contains("v1.0"), "search projection refreshed with the labels");

        let LogRow::Commit(c2) = &app.log.rows[2] else { panic!("row 2 should be a commit") };
        assert!(c2.refs.is_empty(), "rows past first_batch_rows were built with refs live and are not touched");
    }

    #[test]
    fn refs_loaded_with_stale_generation_is_dropped() {
        let (mut app, _ends) = test_app();
        app.apply_history_msg(HistoryMsg::Commits { generation: 0, commits: vec![commit_record(1, "first", "a")] });
        let mut map: HashMap<gix::ObjectId, Vec<RefLabel>> = HashMap::new();
        map.insert(oid(1), vec![RefLabel { name: "v1.0".into(), kind: RefKind::Tag }]);
        app.apply_history_msg(HistoryMsg::RefsLoaded { generation: 7, refs_map: Arc::new(map), first_batch_rows: 1 });
        let LogRow::Commit(c1) = &app.log.rows[1] else { panic!("row 1 should be a commit") };
        assert!(c1.refs.is_empty());
    }

    // --- DiffLoaded / untracked splice sequencing ---

    #[test]
    fn diff_loaded_with_stale_seq_is_dropped() {
        let (mut app, _ends) = test_app();
        app.diff.open = true;
        app.diff.target = Some(DiffTarget::WorkingTree);
        app.diff_seq = 2;

        app.apply_inspect_msg(InspectMsg::DiffLoaded { seq: 1, document: working_tree_doc_with_anchor() });
        assert!(app.diff.document.is_none(), "stale-seq document dropped");

        app.apply_inspect_msg(InspectMsg::DiffLoaded { seq: 2, document: working_tree_doc_with_anchor() });
        assert!(app.diff.document.is_some(), "current-seq document applied");
    }

    #[test]
    fn untracked_update_with_fresh_seq_splices_paths() {
        let (mut app, _ends) = test_app();
        app.diff.open = true;
        app.diff.target = Some(DiffTarget::WorkingTree);
        app.diff_seq = 3;
        app.diff.document = Some(working_tree_doc_with_anchor());

        app.apply_untracked_update(DiffTarget::WorkingTree, 3, vec!["new.txt".to_string()]);

        let doc = app.diff.document.as_ref().expect("doc present");
        assert!(doc.untracked_anchor.is_none(), "anchor consumed");
        assert!(doc.body.iter().any(|l| l.text == "?? new.txt"), "untracked path spliced in");
        assert!(!doc.body.iter().any(|l| l.text.contains("scanning")), "placeholder replaced");
    }

    #[test]
    fn untracked_update_with_stale_seq_leaves_anchor_for_fresh_result() {
        let (mut app, _ends) = test_app();
        app.diff.open = true;
        app.diff.target = Some(DiffTarget::WorkingTree);
        // A newer LoadDiff (seq 4) is in flight; the scan spawned by the
        // older seq-3 request reports late.
        app.diff_seq = 4;
        app.diff.document = Some(working_tree_doc_with_anchor());

        app.apply_untracked_update(DiffTarget::WorkingTree, 3, vec!["stale.txt".to_string()]);

        let doc = app.diff.document.as_ref().expect("doc present");
        assert!(doc.untracked_anchor.is_some(), "stale scan must not consume the fresh document's anchor");
        assert!(!doc.body.iter().any(|l| l.text.contains("stale.txt")));

        // The fresh scan's result still lands.
        app.apply_untracked_update(DiffTarget::WorkingTree, 4, vec!["fresh.txt".to_string()]);
        let doc = app.diff.document.as_ref().expect("doc present");
        assert!(doc.untracked_anchor.is_none());
        assert!(doc.body.iter().any(|l| l.text == "?? fresh.txt"));
    }

    #[test]
    fn untracked_update_clean_tree_emits_clean_line() {
        let (mut app, _ends) = test_app();
        app.diff.open = true;
        app.diff.target = Some(DiffTarget::WorkingTree);
        app.diff_seq = 1;
        app.diff.document = Some(working_tree_doc_with_anchor());

        app.apply_untracked_update(DiffTarget::WorkingTree, 1, Vec::new());

        let doc = app.diff.document.as_ref().expect("doc present");
        assert!(doc.body.iter().any(|l| l.text.contains("working tree clean")), "clean message emitted");
    }

    #[test]
    fn fetch_diff_bumps_seq_and_sends_matching_request() {
        let (mut app, ends) = test_app();
        app.log.rows.push(LogRow::Commit(commit_record(1, "x", "a")));
        app.log.selected = 1;
        app.diff.open = true;

        app.fetch_diff_for_selected();

        assert_eq!(app.diff_seq, 1);
        let req = ends.req_rx.try_recv().expect("request sent");
        match req {
            GitReq::Inspect(InspectReq::LoadDiff { target, seq }) => {
                assert_eq!(seq, app.diff_seq);
                assert_eq!(target, DiffTarget::Commit(oid(1)));
            }
            _ => panic!("expected LoadDiff"),
        }
    }

    // --- input routing ---

    #[test]
    fn ctrl_c_quits_from_every_mode() {
        let ctrl_c = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        for setup in [
            (|_a: &mut App| {}) as fn(&mut App),
            |a| a.show_help = true,
            |a| a.status.open = true,
            |a| a.search.state.active = true,
            |a| a.diff.search.active = true,
        ] {
            let (mut app, _ends) = test_app();
            setup(&mut app);
            app.handle_input(ctrl_c);
            assert!(app.should_quit, "Ctrl+C must quit regardless of mode");
        }
    }

    #[test]
    fn search_mode_captures_chars_instead_of_navigating() {
        let (mut app, _ends) = test_app();
        app.log.rows.push(LogRow::Commit(commit_record(1, "fix things", "a")));
        app.search.state.active = true;

        app.handle_input(key(KeyCode::Char('j'), KeyModifiers::NONE));

        assert_eq!(app.search.state.query, "j", "chars go into the query while search input is active");
        assert_eq!(app.log.selected, 0, "no navigation happened");
    }

    #[test]
    fn help_intercepts_keys_and_closes_on_q() {
        let (mut app, _ends) = test_app();
        app.log.rows.push(LogRow::Commit(commit_record(1, "x", "a")));
        app.show_help = true;

        app.handle_input(key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.log.selected, 0, "keys don't leak through the help popup");

        app.handle_input(key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.show_help, "q closes help");
        assert!(!app.should_quit, "q in help does not quit the app");
    }

    #[test]
    fn diff_jump_file_moves_between_section_starts() {
        use crate::model::FileSection;
        let (mut app, _ends) = test_app();
        app.diff.open = true;
        app.focus = Focus::Diff;
        app.diff.view_height = 5;
        let mut doc = working_tree_doc_with_anchor();
        doc.untracked_anchor = None;
        // header has 1 line, no diffstat (files empty) → offset = 1.
        doc.body = (0..40).map(|i| DiffLine::new(DiffLineKind::Context, format!("line {i}"))).collect();
        doc.sections = vec![
            FileSection { path: "a.txt".into(), body_start: 0 },
            FileSection { path: "b.txt".into(), body_start: 10 },
            FileSection { path: "c.txt".into(), body_start: 30 },
        ];
        app.diff.document = Some(doc);

        app.diff_jump_file(1);
        assert_eq!(app.diff.scroll, 1, "] from top lands on the first section (offset by header)");
        app.diff_jump_file(1);
        assert_eq!(app.diff.scroll, 11);
        app.diff_jump_file(-1);
        assert_eq!(app.diff.scroll, 1);
        app.diff_jump_file(-1);
        assert_eq!(app.diff.scroll, 0, "[ above the first section goes to the top");

        // Jump past the end clamps to max scroll.
        app.diff.scroll = 12;
        app.diff_jump_file(1);
        let max = app.diff.total_visible_lines() - app.diff.view_height;
        assert_eq!(app.diff.scroll, 31.min(max));
    }

    #[test]
    fn untracked_splice_shifts_later_section_starts() {
        use crate::model::FileSection;
        let (mut app, _ends) = test_app();
        app.diff.open = true;
        app.diff.target = Some(DiffTarget::WorkingTree);
        app.diff_seq = 1;
        let mut doc = working_tree_doc_with_anchor(); // anchor at body index 1
        doc.sections = vec![
            FileSection { path: "before.txt".into(), body_start: 0 },
            FileSection { path: "after.txt".into(), body_start: 2 },
        ];
        app.diff.document = Some(doc);

        // Three untracked paths replace the 1-line placeholder: delta +2.
        app.apply_untracked_update(
            DiffTarget::WorkingTree,
            1,
            vec!["u1".to_string(), "u2".to_string(), "u3".to_string()],
        );

        let doc = app.diff.document.as_ref().expect("doc");
        assert_eq!(doc.sections[0].body_start, 0, "sections before the anchor stay put");
        assert_eq!(doc.sections[1].body_start, 4, "sections after the anchor shift by the splice delta");
    }

    #[test]
    fn display_toggles_cycle_modes() {
        let (mut app, _ends) = test_app();
        app.handle_input(key(KeyCode::Char('D'), KeyModifiers::SHIFT));
        assert!(matches!(app.display.date, DateMode::Absolute));
        app.handle_input(key(KeyCode::Char('D'), KeyModifiers::SHIFT));
        assert!(matches!(app.display.date, DateMode::Off));
        app.handle_input(key(KeyCode::Char('D'), KeyModifiers::SHIFT));
        assert!(matches!(app.display.date, DateMode::Relative));

        app.handle_input(key(KeyCode::Char('A'), KeyModifiers::SHIFT));
        assert!(matches!(app.display.author, AuthorMode::Abbrev));

        app.handle_input(key(KeyCode::Char('X'), KeyModifiers::SHIFT));
        assert!(app.display.full_hash);
        app.handle_input(key(KeyCode::Char('X'), KeyModifiers::SHIFT));
        assert!(!app.display.full_hash);
    }

    #[test]
    fn status_mode_routes_keys_to_status_handler() {
        let (mut app, _ends) = test_app();
        app.status.open = true;
        app.handle_input(key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.status.open, "q closes the status view");
        assert!(!app.should_quit);
    }
}
