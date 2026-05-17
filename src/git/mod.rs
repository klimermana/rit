pub mod graph;
pub mod history;
pub mod inspect;

pub use history::{HistoryMsg, HistoryReq};
pub use inspect::{InspectMsg, InspectReq};

use crate::model::{
    CommitRecord, CommitSearchText, DiffDocument, DiffFlags, DiffLine, DiffLineKind, DiffStats, DiffTarget, FileStat,
    PathFilter, RefKind, RefLabel, RepoInfo, StatusDocument,
};
use anyhow::Result;
use chrono::{TimeZone, Utc};
use compact_str::CompactString;
use crossbeam_channel::{Receiver, Sender};
use gix::{ObjectId, bstr::ByteSlice};
use similar::ChangeTag;
use std::{collections::HashMap, sync::Arc};

/// Cutoffs that keep `compute_commit_diff` responsive on pathological
/// commits. Anything past these limits gets a one-line summary instead of
/// a fully inlined hunk-by-hunk diff; the `DiffFlags` on the resulting
/// `DiffDocument` reports what was skipped so the UI can surface it.
const MAX_INLINE_DIFF_BYTES: usize = 256 * 1024;
const MAX_INLINE_DIFF_LINES: usize = 20_000;
const MAX_INLINE_DIFF_FILES: usize = 200;

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
struct TreeDiffCache {
    entries: Vec<(TreeDiffKey, TreeDiffRecords)>,
}

impl TreeDiffCache {
    fn new() -> Self {
        Self { entries: Vec::with_capacity(TREE_DIFF_CACHE_CAP) }
    }

    /// Look up a cached records vec. Bumps the entry to MRU on hit so
    /// the next eviction takes the genuinely least-recently-used item.
    fn get(&mut self, key: &(Option<ObjectId>, ObjectId)) -> Option<&[gix::diff::tree::recorder::Change]> {
        let pos = self.entries.iter().position(|(k, _)| k == key)?;
        let entry = self.entries.remove(pos);
        self.entries.push(entry);
        self.entries.last().map(|(_, v)| v.as_slice())
    }

    fn insert(&mut self, key: (Option<ObjectId>, ObjectId), value: Vec<gix::diff::tree::recorder::Change>) {
        if self.entries.len() >= TREE_DIFF_CACHE_CAP {
            self.entries.remove(0);
        }
        self.entries.push((key, value));
    }
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
        _ = msg_tx.send(GitMsg::History(HistoryMsg::Error(format!("git worker died: {}", e))));
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
            _ = msg_tx.send(GitMsg::History(HistoryMsg::Error(format!("Failed to open repo: {}", e))));
            return Ok(());
        }
    };

    _ = msg_tx.send(GitMsg::History(HistoryMsg::RepoInfo(repo_info_for(&repo))));
    _ = msg_tx.send(GitMsg::Inspect(InspectMsg::WorkingTreeMeta {
        author: working_tree_author(&repo),
        dirty: quick_is_dirty(&repo),
    }));

    let mut walker = Walker::new(&repo, path_filter.clone(), 0, graph_enabled, Arc::new(HashMap::new()))?;
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
                let refs_map = Arc::new(load_refs(&repo));
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
    walker: &mut Walker<'r>,
    refs_loaded: &mut bool,
    tree_diff_cache: &mut TreeDiffCache,
    msg_tx: &Sender<GitMsg>,
) -> Result<bool> {
    match req {
        GitReq::History(HistoryReq::Reload) => {
            let next_gen = walker.generation.wrapping_add(1);
            let refs_map = Arc::new(load_refs(repo));
            // The new Walker's `'r` lifetime ties to `repo`, same as
            // the caller's walker — overwriting in place is fine.
            // The tree-diff cache survives reload: oids are
            // content-addressed so entries from the prior generation
            // are still valid.
            *walker = Walker::new(repo, path_filter.clone(), next_gen, graph_enabled, refs_map)?;
            *refs_loaded = true;
        }
        GitReq::Inspect(InspectReq::LoadDiff(target)) => {
            let document = match target {
                DiffTarget::Commit(id) => compute_commit_diff(repo, id, tree_diff_cache),
                DiffTarget::WorkingTree => compute_working_tree_diff(repo, target),
            };
            _ = msg_tx.send(GitMsg::Inspect(InspectMsg::DiffLoaded(document)));
        }
        GitReq::Inspect(InspectReq::LoadStatus) => {
            let document = compute_status(repo);
            _ = msg_tx.send(GitMsg::Inspect(InspectMsg::StatusLoaded(document)));
        }
        GitReq::Inspect(InspectReq::RefreshWorkingTreeMeta) => {
            _ = msg_tx.send(GitMsg::Inspect(InspectMsg::WorkingTreeMeta {
                author: working_tree_author(repo),
                dirty: quick_is_dirty(repo),
            }));
        }
    }
    Ok(true)
}

struct Walker<'r> {
    repo: &'r gix::Repository,
    refs_map: Arc<HashMap<ObjectId, Vec<RefLabel>>>,
    /// `None` when the `--graph` CLI flag is off — keeps the per-commit lane
    /// bookkeeping out of the hot path entirely.
    graph_state: Option<graph::GraphState>,
    iter: Option<gix::revision::Walk<'r>>,
    done: bool,
    /// Parsed once at construction; held in `&mut` form per call because
    /// `gix::pathspec::Search::pattern_matching_relative_path` mutates
    /// internal counters as it matches.
    pathspec: Option<gix::pathspec::Search>,
    generation: u64,
}

impl<'r> Walker<'r> {
    fn new(
        repo: &'r gix::Repository,
        path_filter: Option<PathFilter>,
        generation: u64,
        graph_enabled: bool,
        refs_map: Arc<HashMap<ObjectId, Vec<RefLabel>>>,
    ) -> Result<Self> {
        let (iter, done) = match repo.head_id() {
            Ok(head_id) => (Some(head_id.ancestors().all()?), false),
            Err(_) => (None, true),
        };
        let pathspec = match path_filter {
            Some(pf) => Some(build_pathspec_search(&pf)?),
            None => None,
        };
        Ok(Self {
            repo,
            refs_map,
            graph_state: graph_enabled.then(graph::GraphState::default),
            iter,
            done,
            pathspec,
            generation,
        })
    }

    /// Pull and emit one batch. Returns the count of commits emitted —
    /// the worker uses this to tell `RefsLoaded` how far the
    /// refs-less prefix extends so the app can bound its backfill loop.
    fn load_more(&mut self, requested: usize, cache: &mut TreeDiffCache, msg_tx: &Sender<GitMsg>) -> Result<usize> {
        if self.done {
            _ = msg_tx.send(GitMsg::History(HistoryMsg::WalkDone { generation: self.generation }));
            return Ok(0);
        }
        let Some(iter) = self.iter.as_mut() else {
            self.done = true;
            _ = msg_tx.send(GitMsg::History(HistoryMsg::WalkDone { generation: self.generation }));
            return Ok(0);
        };

        let target = requested.max(1);
        let mut batch: Vec<CommitRecord> = Vec::with_capacity(target.min(256));
        // Cap iterator pulls so a path filter that rejects everything can't pin the
        // worker.
        let pull_cap = target.saturating_mul(64);
        let mut pulls = 0usize;

        while batch.len() < target && pulls < pull_cap {
            pulls += 1;
            let info = match iter.next() {
                None => {
                    self.done = true;
                    break;
                }
                Some(Err(_)) => continue,
                Some(Ok(info)) => info,
            };

            let parent_ids: Vec<ObjectId> = info.parent_ids.iter().copied().collect();

            if let Some(search) = self.pathspec.as_mut()
                && !commit_touches_pathspec(self.repo, info.id, &parent_ids, search, cache)
            {
                continue;
            }

            if let Some(commit_record) =
                build_commit_info(self.repo, info.id, &parent_ids, &self.refs_map, self.graph_state.as_mut())
            {
                batch.push(commit_record);
            }
        }

        let emitted = batch.len();
        if !batch.is_empty() {
            _ = msg_tx.send(GitMsg::History(HistoryMsg::Commits { generation: self.generation, commits: batch }));
        }
        if self.done {
            _ = msg_tx.send(GitMsg::History(HistoryMsg::WalkDone { generation: self.generation }));
        }
        Ok(emitted)
    }
}

