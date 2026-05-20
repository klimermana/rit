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
    positional: Option<String>,
    graph_enabled: bool,
) {
    if let Err(e) = run_git_thread_inner(req_rx, msg_tx.clone(), positional, graph_enabled) {
        _ = msg_tx.send(GitMsg::History(HistoryMsg::Error(format!("git worker died: {e}"))));
    }
}

fn run_git_thread_inner(
    req_rx: Receiver<GitReq>,
    msg_tx: Sender<GitMsg>,
    positional: Option<String>,
    graph_enabled: bool,
) -> Result<()> {
    let repo = match gix::discover(".") {
        Ok(r) => r,
        Err(e) => {
            _ = msg_tx.send(GitMsg::History(HistoryMsg::Error(format!("Failed to open repo: {e}"))));
            return Ok(());
        }
    };

    // Try the positional as a revision (full or short hash, branch, tag,
    // `HEAD~3`, etc.); fall back to pathspec only when rev-parse can't
    // peel it to a commit. Hash-first matches the user's expectation
    // that `rit abc1234` walks from that commit even when a path with
    // the same name happens to exist.
    let (start_id, path_filter) = resolve_positional(&repo, positional);

    _ = msg_tx.send(GitMsg::History(HistoryMsg::RepoInfo(meta::repo_info_for(&repo))));
    // Send the working-tree author immediately so the UI can paint the
    // "Not Committed Yet" row, but defer the dirty flag — `quick_is_dirty`
    // walks the full worktree, which on a wide checkout (10k+ tracked
    // files) is the single dominant cost between worker spawn and first
    // commit batch. The follow-up `WorkingTreeMeta` with the real `dirty`
    // bit is sent by `spawn_dirty_check` (kicked off *after* the first
    // batch — see the `!refs_loaded` arm of the loop below), and the
    // app's `apply_inspect_msg` (src/app/mod.rs) already keeps its prior
    // indicator across `dirty: None` updates.
    _ = msg_tx
        .send(GitMsg::Inspect(InspectMsg::WorkingTreeMeta { author: meta::working_tree_author(&repo), dirty: None }));

    let mut walker =
        walk::Walker::new(&repo, path_filter.clone(), start_id, 0, graph_enabled, Arc::new(HashMap::new()))?;
    let mut refs_loaded = false;
    // Per-worker shared cache: the pathspec filter populates it during
    // walking; LoadDiff requests for the same commit reuse the records.
    let mut tree_diff_cache = TreeDiffCache::new();
    // Side-thread `quick_is_dirty` handles. Owned here so the Drop guard
    // joins them on any worker-exit path; this keeps the bench
    // deterministic (no cross-iteration thread leakage) and is a no-op
    // in production where the worker outlives its one startup dirty
    // check by orders of magnitude.
    let mut dirty_handles = DirtyJoin(Vec::new());

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
                        start_id,
                        graph_enabled,
                        &mut walker,
                        &mut refs_loaded,
                        &mut tree_diff_cache,
                        &mut dirty_handles,
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
            // decorations appear without blocking startup. The dirty
            // check goes here too — running it on a side thread *before*
            // the first batch was emitted let its worktree scan compete
            // with the walker's commit-object reads for FS bandwidth,
            // which regressed TTFB on wide checkouts (the very case we
            // were trying to fix). Spawning it once the latency-critical
            // first batch is already on the channel lets the scan run in
            // parallel with subsequent page-sized batches at no cost to
            // first paint.
            if !refs_loaded {
                refs_loaded = true;
                dirty_handles.0.push(spawn_dirty_check(&repo, msg_tx.clone()));
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
                    start_id,
                    graph_enabled,
                    &mut walker,
                    &mut refs_loaded,
                    &mut tree_diff_cache,
                    &mut dirty_handles,
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
    reason = "single worker-loop callsite; all params are intrinsic worker state (dirty-check handles, walk root, etc.), no wrapper struct would clarify"
)]
fn process_request<'r>(
    req: GitReq,
    repo: &'r gix::Repository,
    path_filter: &Option<PathFilter>,
    start_id: Option<ObjectId>,
    graph_enabled: bool,
    walker: &mut walk::Walker<'r>,
    refs_loaded: &mut bool,
    tree_diff_cache: &mut TreeDiffCache,
    dirty_handles: &mut DirtyJoin,
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
            // are still valid. `start_id` is captured at CLI parse
            // time, so reload keeps walking from the same pinned root.
            *walker = walk::Walker::new(repo, path_filter.clone(), start_id, next_gen, graph_enabled, refs_map)?;
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
            // Mirror the startup path: hand the worktree scan off to a
            // side thread so a Reload doesn't re-introduce the same
            // pre-first-batch stall the startup change eliminates.
            dirty_handles.0.push(spawn_dirty_check(repo, msg_tx.clone()));
        }
    }
    Ok(true)
}

