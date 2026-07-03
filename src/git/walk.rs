//! Commit walking + pathspec filtering. `Walker` owns the `gix`
//! ancestor iterator and a parsed pathspec; `build_commit_info`
//! decodes one commit into a `CommitRecord` for the UI.

use crate::{
    git::{GitMsg, TreeDiffCache, compute_tree_diff_records, graph, history::HistoryMsg, meta::relative_time},
    model::{CommitRecord, CommitSearchText, PathFilter, RefLabel},
};
use anyhow::Result;
use compact_str::CompactString;
use crossbeam_channel::Sender;
use gix::{ObjectId, bstr::ByteSlice, prelude::ObjectIdExt};
use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

pub struct Walker<'r> {
    repo: &'r gix::Repository,
    pub refs_map: Arc<HashMap<ObjectId, Vec<RefLabel>>>,
    /// `None` when the `--graph` CLI flag is off — keeps the per-commit lane
    /// bookkeeping out of the hot path entirely.
    graph_state: Option<graph::GraphState>,
    iter: Option<gix::revision::Walk<'r>>,
    pub done: bool,
    /// Path filter, `None` when no path was given on the CLI. A literal
    /// path uses the tree-entry-oid fast-path; a glob/magic pathspec
    /// falls back to the full per-commit tree-diff. Held in `&mut` form
    /// per call because the spec matcher mutates internal counters.
    path_matcher: Option<PathMatcher>,
    pub generation: u64,
}

impl<'r> Walker<'r> {
    pub fn new(
        repo: &'r gix::Repository,
        path_filter: Option<PathFilter>,
        start_id: Option<ObjectId>,
        generation: u64,
        graph_enabled: bool,
        refs_map: Arc<HashMap<ObjectId, Vec<RefLabel>>>,
    ) -> Result<Self> {
        // `start_id` lets a CLI-supplied commit hash pin the walk root;
        // falling back to HEAD preserves the no-arg behavior. A bare
        // repo with no HEAD still produces an empty walk.
        let start: Option<ObjectId> = start_id.or_else(|| repo.head_id().ok().map(|h| h.detach()));
        let (iter, done) = match start {
            Some(oid) => (Some(oid.attach(repo).ancestors().all()?), false),
            None => (None, true),
        };
        let path_matcher = match path_filter {
            Some(pf) => Some(PathMatcher::from_filter(&pf)?),
            None => None,
        };
        Ok(Self {
            repo,
            refs_map,
            graph_state: graph_enabled.then(graph::GraphState::default),
            iter,
            done,
            path_matcher,
            generation,
        })
    }

