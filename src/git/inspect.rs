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
    LoadDiff(DiffTarget),
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
    DiffLoaded(DiffDocument),
    StatusLoaded(StatusDocument),
    WorkingTreeMeta {
        author: String,
        /// `None` when the dirty check failed (e.g. no worktree or status
        /// query errored); the UI keeps its previous indicator in that
        /// case. `Some(true)` if anything (staged / unstaged / untracked)
        /// differs from HEAD; `Some(false)` for a clean tree.
        dirty: Option<bool>,
    },
}