fn build_commit_info(
    repo: &gix::Repository,
    id: ObjectId,
    parent_ids: &[ObjectId],
    refs_map: &HashMap<ObjectId, Vec<RefLabel>>,
    graph_state: Option<&mut graph::GraphState>,
) -> Option<CommitRecord> {
    let obj = repo.find_object(id).ok()?;
    let commit = obj.try_into_commit().ok()?;
    let decoded = commit.decode().ok()?;

    let short_id = id.to_hex_with_len(7).to_string().into();
    let author = decoded.author().ok()?;
    // Store the full author name so search can find substrings beyond the
    // 20-char column width. The UI is responsible for truncating at render
    // time.
    let author_full: CompactString = author.name.to_str_lossy().into_owned().into();
    let author_lower: CompactString = author_full.to_lowercase();
    let authored_unix_secs = author.time().map(|t| t.seconds).unwrap_or(0);
    let authored_relative = relative_time(authored_unix_secs);
    let summary = decoded.message().summary().to_str_lossy().into_owned();
    let summary_lower = summary.to_lowercase();
    let refs = refs_map.get(&id).cloned().unwrap_or_default();
    let graph_prefix = graph_state.map(|gs| gs.next(id, parent_ids)).unwrap_or_default();

    Some(CommitRecord {
        id,
        short_id,
        authored_unix_secs,
        authored_relative,
        author: author_full,
        summary,
        refs,
        graph: graph_prefix,
        search: CommitSearchText { author_lower, summary_lower },
    })
}

/// Parse the CLI path argument into a `gix::pathspec::Search`. Treats the
/// raw input as a single pathspec — `src/ui`, `:!target`, `*.rs` and the
/// other usual magic forms all work, matching `git log -- <pathspec>`
/// behavior.
fn build_pathspec_search(filter: &PathFilter) -> Result<gix::pathspec::Search> {
    use gix::pathspec::{self, Pattern};
    let pattern: Pattern = pathspec::parse(filter.as_str().as_bytes(), pathspec::Defaults::default())?;
    let search = pathspec::Search::from_specs(std::iter::once(pattern), None, std::path::Path::new(""))?;
    Ok(search)
}

fn commit_touches_pathspec(
    repo: &gix::Repository,
    commit_id: ObjectId,
    parent_ids: &[ObjectId],
    search: &mut gix::pathspec::Search,
    cache: &mut TreeDiffCache,
) -> bool {
    commit_touches_pathspec_inner(repo, commit_id, parent_ids, search, cache).unwrap_or(false)
}

fn commit_touches_pathspec_inner(
    repo: &gix::Repository,
    commit_id: ObjectId,
    parent_ids: &[ObjectId],
    search: &mut gix::pathspec::Search,
    cache: &mut TreeDiffCache,
) -> Result<bool> {
    use gix::diff::tree::recorder::Change;

    // No attribute-driven pathspec magic is supported in this build; the
    // stub closure satisfies the signature without doing any work.
    let mut attrs = |_: &_, _: _, _: _, _: &mut _| true;

    let records = compute_tree_diff_records(repo, parent_ids.first().copied(), commit_id, cache)?;
    for change in records {
        let path = match change {
            Change::Addition { path, .. } | Change::Deletion { path, .. } | Change::Modification { path, .. } => path,
        };
        if search.pattern_matching_relative_path(path.as_ref(), Some(false), &mut attrs).is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Look up (or compute and cache) the `gix::diff::tree` records for a
/// commit against its first parent (or the empty tree for root
/// commits). The cached vec is returned by slice — callers iterate
/// it in place without taking ownership.
fn compute_tree_diff_records<'c>(
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

fn load_refs(repo: &gix::Repository) -> HashMap<ObjectId, Vec<RefLabel>> {
    let mut map: HashMap<ObjectId, Vec<RefLabel>> = HashMap::new();
    let Ok(refs) = repo.references() else {
        return map;
    };
    let Ok(all_refs) = refs.all() else { return map };
    let head_id = repo.head_id().ok().map(|id| id.detach());

    for ref_result in all_refs.flatten() {
        let full_name = ref_result.name().as_bstr().to_str_lossy().into_owned();
        let Some(target_id) = ref_result.target().try_id().map(|id| id.to_owned()) else {
            continue;
        };
        let (name, kind) = if full_name == "HEAD" {
            ("HEAD".into(), RefKind::Head)
        } else if let Some(b) = full_name.strip_prefix("refs/heads/") {
            (b.into(), RefKind::LocalBranch)
        } else if let Some(r) = full_name.strip_prefix("refs/remotes/") {
            (r.into(), RefKind::RemoteBranch)
        } else if let Some(t) = full_name.strip_prefix("refs/tags/") {
            (t.into(), RefKind::Tag)
        } else {
            continue;
        };
        map.entry(target_id).or_default().push(RefLabel { name, kind });
    }

    if let Some(head) = head_id {
        let has_head = map.get(&head).map(|ls| ls.iter().any(|l| l.kind == RefKind::Head)).unwrap_or(false);
        if !has_head {
            map.entry(head).or_default().insert(0, RefLabel { name: "HEAD".into(), kind: RefKind::Head });
        }
    }
    map
}

fn repo_info_for(repo: &gix::Repository) -> RepoInfo {
    let name = repo
        .workdir()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .or_else(|| std::env::current_dir().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())))
        .unwrap_or_else(|| "unknown".to_string());
    let branch = repo.head_name().ok().flatten().map(|n| n.shorten().to_string()).unwrap_or_else(|| "HEAD".to_string());
    RepoInfo { name, branch }
}

fn working_tree_author(repo: &gix::Repository) -> String {
    // Prefer git config user.name; fall back to env, then a literal.
    if let Some(name) = repo.config_snapshot().string("user.name") {
        let s = name.to_string();
        if !s.trim().is_empty() {
            return s;
        }
    }
    std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "you".to_string())
}

