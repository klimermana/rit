//! Worker thread, request envelope, and the tree-diff LRU cache that's
//! shared between the walker and the commit-diff renderer. The actual
//! work lives in submodules:
//!
//!   - `walk`   — `Walker`, `build_commit_info`, pathspec filtering
//!   - `diff`   — commit-diff rendering, `DiffSink`, per-file helpers
//!   - `status` — single-pass `gix::status` sweep + section renderers
//!   - `meta`   — `repo_info_for`, `working_tree_author`,
//!     `quick_is_dirty`, ref enumeration, time formatters
//!   - `graph`  — ASCII commit-graph state (when `--graph` is on)

pub mod diff;
pub mod graph;
pub mod history;
pub mod inspect;
pub mod meta;
pub mod status;
pub mod walk;

pub use history::{HistoryMsg, HistoryReq};
pub use inspect::{InspectMsg, InspectReq};

use crate::model::PathFilter;
use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use gix::ObjectId;
use std::{collections::HashMap, sync::Arc};

/// Cutoffs that keep `compute_commit_diff` responsive on pathological
/// commits. Anything past these limits gets a one-line summary instead of
/// a fully inlined hunk-by-hunk diff; the `DiffFlags` on the resulting
/// `DiffDocument` reports what was skipped so the UI can surface it.
pub const MAX_INLINE_DIFF_BYTES: usize = 256 * 1024;
pub const MAX_INLINE_DIFF_LINES: usize = 20_000;
pub const MAX_INLINE_DIFF_FILES: usize = 200;

/// Upper bound on the pathspec → commit-diff cache. 64 entries are
/// enough to absorb a few seconds of scrollback while the user
/// navigates a pathspec-filtered log; the eviction policy is LRU.
const TREE_DIFF_CACHE_CAP: usize = 64;

/// Key for `TreeDiffCache` entries: `(parent_oid, commit_oid)`, where
/// `parent_oid` is `None` for root commits diffed against an empty tree.
type TreeDiffKey = (Option<ObjectId>, ObjectId);
type TreeDiffRecords = Vec<gix::diff::tree::recorder::Change>;

/// Per-pair LRU cache of `gix::diff::tree` records, shared between the
/// pathspec filter (which runs a tree diff to decide whether to keep
/// each walked commit) and `compute_commit_diff_inner` (which runs the
/// same diff when the user opens that commit). Since commit / parent
/// oids are content-addressed, cache entries never become stale — only
/// evicted when the cache fills.
pub struct TreeDiffCache {
    entries: Vec<(TreeDiffKey, TreeDiffRecords)>,
}

impl TreeDiffCache {
    pub fn new() -> Self {
        Self { entries: Vec::with_capacity(TREE_DIFF_CACHE_CAP) }
    }

    /// Look up a cached records vec. Bumps the entry to MRU on hit so
    /// the next eviction takes the genuinely least-recently-used item.
    fn get(&mut self, key: &TreeDiffKey) -> Option<&[gix::diff::tree::recorder::Change]> {
        let pos = self.entries.iter().position(|(k, _)| k == key)?;
        let entry = self.entries.remove(pos);
        self.entries.push(entry);
        self.entries.last().map(|(_, v)| v.as_slice())
    }

    fn insert(&mut self, key: TreeDiffKey, value: TreeDiffRecords) {
        if self.entries.len() >= TREE_DIFF_CACHE_CAP {
            self.entries.remove(0);
        }
        self.entries.push((key, value));
    }
}

