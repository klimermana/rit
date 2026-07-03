//! Type protocol for the inspect side of the git worker (commit diffs,
//! working-tree status, and working-tree metadata refresh).
//!
//! Owns: per-commit / working-tree diff payload assembly, the
//! `git status --short`-equivalent rendering, and the
//! working-tree-author lookup. Doesn't walk history.
//!
//! Runtime is still a single worker thread shared with the history
//! protocol; the split lives only in the type system today.

use crate::model::{DiffDocument, DiffTarget, StatusDocument};

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
    /// Re-read the configured author name (e.g. after the user changes
    /// `user.name` in another terminal). Currently triggered alongside
    /// `Reload` from the app.
    RefreshWorkingTreeMeta,
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
