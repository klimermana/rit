pub mod graph;

use crate::app::{CommitInfo, RefKind, RefLabel};
use anyhow::Result;
use chrono::{TimeZone, Utc};
use compact_str::CompactString;
use crossbeam_channel::{Receiver, Sender};
use gix::bstr::ByteSlice;
use gix::ObjectId;
use similar::ChangeTag;
use std::collections::{BTreeMap, HashMap};

const AUTHOR_DISPLAY_CHARS: usize = 20;

pub struct DiffStats {
    pub files: usize,
    pub insertions: usize,
    pub deletions: usize,
}

#[derive(Clone)]
pub struct FileStat {
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    // Commit metadata
    CommitHeader,
    Meta,
    Message,
    Blank,
    // Per-file headers
    FileHeader,
    FileMeta,
    OldMarker,
    NewMarker,
    HunkHeader,
    // Diff bodies
    Add,
    Del,
    Context,
    // Diffstat block
    Diffstat,
    DiffstatTotal,
    // Status view
    SectionTitle,
    SectionStaged,
    SectionUnstaged,
    Faint,
    Good,
    StatusOurs,
    StatusTheirs,
}

pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

impl DiffLine {
    fn new(kind: DiffLineKind, text: impl Into<String>) -> Self {
        Self { kind, text: text.into() }
    }
}

pub struct RepoInfo {
    pub name: String,
    pub branch: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffTarget {
    Commit(ObjectId),
    WorkingTree,
}

pub enum GitMsg {
    RepoInfo(RepoInfo),
    /// `gen` matches the worker's current walk generation. The app drops
    /// commits whose generation predates the most recent reload.
    Commits { gen: u64, commits: Vec<CommitInfo> },
    Diff {
        target: DiffTarget,
        header_lines: Vec<DiffLine>,
        body_lines: Vec<DiffLine>,
        stats: DiffStats,
        files: Vec<FileStat>,
    },
    Status(Vec<DiffLine>),
    WorkingTreeMeta { author: String },
    WalkDone { gen: u64 },
    Error(String),
}

pub enum GitReq {
    LoadMore(usize),
    FetchDiff(DiffTarget),
    FetchStatus,
    CheckWorkingTree,
    Reload,
}

pub fn run_git_thread(req_rx: Receiver<GitReq>, msg_tx: Sender<GitMsg>, path_filter: Option<String>) {
    if let Err(e) = run_git_thread_inner(req_rx, msg_tx.clone(), path_filter) {
        let _ = msg_tx.send(GitMsg::Error(format!("git worker died: {}", e)));
    }
}

fn run_git_thread_inner(
    req_rx: Receiver<GitReq>,
    msg_tx: Sender<GitMsg>,
    path_filter: Option<String>,
) -> Result<()> {
    let repo = match gix::discover(".") {
        Ok(r) => r,
        Err(e) => {
            let _ = msg_tx.send(GitMsg::Error(format!("Failed to open repo: {}", e)));
            return Ok(());
        }
    };

    let repo_path = repo
        .workdir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());

    let _ = msg_tx.send(GitMsg::RepoInfo(repo_info_for(&repo)));
    let _ = msg_tx.send(GitMsg::WorkingTreeMeta { author: working_tree_author(&repo) });

    let mut walker = Walker::new(&repo, path_filter.as_deref(), 0)?;

    while let Ok(req) = req_rx.recv() {
        match req {
            GitReq::LoadMore(n) => {
                walker.load_more(n, &msg_tx)?;
            }
            GitReq::FetchDiff(target) => {
                let payload = match target {
                    DiffTarget::Commit(id) => compute_commit_diff(&repo, id),
                    DiffTarget::WorkingTree => compute_working_tree_diff(&repo_path),
                };
                let _ = msg_tx.send(GitMsg::Diff {
                    target,
                    header_lines: payload.header,
                    body_lines: payload.body,
                    stats: payload.stats,
                    files: payload.files,
                });
            }
            GitReq::FetchStatus => {
                let lines = compute_status(&repo_path);
                let _ = msg_tx.send(GitMsg::Status(lines));
            }
            GitReq::CheckWorkingTree => {
                let _ = msg_tx.send(GitMsg::WorkingTreeMeta { author: working_tree_author(&repo) });
            }
            GitReq::Reload => {
                let next_gen = walker.gen.wrapping_add(1);
                walker = Walker::new(&repo, path_filter.as_deref(), next_gen)?;
            }
        }
    }

    Ok(())
}