impl Default for TreeDiffCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Look up (or compute and cache) the `gix::diff::tree` records for a
/// commit against its first parent (or the empty tree for root
/// commits). The cached vec is returned by slice — callers iterate
/// it in place without taking ownership.
pub fn compute_tree_diff_records<'c>(
    repo: &gix::Repository,
    parent_id: Option<ObjectId>,
    commit_id: ObjectId,
    cache: &'c mut TreeDiffCache,
) -> Result<&'c [gix::diff::tree::recorder::Change]> {
    use gix::objs::TreeRefIter;

    let key = (parent_id, commit_id);
    // Borrow checker doesn't like `if let Some(...) = cache.get(...)`
    // followed by an else-branch that also touches `cache`, so
    // check-then-act through a sentinel.
    if cache.get(&key).is_some() {
        return Ok(cache.get(&key).unwrap_or(&[]));
    }

    let cur_commit = repo.find_object(commit_id)?.try_into_commit()?;
    let cur_tree = cur_commit.tree()?;
    let par_tree = match parent_id {
        None => repo.empty_tree(),
        Some(parent) => repo.find_object(parent)?.try_into_commit()?.tree()?,
    };

    let hash_kind = repo.object_hash();
    let mut recorder = gix::diff::tree::Recorder::default();
    gix::diff::tree(
        TreeRefIter::from_bytes(&par_tree.data, hash_kind),
        TreeRefIter::from_bytes(&cur_tree.data, hash_kind),
        gix::diff::tree::State::default(),
        &repo.objects,
        &mut recorder,
    )?;

    cache.insert(key, recorder.records);
    Ok(cache.get(&key).unwrap_or(&[]))
}

/// Combined request envelope so the app can keep one channel pair to the
/// (currently single) worker thread, while the type system still shows
/// which side -- history or inspect -- owns each operation.
pub enum GitReq {
    History(HistoryReq),
    Inspect(InspectReq),
}

/// Combined reply envelope from the worker. The app's `apply_msg`
/// dispatches on the outer variant then the inner variant.
pub enum GitMsg {
    History(HistoryMsg),
    Inspect(InspectMsg),
}

pub fn run_git_thread(
    req_rx: Receiver<GitReq>,
    msg_tx: Sender<GitMsg>,
    path_filter: Option<PathFilter>,
    graph_enabled: bool,
) {
    if let Err(e) = run_git_thread_inner(req_rx, msg_tx.clone(), path_filter, graph_enabled) {
        _ = msg_tx.send(GitMsg::History(HistoryMsg::Error(format!("git worker died: {e}"))));
    }
}