/// Fast at-a-glance dirty check: walk `gix::status` and return on the
/// first observed change (staged, unstaged, or untracked). Returns `None`
/// when the status query itself errors — the UI keeps its previous
/// indicator in that case rather than flashing to "clean".
fn quick_is_dirty(repo: &gix::Repository) -> Option<bool> {
    use gix::status::{Item, UntrackedFiles, index_worktree, plumbing::index_as_worktree};
    let platform = repo.status(gix::progress::Discard).ok()?;
    let iter = platform.untracked_files(UntrackedFiles::Collapsed).into_iter(Vec::new()).ok()?;
    for item in iter.flatten() {
        match item {
            Item::TreeIndex(_) => return Some(true),
            Item::IndexWorktree(iw) => match iw {
                index_worktree::Item::Modification { status, .. } => {
                    // NeedsUpdate is a stat-cache refresh hint, not a user-visible change.
                    if !matches!(status, index_as_worktree::EntryStatus::NeedsUpdate(_)) {
                        return Some(true);
                    }
                }
                index_worktree::Item::DirectoryContents { entry, .. } => {
                    if matches!(entry.status, gix::dir::entry::Status::Untracked) {
                        return Some(true);
                    }
                }
                index_worktree::Item::Rewrite { .. } => return Some(true),
            },
        }
    }
    Some(false)
}

fn relative_time(unix_secs: i64) -> CompactString {
    let now = Utc::now();
    let t = Utc.timestamp_opt(unix_secs, 0).single().unwrap_or(now);
    let s = now.signed_duration_since(t).num_seconds();
    // Clock-skewed or future-dated commits collapse to "now" — otherwise
    // the < 60 branch would print "-12s ago" and similar.
    if s < 0 {
        "now".into()
    } else if s < 60 {
        format!("{s}s ago").into()
    } else if s < 3600 {
        format!("{}m ago", s / 60).into()
    } else if s < 86400 {
        format!("{}h ago", s / 3600).into()
    } else if s < 86400 * 30 {
        format!("{}d ago", s / 86400).into()
    } else if s < 86400 * 365 {
        format!("{}mo ago", s / (86400 * 30)).into()
    } else {
        format!("{}y ago", s / (86400 * 365)).into()
    }
}

fn empty_error_document(target: DiffTarget, e: anyhow::Error) -> DiffDocument {
    DiffDocument {
        target,
        header: vec![DiffLine::new(DiffLineKind::Faint, format!("Error: {}", e))],
        body: Vec::new(),
        files: Vec::new(),
        stats: DiffStats { files: 0, insertions: 0, deletions: 0 },
        flags: DiffFlags::default(),
    }
}

fn compute_commit_diff(repo: &gix::Repository, id: ObjectId, cache: &mut TreeDiffCache) -> DiffDocument {
    let target = DiffTarget::Commit(id);
    compute_commit_diff_inner(repo, id, target, cache).unwrap_or_else(|e| empty_error_document(target, e))
}

fn compute_commit_diff_inner(
    repo: &gix::Repository,
    id: ObjectId,
    target: DiffTarget,
    cache: &mut TreeDiffCache,
) -> Result<DiffDocument> {
    let commit_obj = repo.find_object(id)?.try_into_commit()?;
    let decoded = commit_obj.decode()?;
    let mut header: Vec<DiffLine> = Vec::new();
    let mut body: Vec<DiffLine> = Vec::new();
    let mut stats = DiffStats { files: 0, insertions: 0, deletions: 0 };
    let mut files: Vec<FileStat> = Vec::new();

    header.push(DiffLine::new(DiffLineKind::CommitHeader, format!("commit {}", id)));
    let author = decoded.author()?;
    let committer = decoded.committer()?;
    header.push(DiffLine::new(
        DiffLineKind::Meta,
        format!("Author:     {} <{}>", author.name.to_str_lossy(), author.email.to_str_lossy()),
    ));
    header.push(DiffLine::new(DiffLineKind::Meta, format!("AuthorDate: {}", format_timestamp(author.time()?.seconds))));
    header.push(DiffLine::new(
        DiffLineKind::Meta,
        format!("Commit:     {} <{}>", committer.name.to_str_lossy(), committer.email.to_str_lossy()),
    ));
    header.push(DiffLine::new(
        DiffLineKind::Meta,
        format!("CommitDate: {}", format_timestamp(committer.time()?.seconds)),
    ));
    header.push(DiffLine::new(DiffLineKind::Blank, ""));
    header.push(DiffLine::new(DiffLineKind::Message, format!("    {}", decoded.message_summary().to_str_lossy())));
    header.push(DiffLine::new(DiffLineKind::Blank, ""));

    let parent_ids: Vec<ObjectId> = decoded.parents().collect();

    // Cached tree-diff records. The pathspec filter populated this
    // during walking for every commit it considered; on a pathspec
    // walk this is a cache hit.
    let records = compute_tree_diff_records(repo, parent_ids.first().copied(), id, cache)?.to_vec();

    let mut flags = DiffFlags::default();
    {
        let mut sink = DiffSink { lines: &mut body, stats: &mut stats, files: &mut files, flags: &mut flags };
        render_diff_records(repo, &records, &mut sink);
    }

    Ok(DiffDocument { target, header, body, files, stats, flags })
}

/// Render previously-computed tree-diff records into the sink. Split
/// out of the old `diff_trees` so the cache lookup can sit outside the
/// renderer.
fn render_diff_records(repo: &gix::Repository, records: &[gix::diff::tree::recorder::Change], sink: &mut DiffSink<'_>) {
    use gix::diff::tree::recorder::Change;
    for change in records {
        // File-count and line-count caps apply before we even fetch blobs.
        if sink.guardrail_exceeded() {
            sink.account_skipped_file(change_path(change));
            continue;
        }

        match change {
            Change::Addition { entry_mode, oid, path, .. } if entry_mode.is_blob() => {
                let p = path.to_str_lossy().into_owned();
                if let Ok(new) = repo.find_object(*oid) {
                    render_file_addition(sink, &p, &new.data);
                }
            }
            Change::Deletion { entry_mode, oid, path, .. } if entry_mode.is_blob() => {
                let p = path.to_str_lossy().into_owned();
                if let Ok(old) = repo.find_object(*oid) {
                    render_file_deletion(sink, &p, &old.data);
                }
            }
            Change::Modification { entry_mode, previous_oid, oid, path, .. } if entry_mode.is_blob() => {
                let p = path.to_str_lossy().into_owned();
                if let (Ok(old), Ok(new)) = (repo.find_object(*previous_oid), repo.find_object(*oid)) {
                    render_file_modification(sink, &p, &old.data, &new.data);
                }
            }
            _ => {}
        }
    }
}

/// Mutable accumulator threaded through every file-render call. Keeps the
/// four output collections (`lines`, `stats`, `files`, `flags`) together
/// so call sites just pass `sink` instead of four borrows, and so the
/// guardrail logic stays in one place.
struct DiffSink<'a> {
    lines: &'a mut Vec<DiffLine>,
    stats: &'a mut DiffStats,
    files: &'a mut Vec<FileStat>,
    flags: &'a mut DiffFlags,
}

