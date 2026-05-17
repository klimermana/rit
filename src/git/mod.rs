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
use gix::{bstr::ByteSlice, ObjectId};
use similar::ChangeTag;
use std::collections::{BTreeMap, HashMap};

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
        let _ = msg_tx.send(GitMsg::History(HistoryMsg::Error(format!("git worker died: {}", e))));
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
            let _ = msg_tx.send(GitMsg::History(HistoryMsg::Error(format!("Failed to open repo: {}", e))));
            return Ok(());
        }
    };

    let repo_path = repo.workdir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| ".".to_string());

    let _ = msg_tx.send(GitMsg::History(HistoryMsg::RepoInfo(repo_info_for(&repo))));
    let _ = msg_tx.send(GitMsg::Inspect(InspectMsg::WorkingTreeMeta { author: working_tree_author(&repo) }));

    let mut walker = Walker::new(&repo, path_filter.clone(), 0, graph_enabled, HashMap::new())?;
    let mut refs_loaded = false;

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
                        &repo_path,
                        &path_filter,
                        graph_enabled,
                        &mut walker,
                        &mut refs_loaded,
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
            walker.load_more(n, &msg_tx)?;
            // After the first batch, load refs and backfill so branch/tag
            // decorations appear without blocking startup.
            if !refs_loaded {
                refs_loaded = true;
                let refs_map = load_refs(&repo);
                walker.refs_map = refs_map.clone();
                let _ = msg_tx.send(GitMsg::History(HistoryMsg::RefsLoaded { gen: walker.gen, refs_map }));
            }
            continue;
        }

        // Indexing finished — block for the next request.
        match req_rx.recv() {
            Ok(req) => {
                if !process_request(
                    req,
                    &repo,
                    &repo_path,
                    &path_filter,
                    graph_enabled,
                    &mut walker,
                    &mut refs_loaded,
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
fn process_request<'r>(
    req: GitReq,
    repo: &'r gix::Repository,
    repo_path: &str,
    path_filter: &Option<PathFilter>,
    graph_enabled: bool,
    walker: &mut Walker<'r>,
    refs_loaded: &mut bool,
    msg_tx: &Sender<GitMsg>,
) -> Result<bool> {
    match req {
        GitReq::History(HistoryReq::Reload) => {
            let next_gen = walker.gen.wrapping_add(1);
            let refs_map = load_refs(repo);
            // Safety: lifetime ties to `repo`, same as the caller's walker.
            *walker = Walker::new(repo, path_filter.clone(), next_gen, graph_enabled, refs_map)?;
            *refs_loaded = true;
        }
        GitReq::Inspect(InspectReq::LoadDiff(target)) => {
            let document = match target {
                DiffTarget::Commit(id) => compute_commit_diff(repo, id),
                DiffTarget::WorkingTree => compute_working_tree_diff(repo, repo_path, target),
            };
            let _ = msg_tx.send(GitMsg::Inspect(InspectMsg::DiffLoaded(document)));
        }
        GitReq::Inspect(InspectReq::LoadStatus) => {
            let document = compute_status(repo, repo_path);
            let _ = msg_tx.send(GitMsg::Inspect(InspectMsg::StatusLoaded(document)));
        }
        GitReq::Inspect(InspectReq::RefreshWorkingTreeMeta) => {
            let _ = msg_tx.send(GitMsg::Inspect(InspectMsg::WorkingTreeMeta {
                author: working_tree_author(repo),
            }));
        }
    }
    Ok(true)
}

struct Walker<'r> {
    repo: &'r gix::Repository,
    refs_map: HashMap<ObjectId, Vec<RefLabel>>,
    /// `None` when the `--graph` CLI flag is off — keeps the per-commit lane
    /// bookkeeping out of the hot path entirely.
    graph_state: Option<graph::GraphState>,
    iter: Option<gix::revision::Walk<'r>>,
    done: bool,
    /// Parsed once at construction; held in `&mut` form per call because
    /// `gix::pathspec::Search::pattern_matching_relative_path` mutates
    /// internal counters as it matches.
    pathspec: Option<gix::pathspec::Search>,
    gen: u64,
}

impl<'r> Walker<'r> {
    fn new(
        repo: &'r gix::Repository,
        path_filter: Option<PathFilter>,
        gen: u64,
        graph_enabled: bool,
        refs_map: HashMap<ObjectId, Vec<RefLabel>>,
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
            gen,
        })
    }

    fn load_more(&mut self, requested: usize, msg_tx: &Sender<GitMsg>) -> Result<()> {
        if self.done {
            let _ = msg_tx.send(GitMsg::History(HistoryMsg::WalkDone { gen: self.gen }));
            return Ok(());
        }
        let Some(iter) = self.iter.as_mut() else {
            self.done = true;
            let _ = msg_tx.send(GitMsg::History(HistoryMsg::WalkDone { gen: self.gen }));
            return Ok(());
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

            if let Some(search) = self.pathspec.as_mut() {
                if !commit_touches_pathspec(self.repo, info.id, &parent_ids, search) {
                    continue;
                }
            }

            if let Some(commit_record) =
                build_commit_info(self.repo, info.id, &parent_ids, &self.refs_map, self.graph_state.as_mut())
            {
                batch.push(commit_record);
            }
        }

        if !batch.is_empty() {
            let _ = msg_tx.send(GitMsg::History(HistoryMsg::Commits { gen: self.gen, commits: batch }));
        }
        if self.done {
            let _ = msg_tx.send(GitMsg::History(HistoryMsg::WalkDone { gen: self.gen }));
        }
        Ok(())
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
) -> bool {
    commit_touches_pathspec_inner(repo, commit_id, parent_ids, search).unwrap_or(false)
}