fn run_git_thread_inner(
    req_rx: Receiver<GitReq>,
    msg_tx: Sender<GitMsg>,
    path_filter: Option<PathFilter>,
    graph_enabled: bool,
) -> Result<()> {
    let repo = match gix::discover(".") {
        Ok(r) => r,
        Err(e) => {
            _ = msg_tx.send(GitMsg::History(HistoryMsg::Error(format!("Failed to open repo: {e}"))));
            return Ok(());
        }
    };

    _ = msg_tx.send(GitMsg::History(HistoryMsg::RepoInfo(meta::repo_info_for(&repo))));
    _ = msg_tx.send(GitMsg::Inspect(InspectMsg::WorkingTreeMeta {
        author: meta::working_tree_author(&repo),
        dirty: meta::quick_is_dirty(&repo),
    }));

    let mut walker = walk::Walker::new(&repo, path_filter.clone(), 0, graph_enabled, Arc::new(HashMap::new()))?;
    let mut refs_loaded = false;
    // Per-worker shared cache: the pathspec filter populates it during
    // walking; LoadDiff requests for the same commit reuse the records.
    let mut tree_diff_cache = TreeDiffCache::new();

    // Self-paced indexing loop:
    //   1. Drain any pending requests (non-blocking) so Reload and inspect
    //      operations preempt indexing instead of waiting for the walk to
    //      finish.
    //   2. If the walk isn't done, emit one batch and loop.
    //   3. Otherwise block waiting for the next request.
    //
    // The first batch is small for fast first paint; subsequent batches
    // are page-sized.
    const INITIAL_BATCH: usize = 64;
    const PAGE_BATCH: usize = 256;

    loop {
        // Drain any queued requests first.
        loop {
            match req_rx.try_recv() {
                Ok(req) => {
                    if !process_request(
                        req,
                        &repo,
                        &path_filter,
                        graph_enabled,
                        &mut walker,
                        &mut refs_loaded,
                        &mut tree_diff_cache,
                        &msg_tx,
                    )? {
                        return Ok(());
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        if !walker.done {
            let n = if refs_loaded { PAGE_BATCH } else { INITIAL_BATCH };
            let emitted = walker.load_more(n, &mut tree_diff_cache, &msg_tx)?;
            // After the first batch, load refs and backfill so branch/tag
            // decorations appear without blocking startup.
            if !refs_loaded {
                refs_loaded = true;
                let refs_map = Arc::new(meta::load_refs(&repo));
                walker.refs_map = Arc::clone(&refs_map);
                _ = msg_tx.send(GitMsg::History(HistoryMsg::RefsLoaded {
                    generation: walker.generation,
                    refs_map,
                    first_batch_rows: emitted,
                }));
            }
            continue;
        }

        // Indexing finished — block for the next request.
        match req_rx.recv() {
            Ok(req) => {
                if !process_request(
                    req,
                    &repo,
                    &path_filter,
                    graph_enabled,
                    &mut walker,
                    &mut refs_loaded,
                    &mut tree_diff_cache,
                    &msg_tx,
                )? {
                    return Ok(());
                }
            }
            Err(_) => return Ok(()),
        }
    }
}

/// Process one request. Returns `Ok(false)` if the loop should exit,
/// `Ok(true)` to continue. Kept as a free function so both the
/// try_recv-drain and the blocking-recv branches share it.
#[expect(
    clippy::too_many_arguments,
    reason = "single worker-loop callsite; all params are intrinsic state of the worker, no struct would clarify"
)]
fn process_request<'r>(
    req: GitReq,
    repo: &'r gix::Repository,
    path_filter: &Option<PathFilter>,
    graph_enabled: bool,
    walker: &mut walk::Walker<'r>,
    refs_loaded: &mut bool,
    tree_diff_cache: &mut TreeDiffCache,
    msg_tx: &Sender<GitMsg>,
) -> Result<bool> {
    match req {
        GitReq::History(HistoryReq::Reload) => {
            let next_gen = walker.generation.wrapping_add(1);
            let refs_map = Arc::new(meta::load_refs(repo));
            // The new Walker's `'r` lifetime ties to `repo`, same as
            // the caller's walker — overwriting in place is fine.
            // The tree-diff cache survives reload: oids are
            // content-addressed so entries from the prior generation
            // are still valid.
            *walker = walk::Walker::new(repo, path_filter.clone(), next_gen, graph_enabled, refs_map)?;
            *refs_loaded = true;
        }
        GitReq::Inspect(InspectReq::LoadDiff(target)) => {
            use crate::model::DiffTarget;
            let document = match target {
                DiffTarget::Commit(id) => diff::compute_commit_diff(repo, id, tree_diff_cache),
                DiffTarget::WorkingTree => status::compute_working_tree_diff(repo, target),
            };
            _ = msg_tx.send(GitMsg::Inspect(InspectMsg::DiffLoaded(document)));
        }
        GitReq::Inspect(InspectReq::LoadStatus) => {
            let document = status::compute_status(repo);
            _ = msg_tx.send(GitMsg::Inspect(InspectMsg::StatusLoaded(document)));
        }
        GitReq::Inspect(InspectReq::RefreshWorkingTreeMeta) => {
            _ = msg_tx.send(GitMsg::Inspect(InspectMsg::WorkingTreeMeta {
                author: meta::working_tree_author(repo),
                dirty: meta::quick_is_dirty(repo),
            }));
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::meta::{quick_is_dirty, relative_time};
    use crate::test_support::{commit_all, make_fixture_repo, write_file};

    #[test]
    fn relative_time_clamps_future_dated_to_now() {
        let far_future = chrono::Utc::now().timestamp() + 86400 * 365 * 10;
        assert_eq!(relative_time(far_future).as_str(), "now");
    }

    #[test]
    fn relative_time_recent_past_renders_relative() {
        let recent = chrono::Utc::now().timestamp() - 120;
        assert_eq!(relative_time(recent).as_str(), "2m ago");
    }

    #[test]
    fn quick_is_dirty_reports_clean_then_dirty() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();

        write_file(path, "tracked.txt", "hi\n");
        commit_all(path, "baseline");
        assert_eq!(quick_is_dirty(&repo), Some(false), "freshly committed worktree should be clean");

        write_file(path, "tracked.txt", "hi\nthere\n");
        assert_eq!(quick_is_dirty(&repo), Some(true), "tracked-file mod should flip to dirty");

        commit_all(path, "second");
        assert_eq!(quick_is_dirty(&repo), Some(false), "after commit, back to clean");
        std::fs::write(path.join("new_untracked.txt"), "x").expect("write");
        assert_eq!(quick_is_dirty(&repo), Some(true), "untracked file should count as dirty");
    }
}