impl DiffSink<'_> {
    /// True once a file-count or line-count guardrail has fired. Callers
    /// should stop materialising hunks but still call `account_skipped_file`
    /// so the diffstat counts every changed file.
    fn guardrail_exceeded(&mut self) -> bool {
        if self.stats.files >= MAX_INLINE_DIFF_FILES {
            self.note_truncation(format!(
                "… {} files changed; remaining file diffs suppressed (>{} files)",
                self.stats.files, MAX_INLINE_DIFF_FILES,
            ));
            return true;
        }
        if self.lines.len() >= MAX_INLINE_DIFF_LINES {
            self.note_truncation(format!(
                "… diff truncated at {} lines; remaining files summarised",
                MAX_INLINE_DIFF_LINES,
            ));
            return true;
        }
        false
    }

    fn note_truncation(&mut self, message: String) {
        if !self.flags.truncated {
            self.flags.truncated = true;
            self.lines.push(DiffLine::new(DiffLineKind::Faint, message));
        }
    }

    fn account_skipped_file(&mut self, path: String) {
        self.stats.files += 1;
        self.files.push(FileStat { path, additions: 0, deletions: 0 });
    }

    fn record_file(&mut self, path: String, additions: usize, deletions: usize) {
        self.stats.files += 1;
        self.files.push(FileStat { path, additions, deletions });
    }
}

/// Render a pure-addition file diff (new file in the new revision).
///
/// Body lines are stored without the leading `+` — the renderer prepends
/// the marker at draw time based on `DiffLineKind::Add`. The per-line
/// `format!` was a measurable allocation in the original implementation
/// on large diffs.
fn render_file_addition(sink: &mut DiffSink<'_>, path: &str, new: &[u8]) {
    push_file_headers(sink.lines, path, Some("new file"));
    let (additions, deletions) = match classify_skip(&[], new) {
        Some(reason) => {
            push_skip_summary(sink.lines, sink.flags, reason, new.len());
            (0, 0)
        }
        None => {
            let mut count = 0usize;
            for line in new.to_str_lossy().lines() {
                sink.stats.insertions += 1;
                count += 1;
                sink.lines.push(DiffLine::new(DiffLineKind::Add, line));
            }
            (count, 0)
        }
    };
    sink.record_file(path.to_string(), additions, deletions);
}

/// Render a pure-deletion file diff (file present in old, absent in new).
/// Body lines are stored without the leading `-` (same convention as
/// `render_file_addition`).
fn render_file_deletion(sink: &mut DiffSink<'_>, path: &str, old: &[u8]) {
    push_file_headers(sink.lines, path, Some("deleted file"));
    let (additions, deletions) = match classify_skip(old, &[]) {
        Some(reason) => {
            push_skip_summary(sink.lines, sink.flags, reason, old.len());
            (0, 0)
        }
        None => {
            let mut count = 0usize;
            for line in old.to_str_lossy().lines() {
                sink.stats.deletions += 1;
                count += 1;
                sink.lines.push(DiffLine::new(DiffLineKind::Del, line));
            }
            (0, count)
        }
    };
    sink.record_file(path.to_string(), additions, deletions);
}

/// Render a per-file modification (both sides present) as one or more
/// `@@`-headed hunks. Shared between commit diffs (HEAD vs HEAD~1), staged
/// diffs (HEAD vs index), and unstaged diffs (index vs worktree).
fn render_file_modification(sink: &mut DiffSink<'_>, path: &str, old: &[u8], new: &[u8]) {
    push_file_headers(sink.lines, path, None);
    let (additions, deletions) = match classify_skip(old, new) {
        Some(reason) => {
            push_skip_summary(sink.lines, sink.flags, reason, old.len().max(new.len()));
            (0, 0)
        }
        None => {
            let old_s = old.to_str_lossy();
            let new_s = new.to_str_lossy();
            let diff = similar::TextDiff::from_lines(old_s.as_ref(), new_s.as_ref());
            let mut file_add = 0usize;
            let mut file_del = 0usize;
            for group in diff.grouped_ops(3) {
                sink.lines.push(DiffLine::new(DiffLineKind::HunkHeader, hunk_header(&group)));
                for op in &group {
                    for ch in diff.iter_changes(op) {
                        push_change(sink.lines, sink.stats, &mut file_add, &mut file_del, ch.tag(), ch.value());
                    }
                }
            }
            (file_add, file_del)
        }
    };
    sink.record_file(path.to_string(), additions, deletions);
}

/// Reason a file's inline diff was omitted. Each maps to a single
/// faint-rendered line in the output and bumps a counter on `DiffFlags`.
enum SkipReason {
    Binary,
    Oversize,
}

/// Decide whether a file should be summarised rather than fully diffed.
/// `old` and `new` are raw blob bytes (either may be empty for pure
/// add/delete).
fn classify_skip(old: &[u8], new: &[u8]) -> Option<SkipReason> {
    // NUL-byte sniff matches the convention used in compute_numstat_gix.
    if old.contains(&0) || new.contains(&0) {
        return Some(SkipReason::Binary);
    }
    if old.len() > MAX_INLINE_DIFF_BYTES || new.len() > MAX_INLINE_DIFF_BYTES {
        return Some(SkipReason::Oversize);
    }
    None
}

fn push_skip_summary(lines: &mut Vec<DiffLine>, flags: &mut DiffFlags, reason: SkipReason, biggest_side_bytes: usize) {
    let text = match reason {
        SkipReason::Binary => {
            flags.skipped_binary_files += 1;
            "Binary file — diff suppressed".to_string()
        }
        SkipReason::Oversize => {
            flags.skipped_large_files += 1;
            let kib = biggest_side_bytes / 1024;
            format!("Large file ({kib} KiB) — diff suppressed (>{} KiB)", MAX_INLINE_DIFF_BYTES / 1024)
        }
    };
    flags.truncated = true;
    lines.push(DiffLine::new(DiffLineKind::Faint, text));
}

fn push_file_headers(lines: &mut Vec<DiffLine>, path: &str, mode_meta: Option<&str>) {
    lines.push(DiffLine::new(DiffLineKind::FileHeader, format!("diff --git a/{path} b/{path}")));
    if let Some(m) = mode_meta {
        lines.push(DiffLine::new(DiffLineKind::FileMeta, m));
    }
    let (old_marker, new_marker) = match mode_meta {
        Some("new file") => ("--- /dev/null".to_string(), format!("+++ b/{path}")),
        Some("deleted file") => (format!("--- a/{path}"), "+++ /dev/null".to_string()),
        _ => (format!("--- a/{path}"), format!("+++ b/{path}")),
    };
    lines.push(DiffLine::new(DiffLineKind::OldMarker, old_marker));
    lines.push(DiffLine::new(DiffLineKind::NewMarker, new_marker));
}

fn change_path(change: &gix::diff::tree::recorder::Change) -> String {
    use gix::diff::tree::recorder::Change;
    let path = match change {
        Change::Addition { path, .. } | Change::Deletion { path, .. } | Change::Modification { path, .. } => path,
    };
    path.to_str_lossy().into_owned()
}

/// Build a `@@ -old_start,old_len +new_start,new_len @@` header that covers
/// the entire grouped op set, not just its first op. The earlier
/// implementation used `group.first()` for both start and length, which
/// truncated the range on any group containing more than one op
/// (e.g. Context + Replace + Context) — the emitted header would describe
/// only the leading context.
fn hunk_header(group: &[similar::DiffOp]) -> String {
    let or_start = group.first().map(|op| op.old_range().start).unwrap_or(0);
    let or_end = group.last().map(|op| op.old_range().end).unwrap_or(0);
    let nr_start = group.first().map(|op| op.new_range().start).unwrap_or(0);
    let nr_end = group.last().map(|op| op.new_range().end).unwrap_or(0);
    let or_len = or_end.saturating_sub(or_start);
    let nr_len = nr_end.saturating_sub(nr_start);
    // Unified-diff convention: when a side's range is empty, the start
    // is reported as 0 (the line before which the insertion happens,
    // or after which the deletion happens). Otherwise it's the 1-based
    // line number of the first line in the range.
    let or_display_start = if or_len == 0 { 0 } else { or_start + 1 };
    let nr_display_start = if nr_len == 0 { 0 } else { nr_start + 1 };
    format!("@@ -{or_display_start},{or_len} +{nr_display_start},{nr_len} @@")
}