fn commit_touches_pathspec_inner(
    repo: &gix::Repository,
    commit_id: ObjectId,
    parent_ids: &[ObjectId],
    search: &mut gix::pathspec::Search,
) -> Result<bool> {
    use gix::{diff::tree::recorder::Change, objs::TreeRefIter};

    let cur_commit = repo.find_object(commit_id)?.try_into_commit()?;
    let cur_tree = cur_commit.tree()?;

    let par_tree = if parent_ids.is_empty() {
        repo.empty_tree()
    } else {
        repo.find_object(parent_ids[0])?.try_into_commit()?.tree()?
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

    // No attribute-driven pathspec magic is supported in this build; the
    // stub closure satisfies the signature without doing any work.
    let mut attrs = |_: &_, _: _, _: _, _: &mut _| true;

    for change in &recorder.records {
        let path = match change {
            Change::Addition { path, .. } | Change::Deletion { path, .. } | Change::Modification { path, .. } => path,
        };
        if search.pattern_matching_relative_path(path.as_ref(), Some(false), &mut attrs).is_some() {
            return Ok(true);
        }
    }
    Ok(false)
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

fn relative_time(unix_secs: i64) -> CompactString {
    let now = Utc::now();
    let t = Utc.timestamp_opt(unix_secs, 0).single().unwrap_or(now);
    let s = now.signed_duration_since(t).num_seconds();
    if s < 60 {
        format!("{}s ago", s).into()
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

fn compute_commit_diff(repo: &gix::Repository, id: ObjectId) -> DiffDocument {
    let target = DiffTarget::Commit(id);
    compute_commit_diff_inner(repo, id, target).unwrap_or_else(|e| empty_error_document(target, e))
}

fn compute_commit_diff_inner(repo: &gix::Repository, id: ObjectId, target: DiffTarget) -> Result<DiffDocument> {
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

    let cur_tree = commit_obj.tree()?;
    let parent_ids: Vec<ObjectId> = decoded.parents().collect();

    if parent_ids.is_empty() {
        diff_trees(repo, &repo.empty_tree(), &cur_tree, &mut body, &mut stats, &mut files)?;
    } else {
        let par_tree = repo.find_object(parent_ids[0])?.try_into_commit()?.tree()?;
        diff_trees(repo, &par_tree, &cur_tree, &mut body, &mut stats, &mut files)?;
    }

    Ok(DiffDocument { target, header, body, files, stats, flags: DiffFlags::default() })
}

fn diff_trees<'r>(
    repo: &'r gix::Repository,
    old_tree: &gix::Tree<'r>,
    new_tree: &gix::Tree<'r>,
    lines: &mut Vec<DiffLine>,
    stats: &mut DiffStats,
    files: &mut Vec<FileStat>,
) -> Result<()> {
    use gix::{diff::tree::recorder::Change, objs::TreeRefIter};

    let hash_kind = repo.object_hash();
    let mut recorder = gix::diff::tree::Recorder::default();
    gix::diff::tree(
        TreeRefIter::from_bytes(&old_tree.data, hash_kind),
        TreeRefIter::from_bytes(&new_tree.data, hash_kind),
        gix::diff::tree::State::default(),
        &repo.objects,
        &mut recorder,
    )?;

    for change in &recorder.records {
        match change {
            Change::Addition { entry_mode, oid, path, .. } if entry_mode.is_blob() => {
                let p = path.to_str_lossy().into_owned();
                lines.push(DiffLine::new(DiffLineKind::FileHeader, format!("diff --git a/{p} b/{p}")));
                lines.push(DiffLine::new(DiffLineKind::FileMeta, "new file"));
                lines.push(DiffLine::new(DiffLineKind::OldMarker, "--- /dev/null"));
                lines.push(DiffLine::new(DiffLineKind::NewMarker, format!("+++ b/{p}")));
                let mut file_add = 0usize;
                if let Ok(blob) = repo.find_object(*oid) {
                    let content = blob.data.to_str_lossy();
                    for line in content.lines() {
                        stats.insertions += 1;
                        file_add += 1;
                        lines.push(DiffLine::new(DiffLineKind::Add, format!("+{}", line)));
                    }
                }
                stats.files += 1;
                files.push(FileStat { path: p, additions: file_add, deletions: 0 });
            }
            Change::Deletion { entry_mode, oid, path, .. } if entry_mode.is_blob() => {
                let p = path.to_str_lossy().into_owned();
                lines.push(DiffLine::new(DiffLineKind::FileHeader, format!("diff --git a/{p} b/{p}")));
                lines.push(DiffLine::new(DiffLineKind::FileMeta, "deleted file"));
                lines.push(DiffLine::new(DiffLineKind::OldMarker, format!("--- a/{p}")));
                lines.push(DiffLine::new(DiffLineKind::NewMarker, "+++ /dev/null"));
                let mut file_del = 0usize;
                if let Ok(blob) = repo.find_object(*oid) {
                    let content = blob.data.to_str_lossy();
                    for line in content.lines() {
                        stats.deletions += 1;
                        file_del += 1;
                        lines.push(DiffLine::new(DiffLineKind::Del, format!("-{}", line)));
                    }
                }
                stats.files += 1;
                files.push(FileStat { path: p, additions: 0, deletions: file_del });
            }
            Change::Modification { entry_mode, previous_oid, oid, path, .. } if entry_mode.is_blob() => {
                let p = path.to_str_lossy().into_owned();
                lines.push(DiffLine::new(DiffLineKind::FileHeader, format!("diff --git a/{p} b/{p}")));
                lines.push(DiffLine::new(DiffLineKind::OldMarker, format!("--- a/{p}")));
                lines.push(DiffLine::new(DiffLineKind::NewMarker, format!("+++ b/{p}")));
                let old =
                    repo.find_object(*previous_oid).map(|o| o.data.to_str_lossy().into_owned()).unwrap_or_default();
                let new = repo.find_object(*oid).map(|o| o.data.to_str_lossy().into_owned()).unwrap_or_default();
                let diff = similar::TextDiff::from_lines(old.as_str(), new.as_str());
                let mut file_add = 0usize;
                let mut file_del = 0usize;
                for group in diff.grouped_ops(3) {
                    lines.push(DiffLine::new(DiffLineKind::HunkHeader, hunk_header(&group)));
                    for op in &group {
                        for ch in diff.iter_changes(op) {
                            push_change(lines, stats, &mut file_add, &mut file_del, ch.tag(), ch.value());
                        }
                    }
                }
                stats.files += 1;
                files.push(FileStat { path: p, additions: file_add, deletions: file_del });
            }
            _ => {}
        }
    }
    Ok(())
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
    format!("@@ -{},{} +{},{} @@", or_start + 1, or_len, nr_start + 1, nr_len)
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
            lines.push(DiffLine::new(DiffLineKind::Add, format!("+{}", v)));
        }
        ChangeTag::Delete => {
            stats.deletions += 1;
            *file_del += 1;
            lines.push(DiffLine::new(DiffLineKind::Del, format!("-{}", v)));
        }
        ChangeTag::Equal => {
            lines.push(DiffLine::new(DiffLineKind::Context, format!(" {}", v)));
        }
    }
}

fn format_timestamp(unix_secs: i64) -> String {
    Utc.timestamp_opt(unix_secs, 0).single().unwrap_or_else(Utc::now).format("%a %b %e %T %Y +0000").to_string()
}

// `git status --short` and per-file numstat are now produced natively via
// `gix::status`. The two `git diff [--cached]` shell-outs inside
// `compute_status_lines` below still remain — replicating git's exact
// human-readable diff text (including the `index <oid>..<oid> <mode>` line,
// rename headers, and binary-file callouts) is out of scope for this commit
// and is left as a follow-up.
fn compute_status(repo: &gix::Repository, repo_path: &str) -> StatusDocument {
    StatusDocument { lines: compute_status_lines(repo, repo_path) }
}

fn compute_status_lines(repo: &gix::Repository, repo_path: &str) -> Vec<DiffLine> {
    use std::process::Command;
    let mut lines: Vec<DiffLine> = Vec::new();

    lines.push(DiffLine::new(DiffLineKind::SectionTitle, "Working Tree Status"));
    lines.push(DiffLine::new(DiffLineKind::Blank, ""));

    let short = compute_short_status_lines_gix(repo);
    lines.extend(short);

    lines.push(DiffLine::new(DiffLineKind::Blank, ""));
    lines.push(DiffLine::new(DiffLineKind::SectionStaged, "── Staged ──────────────────────────────────────────────"));
    lines.push(DiffLine::new(DiffLineKind::Blank, ""));

    if let Ok(out) = Command::new("git").args(["-C", repo_path, "diff", "--cached"]).output() {
        let s = String::from_utf8_lossy(&out.stdout);
        if s.trim().is_empty() {
            lines.push(DiffLine::new(DiffLineKind::Faint, "(no staged changes)"));
        } else {
            for line in s.lines() {
                lines.push(classify_raw_diff_line(line));
            }
        }
    }

    lines.push(DiffLine::new(DiffLineKind::Blank, ""));
    lines
        .push(DiffLine::new(DiffLineKind::SectionUnstaged, "── Unstaged ────────────────────────────────────────────"));
    lines.push(DiffLine::new(DiffLineKind::Blank, ""));

    if let Ok(out) = Command::new("git").args(["-C", repo_path, "diff"]).output() {
        let s = String::from_utf8_lossy(&out.stdout);
        if s.trim().is_empty() {
            lines.push(DiffLine::new(DiffLineKind::Faint, "(no unstaged changes)"));
        } else {
            for line in s.lines() {
                lines.push(classify_raw_diff_line(line));
            }
        }
    }

    lines
}

/// Compute the equivalent of `git status --short` natively via `gix::status`.
///
/// Each tracked path's two-column status (staged column / unstaged column) is
/// folded together so files modified in both stages emit a single `MM <path>`
/// row, matching git. Untracked entries surface as `?? <path>` rows.
fn compute_short_status_lines_gix(repo: &gix::Repository) -> Vec<DiffLine> {
    use gix::status::{index_worktree, plumbing::index_as_worktree, UntrackedFiles};
    use std::collections::BTreeMap;

    // Per-path: (staged_char, unstaged_char). Space means "no change in that
    // column". Untracked is special-cased as ('?', '?').
    let mut by_path: BTreeMap<String, (char, char)> = BTreeMap::new();
    // Errors (e.g. detached worktree without an index) just yield an empty
    // status; the caller still prints the surrounding section frame.
    if let Ok(platform) = repo.status(gix::progress::Discard) {
        let iter = match platform.untracked_files(UntrackedFiles::Collapsed).into_iter(Vec::new()) {
            Ok(it) => it,
            Err(_) => return vec![DiffLine::new(DiffLineKind::Good, "Nothing to commit, working tree clean")],
        };
        for item in iter.flatten() {
            match item {
                gix::status::Item::TreeIndex(change) => {
                    // Staged (HEAD vs index) — affects the first column.
                    use gix::diff::index::Change;
                    let (path, c) = match change {
                        Change::Addition { location, .. } => (location.to_string(), 'A'),
                        Change::Deletion { location, .. } => (location.to_string(), 'D'),
                        Change::Modification { location, .. } => (location.to_string(), 'M'),
                        Change::Rewrite { location, copy, .. } => (location.to_string(), if copy { 'C' } else { 'R' }),
                    };
                    let entry = by_path.entry(path).or_insert((' ', ' '));
                    entry.0 = c;
                }
                gix::status::Item::IndexWorktree(iw) => match iw {
                    index_worktree::Item::Modification { rela_path, status, .. } => {
                        let c = match status {
                            index_as_worktree::EntryStatus::Change(index_as_worktree::Change::Removed) => 'D',
                            index_as_worktree::EntryStatus::Change(
                                index_as_worktree::Change::Modification { .. }
                                | index_as_worktree::Change::Type { .. }
                                | index_as_worktree::Change::SubmoduleModification(_),
                            ) => 'M',
                            index_as_worktree::EntryStatus::Conflict { .. } => 'U',
                            index_as_worktree::EntryStatus::IntentToAdd => 'A',
                            // NeedsUpdate is a stat-refresh hint, not a user-visible change.
                            index_as_worktree::EntryStatus::NeedsUpdate(_) => continue,
                        };
                        let entry = by_path.entry(rela_path.to_string()).or_insert((' ', ' '));
                        entry.1 = c;
                    }
                    index_worktree::Item::DirectoryContents { entry, .. } => {
                        // Only true untracked entries show up as `?? <path>`.
                        if matches!(entry.status, gix::dir::entry::Status::Untracked) {
                            by_path.entry(entry.rela_path.to_string()).or_insert(('?', '?'));
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
    }

    if by_path.is_empty() {
        return vec![DiffLine::new(DiffLineKind::Good, "Nothing to commit, working tree clean")];
    }

    let mut out = Vec::with_capacity(by_path.len());
    for (path, (a, b)) in by_path {
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

fn classify_raw_diff_line(line: &str) -> DiffLine {
    let kind = if line.starts_with("+++")
        || line.starts_with("---")
        || line.starts_with("diff ")
        || line.starts_with("index ")
    {
        DiffLineKind::Faint
    } else if line.starts_with('+') {
        DiffLineKind::Add
    } else if line.starts_with('-') {
        DiffLineKind::Del
    } else if line.starts_with("@@") {
        DiffLineKind::HunkHeader
    } else {
        DiffLineKind::Context
    };
    DiffLine::new(kind, line)
}

fn compute_working_tree_diff(repo: &gix::Repository, repo_path: &str, target: DiffTarget) -> DiffDocument {
    let body = compute_status_lines(repo, repo_path);
    let mut by_path: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for fs in compute_numstat_gix(repo) {
        let entry = by_path.entry(fs.path).or_insert((0, 0));
        entry.0 += fs.additions;
        entry.1 += fs.deletions;
    }
    let mut totals = DiffStats { files: 0, insertions: 0, deletions: 0 };
    let files: Vec<FileStat> = by_path
        .into_iter()
        .map(|(path, (a, d))| {
            totals.files += 1;
            totals.insertions += a;
            totals.deletions += d;
            FileStat { path, additions: a, deletions: d }
        })
        .collect();
    DiffDocument { target, header: Vec::new(), body, files, stats: totals, flags: DiffFlags::default() }
}

/// Compute per-file numstat (additions / deletions) for the union of
/// HEAD-vs-index (staged) and index-vs-worktree (unstaged) changes,
/// replacing `git diff --numstat [--cached]`.
///
/// Line-level diffing is delegated to `similar::TextDiff` against blobs
/// fetched from the object database (for tracked content) or read from the
/// worktree (for unstaged content). Binary content is treated as a single
/// addition or deletion line, which mirrors how git's numstat reports `-` —
/// but here we surface a number so the caller's stats arithmetic still works.
fn compute_numstat_gix(repo: &gix::Repository) -> Vec<FileStat> {
    use gix::status::{index_worktree, plumbing::index_as_worktree, UntrackedFiles};

    let mut out: Vec<FileStat> = Vec::new();

    let Ok(platform) = repo.status(gix::progress::Discard) else {
        return out;
    };
    let iter = match platform.untracked_files(UntrackedFiles::None).into_iter(Vec::new()) {
        Ok(it) => it,
        Err(_) => return out,
    };

    let workdir = repo.workdir().map(|p| p.to_owned());

    for item in iter.flatten() {
        match item {
            gix::status::Item::TreeIndex(change) => {
                use gix::diff::index::Change;
                let (path, old_id, new_id) = match change {
                    Change::Addition { location, id, .. } => {
                        (location.to_string(), None, Some(id.into_owned()))
                    }
                    Change::Deletion { location, id, .. } => {
                        (location.to_string(), Some(id.into_owned()), None)
                    }
                    Change::Modification { location, previous_id, id, .. } => {
                        (location.to_string(), Some(previous_id.into_owned()), Some(id.into_owned()))
                    }
                    Change::Rewrite { location, source_id, id, .. } => {
                        (location.to_string(), Some(source_id.into_owned()), Some(id.into_owned()))
                    }
                };
                let old = old_id
                    .and_then(|id| repo.find_object(id).ok().map(|o| o.data.clone()))
                    .unwrap_or_default();
                let new = new_id
                    .and_then(|id| repo.find_object(id).ok().map(|o| o.data.clone()))
                    .unwrap_or_default();
                let (additions, deletions) = numstat_for_blobs(&old, &new);
                out.push(FileStat { path, additions, deletions });
            }
            gix::status::Item::IndexWorktree(iw) => {
                if let index_worktree::Item::Modification { rela_path, status, entry, .. } = iw {
                    let path = rela_path.to_string();
                    match status {
                        index_as_worktree::EntryStatus::Change(index_as_worktree::Change::Removed) => {
                            let old = repo.find_object(entry.id).ok().map(|o| o.data.clone()).unwrap_or_default();
                            let (additions, deletions) = numstat_for_blobs(&old, &[]);
                            out.push(FileStat { path, additions, deletions });
                        }
                        index_as_worktree::EntryStatus::Change(index_as_worktree::Change::Modification { .. })
                        | index_as_worktree::EntryStatus::Change(index_as_worktree::Change::Type { .. }) => {
                            let old = repo.find_object(entry.id).ok().map(|o| o.data.clone()).unwrap_or_default();
                            let new_bytes = workdir
                                .as_ref()
                                .and_then(|wd| std::fs::read(wd.join(&path)).ok())
                                .unwrap_or_default();
                            let (additions, deletions) = numstat_for_blobs(&old, &new_bytes);
                            out.push(FileStat { path, additions, deletions });
                        }
                        // Conflicts, intent-to-add, submodule modifications, and stat-only
                        // refreshes don't contribute numstat numbers comparable to git's.
                        _ => {}
                    }
                }
            }
        }
    }

    out
}

fn numstat_for_blobs(old: &[u8], new: &[u8]) -> (usize, usize) {
    // Crude binary detection: a NUL byte in either side means we can't do a
    // line-diff. Report it as a wholesale rewrite (1 add + 1 del) so the
    // caller still surfaces a non-zero entry.
    if old.contains(&0) || new.contains(&0) {
        let a = if new.is_empty() { 0 } else { 1 };
        let d = if old.is_empty() { 0 } else { 1 };
        return (a, d);
    }
    let old_s = String::from_utf8_lossy(old);
    let new_s = String::from_utf8_lossy(new);
    let diff = similar::TextDiff::from_lines(old_s.as_ref(), new_s.as_ref());
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => additions += 1,
            ChangeTag::Delete => deletions += 1,
            ChangeTag::Equal => {}
        }
    }
    (additions, deletions)
}

#[cfg(test)]
mod tests {
    use super::{build_pathspec_search, hunk_header};
    use crate::model::PathFilter;
    use gix::bstr::BStr;
    use similar::TextDiff;

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
}
