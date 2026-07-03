//! Type protocol for the inspect side of the git worker (commit diffs,
//! working-tree status, and working-tree metadata refresh).
//!
//! Owns: per-commit / working-tree diff payload assembly, the
//! `git status --short`-equivalent rendering, and the
//! working-tree-author lookup. Doesn't walk history.
//!
//! Runtime is still a single worker thread shared with the history
//! protocol; the split lives only in the type system today.

use crate::model::{BlameDocument, DiffDocument, DiffTarget, RefEntry, StatusDocument};

/// Requests the inspect side answers.
pub enum InspectReq {
    /// `seq` is the app's monotonically increasing diff-request counter.
    /// It is echoed back on `DiffLoaded` and `UntrackedFilesUpdate` so
    /// the app can drop results that a newer request has superseded —
    /// target equality alone can't distinguish "result for the current
    /// request" from "stale result for an identical earlier target"
    /// (e.g. reopening the working-tree diff while the previous
    /// untracked scan is still in flight).
    LoadDiff {
        target: DiffTarget,
        seq: u64,
    },
    LoadStatus,
    /// Build the refs-view listing: every branch and tag peeled to its
    /// commit, with summary + relative date. Requested each time the
    /// user opens the refs view so the listing reflects refs created
    /// since startup.
    LoadRefs,
    /// Re-read the configured author name (e.g. after the user changes
    /// `user.name` in another terminal). Currently triggered alongside
    /// `Reload` from the app.
    RefreshWorkingTreeMeta,
    /// Annotate `path` with gix-blame. Runs on a worker side thread
    /// (blame walks history and can take seconds on long-lived files)
    /// so the walk and diff requests stay live; `seq` gates stale
    /// replies exactly like `LoadDiff`.
    LoadBlame {
        path: String,
        at: BlameAt,
        seq: u64,
    },
}

/// Which revision a blame runs against. The worker resolves `Head` and
/// `ParentOfCommit` so the app never needs its own rev-parse access.
#[derive(Clone, Copy, Debug)]
pub enum BlameAt {
    Head,
    Commit(gix::ObjectId),
    /// Re-blame at the first parent of this commit (`,` on a blame
    /// line). Errors if the commit has no parent.
    ParentOfCommit(gix::ObjectId),
}

/// Replies the inspect side emits. (An `Error` variant will land with the
/// gix::status migration; today, the diff/status producers inline error
/// content into their documents directly so there are no inspect-side
/// errors to surface.)
pub enum InspectMsg {
    /// `seq` echoes the `LoadDiff` request this document answers.
    DiffLoaded {
        seq: u64,
        document: DiffDocument,
    },
    StatusLoaded(StatusDocument),
    /// Reply to `LoadRefs`: sorted (branches, remotes, tags; name order
    /// within kind) entries for the refs view.
    RefsListLoaded(Vec<RefEntry>),
    /// Reply to `LoadBlame`. `Err` carries a display-ready message
    /// (file not found at that revision, commit has no parent, …).
    BlameLoaded {
        seq: u64,
        result: Result<BlameDocument, String>,
    },
    WorkingTreeMeta {
        author: String,
        /// `None` when the dirty check failed (e.g. no worktree or status
        /// query errored); the UI keeps its previous indicator in that
        /// case. `Some(true)` if anything (staged / unstaged / untracked)
        /// differs from HEAD; `Some(false)` for a clean tree.
        dirty: Option<bool>,
    },
    /// Follow-up to a working-tree `LoadDiff` once the off-thread
    /// untracked-files walk completes. The app splices `paths` into the
    /// current `DiffDocument` at its `untracked_anchor`. Carries the
    /// originating request's `seq` (and `target` as defense in depth) so
    /// the app can ignore stale results — without the seq, a scan
    /// spawned by an *earlier* working-tree LoadDiff could consume the
    /// anchor of a newer document and the fresh scan's result would then
    /// be dropped as a no-op.
    UntrackedFilesUpdate {
        target: DiffTarget,
        seq: u64,
        paths: Vec<String>,
    },
}