fn push_change(
    lines: &mut Vec<DiffLine>,
    stats: &mut DiffStats,
    file_add: &mut usize,
    file_del: &mut usize,
    tag: ChangeTag,
    value: &str,
) {
    let v = value.trim_end_matches('\n');
    match tag {
        ChangeTag::Insert => {
            stats.insertions += 1;
            *file_add += 1;
            lines.push(DiffLine::new(DiffLineKind::Add, v));
        }
        ChangeTag::Delete => {
            stats.deletions += 1;
            *file_del += 1;
            lines.push(DiffLine::new(DiffLineKind::Del, v));
        }
        ChangeTag::Equal => {
            lines.push(DiffLine::new(DiffLineKind::Context, v));
        }
    }
}

fn format_timestamp(unix_secs: i64) -> String {
    Utc.timestamp_opt(unix_secs, 0).single().unwrap_or_else(Utc::now).format("%a %b %e %T %Y +0000").to_string()
}

/// Working-tree status pane. Reuses `compute_working_tree_diff`'s body
/// production so the status pane and the working-tree diff pane stay in
/// lockstep — there is exactly one renderer.
fn compute_status(repo: &gix::Repository) -> StatusDocument {
    let doc = compute_working_tree_diff(repo, DiffTarget::WorkingTree);
    StatusDocument { lines: doc.body }
}

fn render_staged_change(repo: &gix::Repository, sink: &mut DiffSink<'_>, change: gix::diff::index::Change) -> usize {
    use gix::diff::index::Change;
    match change {
        Change::Addition { location, id, .. } => {
            let path = location.to_string();
            if let Ok(new) = repo.find_object(id.into_owned()) {
                render_file_addition(sink, &path, &new.data);
            }
            1
        }
        Change::Deletion { location, id, .. } => {
            let path = location.to_string();
            if let Ok(old) = repo.find_object(id.into_owned()) {
                render_file_deletion(sink, &path, &old.data);
            }
            1
        }
        Change::Modification { location, previous_id, id, .. } => {
            let path = location.to_string();
            if let (Ok(old), Ok(new)) = (repo.find_object(previous_id.into_owned()), repo.find_object(id.into_owned()))
            {
                render_file_modification(sink, &path, &old.data, &new.data);
            }
            1
        }
        Change::Rewrite { location, source_id, id, .. } => {
            // Treat renames as modifications under the destination path. A
            // rename header `R old -> new` is a follow-up; today both git
            // and we show this as a modification at the new path.
            let path = location.to_string();
            if let (Ok(old), Ok(new)) = (repo.find_object(source_id.into_owned()), repo.find_object(id.into_owned())) {
                render_file_modification(sink, &path, &old.data, &new.data);
            }
            1
        }
    }
}

fn staged_change_path(change: &gix::diff::index::Change) -> String {
    use gix::diff::index::Change;
    let loc = match change {
        Change::Addition { location, .. }
        | Change::Deletion { location, .. }
        | Change::Modification { location, .. }
        | Change::Rewrite { location, .. } => location,
    };
    loc.to_string()
}

/// One unstaged change reduced to "what we need to render the unified
/// diff" — drops the gix-side `EntryStatus` enum (which contains many
/// variants that don't map to a hunk) so the renderer only sees the two
/// cases it actually handles.
enum UnstagedRender {
    /// File deleted in worktree; render as a deletion against the index blob.
    Removed { path: String, index_id: ObjectId },
    /// File modified or had its type change; diff index blob vs worktree
    /// bytes.
    Modified { path: String, index_id: ObjectId },
}

/// Result of the single `repo.status()` pass — everything needed to
/// render the short-status header, the staged section, and the
/// unstaged section without re-walking the filesystem.
struct StatusSweep {
    /// `path → (staged_char, unstaged_char)` for the short-status header.
    /// Untracked entries land here as `('?', '?')`.
    short: std::collections::BTreeMap<String, (char, char)>,
    /// Index-vs-HEAD changes, ready to feed `render_staged_change`.
    staged: Vec<gix::diff::index::Change>,
    /// Worktree-vs-index changes, classified for the renderer.
    unstaged: Vec<UnstagedRender>,
}

/// One walk of `gix::status` produces everything three consumers
/// (short-status, staged diff, unstaged diff) need. Previously each
/// consumer kicked off its own `repo.status(...)` pass, paying for the
/// stat-walk three times per working-tree view.
///
/// Returns `Err` when the platform / iterator can't be created
/// (corrupt index, detached worktree, ODB error, …). Callers decide
/// how to surface that.
fn sweep_status(repo: &gix::Repository) -> Result<StatusSweep> {
    use gix::status::{UntrackedFiles, index_worktree, plumbing::index_as_worktree};
    use std::collections::BTreeMap;

    let platform = repo.status(gix::progress::Discard)?;
    let iter = platform.untracked_files(UntrackedFiles::Collapsed).into_iter(Vec::new())?;

    let mut short: BTreeMap<String, (char, char)> = BTreeMap::new();
    let mut staged: Vec<gix::diff::index::Change> = Vec::new();
    let mut unstaged: Vec<UnstagedRender> = Vec::new();

    for item in iter.flatten() {
        match item {
            gix::status::Item::TreeIndex(change) => {
                // Staged (HEAD vs index) — affects the first column.
                use gix::diff::index::Change;
                let (path, c) = match &change {
                    Change::Addition { location, .. } => (location.to_string(), 'A'),
                    Change::Deletion { location, .. } => (location.to_string(), 'D'),
                    Change::Modification { location, .. } => (location.to_string(), 'M'),
                    Change::Rewrite { location, copy, .. } => (location.to_string(), if *copy { 'C' } else { 'R' }),
                };
                short.entry(path).or_insert((' ', ' ')).0 = c;
                staged.push(change);
            }
            gix::status::Item::IndexWorktree(iw) => match iw {
                index_worktree::Item::Modification { rela_path, status, entry, .. } => {
                    let path = rela_path.to_string();
                    // Classify into both the short-status char and the
                    // renderable variant in one match.
                    let column_char = match &status {
                        index_as_worktree::EntryStatus::Change(index_as_worktree::Change::Removed) => Some('D'),
                        index_as_worktree::EntryStatus::Change(
                            index_as_worktree::Change::Modification { .. }
                            | index_as_worktree::Change::Type { .. }
                            | index_as_worktree::Change::SubmoduleModification(_),
                        ) => Some('M'),
                        index_as_worktree::EntryStatus::Conflict { .. } => Some('U'),
                        index_as_worktree::EntryStatus::IntentToAdd => Some('A'),
                        // NeedsUpdate is a stat-refresh hint, not a user-visible change.
                        index_as_worktree::EntryStatus::NeedsUpdate(_) => None,
                    };
                    if let Some(c) = column_char {
                        short.entry(path.clone()).or_insert((' ', ' ')).1 = c;
                    }
                    // Submodule and conflict variants don't produce a hunk;
                    // only Removed and Modification/Type render.
                    match status {
                        index_as_worktree::EntryStatus::Change(index_as_worktree::Change::Removed) => {
                            unstaged.push(UnstagedRender::Removed { path, index_id: entry.id });
                        }
                        index_as_worktree::EntryStatus::Change(
                            index_as_worktree::Change::Modification { .. } | index_as_worktree::Change::Type { .. },
                        ) => {
                            unstaged.push(UnstagedRender::Modified { path, index_id: entry.id });
                        }
                        _ => {}
                    }
                }
                index_worktree::Item::DirectoryContents { entry, .. } => {
                    if matches!(entry.status, gix::dir::entry::Status::Untracked) {
                        short.entry(entry.rela_path.to_string()).or_insert(('?', '?'));
                    }
                }
                // Rewrites/copies between index and worktree would require
                // a separately-configured iterator; treat the two halves
                // as plain add+delete events, which is how the rest of
                // this iteration already sees them.
                index_worktree::Item::Rewrite { .. } => {}
            },
        }
    }

    Ok(StatusSweep { short, staged, unstaged })
}

