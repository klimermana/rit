//! Type protocol for the history-walking side of the git worker.
//!
//! Owns: the commit walk, ref enumeration, pathspec filtering. Doesn't
//! produce diffs, status, or working-tree metadata refreshes.
//!
//! Runtime is still a single worker thread that handles both this and the
//! inspect protocol; the split lives only in the type system today.

use crate::model::{CommitRecord, RefLabel, RepoInfo};
use gix::ObjectId;
use std::{collections::HashMap, sync::Arc};

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
    /// Shared via `Arc` so the walker (which keeps its own clone for the
    /// next batch's build_commit_info) and this message don't duplicate
    /// the map for ref-heavy repos.
    ///
    /// `first_batch_rows` is the count of commit rows the app received
    /// before refs were live — i.e. the rows that need backfill. Every
    /// commit after that index was built with the refs already loaded.
    /// On a 100k-commit repo this turns the backfill loop from
    /// O(all-rows) into O(first-batch) ≈ O(64).
    RefsLoaded {
        generation: u64,
        refs_map: Arc<HashMap<ObjectId, Vec<RefLabel>>>,
        first_batch_rows: usize,
    },
    WalkDone {
        generation: u64,
    },
    Error(String),
}
