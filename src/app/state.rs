//! Plain-data app state. No methods that drive the worker or the UI
//! live here — those stay on `App` in `mod.rs`. Inputs to the
//! renderer in `crate::ui` are the `LogState` / `DiffState` /
//! `StatusState` snapshots produced from these structs.

use crate::{
    app::search::cycle,
    model::{BlameDocument, CommitRecord, DiffDocument, DiffTarget, RefEntry, StatusDocument},
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

#[expect(
    clippy::large_enum_variant,
    reason = "the WorkingTree variant occurs exactly once (index 0, see commits_len) so no per-row \
              memory is wasted, and boxing CommitRecord would add a pointer chase to every commit \
              row on the per-frame render and search hot paths"
)]
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

/// Read-only view of either search's pageable bits, used by the status-bar
/// renderer so it doesn't depend on the concrete state type.
pub struct SearchSnapshot<'a> {
    pub active: bool,
    pub query: &'a str,
    pub matches_len: usize,
    pub display_index: usize,
}

/// Fields shared by both panes' search state: input-mode flag, the
/// current query string, the match indices, and the cycle cursor.
pub struct SearchState {
    pub active: bool,
    pub query: String,
    pub matches: Vec<usize>,
    pub current: usize,
}

impl Default for SearchState {
    fn default() -> Self {
        Self::new()
    }
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

/// The diff pane uses bare `SearchState` — no narrowing logic.
pub type DiffSearchState = SearchState;

/// Search state for the log pane. Wraps `SearchState` with the
/// narrowing fields used by `update_commit_matches`: when the user
/// extends a query within the same walk generation, we filter the
/// prior match set instead of rescanning every row.
pub struct CommitSearchState {
    pub state: SearchState,
    pub last_query: String,
    pub last_generation: u64,
}

impl Default for CommitSearchState {
    fn default() -> Self {
        Self::new()
    }
}

impl CommitSearchState {
    pub fn new() -> Self {
        Self { state: SearchState::new(), last_query: String::new(), last_generation: 0 }
    }

    pub fn clear(&mut self) {
        self.state.clear();
        // Reset the narrowing cursor so the next type does a full rescan
        // instead of attempting to narrow a now-empty matches set.
        self.last_query.clear();
    }

    pub fn advance(&mut self, delta: isize) -> Option<usize> {
        self.state.advance(delta)
    }

    pub fn current_pos(&self) -> Option<usize> {
        self.state.current_pos()
    }

    pub fn snapshot(&self) -> SearchSnapshot<'_> {
        self.state.snapshot()
    }
}

pub struct DiffState {
    pub open: bool,
    pub target: Option<DiffTarget>,
    pub document: Option<DiffDocument>,
    pub scroll: usize,
    /// Column offset for horizontal scroll. Applied via the underlying
    /// `Paragraph::scroll` so the gutter (line numbers, +/-) shifts with
    /// the content. Reset to 0 whenever the diff document changes.
    pub horizontal_scroll: usize,
    pub loading: bool,
    pub show_line_numbers: bool,
    pub show_hunks: bool,
    /// `f`: limit commit diffs to the CLI pathspec. On by default so
    /// `rit <path>` opens diffs already scoped; meaningless (and the
    /// key is a no-op) when no pathspec was given.
    pub scoped: bool,
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

/// Full-screen blame view (`b` from the diff pane, or `rit blame
/// <path>`). Line-cursor based: `Enter` opens the selected line's
/// commit, `,` re-blames at its parent, Backspace pops `history`.
pub struct BlameState {
    pub open: bool,
    pub loading: bool,
    pub document: Option<BlameDocument>,
    pub selected: usize,
    pub scroll: usize,
    /// Inner height of the blame pane, updated by the renderer each
    /// frame — cursor movement clamps against it at input time.
    pub view_height: usize,
    /// Display-ready error from the last blame attempt (file not in
    /// that revision, commit has no parent, …). Cleared on request.
    pub error: Option<String>,
    /// Re-blame trail: (suspect, path) pairs pushed by `,` so
    /// Backspace can walk back to where the user came from.
    pub history: Vec<(gix::ObjectId, String)>,
    /// Staged history entry for an in-flight `,` re-blame: committed
    /// to `history` only when the request succeeds, so a failed
    /// re-blame (root commit) doesn't leave a bogus Backspace target.
    pub pending_return: Option<(gix::ObjectId, String)>,
}

impl BlameState {
    pub fn new() -> Self {
        Self {
            open: false,
            loading: false,
            document: None,
            selected: 0,
            scroll: 0,
            view_height: 1,
            error: None,
            history: Vec::new(),
            pending_return: None,
        }
    }
}

impl Default for BlameState {
    fn default() -> Self {
        Self::new()
    }
}

/// Full-screen refs browser (`r`). `Enter` re-roots the log at the
/// selected ref.
pub struct RefsState {
    pub open: bool,
    pub loading: bool,
    pub entries: Vec<RefEntry>,
    pub selected: usize,
    pub scroll: usize,
    /// Inner height of the refs pane, updated by the renderer each
    /// frame — selection movement clamps against it at input time.
    pub view_height: usize,
}

impl RefsState {
    pub fn new() -> Self {
        Self { open: false, loading: false, entries: Vec::new(), selected: 0, scroll: 0, view_height: 1 }
    }
}

impl Default for RefsState {
    fn default() -> Self {
        Self::new()
    }
}

pub struct YankFeedback {
    pub text: String,
    pub shown_at: Instant,
}

/// How the log view renders the date column. Cycled with `D`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DateMode {
    /// "2d ago" — the default.
    Relative,
    /// "2026-07-03 14:05", rendered from `authored_unix_secs` in the
    /// local timezone.
    Absolute,
    Off,
}

impl DateMode {
    pub fn next(self) -> Self {
        match self {
            Self::Relative => Self::Absolute,
            Self::Absolute => Self::Off,
            Self::Off => Self::Relative,
        }
    }

    /// Column width including the trailing separator space; 0 = hidden.
    pub fn col_width(self) -> usize {
        match self {
            Self::Relative => 8,
            Self::Absolute => 16,
            Self::Off => 0,
        }
    }
}

/// How the log view renders the author column. Cycled with `A`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AuthorMode {
    /// 20-char column — the default.
    Full,
    /// 10-char column.
    Abbrev,
    Off,
}

impl AuthorMode {
    pub fn next(self) -> Self {
        match self {
            Self::Full => Self::Abbrev,
            Self::Abbrev => Self::Off,
            Self::Off => Self::Full,
        }
    }

    pub fn col_width(self) -> usize {
        match self {
            Self::Full => 20,
            Self::Abbrev => 10,
            Self::Off => 0,
        }
    }
}

/// Render-time display toggles for the log view (tig's `D` / `A` / `X`).
/// Pure UI state — the underlying `CommitRecord` always stores the full
/// data, so cycling a mode is just a redraw.
pub struct DisplayOptions {
    pub date: DateMode,
    pub author: AuthorMode,
    /// `X`: render the full 40-char commit id instead of the short one.
    pub full_hash: bool,
}

impl Default for DisplayOptions {
    fn default() -> Self {
        Self { date: DateMode::Relative, author: AuthorMode::Full, full_hash: false }
    }
}