/// Render the short-status block from a pre-collected `StatusSweep`.
/// An empty `short` map means "actually clean".
fn render_short_status(short: &std::collections::BTreeMap<String, (char, char)>) -> Vec<DiffLine> {
    if short.is_empty() {
        return vec![DiffLine::new(DiffLineKind::Good, "Nothing to commit, working tree clean")];
    }
    let mut out = Vec::with_capacity(short.len());
    for (path, &(a, b)) in short {
        let text = format!("{a}{b} {path}");
        let kind = if a == '?' && b == '?' {
            DiffLineKind::StatusTheirs
        } else if a != ' ' && b == ' ' {
            DiffLineKind::StatusOurs
        } else if a == ' ' && b != ' ' {
            DiffLineKind::StatusTheirs
        } else {
            // Combined states like `MM` or anything unusual: render faintly.
            DiffLineKind::Faint
        };
        out.push(DiffLine::new(kind, text));
    }
    out
}

/// Render the staged section from pre-collected changes. Returns the
/// number of files emitted (or skipped by the guardrail) so the caller
/// can fall back to "(no staged changes)" when empty.
fn render_staged_section(
    repo: &gix::Repository,
    sink: &mut DiffSink<'_>,
    staged: Vec<gix::diff::index::Change>,
) -> usize {
    let mut emitted = 0usize;
    for change in staged {
        if sink.guardrail_exceeded() {
            sink.account_skipped_file(staged_change_path(&change));
            emitted += 1;
            continue;
        }
        emitted += render_staged_change(repo, sink, change);
    }
    emitted
}

/// Render the unstaged section from pre-collected items. Worktree
/// bytes are read on demand (one fs::read per Modified item) so the
/// caller doesn't have to thread `workdir` in.
fn render_unstaged_section(repo: &gix::Repository, sink: &mut DiffSink<'_>, items: Vec<UnstagedRender>) -> usize {
    let workdir = repo.workdir().map(|p| p.to_owned());
    let mut emitted = 0usize;
    for item in items {
        let path = match &item {
            UnstagedRender::Removed { path, .. } | UnstagedRender::Modified { path, .. } => path.clone(),
        };
        if sink.guardrail_exceeded() {
            sink.account_skipped_file(path);
            emitted += 1;
            continue;
        }
        match item {
            UnstagedRender::Removed { path, index_id } => {
                if let Ok(old) = repo.find_object(index_id) {
                    render_file_deletion(sink, &path, &old.data);
                }
                emitted += 1;
            }
            UnstagedRender::Modified { path, index_id } => {
                let new = workdir.as_ref().and_then(|wd| std::fs::read(wd.join(&path)).ok()).unwrap_or_default();
                if let Ok(old) = repo.find_object(index_id) {
                    render_file_modification(sink, &path, &old.data, &new);
                }
                emitted += 1;
            }
        }
    }
    emitted
}

/// Assemble the working-tree diff document. The key win over the prior
/// implementation: only one `repo.status(...)` pass instead of three
/// (the staged renderer, unstaged renderer, and short-status header
/// each used to do their own). On a non-tiny repo the stat-walk
/// dominates, so collapsing the three passes into one is the biggest
/// single perf change in this stage.
fn compute_working_tree_diff(repo: &gix::Repository, target: DiffTarget) -> DiffDocument {
    let mut body: Vec<DiffLine> = Vec::new();
    let mut files: Vec<FileStat> = Vec::new();
    let mut stats = DiffStats { files: 0, insertions: 0, deletions: 0 };
    let mut flags = DiffFlags::default();

    body.push(DiffLine::new(DiffLineKind::SectionTitle, "Working Tree Status"));
    body.push(DiffLine::new(DiffLineKind::Blank, ""));

    let sweep = sweep_status(repo);
    match &sweep {
        Ok(items) => body.extend(render_short_status(&items.short)),
        Err(e) => body.push(DiffLine::new(DiffLineKind::Faint, format!("Status query failed: {e}"))),
    }

    body.push(DiffLine::new(DiffLineKind::Blank, ""));
    body.push(DiffLine::new(DiffLineKind::SectionStaged, "── Staged ──────────────────────────────────────────────"));
    body.push(DiffLine::new(DiffLineKind::Blank, ""));

    // Extract the staged / unstaged vecs once so we can move them into
    // the section renderers without re-borrowing `sweep`.
    let (staged, unstaged) = match sweep {
        Ok(s) => (s.staged, s.unstaged),
        Err(_) => (Vec::new(), Vec::new()),
    };

    {
        let mut sink = DiffSink { lines: &mut body, stats: &mut stats, files: &mut files, flags: &mut flags };
        if render_staged_section(repo, &mut sink, staged) == 0 {
            sink.lines.push(DiffLine::new(DiffLineKind::Faint, "(no staged changes)"));
        }
    }

    body.push(DiffLine::new(DiffLineKind::Blank, ""));
    body.push(DiffLine::new(DiffLineKind::SectionUnstaged, "── Unstaged ────────────────────────────────────────────"));
    body.push(DiffLine::new(DiffLineKind::Blank, ""));
    {
        let mut sink = DiffSink { lines: &mut body, stats: &mut stats, files: &mut files, flags: &mut flags };
        if render_unstaged_section(repo, &mut sink, unstaged) == 0 {
            sink.lines.push(DiffLine::new(DiffLineKind::Faint, "(no unstaged changes)"));
        }
    }

    DiffDocument { target, header: Vec::new(), body, files, stats, flags }
}

#[cfg(test)]
mod tests {
    use super::{
        GitMsg, MAX_INLINE_DIFF_BYTES, SkipReason, TreeDiffCache, Walker, build_commit_info, build_pathspec_search,
        classify_skip, hunk_header, quick_is_dirty, relative_time,
    };
    use crate::{
        model::PathFilter,
        test_support::{commit_all, commit_all_as, drain_commits, make_fixture_repo, run_git, write_file},
    };
    use gix::bstr::BStr;
    use similar::TextDiff;
    use std::{collections::HashMap, sync::Arc};

