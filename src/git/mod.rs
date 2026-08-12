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

pub mod blame;
pub mod diff;
pub mod graph;
pub mod history;
pub mod inspect;
pub mod meta;
pub mod status;
pub mod walk;

pub use history::{HistoryMsg, HistoryReq};
pub use inspect::{BlameAt, InspectMsg, InspectReq};

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

/// Byte cap for the on-demand single-file view (`o` in the diff pane).
/// Deliberately far above `MAX_INLINE_DIFF_BYTES`: the user asked for
/// exactly one file, so only truly pathological blobs are refused.
pub const MAX_FILE_VIEW_DIFF_BYTES: usize = 8 * 1024 * 1024;

/// Floor for the worker repo's decoded-object cache. gix leaves the
/// cache **unset by default**, which makes every tree/commit lookup
/// re-decode from scratch — the history walk and its pathspec filter
/// revisit shared subtrees constantly, so a few MB here pays for
/// itself quickly.
pub const OBJECT_CACHE_BYTES: usize = 4 * 1024 * 1024;

/// Clone `repo` for rayon fan-out with the object cache stripped.
/// `gix_odb::Cache`'s Clone impl carries the cache *factory* and
/// eagerly instantiates a fresh cache per clone — so without this,
/// every `to_thread_local()` in a `map_init` builds a cache per
/// worker per render call. The per-file render paths read each blob
/// exactly once, so those caches never hit; they only add
/// construction and copy-on-miss overhead (`many_files_200`
/// regressed ~16% when the workers inherited the cache).
pub(crate) fn sync_repo_without_object_cache(repo: &gix::Repository) -> gix::ThreadSafeRepository {
    let mut clone = repo.clone();
    clone.object_cache_size(0usize);
    clone.into_sync()
}