struct Walker<'r> {
    repo: &'r gix::Repository,
    refs_map: HashMap<ObjectId, Vec<RefLabel>>,
    graph_state: graph::GraphState,
    iter: Option<gix::revision::Walk<'r>>,
    done: bool,
    path_filter: Option<String>,
    gen: u64,
}

impl<'r> Walker<'r> {
    fn new(repo: &'r gix::Repository, path_filter: Option<&str>, gen: u64) -> Result<Self> {
        let refs_map = load_refs(repo);
        let (iter, done) = match repo.head_id() {
            Ok(head_id) => (Some(head_id.ancestors().all()?), false),
            Err(_) => (None, true),
        };
        Ok(Self {
            repo,
            refs_map,
            graph_state: graph::GraphState::default(),
            iter,
            done,
            path_filter: path_filter.map(|s| s.to_string()),
            gen,
        })
    }

    fn load_more(&mut self, requested: usize, msg_tx: &Sender<GitMsg>) -> Result<()> {
        if self.done {
            let _ = msg_tx.send(GitMsg::WalkDone { gen: self.gen });
            return Ok(());
        }
        let Some(iter) = self.iter.as_mut() else {
            self.done = true;
            let _ = msg_tx.send(GitMsg::WalkDone { gen: self.gen });
            return Ok(());
        };

        let target = requested.max(1);
        let mut batch: Vec<CommitInfo> = Vec::with_capacity(target.min(256));
        // Cap iterator pulls so a path filter that rejects everything can't pin the worker.
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

            if let Some(filter) = &self.path_filter {
                if !commit_touches_path(self.repo, info.id, &parent_ids, filter) {
                    continue;
                }
            }

            if let Some(commit_info) = build_commit_info(self.repo, info.id, &parent_ids, &self.refs_map, &mut self.graph_state) {
                batch.push(commit_info);
            }
        }

        if !batch.is_empty() {
            let _ = msg_tx.send(GitMsg::Commits { gen: self.gen, commits: batch });
        }
        if self.done {
            let _ = msg_tx.send(GitMsg::WalkDone { gen: self.gen });
        }
        Ok(())
    }
}

fn build_commit_info(
    repo: &gix::Repository,
    id: ObjectId,
    parent_ids: &[ObjectId],
    refs_map: &HashMap<ObjectId, Vec<RefLabel>>,
    graph_state: &mut graph::GraphState,
) -> Option<CommitInfo> {
    let obj = repo.find_object(id).ok()?;
    let commit = obj.try_into_commit().ok()?;
    let decoded = commit.decode().ok()?;

    let short_id = id.to_hex_with_len(7).to_string().into();
    let author = decoded.author().ok()?;
    let author_full = author.name.to_str_lossy().into_owned();
    let author_display: CompactString = truncate_chars(&author_full, AUTHOR_DISPLAY_CHARS).into();
    let author_lower: CompactString = author_display.to_lowercase();
    let date = relative_time(author.time().map(|t| t.seconds).unwrap_or(0));
    let summary = decoded.message().summary().to_str_lossy().into_owned();
    let summary_lower = summary.to_lowercase();
    let refs = refs_map.get(&id).cloned().unwrap_or_default();
    let graph_prefix = graph_state.next(id, parent_ids);

    Some(CommitInfo {
        id,
        short_id,
        author: author_display,
        author_lower,
        date,
        summary,
        summary_lower,
        refs,
        graph: graph_prefix,
    })
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((boundary, _)) => s[..boundary].to_string(),
        None => s.to_string(),
    }
}

fn commit_touches_path(
    repo: &gix::Repository,
    commit_id: ObjectId,
    parent_ids: &[ObjectId],
    filter: &str,
) -> bool {
    commit_touches_path_inner(repo, commit_id, parent_ids, filter).unwrap_or(false)
}

