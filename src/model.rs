//! Shared domain types for rit. No rendering / no ratatui imports —
//! everything in here is meant to be consumable by both the git worker(s)
//! and the UI layer.

use compact_str::CompactString;
use gix::ObjectId;

/// Repository identity surfaced in the title bar.
pub struct RepoInfo {
    pub name: String,
    pub branch: String,
}

/// Wraps the path/pathspec passed on the command line. Currently kept as a
/// raw string and consumed by the history walker's substring filter; the
/// shape exists so the pathspec switchover can change semantics without
/// further re-typing the plumbing.
#[derive(Clone)]
pub struct PathFilter {
    pub raw: String,
}

impl PathFilter {
    pub fn new(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

/// One indexed commit. `author` / `summary` are display-oriented; the
/// `search` projection holds the lowercased text used for matching so the
/// rendering and search code can evolve independently.
pub struct CommitRecord {
    pub id: ObjectId,
    pub short_id: CompactString,
    /// Raw author-time epoch seconds. Rendered by the log view's
    /// absolute date mode (`D`) without re-decoding the commit.
    pub authored_unix_secs: i64,
    /// Pre-formatted relative timestamp ("2d ago").
    pub authored_relative: CompactString,
    /// Full author name; the UI truncates to the active column width at
    /// render time so search can match substrings past the display cap.
    pub author: CompactString,
    pub summary: String,
    pub refs: Vec<RefLabel>,
    /// ASCII graph prefix. Empty unless the `--graph` CLI flag was passed —
    /// the renderer only emits a graph column when this is non-empty.
    pub graph: CompactString,
    pub search: CommitSearchText,
}

/// Lowercased projections of a commit's searchable fields.
pub struct CommitSearchText {
    pub author_lower: CompactString,
    pub summary_lower: String,
    /// Space-joined, lowercased names of every `RefLabel` attached to the
    /// commit — tags, local branches, remote branches, and HEAD alike.
    /// Empty for the common case of a refless commit, and rebuilt by
    /// `RefsLoaded` backfill when refs land after the commit row was
    /// first emitted.
    pub refs_lower: CompactString,
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

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum DiffTarget {
    Commit(ObjectId),
    WorkingTree,
}

/// A complete diff payload as produced by the inspect path. Bundling
/// header/body/diffstat/flags into one record makes the request/response
/// types simpler and gives later commits a single place to add things like
/// truncation flags or cache keys.
#[derive(Clone)]
pub struct DiffDocument {
    pub target: DiffTarget,
    pub header: Vec<DiffLine>,
    pub body: Vec<DiffLine>,
    pub files: Vec<FileStat>,
    pub stats: DiffStats,
    /// Per-document guardrail state — surfaced in the diff title bar
    /// via `truncation_tag`.
    pub flags: DiffFlags,
    /// Index in `body` of the "(scanning for untracked files…)"
    /// placeholder line. Set by `compute_working_tree_diff` when it
    /// defers the untracked walk to a side thread; consumed by the
    /// app on `InspectMsg::UntrackedFilesUpdate` to splice the actual
    /// `?? path` entries into place. `None` once consumed (so stale
    /// updates from a prior LoadDiff become no-ops) and `None` for
    /// commit diffs, which have no untracked section.
    pub untracked_anchor: Option<usize>,
    /// Per-file section starts into `body`, in render order. Drives
    /// `[` / `]` file navigation in the diff pane (and gives `b` blame
    /// the path under the viewport). Files summarised by the
    /// file-count guardrail emit no body lines and so have no section.
    pub sections: Vec<FileSection>,
}

/// One file's slice of a `DiffDocument::body`.
#[derive(Clone)]
pub struct FileSection {
    pub path: String,
    /// Index into `body` of this file's first header line.
    pub body_start: usize,
}

/// Out-of-band info about how the diff was produced — the binary /
/// oversize guardrail counts and a truncation flag the title bar
/// consumes.
#[derive(Clone, Default)]
pub struct DiffFlags {
    pub truncated: bool,
    pub skipped_binary_files: usize,
    pub skipped_large_files: usize,
}

#[derive(Clone)]
pub struct StatusDocument {
    pub lines: Vec<DiffLine>,
}

#[derive(Clone)]
pub struct DiffStats {
    pub files: usize,
    pub insertions: usize,
    pub deletions: usize,
}

#[derive(Clone)]
pub struct FileStat {
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Clone)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

impl DiffLine {
    pub fn new(kind: DiffLineKind, text: impl Into<String>) -> Self {
        Self { kind, text: text.into() }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    // Commit metadata
    CommitHeader,
    Meta,
    Message,
    Blank,
    // Per-file headers
    FileHeader,
    FileMeta,
    OldMarker,
    NewMarker,
    HunkHeader,
    // Diff bodies
    Add,
    Del,
    Context,
    // Diffstat block
    Diffstat,
    DiffstatTotal,
    // Status view
    SectionTitle,
    SectionStaged,
    SectionUnstaged,
    Faint,
    Good,
    StatusOurs,
    StatusTheirs,
}