    /// Pull and emit one batch. Returns the count of commits emitted —
    /// the worker uses this to tell `RefsLoaded` how far the
    /// refs-less prefix extends so the app can bound its backfill loop.
    ///
    /// `preempt` is polled once per examined commit; when it returns true
    /// the sweep stops early, emits whatever it has gathered so far, and
    /// returns. The iterator state persists on `self`, so the next call
    /// resumes where this one left off. The worker wires this to "is a
    /// request queued?" so opening a commit (or a reload) is serviced
    /// promptly instead of waiting for the whole batch — which, for a
    /// selective pathspec, can mean thousands of tree-diffs.
    pub fn load_more(
        &mut self,
        requested: usize,
        cache: &mut TreeDiffCache,
        msg_tx: &Sender<GitMsg>,
        preempt: impl Fn() -> bool,
    ) -> Result<usize> {
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
        // Bound the number of commits *examined* per call to `target`,
        // not just the number emitted. For a pathspec walk this is the
        // key to responsiveness: instead of tree-diffing up to `target *
        // 64` commits to fill a single batch (a long, un-preemptible
        // stall), each call sweeps a bounded window, streams whatever
        // matched, and returns so the worker can drain pending requests
        // and resume. For the common no-pathspec case every examined
        // commit is emitted, so this is one full batch per call exactly
        // as before.
        let mut scanned = 0usize;

        // Poll for a queued request every `PREEMPT_POLL` commits rather
        // than on every one — the check is a cheap atomic load, but at one
        // per commit it's measurable against the tiny per-commit work on a
        // warm repo. A window of 32 bounds yield latency to a handful of
        // tree-diffs (sub-millisecond), which is imperceptible for the
        // open-commit case this exists to unblock. A decrementing counter
        // keeps the hot path to a compare-and-branch instead of a modulo.
        const PREEMPT_POLL: usize = 32;
        let mut until_poll = 0usize;

        while scanned < target {
            // Yield to a queued request (open-commit diff, reload) rather
            // than finishing the sweep first.
            if until_poll == 0 {
                if preempt() {
                    break;
                }
                until_poll = PREEMPT_POLL;
            }
            until_poll -= 1;
            scanned += 1;
            let info = match iter.next() {
                None => {
                    self.done = true;
                    break;
                }
                Some(Err(_)) => continue,
                Some(Ok(info)) => info,
            };

            let parent_ids: Vec<ObjectId> = info.parent_ids.iter().copied().collect();

            if let Some(matcher) = self.path_matcher.as_mut()
                && !matcher.commit_touches(self.repo, info.id, &parent_ids, cache)
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

pub fn build_commit_info(
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
    let refs_lower = refs_lower_from_refs(&refs);
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
        search: CommitSearchText { author_lower, summary_lower, refs_lower },
    })
}

/// Build the lowercased, space-joined ref-name blob stored on a commit's
/// `CommitSearchText`. All `RefLabel` kinds are included — tags, local /
/// remote branches, and HEAD — so any label visible in the log row can
/// be searched. Shared between fresh builds in the walker and the
/// `RefsLoaded` backfill path that fills refs in on commits which were
/// emitted before the refs table was ready.
pub fn refs_lower_from_refs(refs: &[RefLabel]) -> CompactString {
    let mut out = String::new();
    for r in refs {
        if !out.is_empty() {
            out.push(' ');
        }
        // Lowercasing here means `commit_matches` can stick to a single
        // `.contains` against the already-lowercased query.
        out.extend(r.name.chars().flat_map(|c| c.to_lowercase()));
    }
    out.into()
}

/// How a path argument is matched against each walked commit.
enum PathMatcher {
    /// A literal file or directory path (no glob and no magic pathspec
    /// signature). Matched by comparing the tree-entry oid at the path
    /// between a commit and its first parent — O(path depth) object loads
    /// instead of a full recursive tree-diff, and with no dependence on a
    /// commit-graph. A directory's tree oid changes iff anything under it
    /// changed, so this is exact for both files and directories.
    Literal(PathBuf),
    /// A glob or magic pathspec (`*.rs`, `:!target`, …). Needs the real
    /// pathspec matcher, which means tree-diffing each commit to get the
    /// changed paths to test.
    Spec(gix::pathspec::Search),
}

impl PathMatcher {
    fn from_filter(filter: &PathFilter) -> Result<Self> {
        match literal_path(filter.as_str()) {
            Some(path) => Ok(PathMatcher::Literal(path)),
            None => Ok(PathMatcher::Spec(build_pathspec_search(filter)?)),
        }
    }

    /// Does `commit_id` change anything under the filtered path relative
    /// to its first parent? Root commits (no parent) compare against the
    /// empty tree, matching `git log`'s treatment of the initial commit.
    fn commit_touches(
        &mut self,
        repo: &gix::Repository,
        commit_id: ObjectId,
        parent_ids: &[ObjectId],
        cache: &mut TreeDiffCache,
    ) -> bool {
        match self {
            PathMatcher::Literal(path) => {
                literal_commit_touches(repo, commit_id, parent_ids.first().copied(), path).unwrap_or(false)
            }
            PathMatcher::Spec(search) => commit_touches_pathspec(repo, commit_id, parent_ids, search, cache),
        }
    }
}

/// Interpret the raw path argument as a literal path when it carries no
/// glob metacharacters, no magic `:` signature, and no `!` negation, and
/// resolves to plain normal components (no `..`, absolute root, or `.`).
/// Anything fancier returns `None` and falls back to the full pathspec
/// matcher so semantics stay identical to `git log -- <spec>`.
fn literal_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty()
        || trimmed.starts_with(':')
        || trimmed.starts_with('!')
        || trimmed.bytes().any(|b| matches!(b, b'*' | b'?' | b'[' | b']'))
    {
        return None;
    }
    let path = PathBuf::from(trimmed);
    path.components().all(|c| matches!(c, Component::Normal(_))).then_some(path)
}