    fn pathspec_matches(spec: &str, path: &str) -> bool {
        let mut search = build_pathspec_search(&PathFilter::new(spec)).expect("parse");
        let mut attrs = |_: &_, _: _, _: _, _: &mut _| true;
        search.pattern_matching_relative_path(<&BStr>::from(path.as_bytes()), Some(false), &mut attrs).is_some()
    }

    #[test]
    fn pathspec_directory_matches_files_under_it() {
        assert!(pathspec_matches("src/ui", "src/ui/log_view.rs"));
        assert!(pathspec_matches("src/ui", "src/ui/mod.rs"));
    }

    #[test]
    fn pathspec_directory_rejects_siblings() {
        assert!(!pathspec_matches("src/ui", "src/app.rs"));
        assert!(!pathspec_matches("src/ui", "Cargo.toml"));
    }

    #[test]
    fn pathspec_glob_matches_extension() {
        assert!(pathspec_matches("*.rs", "src/main.rs"));
        assert!(pathspec_matches("*.rs", "src/ui/log_view.rs"));
        assert!(!pathspec_matches("*.rs", "Cargo.toml"));
    }

    #[test]
    fn header_spans_full_group_not_just_first_op() {
        // Single replacement surrounded by enough context that the diff
        // produces one group of [Context, Replace, Context]. The old
        // implementation used only the first op's range, which would
        // truncate the header to the leading context window.
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nb\nX\nd\ne\n";
        let diff = TextDiff::from_lines(old, new);
        let groups: Vec<_> = diff.grouped_ops(3).into_iter().collect();
        assert_eq!(groups.len(), 1, "expected one grouped hunk for this small diff");
        // The whole 5-line file is one group: lines 1..=5 on both sides.
        assert_eq!(hunk_header(&groups[0]), "@@ -1,5 +1,5 @@");
    }

    #[test]
    fn header_for_pure_insertion_at_end() {
        // Appending lines: old has 2 lines, new has 4. The trailing
        // additions form a group; the header should cover the appended
        // range on the new side and zero-length on the old side at the
        // appropriate offset.
        let old = "a\nb\n";
        let new = "a\nb\nc\nd\n";
        let diff = TextDiff::from_lines(old, new);
        let groups: Vec<_> = diff.grouped_ops(3).into_iter().collect();
        assert_eq!(groups.len(), 1);
        // Group covers the trailing 2 context lines + 2 inserts. With
        // 3-line context the whole file fits: old 1..=2 (len 2), new 1..=4 (len 4).
        assert_eq!(hunk_header(&groups[0]), "@@ -1,2 +1,4 @@");
    }

    #[test]
    fn header_with_two_disjoint_groups() {
        // Two changes far enough apart to produce two separate groups,
        // each with its own correct range.
        let old: String = (0..30).map(|i| format!("line{i}\n")).collect();
        let mut new_lines: Vec<String> = (0..30).map(|i| format!("line{i}\n")).collect();
        new_lines[2] = "CHANGED\n".to_string();
        new_lines[27] = "CHANGED\n".to_string();
        let new: String = new_lines.concat();
        let diff = TextDiff::from_lines(&old, &new);
        let groups: Vec<_> = diff.grouped_ops(3).into_iter().collect();
        assert_eq!(groups.len(), 2, "expected two separate hunks");
        let headers: Vec<String> = groups.iter().map(|g| hunk_header(g)).collect();
        // First hunk: lines around index 2 -> 1-based 3 with 3 context above
        // and below, so old 1..=6 (len 6), new 1..=6 (len 6).
        assert_eq!(headers[0], "@@ -1,6 +1,6 @@");
        // Second hunk: lines around index 27 -> 1-based 28, 3 above and 2 below.
        assert_eq!(headers[1], "@@ -25,6 +25,6 @@");
    }

    #[test]
    fn header_uses_zero_start_for_empty_old_range() {
        // Pure insertion into a previously empty file. Git's unified-diff
        // convention is `@@ -0,0 +1,N @@`, not `-1,0`. The old `start + 1`
        // form would have emitted `-1,0`.
        let diff = TextDiff::from_lines("", "x\ny\n");
        let groups: Vec<_> = diff.grouped_ops(3).into_iter().collect();
        assert_eq!(groups.len(), 1);
        assert_eq!(hunk_header(&groups[0]), "@@ -0,0 +1,2 @@");
    }

    #[test]
    fn header_uses_zero_start_for_empty_new_range() {
        // Pure deletion that empties the file. Git emits `@@ -1,N +0,0 @@`.
        let diff = TextDiff::from_lines("x\ny\n", "");
        let groups: Vec<_> = diff.grouped_ops(3).into_iter().collect();
        assert_eq!(groups.len(), 1);
        assert_eq!(hunk_header(&groups[0]), "@@ -1,2 +0,0 @@");
    }

    #[test]
    fn relative_time_clamps_future_dated_to_now() {
        // A commit timestamped far in the future shouldn't render as
        // "-12345s ago" — the < 0 case collapses to "now".
        let far_future = chrono::Utc::now().timestamp() + 86400 * 365 * 10;
        assert_eq!(relative_time(far_future).as_str(), "now");
    }

    #[test]
    fn relative_time_recent_past_renders_relative() {
        // Sanity that the happy path still works after the future-clamp
        // branch was added.
        let recent = chrono::Utc::now().timestamp() - 120;
        assert_eq!(relative_time(recent).as_str(), "2m ago");
    }

    #[test]
    fn classify_skip_flags_binary_by_nul_byte() {
        assert!(matches!(classify_skip(b"hello\0world", b"new"), Some(SkipReason::Binary)));
        assert!(matches!(classify_skip(b"old", b"\x00binary"), Some(SkipReason::Binary)));
    }

    #[test]
    fn classify_skip_flags_oversize() {
        // Either side larger than MAX_INLINE_DIFF_BYTES triggers the cap.
        let big = vec![b'a'; MAX_INLINE_DIFF_BYTES + 1];
        assert!(matches!(classify_skip(&big, b""), Some(SkipReason::Oversize)));
        assert!(matches!(classify_skip(b"", &big), Some(SkipReason::Oversize)));
    }

    #[test]
    fn classify_skip_passes_normal_text_pair() {
        assert!(classify_skip(b"hello\n", b"world\n").is_none());
        // Right at the boundary -- equal to the cap should still pass.
        let at_cap = vec![b'a'; MAX_INLINE_DIFF_BYTES];
        assert!(classify_skip(&at_cap, b"").is_none());
    }

    #[test]
    fn classify_skip_binary_wins_over_size() {
        // A binary file that's also oversized should report Binary first
        // -- the user cares about "this is binary" more than "this is big".
        let mut big_binary = vec![b'a'; MAX_INLINE_DIFF_BYTES + 1];
        big_binary[0] = 0;
        assert!(matches!(classify_skip(&big_binary, b""), Some(SkipReason::Binary)));
    }

    // ---- Fixture-repo integration tests ----
    //
    // These shell out to the `git` CLI to set up the test repo, then drive
    // rit's internal walker / build_commit_info against it. They cover
    // behavior that is hard to verify from pure unit tests because it
    // depends on the actual gix tree-diff and pathspec implementations.

    #[test]
    fn walker_pathspec_filters_to_matching_commits() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();

