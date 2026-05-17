//! Plain-data app state. No methods that drive the worker or the UI
//! live here — those stay on `App` in `mod.rs`. Inputs to the
//! renderer in `crate::ui` are the `LogState` / `DiffState` /
//! `StatusState` snapshots produced from these structs.

use crate::{
    app::search::cycle,
    model::{CommitRecord, DiffDocument, DiffTarget, StatusDocument},
};
use compact_str::CompactString;
use std::time::Instant;

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
