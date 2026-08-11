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
pub mod mouse;
pub mod search;
pub mod state;

pub use state::{
    AuthorMode, BlameState, CommitSearchState, DateMode, DiffSearchState, DiffSelection, DiffState, DisplayOptions,
    FileViewReturn, Focus, LogRow, LogState, PaneRects, RefsState, SearchSnapshot, SearchState, StatusState,
    WorkingTreeRow, YankFeedback,
};

use crate::{
    app::{
        clipboard::yank_to_clipboard,
        search::{commit_matches, jump_first_at_or_after, should_narrow},
    },
    git::{BlameAt, GitMsg, GitReq, HistoryMsg, HistoryReq, InspectMsg, InspectReq, walk::refs_lower_from_refs},
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
    pub refs: RefsState,
    pub blame: BlameState,
    pub focus: Focus,
    /// Pane geometry recorded by the renderer each frame — the mouse
    /// handler routes wheel/click/drag events by hit-testing these.
    pub panes: PaneRects,
    /// Render-time display toggles (`D` date, `A` author, `X` hash).
    pub display: DisplayOptions,
    pub show_help: bool,
    pub yank_message: Option<YankFeedback>,
    pub error: Option<String>,
    pub repo_name: String,
    pub branch_name: String,
    /// The CLI pathspec, when the positional argument resolved to one.
    /// Reported by the worker via `RepoInfo`; gates the diff pane's
    /// `f` scope toggle and feeds its title tag.
    pub path_filter: Option<String>,
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
    /// Same discipline for `LoadBlame` / `BlameLoaded` — rapid `,`
    /// re-blames supersede each other.
    pub blame_seq: u64,
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
                scoped: true,
                view_height: 1,
                search: DiffSearchState::new(),
                header_lower: None,
                body_lower: None,
                file_view_return: None,
                file_picker: None,
                picker_filter: SearchState::new(),
                selection: None,
            },
            status: StatusState { open: false, document: None, scroll: 0, loading: false },
            refs: RefsState::new(),
            blame: BlameState::new(),
            focus: Focus::Log,
            panes: PaneRects::default(),
            display: DisplayOptions::default(),
            show_help: false,
            yank_message: None,
            error: None,
            repo_name: "unknown".to_string(),
            branch_name: "HEAD".to_string(),
            path_filter: None,
            walk_done: false,
            walk_gen: 0,
            diff_seq: 0,
            blame_seq: 0,
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
                    Ok(Event::Mouse(m)) => self.handle_mouse(m),
                    // Resize / focus / etc. just wake the loop so we redraw.
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
            HistoryMsg::RepoInfo(RepoInfo { name, branch, path_filter }) => {
                self.repo_name = name;
                self.branch_name = branch;
                self.path_filter = path_filter;
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
                    // A fresh multi-file document invalidates the picker
                    // cursor (the request path already dropped any stale
                    // single-file return slot).
                    self.diff.close_picker();
                    // New content invalidates the lowercased mirrors and
                    // any mouse selection.
                    self.diff.header_lower = None;
                    self.diff.body_lower = None;
                    self.diff.selection = None;
                    // Re-run diff search against new content.
                    self.update_diff_matches();
                }
            }
            InspectMsg::FileDiffLoaded { seq, document } => {
                // Same seq/target discipline as DiffLoaded; additionally
                // require the pane to still be open — a reply landing
                // after q closed the pane must not resurrect state.
                if seq == self.diff_seq && self.diff.open && self.diff.target == Some(document.target) {
                    // Stash the multi-file document (and viewport) the
                    // takeover replaces, so q/Esc can restore it without
                    // a refetch.
                    if let Some(current) = self.diff.document.take() {
                        self.diff.file_view_return = Some(Box::new(FileViewReturn {
                            document: current,
                            scroll: self.diff.scroll,
                            horizontal_scroll: self.diff.horizontal_scroll,
                        }));
                    }
                    self.diff.loading = false;
                    self.diff.document = Some(document);
                    self.diff.scroll = self.file_view_first_change_scroll();
                    self.diff.horizontal_scroll = 0;
                    self.diff.close_picker();
                    self.diff.header_lower = None;
                    self.diff.body_lower = None;
                    self.diff.selection = None;
                    self.update_diff_matches();
                }
            }
            InspectMsg::StatusLoaded(document) => {
                self.status.loading = false;
                self.status.document = Some(document);
            }
            InspectMsg::RefsListLoaded(entries) => {
                self.refs.loading = false;
                self.refs.entries = entries;
                self.refs.selected = self.refs.selected.min(self.refs.entries.len().saturating_sub(1));
            }
            InspectMsg::BlameLoaded { seq, result } => {
                if seq != self.blame_seq {
                    return;
                }
                self.blame.loading = false;
                match result {
                    Ok(document) => {
                        // A successful `,` re-blame commits its staged
                        // return point to the Backspace trail.
                        if let Some(entry) = self.blame.pending_return.take() {
                            self.blame.history.push(entry);
                        }
                        self.blame.document = Some(document);
                        self.blame.error = None;
                        self.blame.selected = 0;
                        self.blame.scroll = 0;
                    }
                    Err(message) => {
                        // Keep the previous document visible under the
                        // error so a failed re-blame doesn't blank the
                        // view; discard the staged return point.
                        self.blame.pending_return = None;
                        self.blame.error = Some(message);
                    }
                }
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

        // Body indices shifted, so the lowercased mirrors, any active
        // diff-search match set, and any mouse selection are stale.
        // Mirror the invalidation path `DiffLoaded` already does so the
        // next search keystroke sees fresh content.
        self.diff.header_lower = None;
        self.diff.body_lower = None;
        self.diff.selection = None;
        self.update_diff_matches();
    }

    pub(crate) fn reload(&mut self) {
        // `root: None` keeps the worker's current walk root.
        self.reload_with_root(None);
    }

    /// Reset the log/diff/search state and restart the walk. `Some(id)`
    /// re-roots the walk at that commit (refs-view `Enter`); `None`
    /// keeps the worker's current root (plain `R`).
    pub(crate) fn reload_with_root(&mut self, root: Option<gix::ObjectId>) {
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
        self.diff.file_view_return = None;
        self.diff.close_picker();
        self.status.document = None;
        self.status.loading = false;
        self.search.clear();
        self.diff.search.clear();
        self.walk_done = false;
        self.walk_gen = self.walk_gen.wrapping_add(1);
        self.error = None;
        // Reload restarts the worker's continuous walk; no explicit
        // LoadMore needed, the worker streams batches on its own.
        self.send_history(HistoryReq::Reload { generation: self.walk_gen, root });
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
        self.request_diff(target);
    }

    /// Point the diff pane at `target` and request its document (no-op
    /// when the pane already shows that target). Shared by log
    /// navigation and the blame view's `Enter`.
    pub(crate) fn request_diff(&mut self, target: DiffTarget) {
        if self.diff.target != Some(target) {
            self.diff.target = Some(target);
            self.diff.document = None;
            self.diff.header_lower = None;
            self.diff.body_lower = None;
            self.diff.selection = None;
            // A new target invalidates the single-file takeover and any
            // picker cursor along with the document they belonged to.
            self.diff.file_view_return = None;
            self.diff.close_picker();
            self.diff.loading = true;
            self.diff_seq = self.diff_seq.wrapping_add(1);
            self.send_inspect(InspectReq::LoadDiff { target, seq: self.diff_seq, scoped: self.diff.scoped });
        }
    }

    /// `f`: flip pathspec scoping for commit diffs and re-request the
    /// current target. No-op unless a CLI pathspec is active — with no
    /// filter there is nothing to scope to. Bypasses `request_diff`'s
    /// same-target short-circuit and keeps the current document on
    /// screen until the re-rendered one arrives (the seq gate swaps it
    /// in atomically).
    pub(crate) fn toggle_diff_scope(&mut self) {
        if self.path_filter.is_none() {
            return;
        }
        self.diff.scoped = !self.diff.scoped;
        let Some(target) = self.diff.target else { return };
        // The incoming re-scoped document replaces whatever is showing —
        // a stashed multi-file view would restore stale content.
        self.diff.file_view_return = None;
        self.diff.close_picker();
        self.diff.loading = true;
        self.diff_seq = self.diff_seq.wrapping_add(1);
        self.send_inspect(InspectReq::LoadDiff { target, seq: self.diff_seq, scoped: self.diff.scoped });
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

    /// `y` in the blame view: yank the selected line's commit hash.
    pub(crate) fn yank_blame_hash(&mut self) {
        let Some(line) = self.blame.document.as_ref().and_then(|d| d.lines.get(self.blame.selected)) else {
            return;
        };
        let hash = line.commit_id.to_string();
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

    /// Open the refs view and request a fresh listing (refs may have
    /// been created/moved since the last look).
    pub(crate) fn open_refs_view(&mut self) {
        self.refs.open = true;
        self.refs.loading = true;
        self.refs.selected = 0;
        self.refs.scroll = 0;
        self.send_inspect(InspectReq::LoadRefs);
    }

    /// Move the refs-view selection by `delta`, clamped, keeping the
    /// selection visible within the pane (clamped at input time — the
    /// renderer only reads `scroll`).
    pub(crate) fn refs_move(&mut self, delta: isize) {
        if self.refs.entries.is_empty() {
            return;
        }
        let last = self.refs.entries.len() - 1;
        let sel = (self.refs.selected as isize).saturating_add(delta).clamp(0, last as isize);
        self.refs.selected = usize::try_from(sel).unwrap_or(0);
        let h = self.refs.view_height.max(1);
        if self.refs.selected < self.refs.scroll {
            self.refs.scroll = self.refs.selected;
        } else if self.refs.selected >= self.refs.scroll + h {
            self.refs.scroll = self.refs.selected - (h - 1);
        }
    }

    /// `Enter` in the refs view: re-root the log at the selected ref.
    pub(crate) fn refs_activate_selected(&mut self) {
        let Some(entry) = self.refs.entries.get(self.refs.selected) else {
            return;
        };
        let target = entry.target;
        self.branch_name = entry.name.to_string();
        self.refs.open = false;
        self.reload_with_root(Some(target));
    }

    /// Open the blame view for `path` at `at`, requesting the
    /// annotation from the worker. Used by the diff pane's `b`, the
    /// CLI's `rit blame <path>` (hence `pub`), and the re-blame/back
    /// keys.
    pub fn open_blame(&mut self, path: String, at: BlameAt) {
        self.blame.open = true;
        self.blame.loading = true;
        self.blame.error = None;
        self.blame_seq = self.blame_seq.wrapping_add(1);
        self.send_inspect(InspectReq::LoadBlame { path, at, seq: self.blame_seq });
    }

    /// `b` in the diff pane: blame the file whose section is at (or
    /// above) the top of the viewport. Commit diffs blame at that
    /// commit; the working-tree diff blames at HEAD.
    pub(crate) fn blame_from_diff_pane(&mut self) {
        let Some(path) = self.diff_file_at_viewport() else { return };
        let Some(doc) = self.diff.document.as_ref() else { return };
        let at = match doc.target {
            DiffTarget::Commit(id) => BlameAt::Commit(id),
            DiffTarget::WorkingTree => BlameAt::Head,
        };
        self.blame.history.clear();
        self.blame.pending_return = None;
        self.open_blame(path, at);
    }

    /// Move the blame line cursor by `delta`, clamped, keeping it
    /// visible (input-time clamping against the renderer-reported
    /// pane height).
    pub(crate) fn blame_move(&mut self, delta: isize) {
        let total = self.blame.document.as_ref().map(|d| d.lines.len()).unwrap_or(0);
        if total == 0 {
            return;
        }
        let sel = (self.blame.selected as isize).saturating_add(delta).clamp(0, (total - 1) as isize);
        self.blame.selected = usize::try_from(sel).unwrap_or(0);
        let h = self.blame.view_height.max(1);
        if self.blame.selected < self.blame.scroll {
            self.blame.scroll = self.blame.selected;
        } else if self.blame.selected >= self.blame.scroll + h {
            self.blame.scroll = self.blame.selected - (h - 1);
        }
    }

    /// `Enter` on a blame line: leave the blame view and open that
    /// commit's diff, selecting it in the log when it's indexed there.
    pub(crate) fn blame_open_commit(&mut self) {
        let Some(id) = self.blame.document.as_ref().and_then(|d| d.lines.get(self.blame.selected)).map(|l| l.commit_id)
        else {
            return;
        };
        self.blame.open = false;
        if let Some(idx) = self.log.rows.iter().position(|r| matches!(r, LogRow::Commit(c) if c.id == id)) {
            self.log.selected = idx;
            self.ensure_selected_visible();
        }
        self.diff.open = true;
        self.focus = Focus::Diff;
        self.diff.scroll = 0;
        self.request_diff(DiffTarget::Commit(id));
    }

    /// `,` on a blame line: re-blame the file at the parent of the
    /// commit that introduced the line — "who touched this before".
    /// Follows renames via the line's `source_path`. The current
    /// position is staged for the Backspace trail and committed when
    /// the request succeeds.
    pub(crate) fn blame_reblame_at_parent(&mut self) {
        let Some(doc) = self.blame.document.as_ref() else { return };
        let Some(line) = doc.lines.get(self.blame.selected) else { return };
        let path = line.source_path.clone().unwrap_or_else(|| doc.path.clone());
        let commit = line.commit_id;
        self.blame.pending_return = Some((doc.at, doc.path.clone()));
        self.open_blame(path, BlameAt::ParentOfCommit(commit));
    }

    /// Backspace in the blame view: return to the most recent re-blame
    /// origin.
    pub(crate) fn blame_back(&mut self) {
        let Some((at, path)) = self.blame.history.pop() else { return };
        self.blame.pending_return = None;
        self.open_blame(path, BlameAt::Commit(at));
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

    /// Path of the file whose section is at (or above) the top of the
    /// diff viewport; before the first section (header/diffstat) falls
    /// back to the first file. Shared by `b` (blame) and `o` (isolate).
    fn diff_file_at_viewport(&self) -> Option<String> {
        let doc = self.diff.document.as_ref()?;
        let offset = doc.header.len() + self.diff.diffstat_line_count();
        let body_pos = self.diff.scroll.saturating_sub(offset);
        doc.sections
            .iter()
            .rev()
            .find(|s| s.body_start <= body_pos)
            .or_else(|| doc.sections.first())
            .map(|s| s.path.clone())
    }

    /// `o` in the diff pane: isolate the file under the viewport into
    /// the single-file takeover view.
    pub(crate) fn open_file_view_at_viewport(&mut self) {
        let Some(path) = self.diff_file_at_viewport() else { return };
        self.open_file_view(path);
    }

    /// Request the full-context single-file document for `path` at the
    /// pane's current target. No-op while a takeover view is already
    /// showing — the view is a one-deep stack, not a browser history.
    pub(crate) fn open_file_view(&mut self, path: String) {
        if self.diff.file_view_return.is_some() {
            return;
        }
        let Some(target) = self.diff.target else { return };
        self.diff_seq = self.diff_seq.wrapping_add(1);
        self.send_inspect(InspectReq::LoadFileDiff { target, path, seq: self.diff_seq });
    }

    /// Initial viewport for a freshly-loaded single-file view: the
    /// first +/- line, kept a few rows down so its leading context is
    /// visible — `o` should land on the change, not the top of the
    /// file. Falls back to the top when the body has no changed lines
    /// (skip summaries) or isn't rendered (summary mode).
    fn file_view_first_change_scroll(&self) -> usize {
        // Match the ±3 context the inline hunk view shows above a change.
        const LEAD_CONTEXT: usize = 3;
        if !self.diff.show_hunks {
            return 0;
        }
        let Some(doc) = self.diff.document.as_ref() else { return 0 };
        let Some(first) = doc.body.iter().position(|l| matches!(l.kind, DiffLineKind::Add | DiffLineKind::Del)) else {
            return 0;
        };
        let offset = doc.header.len() + self.diff.diffstat_line_count();
        let max = self.diff.total_visible_lines().saturating_sub(self.diff.view_height);
        (offset + first).saturating_sub(LEAD_CONTEXT).min(max)
    }

    /// q/Esc while the single-file view is showing: restore the stashed
    /// multi-file document and viewport. Returns false when no takeover
    /// view is active (the caller then closes the pane as usual).
    pub(crate) fn close_file_view(&mut self) -> bool {
        let Some(ret) = self.diff.file_view_return.take() else {
            return false;
        };
        self.diff.document = Some(ret.document);
        self.diff.scroll = ret.scroll;
        self.diff.horizontal_scroll = ret.horizontal_scroll;
        self.diff.header_lower = None;
        self.diff.body_lower = None;
        self.diff.selection = None;
        self.update_diff_matches();
        // If the user isolated a working-tree file before the deferred
        // untracked scan reported back, that update was seq-gated away
        // while the takeover was up — the restored document would show
        // the "(scanning…)" placeholder forever. Re-request it fresh;
        // the restored document stays on screen until the reply lands.
        if self.diff.document.as_ref().is_some_and(|d| d.untracked_anchor.is_some())
            && let Some(target) = self.diff.target
        {
            self.diff.loading = true;
            self.diff_seq = self.diff_seq.wrapping_add(1);
            self.send_inspect(InspectReq::LoadDiff { target, seq: self.diff_seq, scoped: self.diff.scoped });
        }
        true
    }

    /// `t` in the diff pane: enter file-picker mode over the diffstat,
    /// starting at the file currently under the viewport.
    pub(crate) fn open_file_picker(&mut self) {
        let start = self.diff_file_at_viewport();
        let Some(doc) = self.diff.document.as_ref() else { return };
        if doc.files.is_empty() {
            return;
        }
        let idx = start.and_then(|p| doc.files.iter().position(|f| f.path == p)).unwrap_or(0);
        self.diff.picker_filter.clear();
        self.diff.file_picker = Some(idx);
        self.ensure_picker_visible();
    }

    /// Move the picker cursor by `delta` over the navigable rows — all
    /// files normally, only `picker_filter.matches` while a filter is
    /// set. Single steps (j/k) wrap around the ends; larger jumps
    /// (g/G, half-page) clamp.
    pub(crate) fn picker_move(&mut self, delta: isize) {
        let Some(idx) = self.diff.file_picker else { return };
        let total = self.diff.document.as_ref().map(|d| d.files.len()).unwrap_or(0);
        if total == 0 {
            return;
        }
        let sel = if self.diff.picker_filter.query.is_empty() {
            step_wrapping(total, idx.min(total - 1), delta)
        } else {
            let matches = &self.diff.picker_filter.matches;
            if matches.is_empty() {
                return;
            }
            // The cursor sits on a match whenever the filter is set
            // (update_picker_matches snaps it); position() is defensive.
            let pos = matches.iter().position(|&m| m >= idx).unwrap_or(matches.len() - 1);
            let pos = step_wrapping(matches.len(), pos, delta);
            let Some(&m) = matches.get(pos) else { return };
            self.diff.picker_filter.current = pos;
            m
        };
        self.diff.file_picker = Some(sel);
        self.ensure_picker_visible();
    }

    /// Recompute `picker_filter.matches` (case-insensitive substring on
    /// the file path) and snap the cursor onto the first match at or
    /// after it, wrapping to the first match overall.
    pub(crate) fn update_picker_matches(&mut self) {
        let query = self.diff.picker_filter.query.to_lowercase();
        let Some(doc) = self.diff.document.as_ref() else { return };
        self.diff.picker_filter.matches = doc
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| !query.is_empty() && f.path.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect();
        let matches = &self.diff.picker_filter.matches;
        if let Some(idx) = self.diff.file_picker {
            let pos = matches.iter().position(|&m| m >= idx).unwrap_or(0);
            if let Some(&m) = matches.get(pos) {
                self.diff.picker_filter.current = pos;
                self.diff.file_picker = Some(m);
                self.ensure_picker_visible();
            }
        }
    }

    /// Keep the picker's diffstat row inside the viewport. Row math
    /// mirrors `append_diffstat`: header lines, then the `---`
    /// separator, then one row per file.
    fn ensure_picker_visible(&mut self) {
        let Some(idx) = self.diff.file_picker else { return };
        let Some(doc) = self.diff.document.as_ref() else { return };
        let line = doc.header.len() + 1 + idx;
        let h = self.diff.view_height.max(1);
        if line < self.diff.scroll {
            self.diff.scroll = line;
        } else if line >= self.diff.scroll + h {
            self.diff.scroll = line + 1 - h;
        }
    }

    /// Enter on a picker row: jump to that file's diff section. Files
    /// with no body section (suppressed past the file/line caps) and
    /// summary mode (`v`, body hidden) fall through to the single-file
    /// view — the only way to see those diffs.
    pub(crate) fn picker_activate(&mut self) {
        let Some(idx) = self.diff.file_picker.take() else { return };
        self.diff.picker_filter.clear();
        let Some(doc) = self.diff.document.as_ref() else { return };
        let Some(stat) = doc.files.get(idx) else { return };
        let path = stat.path.clone();
        let jump_pos = if self.diff.show_hunks {
            let offset = doc.header.len() + self.diff.diffstat_line_count();
            doc.sections.iter().find(|s| s.path == path).map(|s| offset + s.body_start)
        } else {
            None
        };
        match jump_pos {
            Some(pos) => {
                let max = self.diff.total_visible_lines().saturating_sub(self.diff.view_height);
                self.diff.scroll = pos.min(max);
            }
            None => self.open_file_view(path),
        }
    }

    /// `o` on a picker row: open that file's single-file view directly.
    pub(crate) fn picker_open_file(&mut self) {
        let Some(idx) = self.diff.file_picker.take() else { return };
        self.diff.picker_filter.clear();
        let Some(path) = self.diff.document.as_ref().and_then(|d| d.files.get(idx)).map(|f| f.path.clone()) else {
            return;
        };
        self.open_file_view(path);
    }
}

/// Advance position `pos` by `delta` within a list of `len` rows:
/// single steps wrap around the ends, larger deltas (g/G's
/// `isize::MIN`/`MAX`, half-pages) clamp. `len` must be non-zero.
fn step_wrapping(len: usize, pos: usize, delta: isize) -> usize {
    match delta {
        1 if pos + 1 >= len => 0,
        -1 if pos == 0 => len - 1,
        _ => usize::try_from((pos as isize).saturating_add(delta).clamp(0, (len - 1) as isize)).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        git::GitReq,
        model::{
            CommitRecord, CommitSearchText, DiffDocument, DiffFlags, DiffStats, DiffTarget, FileSection, FileStat,
            RefKind, RefLabel,
        },
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

    /// A commit diff with three files in the diffstat, two of which
    /// have body sections ("capped.rs" was suppressed past the caps).
    /// Header is 1 line, diffstat renders as 6 (separator + 3 rows +
    /// totals + blank), so body offset = 7.
    fn commit_doc_with_sections() -> DiffDocument {
        DiffDocument {
            target: DiffTarget::Commit(oid(7)),
            header: vec![DiffLine::new(DiffLineKind::CommitHeader, "commit 7")],
            body: (0..40).map(|i| DiffLine::new(DiffLineKind::Context, format!("line {i}"))).collect(),
            files: vec![
                FileStat { path: "a.rs".into(), additions: 1, deletions: 0 },
                FileStat { path: "b.rs".into(), additions: 2, deletions: 1 },
                FileStat { path: "capped.rs".into(), additions: 0, deletions: 0 },
            ],
            stats: DiffStats { files: 3, insertions: 3, deletions: 1 },
            flags: DiffFlags::default(),
            untracked_anchor: None,
            sections: vec![
                FileSection { path: "a.rs".into(), body_start: 0 },
                FileSection { path: "b.rs".into(), body_start: 15 },
            ],
        }
    }

    fn single_file_doc(path: &str) -> DiffDocument {
        DiffDocument {
            target: DiffTarget::Commit(oid(7)),
            header: Vec::new(),
            body: vec![DiffLine::new(DiffLineKind::Context, "full file")],
            files: vec![FileStat { path: path.into(), additions: 2, deletions: 1 }],
            stats: DiffStats { files: 1, insertions: 2, deletions: 1 },
            flags: DiffFlags::default(),
            untracked_anchor: None,
            sections: vec![FileSection { path: path.into(), body_start: 0 }],
        }
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
            GitReq::Inspect(InspectReq::LoadDiff { target, seq, scoped }) => {
                assert_eq!(seq, app.diff_seq);
                assert_eq!(target, DiffTarget::Commit(oid(1)));
                assert!(scoped, "diffs are scoped by default");
            }
            _ => panic!("expected LoadDiff"),
        }
    }

    #[test]
    fn f_toggles_diff_scope_and_rerequests() {
        let (mut app, ends) = test_app();
        app.path_filter = Some("src".to_string());
        app.log.rows.push(LogRow::Commit(commit_record(1, "one", "alice")));
        app.log.selected = 1;
        app.diff.open = true;
        app.fetch_diff_for_selected();
        _ = ends.req_rx.try_recv().expect("initial LoadDiff");

        app.handle_input(key(KeyCode::Char('f'), KeyModifiers::NONE));
        assert!(!app.diff.scoped, "f flips scoping off");
        match ends.req_rx.try_recv().expect("toggle re-requests the same target") {
            GitReq::Inspect(InspectReq::LoadDiff { target, seq, scoped }) => {
                assert_eq!(target, DiffTarget::Commit(oid(1)));
                assert_eq!(seq, app.diff_seq, "toggle must bump the seq so the old document is superseded");
                assert!(!scoped);
            }
            _ => panic!("expected LoadDiff"),
        }
    }

    #[test]
    fn f_is_noop_without_path_filter() {
        let (mut app, ends) = test_app();
        app.log.rows.push(LogRow::Commit(commit_record(1, "one", "alice")));
        app.log.selected = 1;
        app.diff.open = true;
        app.fetch_diff_for_selected();
        _ = ends.req_rx.try_recv().expect("initial LoadDiff");

        app.handle_input(key(KeyCode::Char('f'), KeyModifiers::NONE));
        assert!(app.diff.scoped, "no pathspec, nothing to toggle");
        assert!(ends.req_rx.try_recv().is_err(), "no re-request without a pathspec");
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
    fn o_isolates_the_viewport_file_and_q_restores_the_multi_file_view() {
        let (mut app, ends) = test_app();
        app.diff.open = true;
        app.focus = Focus::Diff;
        app.diff.target = Some(DiffTarget::Commit(oid(7)));
        app.diff.view_height = 5;
        app.diff.document = Some(commit_doc_with_sections());
        // Body offset is 7 (see the doc helper); scroll 23 puts b.rs
        // (body_start 15 → virtual 22) at the viewport top.
        app.diff.scroll = 23;

        app.handle_input(key(KeyCode::Char('o'), KeyModifiers::NONE));
        let seq = app.diff_seq;
        match ends.req_rx.try_recv().expect("LoadFileDiff sent") {
            GitReq::Inspect(InspectReq::LoadFileDiff { target, path, seq: s }) => {
                assert_eq!(target, DiffTarget::Commit(oid(7)));
                assert_eq!(path, "b.rs", "o targets the file under the viewport, same rule as b");
                assert_eq!(s, seq);
            }
            _ => panic!("expected LoadFileDiff"),
        }

        app.apply_inspect_msg(InspectMsg::FileDiffLoaded { seq, document: single_file_doc("b.rs") });
        assert!(app.diff.file_view_return.is_some(), "multi-file view stashed for q");
        assert_eq!(app.diff.scroll, 0, "stub body has no +/- lines, so the view opens at the top");
        assert_eq!(app.diff.document.as_ref().map(|d| d.files.len()), Some(1));

        // First q pops back to the multi-file view at the old viewport.
        app.handle_input(key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(app.diff.open, "first q only closes the file view");
        assert!(app.diff.file_view_return.is_none());
        assert_eq!(app.diff.document.as_ref().map(|d| d.files.len()), Some(3));
        assert_eq!(app.diff.scroll, 23, "viewport restored");

        // Second q closes the pane as before.
        app.handle_input(key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.diff.open);
    }

    #[test]
    fn file_view_opens_scrolled_to_the_first_change() {
        let (mut app, _ends) = test_app();
        app.diff.open = true;
        app.diff.target = Some(DiffTarget::Commit(oid(7)));
        app.diff.view_height = 10;

        let mut doc = single_file_doc("b.rs");
        doc.body = (0..20)
            .map(|i| DiffLine::new(DiffLineKind::Context, format!("line {i}")))
            .chain(std::iter::once(DiffLine::new(DiffLineKind::Add, "added")))
            .chain((21..30).map(|i| DiffLine::new(DiffLineKind::Context, format!("line {i}"))))
            .collect();
        app.apply_inspect_msg(InspectMsg::FileDiffLoaded { seq: app.diff_seq, document: doc });

        // Empty header + 4-line diffstat block put the Add (body index
        // 20) at virtual line 24; minus 3 lines of lead context = 21.
        assert_eq!(app.diff.scroll, 21);
    }

    #[test]
    fn file_view_first_change_near_the_top_clamps_to_zero() {
        let (mut app, _ends) = test_app();
        app.diff.open = true;
        app.diff.target = Some(DiffTarget::Commit(oid(7)));
        app.diff.view_height = 10;

        let mut doc = single_file_doc("b.rs");
        doc.header = Vec::new();
        doc.files = Vec::new(); // no diffstat block
        doc.body = vec![DiffLine::new(DiffLineKind::Add, "added first line")];
        app.apply_inspect_msg(InspectMsg::FileDiffLoaded { seq: app.diff_seq, document: doc });

        assert_eq!(app.diff.scroll, 0, "lead context saturates at the top of the document");
    }

    #[test]
    fn file_diff_loaded_with_stale_seq_is_dropped() {
        let (mut app, _ends) = test_app();
        app.diff.open = true;
        app.diff.target = Some(DiffTarget::Commit(oid(7)));
        app.diff_seq = 5;
        app.diff.document = Some(commit_doc_with_sections());

        app.apply_inspect_msg(InspectMsg::FileDiffLoaded { seq: 4, document: single_file_doc("b.rs") });

        assert!(app.diff.file_view_return.is_none());
        assert_eq!(app.diff.document.as_ref().map(|d| d.files.len()), Some(3), "stale reply dropped");
    }

    #[test]
    fn picker_enter_jumps_to_the_selected_files_section() {
        let (mut app, _ends) = test_app();
        app.diff.open = true;
        app.focus = Focus::Diff;
        app.diff.target = Some(DiffTarget::Commit(oid(7)));
        app.diff.view_height = 10;
        app.diff.document = Some(commit_doc_with_sections());

        app.handle_input(key(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(app.diff.file_picker, Some(0), "picker starts at the viewport file");

        app.handle_input(key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.diff.file_picker, Some(1));

        app.handle_input(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.diff.file_picker.is_none(), "Enter exits picker mode");
        assert_eq!(app.diff.scroll, 22, "jumped to b.rs: offset 7 + body_start 15");
    }

    #[test]
    fn picker_jk_wraps_around_the_ends() {
        let (mut app, _ends) = test_app();
        app.diff.open = true;
        app.focus = Focus::Diff;
        app.diff.target = Some(DiffTarget::Commit(oid(7)));
        app.diff.view_height = 10;
        app.diff.document = Some(commit_doc_with_sections());

        app.handle_input(key(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(app.diff.file_picker, Some(0));

        app.handle_input(key(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.diff.file_picker, Some(2), "k at the top wraps to the last file");
        app.handle_input(key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.diff.file_picker, Some(0), "j at the bottom wraps back to the first");

        // Large jumps still clamp instead of wrapping.
        app.handle_input(key(KeyCode::Char('G'), KeyModifiers::SHIFT));
        app.handle_input(key(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(app.diff.file_picker, Some(2), "repeated G pins to the last file");
    }

    #[test]
    fn picker_filter_narrows_movement_to_matching_files() {
        let (mut app, _ends) = test_app();
        app.diff.open = true;
        app.focus = Focus::Diff;
        app.diff.target = Some(DiffTarget::Commit(oid(7)));
        app.diff.view_height = 10;
        app.diff.document = Some(commit_doc_with_sections());

        app.handle_input(key(KeyCode::Char('t'), KeyModifiers::NONE));
        app.handle_input(key(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(app.diff.picker_filter.active, "/ enters filter input");

        // "a" matches a.rs and capped.rs but not b.rs.
        app.handle_input(key(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(app.diff.picker_filter.matches, vec![0, 2]);
        assert_eq!(app.diff.file_picker, Some(0), "cursor snaps to the first match at-or-after it");

        app.handle_input(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.diff.picker_filter.active, "Enter keeps the filter and returns to navigation");
        assert_eq!(app.diff.file_picker, Some(0));

        app.handle_input(key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.diff.file_picker, Some(2), "j skips the non-matching b.rs");
        app.handle_input(key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.diff.file_picker, Some(0), "movement wraps within the matches");

        // Esc peels the filter first, then the picker.
        app.handle_input(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.diff.file_picker.is_some(), "first Esc only clears the filter");
        assert!(app.diff.picker_filter.query.is_empty());
        app.handle_input(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.diff.file_picker.is_none(), "second Esc closes the picker");
    }

    #[test]
    fn picker_filter_enter_jumps_and_clears_the_filter() {
        let (mut app, _ends) = test_app();
        app.diff.open = true;
        app.focus = Focus::Diff;
        app.diff.target = Some(DiffTarget::Commit(oid(7)));
        app.diff.view_height = 10;
        app.diff.document = Some(commit_doc_with_sections());

        app.handle_input(key(KeyCode::Char('t'), KeyModifiers::NONE));
        app.handle_input(key(KeyCode::Char('/'), KeyModifiers::NONE));
        app.handle_input(key(KeyCode::Char('b'), KeyModifiers::NONE));
        assert_eq!(app.diff.file_picker, Some(1), "typing jumps the cursor onto b.rs");

        app.handle_input(key(KeyCode::Enter, KeyModifiers::NONE));
        app.handle_input(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.diff.file_picker.is_none());
        assert!(app.diff.picker_filter.query.is_empty(), "activation drops the filter with the picker");
        assert_eq!(app.diff.scroll, 22, "jumped to b.rs: offset 7 + body_start 15");
    }

    #[test]
    fn picker_falls_through_to_file_view_when_no_section_exists() {
        let (mut app, ends) = test_app();
        app.diff.open = true;
        app.focus = Focus::Diff;
        app.diff.target = Some(DiffTarget::Commit(oid(7)));
        app.diff.view_height = 10;
        app.diff.document = Some(commit_doc_with_sections());

        app.handle_input(key(KeyCode::Char('t'), KeyModifiers::NONE));
        app.handle_input(key(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(app.diff.file_picker, Some(2), "G lands on the last file");

        // capped.rs has no body section — Enter opens the single-file
        // view instead of jumping nowhere.
        app.handle_input(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.diff.file_picker.is_none());
        assert!(matches!(
            ends.req_rx.try_recv(),
            Ok(GitReq::Inspect(InspectReq::LoadFileDiff { path, .. })) if path == "capped.rs"
        ));
    }

    #[test]
    fn picker_o_requests_the_selected_file_view() {
        let (mut app, ends) = test_app();
        app.diff.open = true;
        app.focus = Focus::Diff;
        app.diff.target = Some(DiffTarget::Commit(oid(7)));
        app.diff.view_height = 10;
        app.diff.document = Some(commit_doc_with_sections());

        app.handle_input(key(KeyCode::Char('t'), KeyModifiers::NONE));
        app.handle_input(key(KeyCode::Char('j'), KeyModifiers::NONE));
        app.handle_input(key(KeyCode::Char('o'), KeyModifiers::NONE));

        assert!(app.diff.file_picker.is_none());
        assert!(matches!(
            ends.req_rx.try_recv(),
            Ok(GitReq::Inspect(InspectReq::LoadFileDiff { path, .. })) if path == "b.rs"
        ));
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
    fn refs_view_opens_requests_and_enter_reroots() {
        use crate::model::RefEntry;
        let (mut app, ends) = test_app();

        app.handle_input(key(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(app.refs.open && app.refs.loading);
        assert!(
            matches!(ends.req_rx.try_recv(), Ok(GitReq::Inspect(InspectReq::LoadRefs))),
            "opening the refs view requests a fresh listing"
        );

        app.apply_inspect_msg(InspectMsg::RefsListLoaded(vec![
            RefEntry {
                name: "main".into(),
                kind: RefKind::LocalBranch,
                target: oid(1),
                summary: "tip".into(),
                authored_relative: "1d ago".into(),
            },
            RefEntry {
                name: "v1.0".into(),
                kind: RefKind::Tag,
                target: oid(2),
                summary: "release".into(),
                authored_relative: "9d ago".into(),
            },
        ]));
        assert!(!app.refs.loading);
        assert_eq!(app.refs.entries.len(), 2);

        // Navigate to the tag and activate it.
        app.handle_input(key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.refs.selected, 1);
        let gen_before = app.walk_gen;
        app.handle_input(key(KeyCode::Enter, KeyModifiers::NONE));

        assert!(!app.refs.open, "enter closes the refs view");
        assert_eq!(app.branch_name, "v1.0");
        assert_eq!(app.walk_gen, gen_before + 1);
        let mut saw_reroot = false;
        while let Ok(req) = ends.req_rx.try_recv() {
            if let GitReq::History(HistoryReq::Reload { generation, root }) = req {
                assert_eq!(generation, app.walk_gen);
                assert_eq!(root, Some(oid(2)), "reload is rooted at the selected ref's commit");
                saw_reroot = true;
            }
        }
        assert!(saw_reroot);
    }

    #[test]
    fn refs_move_clamps_and_scrolls() {
        use crate::model::RefEntry;
        let (mut app, _ends) = test_app();
        app.refs.entries = (0..10u8)
            .map(|i| RefEntry {
                name: format!("branch-{i}").into(),
                kind: RefKind::LocalBranch,
                target: oid(i),
                summary: String::new(),
                authored_relative: "".into(),
            })
            .collect();
        app.refs.view_height = 3;

        app.refs_move(-5);
        assert_eq!(app.refs.selected, 0, "clamped at top");
        app.refs_move(isize::MAX);
        assert_eq!(app.refs.selected, 9, "G clamps to last entry");
        assert_eq!(app.refs.scroll, 7, "selection kept visible in a 3-row pane");
        app.refs_move(-9);
        assert_eq!(app.refs.selected, 0);
        assert_eq!(app.refs.scroll, 0);
    }

    fn blame_doc(path: &str, at: gix::ObjectId, commits: &[u8]) -> crate::model::BlameDocument {
        crate::model::BlameDocument {
            path: path.to_string(),
            at,
            lines: commits
                .iter()
                .enumerate()
                .map(|(i, &c)| crate::model::BlameLine {
                    commit_id: oid(c),
                    commit_short: format!("{c:07x}").into(),
                    author: "a".into(),
                    authored_relative: "1d ago".into(),
                    line_no: i as u32 + 1,
                    text: format!("line {i}"),
                    source_path: None,
                })
                .collect(),
        }
    }

    #[test]
    fn blame_loaded_gates_on_seq_and_resets_cursor() {
        let (mut app, _ends) = test_app();
        app.blame.open = true;
        app.blame.loading = true;
        app.blame_seq = 2;

        app.apply_inspect_msg(InspectMsg::BlameLoaded { seq: 1, result: Ok(blame_doc("f", oid(9), &[1])) });
        assert!(app.blame.document.is_none(), "stale-seq blame dropped");
        assert!(app.blame.loading, "still waiting for the current request");

        app.apply_inspect_msg(InspectMsg::BlameLoaded { seq: 2, result: Ok(blame_doc("f", oid(9), &[1, 2])) });
        assert!(!app.blame.loading);
        assert_eq!(app.blame.document.as_ref().map(|d| d.lines.len()), Some(2));
    }

    #[test]
    fn blame_enter_opens_the_lines_commit_diff() {
        let (mut app, ends) = test_app();
        app.log.rows.push(LogRow::Commit(commit_record(5, "target", "a")));
        app.blame.open = true;
        app.blame.document = Some(blame_doc("f", oid(9), &[4, 5]));
        app.blame.selected = 1;

        app.blame_open_commit();

        assert!(!app.blame.open, "enter leaves the blame view");
        assert_eq!(app.log.selected, 1, "the commit is selected in the log when indexed");
        assert!(app.diff.open);
        assert_eq!(app.diff.target, Some(DiffTarget::Commit(oid(5))));
        assert!(matches!(
            ends.req_rx.try_recv(),
            Ok(GitReq::Inspect(InspectReq::LoadDiff { target: DiffTarget::Commit(id), .. })) if id == oid(5)
        ));
    }

    #[test]
    fn reblame_history_commits_only_on_success() {
        use crate::git::BlameAt;
        let (mut app, ends) = test_app();
        app.blame.open = true;
        app.blame.document = Some(blame_doc("f.txt", oid(9), &[4]));

        app.blame_reblame_at_parent();
        assert!(app.blame.history.is_empty(), "push is staged, not committed");
        assert!(matches!(
            ends.req_rx.try_recv(),
            Ok(GitReq::Inspect(InspectReq::LoadBlame { at: BlameAt::ParentOfCommit(id), .. })) if id == oid(4)
        ));

        // Failure (e.g. root commit): staged entry discarded.
        app.apply_inspect_msg(InspectMsg::BlameLoaded { seq: app.blame_seq, result: Err("no parent".into()) });
        assert!(app.blame.history.is_empty());
        assert!(app.blame.error.is_some());
        assert!(app.blame.document.is_some(), "previous document stays visible under the error");

        // Success: staged entry lands on the Backspace trail.
        app.blame_reblame_at_parent();
        app.apply_inspect_msg(InspectMsg::BlameLoaded {
            seq: app.blame_seq,
            result: Ok(blame_doc("f.txt", oid(3), &[3])),
        });
        assert_eq!(app.blame.history.len(), 1);
        assert_eq!(app.blame.history[0], (oid(9), "f.txt".to_string()));

        // Backspace requests the recorded origin.
        app.blame_back();
        let mut saw = false;
        while let Ok(req) = ends.req_rx.try_recv() {
            if let GitReq::Inspect(InspectReq::LoadBlame { at: BlameAt::Commit(id), path, .. }) = req {
                assert_eq!(id, oid(9));
                assert_eq!(path, "f.txt");
                saw = true;
            }
        }
        assert!(saw);
        assert!(app.blame.history.is_empty());
    }

    #[test]
    fn blame_from_diff_pane_picks_section_under_viewport() {
        use crate::model::FileSection;
        let (mut app, ends) = test_app();
        app.diff.open = true;
        app.focus = Focus::Diff;
        let mut doc = working_tree_doc_with_anchor();
        doc.target = DiffTarget::Commit(oid(7));
        doc.untracked_anchor = None;
        doc.body = (0..30).map(|i| DiffLine::new(DiffLineKind::Context, format!("l{i}"))).collect();
        doc.sections = vec![
            FileSection { path: "first.rs".into(), body_start: 0 },
            FileSection { path: "second.rs".into(), body_start: 15 },
        ];
        app.diff.target = Some(DiffTarget::Commit(oid(7)));
        app.diff.document = Some(doc);
        // header len 1, no diffstat → viewport top at virtual line 20 =
        // body index 19 → inside second.rs's section.
        app.diff.scroll = 20;

        app.blame_from_diff_pane();

        assert!(app.blame.open);
        assert!(matches!(
            ends.req_rx.try_recv(),
            Ok(GitReq::Inspect(InspectReq::LoadBlame { at: crate::git::BlameAt::Commit(id), path, .. }))
                if id == oid(7) && path == "second.rs"
        ));
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