        write_file(path, "src/foo.rs", "fn a() {}\n");
        commit_all(path, "modify foo");
        write_file(path, "src/bar.rs", "fn b() {}\n");
        commit_all(path, "modify bar");
        write_file(path, "docs/intro.md", "hello\n");
        commit_all(path, "modify doc");

        let pf = PathFilter::new("src");
        let mut walker = Walker::new(&repo, Some(pf), 0, false, Arc::new(HashMap::new())).expect("walker");
        let (tx, rx) = crossbeam_channel::bounded::<GitMsg>(256);
        walker.load_more(100, &mut TreeDiffCache::new(), &tx).expect("load_more");

        let summaries: Vec<String> = drain_commits(&rx).into_iter().map(|c| c.summary).collect();
        // HEAD ancestors yields newest-first.
        assert_eq!(summaries, vec!["modify bar".to_string(), "modify foo".to_string()]);
    }

    #[test]
    fn walker_pathspec_nested_directory_includes_only_descendants() {
        // Verifies that a nested pathspec only walks commits touching files
        // under that directory -- catches a regression where the previous
        // contains() filter would have spuriously matched `src/cli/...`
        // against the spec `src/a` (substring hit on "src/a" being inside
        // "src/api").
        let (td, repo) = make_fixture_repo();
        let path = td.path();

        write_file(path, "src/api/foo.rs", "fn a() {}\n");
        commit_all(path, "modify api/foo");
        write_file(path, "src/cli/bar.rs", "fn b() {}\n");
        commit_all(path, "modify cli/bar");
        write_file(path, "src/api/baz.rs", "fn c() {}\n");
        commit_all(path, "modify api/baz");

        let pf = PathFilter::new("src/api");
        let mut walker = Walker::new(&repo, Some(pf), 0, false, Arc::new(HashMap::new())).expect("walker");
        let (tx, rx) = crossbeam_channel::bounded::<GitMsg>(256);
        walker.load_more(100, &mut TreeDiffCache::new(), &tx).expect("load_more");

        let summaries: Vec<String> = drain_commits(&rx).into_iter().map(|c| c.summary).collect();
        assert!(summaries.iter().any(|s| s == "modify api/foo"));
        assert!(summaries.iter().any(|s| s == "modify api/baz"));
        assert!(
            !summaries.iter().any(|s| s == "modify cli/bar"),
            "cli commit leaked through src/api spec: {:?}",
            summaries
        );
    }

    #[test]
    fn build_commit_info_preserves_full_author_for_search() {
        // Regression for the bug fixed in the search-full-author commit:
        // search.author_lower must contain substrings past the 20-char
        // display truncation. Without the fix, "rooijen" (at offset 16)
        // would survive but "smith" (at offset 35) would not.
        let (td, repo) = make_fixture_repo();
        let path = td.path();

        write_file(path, "a.txt", "hi\n");
        let long_name = "Christopher van Rooijen-Aalbersberg-Smith";
        commit_all_as(path, "first", long_name, "long@example.com");

        let head_id = repo.head_id().expect("head").detach();
        let info = build_commit_info(&repo, head_id, &[], &HashMap::new(), None).expect("commit info");

        assert_eq!(info.author.as_str(), long_name, "author field should hold the full name, not the truncated form");
        assert!(info.search.author_lower.contains("rooijen"));
        assert!(info.search.author_lower.contains("smith"));
    }

    #[test]
    fn working_tree_diff_renders_staged_and_unstaged_sections() {
        // End-to-end check for the gix-native diff body migration: stage
        // one change, leave another unstaged, and confirm both surface in
        // the right section with non-zero numstat contributions.
        use crate::model::DiffTarget;
        let (td, repo) = make_fixture_repo();
        let path = td.path();

        // Commit a baseline so the index has something to diff against.
        write_file(path, "staged.txt", "one\ntwo\nthree\n");
        write_file(path, "unstaged.txt", "alpha\nbeta\n");
        commit_all(path, "baseline");

        // Stage one modification.
        write_file(path, "staged.txt", "one\nTWO\nthree\nfour\n");
        run_git(path, &["add", "staged.txt"]);

        // Leave another modification unstaged.
        write_file(path, "unstaged.txt", "alpha\nBETA\n");

        let doc = super::compute_working_tree_diff(&repo, DiffTarget::WorkingTree);

        // Diff body lines store text *without* the unified-diff prefix
        // (`+`/`-`/` `); the renderer adds it at draw time. So the
        // staged-section assertion below looks for "four" as an `Add`
        // line, and the unstaged-section assertion looks for "BETA".
        let body_text: String = doc.body.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        assert!(body_text.contains("── Staged"), "staged section title present");
        assert!(body_text.contains("diff --git a/staged.txt b/staged.txt"));
        // The added line "four" lives in the body as an Add line.
        let has_four_add = doc.body.iter().any(|l| matches!(l.kind, super::DiffLineKind::Add) && l.text == "four");
        assert!(has_four_add, "staged diff contains Add line 'four'");
        assert!(body_text.contains("── Unstaged"), "unstaged section title present");
        assert!(body_text.contains("diff --git a/unstaged.txt b/unstaged.txt"));
        let has_beta_add = doc.body.iter().any(|l| matches!(l.kind, super::DiffLineKind::Add) && l.text == "BETA");
        assert!(has_beta_add, "unstaged diff contains Add line 'BETA'");

        // Both files contribute to the diffstat / aggregated stats.
        let paths: Vec<&str> = doc.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"staged.txt"));
        assert!(paths.contains(&"unstaged.txt"));
        assert!(doc.stats.files >= 2);
        assert!(doc.stats.insertions > 0);
    }

    #[test]
    fn working_tree_diff_empty_sections_show_placeholder_text() {
        // Pristine repo: status should report clean, both diff sections
        // should fall back to their "(no … changes)" placeholders.
        use crate::model::DiffTarget;
        let (td, repo) = make_fixture_repo();
        let path = td.path();
        write_file(path, "a.txt", "hi\n");
        commit_all(path, "baseline");

        let doc = super::compute_working_tree_diff(&repo, DiffTarget::WorkingTree);
        let body_text: String = doc.body.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");

        assert!(body_text.contains("Nothing to commit, working tree clean"));
        assert!(body_text.contains("(no staged changes)"));
        assert!(body_text.contains("(no unstaged changes)"));
        assert_eq!(doc.stats.files, 0);
        assert_eq!(doc.stats.insertions, 0);
        assert_eq!(doc.stats.deletions, 0);
    }

    #[test]
    fn quick_is_dirty_reports_clean_then_dirty() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();

        // Baseline commit; clean worktree afterwards.
        write_file(path, "tracked.txt", "hi\n");
        commit_all(path, "baseline");
        assert_eq!(quick_is_dirty(&repo), Some(false), "freshly committed worktree should be clean");

        // Modify a tracked file -> unstaged change.
        write_file(path, "tracked.txt", "hi\nthere\n");
        assert_eq!(quick_is_dirty(&repo), Some(true), "tracked-file mod should flip to dirty");

        // Commit and add an untracked file -> still dirty.
        commit_all(path, "second");
        assert_eq!(quick_is_dirty(&repo), Some(false), "after commit, back to clean");
        std::fs::write(path.join("new_untracked.txt"), "x").expect("write");
        assert_eq!(quick_is_dirty(&repo), Some(true), "untracked file should count as dirty");
    }
}
