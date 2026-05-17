//! Type protocol for the history-walking side of the git worker.
//!
//! Owns: the commit walk, ref enumeration, pathspec filtering. Doesn't
//! produce diffs, status, or working-tree metadata refreshes.
//!
//! Runtime is still a single worker thread that handles both this and the
//! inspect protocol; the split lives only in the type system today.

use crate::model::{CommitRecord, RefLabel, RepoInfo};
use gix::ObjectId;
use std::collections::HashMap;

/// Requests the history side answers. There is intentionally no
/// `LoadMore` — the worker streams batches continuously in the
/// background until the walk is exhausted, so the app never has to
/// prompt for more.
pub enum HistoryReq {
    /// Discard the current walk and start over from HEAD with a fresh
    /// generation. Used after the user presses `R`.
    Reload,
}

/// Replies the history side emits. Each generation-tagged variant is
/// dropped by the app if it predates the current `walk_gen` — the
/// stale-message defence.
pub enum HistoryMsg {
    RepoInfo(RepoInfo),
    Commits {
        generation: u64,
        commits: Vec<CommitRecord>,
    },
    /// Deferred ref labels — sent after the first commit batch so the UI
    /// can backfill branch/tag decorations without blocking startup.
    RefsLoaded {
        generation: u64,
        refs_map: HashMap<ObjectId, Vec<RefLabel>>,
    },
    WalkDone {
        generation: u64,
    },
    Error(String),
}