fn commit_touches_path_inner(
    repo: &gix::Repository,
    commit_id: ObjectId,
    parent_ids: &[ObjectId],
    filter: &str,
) -> Result<bool> {
    use gix::diff::tree::recorder::Change;
    use gix::objs::TreeRefIter;

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

    for change in &recorder.records {
        let p = match change {
            Change::Addition { path, .. } | Change::Deletion { path, .. } | Change::Modification { path, .. } => path,
        };
        if p.to_str_lossy().contains(filter) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn load_refs(repo: &gix::Repository) -> HashMap<ObjectId, Vec<RefLabel>> {
    let mut map: HashMap<ObjectId, Vec<RefLabel>> = HashMap::new();
    let Ok(refs) = repo.references() else { return map };
    let Ok(all_refs) = refs.all() else { return map };
    let head_id = repo.head_id().ok().map(|id| id.detach());

    for ref_result in all_refs.flatten() {
        let full_name = ref_result.name().as_bstr().to_str_lossy().into_owned();
        let Some(target_id) = ref_result.target().try_id().map(|id| id.to_owned()) else { continue };
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
        let has_head = map.get(&head)
            .map(|ls| ls.iter().any(|l| l.kind == RefKind::Head))
            .unwrap_or(false);
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
    let branch = repo
        .head_name()
        .ok()
        .flatten()
        .map(|n| n.shorten().to_string())
        .unwrap_or_else(|| "HEAD".to_string());
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
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "you".to_string())
}

fn relative_time(unix_secs: i64) -> CompactString {
    let now = Utc::now();
    let t = Utc.timestamp_opt(unix_secs, 0).single().unwrap_or(now);
    let s = now.signed_duration_since(t).num_seconds();
    if s < 60 { format!("{}s ago", s).into() }
    else if s < 3600 { format!("{}m ago", s / 60).into() }
    else if s < 86400 { format!("{}h ago", s / 3600).into() }
    else if s < 86400 * 30 { format!("{}d ago", s / 86400).into() }
    else if s < 86400 * 365 { format!("{}mo ago", s / (86400 * 30)).into() }
    else { format!("{}y ago", s / (86400 * 365)).into() }
}

struct DiffPayload {
    header: Vec<DiffLine>,
    body: Vec<DiffLine>,
    stats: DiffStats,
    files: Vec<FileStat>,
}

impl DiffPayload {
    fn empty_error(e: anyhow::Error) -> Self {
        Self {
            header: vec![DiffLine::new(DiffLineKind::Faint, format!("Error: {}", e))],
            body: Vec::new(),
            stats: DiffStats { files: 0, insertions: 0, deletions: 0 },
            files: Vec::new(),
        }
    }
}

fn compute_commit_diff(repo: &gix::Repository, id: ObjectId) -> DiffPayload {
    compute_commit_diff_inner(repo, id).unwrap_or_else(DiffPayload::empty_error)
}

fn compute_commit_diff_inner(repo: &gix::Repository, id: ObjectId) -> Result<DiffPayload> {
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
    header.push(DiffLine::new(
        DiffLineKind::Meta,
        format!("AuthorDate: {}", format_timestamp(author.time()?.seconds)),
    ));
    header.push(DiffLine::new(
        DiffLineKind::Meta,
        format!("Commit:     {} <{}>", committer.name.to_str_lossy(), committer.email.to_str_lossy()),
    ));
    header.push(DiffLine::new(
        DiffLineKind::Meta,
        format!("CommitDate: {}", format_timestamp(committer.time()?.seconds)),
    ));
    header.push(DiffLine::new(DiffLineKind::Blank, ""));
    header.push(DiffLine::new(
        DiffLineKind::Message,
        format!("    {}", decoded.message_summary().to_str_lossy()),
    ));
    header.push(DiffLine::new(DiffLineKind::Blank, ""));

    let cur_tree = commit_obj.tree()?;
    let parent_ids: Vec<ObjectId> = decoded.parents().collect();

    if parent_ids.is_empty() {
        diff_trees(repo, &repo.empty_tree(), &cur_tree, &mut body, &mut stats, &mut files)?;
    } else {
        let par_tree = repo.find_object(parent_ids[0])?.try_into_commit()?.tree()?;
        diff_trees(repo, &par_tree, &cur_tree, &mut body, &mut stats, &mut files)?;
    }

    Ok(DiffPayload { header, body, stats, files })
}

fn diff_trees<'r>(
    repo: &'r gix::Repository,
    old_tree: &gix::Tree<'r>,
    new_tree: &gix::Tree<'r>,
    lines: &mut Vec<DiffLine>,
    stats: &mut DiffStats,
    files: &mut Vec<FileStat>,
) -> Result<()> {
    use gix::diff::tree::recorder::Change;
    use gix::objs::TreeRefIter;

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
                let old = repo.find_object(*previous_oid).map(|o| o.data.to_str_lossy().into_owned()).unwrap_or_default();
                let new = repo.find_object(*oid).map(|o| o.data.to_str_lossy().into_owned()).unwrap_or_default();
                let diff = similar::TextDiff::from_lines(old.as_str(), new.as_str());
                let mut file_add = 0usize;
                let mut file_del = 0usize;
                for group in diff.grouped_ops(3) {
                    let or = group.first().map(|op| op.old_range()).unwrap_or(0..0);
                    let nr = group.first().map(|op| op.new_range()).unwrap_or(0..0);
                    lines.push(DiffLine::new(
                        DiffLineKind::HunkHeader,
                        format!("@@ -{},{} +{},{} @@", or.start + 1, or.len(), nr.start + 1, nr.len()),
                    ));
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
    Utc.timestamp_opt(unix_secs, 0)
        .single()
        .unwrap_or_else(Utc::now)
        .format("%a %b %e %T %Y +0000")
        .to_string()
}

// Working-tree status is still produced by shelling out to `git`. The
// equivalent gix port would have to reimplement staged/unstaged human-readable
// diff formatting on top of gix::status iterators — significantly more code
// than the rest of the worker. Left as a follow-up.
fn compute_status(repo_path: &str) -> Vec<DiffLine> {
    use std::process::Command;
    let mut lines: Vec<DiffLine> = Vec::new();

    lines.push(DiffLine::new(DiffLineKind::SectionTitle, "Working Tree Status"));
    lines.push(DiffLine::new(DiffLineKind::Blank, ""));

    if let Ok(out) = Command::new("git").args(["-C", repo_path, "status", "--short"]).output() {
        let s = String::from_utf8_lossy(&out.stdout);
        if s.trim().is_empty() {
            lines.push(DiffLine::new(DiffLineKind::Good, "Nothing to commit, working tree clean"));
        } else {
            for line in s.lines() {
                let kind = if line.starts_with("M ") || line.starts_with("A ") || line.starts_with("D ") {
                    DiffLineKind::StatusOurs
                } else if line.starts_with(" M") || line.starts_with(" D") || line.starts_with("??") {
                    DiffLineKind::StatusTheirs
                } else {
                    DiffLineKind::Faint
                };
                lines.push(DiffLine::new(kind, line));
            }
        }
    }

    lines.push(DiffLine::new(DiffLineKind::Blank, ""));
    lines.push(DiffLine::new(
        DiffLineKind::SectionStaged,
        "── Staged ──────────────────────────────────────────────",
    ));
    lines.push(DiffLine::new(DiffLineKind::Blank, ""));

    if let Ok(out) = Command::new("git").args(["-C", repo_path, "diff", "--cached"]).output() {
        let s = String::from_utf8_lossy(&out.stdout);
        if s.trim().is_empty() {
            lines.push(DiffLine::new(DiffLineKind::Faint, "(no staged changes)"));
        } else {
            for line in s.lines() { lines.push(classify_raw_diff_line(line)); }
        }
    }

    lines.push(DiffLine::new(DiffLineKind::Blank, ""));
    lines.push(DiffLine::new(
        DiffLineKind::SectionUnstaged,
        "── Unstaged ────────────────────────────────────────────",
    ));
    lines.push(DiffLine::new(DiffLineKind::Blank, ""));

    if let Ok(out) = Command::new("git").args(["-C", repo_path, "diff"]).output() {
        let s = String::from_utf8_lossy(&out.stdout);
        if s.trim().is_empty() {
            lines.push(DiffLine::new(DiffLineKind::Faint, "(no unstaged changes)"));
        } else {
            for line in s.lines() { lines.push(classify_raw_diff_line(line)); }
        }
    }

    lines
}

fn classify_raw_diff_line(line: &str) -> DiffLine {
    let kind = if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") || line.starts_with("index ") {
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

fn compute_working_tree_diff(repo_path: &str) -> DiffPayload {
    let body = compute_status(repo_path);
    let mut by_path: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for fs in run_numstat(repo_path, true).into_iter().chain(run_numstat(repo_path, false)) {
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
    DiffPayload { header: Vec::new(), body, stats: totals, files }
}

fn run_numstat(repo_path: &str, cached: bool) -> Vec<FileStat> {
    use std::process::Command;
    let mut args: Vec<&str> = vec!["-C", repo_path, "diff", "--numstat"];
    if cached {
        args.push("--cached");
    }
    let out = match Command::new("git").args(&args).output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let s = String::from_utf8_lossy(&out.stdout);
    let mut result = Vec::new();
    for line in s.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(a), Some(d), Some(p)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let additions = a.parse().unwrap_or(0);
        let deletions = d.parse().unwrap_or(0);
        result.push(FileStat { path: p.to_string(), additions, deletions });
    }
    result
}