/// Below this many changed files, the per-file diff renderers stay
/// serial. The rayon `par_iter().map_init` setup (work-stealing
/// scheduler hookup + the gix `ThreadSafeRepository` clone for each
/// worker) costs a few hundred microseconds of fixed overhead — fine
/// when amortised across many files, pure waste on a 1-or-2-file
/// commit. Picked from the `working_tree_diff/wide_checkout_5000_tracked`
/// bench, which has 2 modified files: above-threshold parallelization
/// regressed it by ~25% before this cutoff was added.
pub const PARALLEL_FILE_THRESHOLD: usize = 8;

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
    let mut repo = match gix::discover(".") {
        Ok(r) => r,
        Err(e) => {
            _ = msg_tx.send(GitMsg::History(HistoryMsg::Error(format!("Failed to open repo: {e}"))));
            return Ok(());
        }
    };
    // Size the decoded-object cache for tree diffs against this
    // checkout (gix's heuristic: ~10 MB per 10k tracked files), floored
    // at OBJECT_CACHE_BYTES so small checkouts of deep histories still
    // get useful commit/tree caching during the walk.
    let cache_bytes = repo
        .index_or_empty()
        .map(|index| repo.compute_object_cache_size_for_tree_diffs(&index))
        .unwrap_or(0)
        .max(OBJECT_CACHE_BYTES);
    repo.object_cache_size_if_unset(cache_bytes);
    let repo = repo;

    // Try the positional as a revision (full or short hash, branch, tag,
    // `HEAD~3`, etc.); fall back to pathspec only when rev-parse can't
    // peel it to a commit. Hash-first matches the user's expectation
    // that `rit abc1234` walks from that commit even when a path with
    // the same name happens to exist.
    // `walk_root` is the worker's current walk root; a Reload with
    // `root: Some(..)` re-roots (refs-view navigation), `None` keeps it.
    let (start_id, path_filter) = resolve_positional(&repo, positional);
    let mut walk_root = start_id;
    // Pathspec matcher for scoping commit diffs to the CLI path (the
    // diff pane's `f` toggle). Separate from the Walker's matcher: that
    // one decides which commits appear at all, this one filters which
    // files of an opened commit get rendered.
    let mut diff_scope = path_filter.as_ref().and_then(|pf| walk::build_pathspec_search(pf).ok());

    let mut repo_info = meta::repo_info_for(&repo);
    repo_info.path_filter = path_filter.as_ref().map(|pf| pf.as_str().to_string());
    _ = msg_tx.send(GitMsg::History(HistoryMsg::RepoInfo(repo_info)));
    // Send the working-tree author immediately so the UI can paint the
    // "Not Committed Yet" row, but defer the dirty flag — `quick_is_dirty`
    // walks the full worktree, which on a wide checkout (10k+ tracked
    // files) is the single dominant cost between worker spawn and first
    // commit batch. The follow-up `WorkingTreeMeta` with the real `dirty`
    // bit is sent by `spawn_dirty_check` (kicked off *after* the first
    // batch — see the `!refs_loaded` arm of the loop below), and the
    // app's `apply_inspect_msg` (src/app/mod.rs) already keeps its prior
    // indicator across `dirty: None` updates.
    _ = msg_tx.send(GitMsg::Inspect(InspectMsg::WorkingTreeMeta {
        author: meta::working_tree_author(&repo),
        dirty: None,
        staged: None,
    }));

    let mut walker =
        walk::Walker::new(&repo, path_filter.clone(), walk_root, 0, graph_enabled, Arc::new(HashMap::new()))?;
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
        // Collect the whole pending burst before doing any work, so
        // superseded LoadDiffs can be coalesced away instead of each
        // being fully rendered only for the app to drop the result.
        // Non-blocking drain while indexing; once the walk is done and
        // nothing is queued, block for the next request (and pull in
        // the rest of any burst that arrived while blocked).
        let mut batch: Vec<GitReq> = Vec::new();
        loop {
            match req_rx.try_recv() {
                Ok(req) => batch.push(req),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if batch.is_empty() && walker.done {
            match req_rx.recv() {
                Ok(first) => {
                    batch.push(first);
                    while let Ok(req) = req_rx.try_recv() {
                        batch.push(req);
                    }
                }
                Err(_) => return Ok(()),
            }
        }

        coalesce_requests(&mut batch);
        for req in batch {
            if !process_request(
                req,
                &repo,
                &path_filter,
                &mut diff_scope,
                &mut walk_root,
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

        if !walker.done {
            let n = if refs_loaded { PAGE_BATCH } else { INITIAL_BATCH };
            let emitted = walker.load_more(n, &mut tree_diff_cache, &msg_tx, || !req_rx.is_empty())?;
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
    }
}

/// Drop every queued `LoadDiff` except the newest one. A later LoadDiff
/// strictly supersedes an earlier one: the app's `diff.target` already
/// reflects the newest request, so documents produced for earlier
/// targets would be dropped on arrival anyway — computing them only
/// delays the diff the user is actually waiting for (and starves the
/// walk, whose preemption check sees a non-empty queue). All other
/// requests keep their relative order.
fn coalesce_requests(batch: &mut Vec<GitReq>) {
    let is_load_diff = |req: &GitReq| matches!(req, GitReq::Inspect(InspectReq::LoadDiff { .. }));
    let Some(last) = batch.iter().rposition(is_load_diff) else { return };
    let mut i = 0;
    batch.retain(|req| {
        let keep = !is_load_diff(req) || i == last;
        i += 1;
        keep
    });
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
    diff_scope: &mut Option<gix::pathspec::Search>,
    walk_root: &mut Option<ObjectId>,
    graph_enabled: bool,
    walker: &mut walk::Walker<'r>,
    refs_loaded: &mut bool,
    tree_diff_cache: &mut TreeDiffCache,
    dirty_handles: &mut DirtyJoin,
    msg_tx: &Sender<GitMsg>,
) -> Result<bool> {
    match req {
        GitReq::History(HistoryReq::Reload { generation, root }) => {
            if let Some(root) = root {
                *walk_root = Some(root);
            }
            let refs_map = Arc::new(meta::load_refs(repo));
            // The new Walker's `'r` lifetime ties to `repo`, same as
            // the caller's walker — overwriting in place is fine.
            // The tree-diff cache survives reload: oids are
            // content-addressed so entries from the prior generation
            // are still valid. The generation comes from the app (the
            // single authority) rather than a local counter kept in
            // lockstep by convention.
            *walker = walk::Walker::new(repo, path_filter.clone(), *walk_root, generation, graph_enabled, refs_map)?;
            *refs_loaded = true;
        }
        GitReq::Inspect(InspectReq::LoadDiff { target, seq, scoped }) => {
            use crate::model::DiffTarget;
            let document = match target {
                DiffTarget::Commit(id) => {
                    let scope = if scoped { diff_scope.as_mut() } else { None };
                    diff::compute_commit_diff(repo, id, tree_diff_cache, scope)
                }
                DiffTarget::WorkingTree => status::compute_working_tree_diff(repo, target),
                DiffTarget::Staged => status::compute_staged_diff(repo, target),
            };
            // For working-tree diffs the document carries an
            // `untracked_anchor`; spawn the dir walk on a side thread
            // *after* DiffLoaded is on the channel so the UI paints
            // the staged/unstaged content immediately and the
            // untracked rows fold in via `UntrackedFilesUpdate`.
            let needs_untracked_walk = matches!(target, DiffTarget::WorkingTree) && document.untracked_anchor.is_some();
            _ = msg_tx.send(GitMsg::Inspect(InspectMsg::DiffLoaded { seq, document }));
            if needs_untracked_walk {
                dirty_handles.0.push(spawn_untracked_scan(repo, target, seq, msg_tx.clone()));
            }
        }
        GitReq::Inspect(InspectReq::LoadFileDiff { target, path, seq }) => {
            use crate::model::DiffTarget;
            let document = match target {
                DiffTarget::Commit(id) => diff::compute_file_diff(repo, id, &path, tree_diff_cache),
                DiffTarget::WorkingTree => status::compute_worktree_file_diff(repo, &path),
                DiffTarget::Staged => status::compute_staged_file_diff(repo, &path),
            };
            _ = msg_tx.send(GitMsg::Inspect(InspectMsg::FileDiffLoaded { seq, document }));
        }
        GitReq::Inspect(InspectReq::LoadStatus) => {
            let document = status::compute_status(repo);
            _ = msg_tx.send(GitMsg::Inspect(InspectMsg::StatusLoaded(document)));
        }
        GitReq::Inspect(InspectReq::LoadRefs) => {
            let entries = meta::load_ref_entries(repo);
            _ = msg_tx.send(GitMsg::Inspect(InspectMsg::RefsListLoaded(entries)));
        }
        GitReq::Inspect(InspectReq::LoadBlame { path, at, seq }) => {
            // Blame walks history — potentially seconds on long-lived
            // files — so it runs on a side thread and the worker stays
            // free for walk batches and diff requests. Stale replies
            // are dropped by the app's seq gate.
            dirty_handles.0.push(spawn_blame(repo, path, at, seq, msg_tx.clone()));
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
        // The staged probe is in-memory (index vs HEAD) and cheap;
        // run it before the worktree stat walk so its result doesn't
        // wait on the expensive part of the scan.
        let staged = meta::quick_has_staged(&repo);
        let dirty = meta::quick_is_dirty(&repo);
        _ = msg_tx.send(GitMsg::Inspect(InspectMsg::WorkingTreeMeta { author, dirty, staged }));
    })
}

/// Off-thread untracked-files walk for a `LoadDiff(WorkingTree)`. The
/// diff document has already been sent to the app at this point; this
/// scan only fills in the `?? path` rows. Same shutdown discipline as
/// `spawn_dirty_check` — handle goes into the `DirtyJoin` guard so a
/// bench iteration's teardown waits for the walk rather than letting
/// it race the next iteration's status sweep.
fn spawn_untracked_scan(
    repo: &gix::Repository,
    target: crate::model::DiffTarget,
    seq: u64,
    msg_tx: crossbeam_channel::Sender<GitMsg>,
) -> std::thread::JoinHandle<()> {
    let repo = repo.clone();
    std::thread::spawn(move || {
        let paths = status::collect_untracked_paths(&repo);
        _ = msg_tx.send(GitMsg::Inspect(InspectMsg::UntrackedFilesUpdate { target, seq, paths }));
    })
}

/// Off-thread gix-blame run for a `LoadBlame`. Same shutdown
/// discipline as the other side threads — the handle goes into the
/// `DirtyJoin` guard. The cloned repo re-instantiates the worker's
/// object cache (the factory travels with the clone), which blame's
/// repeated history traversal benefits from.
fn spawn_blame(
    repo: &gix::Repository,
    path: String,
    at: BlameAt,
    seq: u64,
    msg_tx: crossbeam_channel::Sender<GitMsg>,
) -> std::thread::JoinHandle<()> {
    let repo = repo.clone();
    std::thread::spawn(move || {
        let result = blame::compute_blame(&repo, &path, at);
        _ = msg_tx.send(GitMsg::Inspect(InspectMsg::BlameLoaded { seq, result }));
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
        GitReq, HistoryReq, InspectReq, coalesce_requests,
        meta::{quick_has_staged, quick_is_dirty, relative_time},
        resolve_positional,
    };
    use crate::{
        model::DiffTarget,
        test_support::{commit_all, make_fixture_repo, write_file},
    };

    #[test]
    fn coalesce_keeps_only_newest_load_diff() {
        let a = gix::ObjectId::empty_tree(gix::hash::Kind::Sha1);
        let b = gix::ObjectId::empty_blob(gix::hash::Kind::Sha1);
        let mut batch = vec![
            GitReq::Inspect(InspectReq::LoadDiff { target: DiffTarget::WorkingTree, seq: 1, scoped: false }),
            GitReq::Inspect(InspectReq::LoadStatus),
            GitReq::Inspect(InspectReq::LoadDiff { target: DiffTarget::Commit(a), seq: 2, scoped: false }),
            GitReq::History(HistoryReq::Reload { generation: 1, root: None }),
            GitReq::Inspect(InspectReq::LoadDiff { target: DiffTarget::Commit(b), seq: 3, scoped: false }),
        ];
        coalesce_requests(&mut batch);
        assert_eq!(batch.len(), 3, "two superseded LoadDiffs dropped");
        assert!(matches!(batch[0], GitReq::Inspect(InspectReq::LoadStatus)));
        assert!(matches!(batch[1], GitReq::History(HistoryReq::Reload { .. })));
        assert!(
            matches!(batch[2], GitReq::Inspect(InspectReq::LoadDiff { target: DiffTarget::Commit(id), seq: 3, .. }) if id == b)
        );
    }

    #[test]
    fn coalesce_no_load_diff_is_noop() {
        let mut batch = vec![
            GitReq::History(HistoryReq::Reload { generation: 1, root: None }),
            GitReq::Inspect(InspectReq::LoadStatus),
        ];
        coalesce_requests(&mut batch);
        assert_eq!(batch.len(), 2);
    }

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
    fn quick_has_staged_tracks_index_vs_head_only() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();

        // Unborn HEAD with a staged file: the empty-tree fallback still
        // sees the index entry.
        write_file(path, "tracked.txt", "hi\n");
        crate::test_support::run_git(path, &["add", "tracked.txt"]);
        assert_eq!(quick_has_staged(&repo), Some(true), "staged add on unborn HEAD counts");

        commit_all(path, "baseline");
        assert_eq!(quick_has_staged(&repo), Some(false), "freshly committed index matches HEAD");

        // Unstaged and untracked changes must NOT flip the staged bit.
        write_file(path, "tracked.txt", "hi\nthere\n");
        std::fs::write(path.join("untracked.txt"), "x").expect("write");
        assert_eq!(quick_has_staged(&repo), Some(false), "worktree-only changes are not staged");

        crate::test_support::run_git(path, &["add", "tracked.txt"]);
        assert_eq!(quick_has_staged(&repo), Some(true), "git add flips to staged");

        commit_all(path, "second");
        assert_eq!(quick_has_staged(&repo), Some(false), "commit clears the staged bit");
    }

    #[test]
    fn worker_answers_staged_load_diff() {
        use crate::{git::InspectMsg, model::DiffLineKind};
        let (td, _repo) = make_fixture_repo();
        let path = td.path();
        write_file(path, "a.txt", "one\n");
        commit_all(path, "baseline");
        write_file(path, "a.txt", "one\nTWO\n");
        crate::test_support::run_git(path, &["add", "a.txt"]);

        // `run_git_thread` discovers the repo from the cwd, so hold the
        // chdir until the worker has answered (the bench does the same).
        let prev = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(path).expect("cd into fixture");
        let (req_tx, req_rx) = crossbeam_channel::unbounded();
        let (msg_tx, msg_rx) = crossbeam_channel::unbounded();
        let handle = std::thread::spawn(move || super::run_git_thread(req_rx, msg_tx, None, false));

        req_tx
            .send(GitReq::Inspect(InspectReq::LoadDiff { target: DiffTarget::Staged, seq: 1, scoped: false }))
            .expect("send");
        let document = loop {
            match msg_rx.recv_timeout(std::time::Duration::from_secs(10)).expect("worker reply") {
                super::GitMsg::Inspect(InspectMsg::DiffLoaded { seq: 1, document }) => break document,
                _ => continue,
            }
        };
        std::env::set_current_dir(prev).expect("cd back");
        assert_eq!(document.target, DiffTarget::Staged);
        assert!(document.body.iter().any(|l| matches!(l.kind, DiffLineKind::Add) && l.text == "TWO"));
        drop(req_tx);
        _ = handle.join();
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