/// Interpret the CLI positional arg. If it rev-parses to something that
/// peels to a commit (full or short hash, branch, tag, `HEAD~3`, …)
/// it's used as the walk root and no pathspec is applied; otherwise
/// it's handed to `PathFilter` for the usual `git log -- <pathspec>`
/// behaviour. Hash-first matches the user-confirmed disambiguation:
/// when both forms could match, the commit wins.
fn resolve_positional(repo: &gix::Repository, positional: Option<String>) -> (Option<ObjectId>, Option<PathFilter>) {
    let Some(raw) = positional else {
        return (None, None);
    };
    let commit_id = repo
        .rev_parse_single(raw.as_str())
        .ok()
        .and_then(|id| id.object().ok())
        .and_then(|obj| obj.peel_to_kind(gix::object::Kind::Commit).ok())
        .map(|obj| obj.id);
    match commit_id {
        Some(id) => (Some(id), None),
        None => (None, Some(PathFilter::new(raw))),
    }
}

/// Off-thread `quick_is_dirty`. The worktree scan it performs is
/// O(tracked files) and dominates startup on wide checkouts; running it
/// in parallel with the walker keeps `quick_is_dirty` off the
/// first-batch critical path. The thread sends a `WorkingTreeMeta`
/// message with the result whenever the scan finishes, and the app's
/// `apply_inspect_msg` (src/app/mod.rs) treats `dirty: None` as
/// "preserve previous indicator" so the initial placeholder doesn't
/// flash to "clean".
///
/// Returns a `JoinHandle` so the worker can collect them in a
/// `DirtyJoin` guard — see that struct's doc for why we don't detach.
fn spawn_dirty_check(repo: &gix::Repository, msg_tx: crossbeam_channel::Sender<GitMsg>) -> std::thread::JoinHandle<()> {
    let repo = repo.clone();
    std::thread::spawn(move || {
        let author = meta::working_tree_author(&repo);
        let dirty = meta::quick_is_dirty(&repo);
        _ = msg_tx.send(GitMsg::Inspect(InspectMsg::WorkingTreeMeta { author, dirty }));
    })
}

/// Collects the worker's outstanding `spawn_dirty_check` handles and
/// joins them on drop. The reason we don't detach instead:
///
/// Detached side threads survive the worker thread's return. In
/// production that's harmless — the process exits and the OS reaps
/// everything — but the `startup` bench iterates the worker hundreds of
/// times per criterion sample, and detached dirty-check threads from
/// prior iterations end up scanning the worktree concurrently with the
/// *current* iteration's `.git/objects` reads. The FS contention
/// inflates TTFB by 50–100× at `worktree_files/10000` and was the
/// signal that triggered this guard.
///
/// Joining on the worker's exit path serializes the side threads
/// against bench iteration boundaries (criterion drops `req_tx`, the
/// worker returns, the guard waits for the in-flight scan). Production
/// behavior is unchanged: the worker only exits when the channel
/// disconnects at shutdown, at which point waiting on the side thread
/// is a no-op (it finishes long before any human-triggered quit).
struct DirtyJoin(Vec<std::thread::JoinHandle<()>>);

impl Drop for DirtyJoin {
    fn drop(&mut self) {
        for h in self.0.drain(..) {
            _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        meta::{quick_is_dirty, relative_time},
        resolve_positional,
    };
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

    #[test]
    fn resolve_positional_recognizes_full_hash() {
        let (td, repo) = make_fixture_repo();
        write_file(td.path(), "a.txt", "x\n");
        commit_all(td.path(), "first");
        let head = repo.head_id().expect("head").detach();

        let (start, path_filter) = resolve_positional(&repo, Some(head.to_string()));
        assert_eq!(start, Some(head));
        assert!(path_filter.is_none());
    }

    #[test]
    fn resolve_positional_recognizes_short_hash() {
        let (td, repo) = make_fixture_repo();
        write_file(td.path(), "a.txt", "x\n");
        commit_all(td.path(), "first");
        let head = repo.head_id().expect("head").detach();

        let short = head.to_hex_with_len(7).to_string();
        let (start, path_filter) = resolve_positional(&repo, Some(short));
        assert_eq!(start, Some(head));
        assert!(path_filter.is_none());
    }

    #[test]
    fn resolve_positional_falls_back_to_pathspec() {
        let (td, repo) = make_fixture_repo();
        write_file(td.path(), "a.txt", "x\n");
        commit_all(td.path(), "first");

        let (start, path_filter) = resolve_positional(&repo, Some("src/foo.rs".to_string()));
        assert!(start.is_none());
        assert_eq!(path_filter.as_ref().map(|p| p.as_str()), Some("src/foo.rs"));
    }

    #[test]
    fn resolve_positional_none_means_no_filter() {
        let (td, repo) = make_fixture_repo();
        write_file(td.path(), "a.txt", "x\n");
        commit_all(td.path(), "first");

        let (start, path_filter) = resolve_positional(&repo, None);
        assert!(start.is_none());
        assert!(path_filter.is_none());
    }
}
