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
    /// Raw author-time epoch seconds. Currently unused by rendering but
    /// stored so callers don't have to re-decode the commit. Kept as a
    /// `pub` field so future consumers don't have to re-add it.
    pub authored_unix_secs: i64,
    /// Pre-formatted relative timestamp ("2d ago").
    pub authored_relative: CompactString,
    /// Author name as displayed. Currently truncated by the history worker
    /// at AUTHOR_DISPLAY_CHARS; a later commit moves the truncation into
    /// the UI layer so search can use the full name.
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
    /// Space-joined, lowercased names of every `RefKind::Tag` attached to
    /// the commit. Empty for the common case of an untagged commit, and
    /// rebuilt by `RefsLoaded` backfill when refs land after the commit
    /// row was first emitted.
    pub tags_lower: CompactString,
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