/// Fast-path touch test: a commit changes `path` iff the tree entry at
/// that path differs between the commit and its parent. Missing on one
/// side (add / delete) is a change; missing on both (path absent from
/// this line of history) is not.
fn literal_commit_touches(
    repo: &gix::Repository,
    commit_id: ObjectId,
    parent_id: Option<ObjectId>,
    path: &Path,
) -> Result<bool> {
    let here = entry_oid_at(repo, commit_id, path)?;
    let parent = match parent_id {
        Some(p) => entry_oid_at(repo, p, path)?,
        None => None,
    };
    Ok(here != parent)
}

/// The oid of the tree entry at `path` within `commit_id`'s tree, or
/// `None` when the path doesn't exist in that tree.
fn entry_oid_at(repo: &gix::Repository, commit_id: ObjectId, path: &Path) -> Result<Option<ObjectId>> {
    let tree = repo.find_object(commit_id)?.try_into_commit()?.tree()?;
    Ok(tree.lookup_entry_by_path(path)?.map(|e| e.object_id()))
}

/// Parse the CLI path argument into a `gix::pathspec::Search`. Treats the
/// raw input as a single pathspec — `src/ui`, `:!target`, `*.rs` and the
/// other usual magic forms all work, matching `git log -- <pathspec>`
/// behavior.
pub fn build_pathspec_search(filter: &PathFilter) -> Result<gix::pathspec::Search> {
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

#[cfg(test)]
mod tests {
    use super::{Walker, build_commit_info, build_pathspec_search};
    use crate::{
        git::{GitMsg, TreeDiffCache},
        model::PathFilter,
        test_support::{commit_all, commit_all_as, drain_commits, make_fixture_repo, write_file},
    };
    use gix::bstr::BStr;
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
        let mut walker = Walker::new(&repo, Some(pf), None, 0, false, Arc::new(HashMap::new())).expect("walker");
        let (tx, rx) = crossbeam_channel::bounded::<GitMsg>(256);
        walker.load_more(100, &mut TreeDiffCache::new(), &tx, || false).expect("load_more");

        let summaries: Vec<String> = drain_commits(&rx).into_iter().map(|c| c.summary).collect();
        assert_eq!(summaries, vec!["modify bar".to_string(), "modify foo".to_string()]);
    }

    #[test]
    fn walker_pathspec_nested_directory_includes_only_descendants() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();

        write_file(path, "src/api/foo.rs", "fn a() {}\n");
        commit_all(path, "modify api/foo");
        write_file(path, "src/cli/bar.rs", "fn b() {}\n");
        commit_all(path, "modify cli/bar");
        write_file(path, "src/api/baz.rs", "fn c() {}\n");
        commit_all(path, "modify api/baz");

        let pf = PathFilter::new("src/api");
        let mut walker = Walker::new(&repo, Some(pf), None, 0, false, Arc::new(HashMap::new())).expect("walker");
        let (tx, rx) = crossbeam_channel::bounded::<GitMsg>(256);
        walker.load_more(100, &mut TreeDiffCache::new(), &tx, || false).expect("load_more");

        let summaries: Vec<String> = drain_commits(&rx).into_iter().map(|c| c.summary).collect();
        assert!(summaries.iter().any(|s| s == "modify api/foo"));
        assert!(summaries.iter().any(|s| s == "modify api/baz"));
        assert!(
            !summaries.iter().any(|s| s == "modify cli/bar"),
            "cli commit leaked through src/api spec: {summaries:?}",
        );
    }

    #[test]
    fn literal_path_classification() {
        use super::literal_path;
        // Plain files and directories are literal; a trailing slash is trimmed.
        assert!(literal_path("src/foo.rs").is_some());
        assert!(literal_path("src").is_some());
        assert!(literal_path("src/").is_some());
        // Globs, magic signatures, negation, and non-normal components fall back.
        assert!(literal_path("*.rs").is_none());
        assert!(literal_path("src/*.rs").is_none());
        assert!(literal_path("a[bc].rs").is_none());
        assert!(literal_path(":!target").is_none());
        assert!(literal_path("!foo").is_none());
        assert!(literal_path("../up").is_none());
        assert!(literal_path("").is_none());
    }

    #[test]
    fn walker_literal_file_filters_to_touching_commits() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();

        write_file(path, "src/foo.rs", "fn a() {}\n");
        commit_all(path, "add foo");
        write_file(path, "src/bar.rs", "fn b() {}\n");
        commit_all(path, "add bar"); // sibling under src/ — must NOT match foo.rs
        write_file(path, "src/foo.rs", "fn a() { x }\n");
        commit_all(path, "edit foo");

        let pf = PathFilter::new("src/foo.rs");
        let mut walker = Walker::new(&repo, Some(pf), None, 0, false, Arc::new(HashMap::new())).expect("walker");
        let (tx, rx) = crossbeam_channel::bounded::<GitMsg>(256);
        walker.load_more(100, &mut TreeDiffCache::new(), &tx, || false).expect("load_more");

        let summaries: Vec<String> = drain_commits(&rx).into_iter().map(|c| c.summary).collect();
        assert_eq!(summaries, vec!["edit foo".to_string(), "add foo".to_string()]);
    }

    #[test]
    fn walker_literal_file_catches_add_and_delete() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();

        write_file(path, "keep.txt", "x\n");
        commit_all(path, "unrelated"); // does not touch gone.txt
        write_file(path, "gone.txt", "here\n");
        commit_all(path, "add gone");
        std::fs::remove_file(path.join("gone.txt")).expect("rm gone.txt");
        commit_all(path, "delete gone");

        // Both the addition (None -> Some) and deletion (Some -> None)
        // register as a change under the entry-oid comparison.
        let pf = PathFilter::new("gone.txt");
        let mut walker = Walker::new(&repo, Some(pf), None, 0, false, Arc::new(HashMap::new())).expect("walker");
        let (tx, rx) = crossbeam_channel::bounded::<GitMsg>(256);
        walker.load_more(100, &mut TreeDiffCache::new(), &tx, || false).expect("load_more");

        let summaries: Vec<String> = drain_commits(&rx).into_iter().map(|c| c.summary).collect();
        assert_eq!(summaries, vec!["delete gone".to_string(), "add gone".to_string()]);
    }

    #[test]
    fn walker_glob_pathspec_falls_back_and_filters() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();

        write_file(path, "a.rs", "fn a() {}\n");
        commit_all(path, "rust file");
        write_file(path, "b.md", "hello\n");
        commit_all(path, "md file");
        write_file(path, "c.rs", "fn c() {}\n");
        commit_all(path, "another rust");

        // `*.md` carries a glob metachar, so this drives the Spec
        // (tree-diff) fallback rather than the literal fast-path.
        let pf = PathFilter::new("*.md");
        let mut walker = Walker::new(&repo, Some(pf), None, 0, false, Arc::new(HashMap::new())).expect("walker");
        let (tx, rx) = crossbeam_channel::bounded::<GitMsg>(256);
        walker.load_more(100, &mut TreeDiffCache::new(), &tx, || false).expect("load_more");

        let summaries: Vec<String> = drain_commits(&rx).into_iter().map(|c| c.summary).collect();
        assert_eq!(summaries, vec!["md file".to_string()]);
    }

    #[test]
    fn walker_start_id_walks_from_named_commit_only() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();

        // Three commits; capture the middle one as the start root.
        write_file(path, "a.txt", "1\n");
        commit_all(path, "first");
        write_file(path, "a.txt", "2\n");
        commit_all(path, "second");
        let middle = repo.head_id().expect("head after second").detach();
        write_file(path, "a.txt", "3\n");
        commit_all(path, "third");

        let mut walker = Walker::new(&repo, None, Some(middle), 0, false, Arc::new(HashMap::new())).expect("walker");
        let (tx, rx) = crossbeam_channel::bounded::<GitMsg>(256);
        walker.load_more(100, &mut TreeDiffCache::new(), &tx, || false).expect("load_more");

        let summaries: Vec<String> = drain_commits(&rx).into_iter().map(|c| c.summary).collect();
        // Walking from `second` should include `second` and `first` (its
        // ancestor), but never the descendant commit `third`.
        assert_eq!(summaries, vec!["second".to_string(), "first".to_string()]);
    }

    #[test]
    fn build_commit_info_preserves_full_author_for_search() {
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
}
