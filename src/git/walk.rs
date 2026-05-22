//! Commit walking + pathspec filtering. `Walker` owns the `gix`
//! ancestor iterator and a parsed pathspec; `build_commit_info`
//! decodes one commit into a `CommitRecord` for the UI.

use crate::{
    git::{GitMsg, TreeDiffCache, compute_tree_diff_records, graph, history::HistoryMsg, meta::relative_time},
    model::{CommitRecord, CommitSearchText, PathFilter, RefKind, RefLabel},
};
use anyhow::Result;
use compact_str::CompactString;
use crossbeam_channel::Sender;
use gix::{ObjectId, bstr::ByteSlice, prelude::ObjectIdExt};
use std::{collections::HashMap, sync::Arc};

pub struct Walker<'r> {
    repo: &'r gix::Repository,
    pub refs_map: Arc<HashMap<ObjectId, Vec<RefLabel>>>,
    /// `None` when the `--graph` CLI flag is off — keeps the per-commit lane
    /// bookkeeping out of the hot path entirely.
    graph_state: Option<graph::GraphState>,
    iter: Option<gix::revision::Walk<'r>>,
    pub done: bool,
    /// Parsed once at construction; held in `&mut` form per call because
    /// `gix::pathspec::Search::pattern_matching_relative_path` mutates
    /// internal counters as it matches.
    pathspec: Option<gix::pathspec::Search>,
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
    pub fn load_more(&mut self, requested: usize, cache: &mut TreeDiffCache, msg_tx: &Sender<GitMsg>) -> Result<usize> {
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
    let tags_lower = tags_lower_from_refs(&refs);
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
        search: CommitSearchText { author_lower, summary_lower, tags_lower },
    })
}

/// Build the lowercased, space-joined tag-name blob stored on a commit's
/// `CommitSearchText`. Shared between fresh builds in the walker and the
/// `RefsLoaded` backfill path that fills refs in on commits which were
/// emitted before the refs table was ready.
pub fn tags_lower_from_refs(refs: &[RefLabel]) -> CompactString {
    let mut out = String::new();
    for r in refs.iter().filter(|r| matches!(r.kind, RefKind::Tag)) {
        if !out.is_empty() {
            out.push(' ');
        }
        // Tag names rarely contain uppercase, but lowercasing here means
        // `commit_matches` can stick to a single `.contains` against the
        // already-lowercased query.
        out.extend(r.name.chars().flat_map(|c| c.to_lowercase()));
    }
    out.into()
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
        walker.load_more(100, &mut TreeDiffCache::new(), &tx).expect("load_more");

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
        walker.load_more(100, &mut TreeDiffCache::new(), &tx).expect("load_more");

        let summaries: Vec<String> = drain_commits(&rx).into_iter().map(|c| c.summary).collect();
        assert!(summaries.iter().any(|s| s == "modify api/foo"));
        assert!(summaries.iter().any(|s| s == "modify api/baz"));
        assert!(
            !summaries.iter().any(|s| s == "modify cli/bar"),
            "cli commit leaked through src/api spec: {summaries:?}",
        );
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
        walker.load_more(100, &mut TreeDiffCache::new(), &tx).expect("load_more");

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
