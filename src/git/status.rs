//! Working-tree status rendering: one `gix::status` sweep produces
//! the short-status header (`git status --short`-equivalent), the
//! staged section, and the unstaged section, all into the same
//! `DiffDocument`. Pre-cleanup this code did three separate
//! `repo.status()` passes; collapsing them into one is the 5c perf win.
//!
//! Untracked-file discovery is split off the diff critical path: the
//! diff-view entry point `compute_working_tree_diff` calls
//! `sweep_status` with `UntrackedFiles::None` (so the worktree dir
//! walk is skipped), parks a placeholder line in the status header,
//! and exposes the anchor via `DiffDocument.untracked_anchor`. The
//! worker spawns `collect_untracked_paths` on a side thread; when it
//! returns, an `InspectMsg::UntrackedFilesUpdate` splices the actual
//! `?? path` rows into place. The standalone status pane
//! (`compute_status`) keeps the synchronous full sweep.

use crate::{
    git::diff::{
        DiffSink, FileDiffOutput, empty_error_document, file_addition_output, file_deletion_output,
        file_full_context_output, file_modification_output, merge_file_output_into_sink, single_file_document,
    },
    model::{
        DiffDocument, DiffFlags, DiffLine, DiffLineKind, DiffStats, DiffTarget, FileSection, FileStat, StatusDocument,
    },
};
use anyhow::Result;
use gix::{ObjectId, status::UntrackedFiles};
use rayon::prelude::*;

/// Working-tree status pane. Synchronous full sweep — untracked files
/// are inline rather than streamed, since the status pane is the
/// dedicated "show me everything `git status` would" view.
///
/// The body is produced via `assemble_working_tree_body` with the same
/// renderers `compute_working_tree_diff` uses, so the staged/unstaged
/// rendering stays in lockstep across the two entry points.
pub fn compute_status(repo: &gix::Repository) -> StatusDocument {
    let (body, _, _, _, _, _) =
        assemble_working_tree_body(repo, UntrackedFiles::Collapsed, /* placeholder */ false);
    StatusDocument { lines: body }
}

/// Staged change normalised to the (path, oids) shape `rayon` can
/// chew on. The original `gix::diff::index::Change` references
/// borrowed `BString`s; this owns them so worker threads can take
/// ownership without coordinating lifetimes back to the iterator.
enum StagedRender {
    Addition {
        path: String,
        oid: ObjectId,
    },
    Deletion {
        path: String,
        oid: ObjectId,
    },
    /// Modification *and* rewrite collapse here — `git diff` displays
    /// renames as a modification at the destination path today, and
    /// the per-file render only needs old/new oids.
    Modification {
        path: String,
        old_oid: ObjectId,
        new_oid: ObjectId,
    },
}

impl StagedRender {
    fn path(&self) -> &str {
        match self {
            Self::Addition { path, .. } | Self::Deletion { path, .. } | Self::Modification { path, .. } => path,
        }
    }

    fn from_change(change: gix::diff::index::Change) -> Self {
        use gix::diff::index::Change;
        match change {
            Change::Addition { location, id, .. } => {
                Self::Addition { path: location.to_string(), oid: id.into_owned() }
            }
            Change::Deletion { location, id, .. } => {
                Self::Deletion { path: location.to_string(), oid: id.into_owned() }
            }
            Change::Modification { location, previous_id, id, .. } => Self::Modification {
                path: location.to_string(),
                old_oid: previous_id.into_owned(),
                new_oid: id.into_owned(),
            },
            Change::Rewrite { location, source_id, id, .. } => Self::Modification {
                path: location.to_string(),
                old_oid: source_id.into_owned(),
                new_oid: id.into_owned(),
            },
        }
    }
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
/// `untracked_files` controls whether the worktree dir walk runs:
/// `None` skips it entirely (so the resulting `short` map has no `??`
/// entries and `DirectoryContents` iterator items are not produced) —
/// the fast path used by `compute_working_tree_diff` so a separate
/// side thread can stream untracked entries in afterwards. `Collapsed`
/// keeps the legacy behavior for callers (like `compute_status`) that
/// want the full sweep synchronously.
///
/// Returns `Err` when the platform / iterator can't be created
/// (corrupt index, detached worktree, ODB error, …). Callers decide
/// how to surface that.
fn sweep_status(repo: &gix::Repository, untracked_files: UntrackedFiles) -> Result<StatusSweep> {
    use gix::status::{index_worktree, plumbing::index_as_worktree};
    use std::collections::BTreeMap;

    let platform = repo.status(gix::progress::Discard)?;
    let iter = platform.untracked_files(untracked_files).into_iter(Vec::new())?;

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
/// An empty `short` map means "actually clean" *only* when the caller
/// already collected untracked files synchronously. When
/// `untracked_pending` is true the caller is going to drop a
/// placeholder right after this block; emitting "Nothing to commit"
/// here would race the side-thread result and clobber it on apply.
fn render_short_status(
    short: &std::collections::BTreeMap<String, (char, char)>,
    untracked_pending: bool,
) -> Vec<DiffLine> {
    if short.is_empty() && !untracked_pending {
        return vec![DiffLine::new(DiffLineKind::Good, "Nothing to commit, working tree clean")];
    }
    if short.is_empty() {
        // Pending case: leave the block empty; the placeholder line
        // appended by the caller covers it until untracked arrives,
        // at which point the app picks the right message based on
        // what was found.
        return Vec::new();
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

/// Render the staged section from pre-collected changes. Each file's
/// diff is built on a rayon worker (see `render_diff_records` in
/// `diff.rs` for the matching pattern + the `ThreadSafeRepository`
/// rationale); the merge back into the sink is serial so guardrails
/// and ordering stay deterministic. Returns the number of files
/// emitted (or skipped by the guardrail) so the caller can fall back
/// to "(no staged changes)" when empty.
fn render_staged_section(
    repo: &gix::Repository,
    sink: &mut DiffSink<'_>,
    staged: Vec<gix::diff::index::Change>,
) -> usize {
    use crate::git::MAX_INLINE_DIFF_FILES;

    let renders: Vec<StagedRender> = staged.into_iter().map(StagedRender::from_change).collect();
    let headroom = MAX_INLINE_DIFF_FILES.saturating_sub(sink.stats.files);
    let (renderable, skipped) =
        if renders.len() > headroom { renders.split_at(headroom) } else { (renders.as_slice(), [].as_slice()) };

    let render_one = |worker_repo: &gix::Repository, item: &StagedRender| -> Option<FileDiffOutput> {
        match item {
            StagedRender::Addition { path, oid } => {
                let new = worker_repo.find_object(*oid).ok()?;
                Some(file_addition_output(path, &new.data))
            }
            StagedRender::Deletion { path, oid } => {
                let old = worker_repo.find_object(*oid).ok()?;
                Some(file_deletion_output(path, &old.data))
            }
            StagedRender::Modification { path, old_oid, new_oid } => {
                let old = worker_repo.find_object(*old_oid).ok()?;
                let new = worker_repo.find_object(*new_oid).ok()?;
                Some(file_modification_output(path, &old.data, &new.data))
            }
        }
    };

    let outputs: Vec<FileDiffOutput> = if renderable.len() >= crate::git::PARALLEL_FILE_THRESHOLD {
        let sync_repo = crate::git::sync_repo_without_object_cache(repo);
        renderable
            .par_iter()
            .map_init(|| sync_repo.to_thread_local(), |worker_repo, item| render_one(worker_repo, item))
            .flatten()
            .collect()
    } else {
        renderable.iter().filter_map(|item| render_one(repo, item)).collect()
    };

    let mut emitted = 0usize;
    for out in outputs {
        if sink.guardrail_exceeded() {
            sink.account_skipped_file(out.path);
        } else {
            merge_file_output_into_sink(sink, out);
        }
        emitted += 1;
    }
    if !skipped.is_empty() {
        let _ = sink.guardrail_exceeded();
        for item in skipped {
            sink.account_skipped_file(item.path().to_string());
            emitted += 1;
        }
    }
    emitted
}

/// Render the unstaged section. Same parallel-then-merge shape as
/// `render_staged_section`, with one wrinkle: the new side of each
/// modification comes from the worktree (`fs::read`) rather than the
/// ODB, so the rayon workers do the disk reads concurrently — useful
/// on slow filesystems or wide working trees.
fn render_unstaged_section(repo: &gix::Repository, sink: &mut DiffSink<'_>, items: Vec<UnstagedRender>) -> usize {
    use crate::git::MAX_INLINE_DIFF_FILES;

    let headroom = MAX_INLINE_DIFF_FILES.saturating_sub(sink.stats.files);
    let (renderable, skipped) =
        if items.len() > headroom { items.split_at(headroom) } else { (items.as_slice(), [].as_slice()) };

    let workdir = repo.workdir().map(|p| p.to_owned());
    let render_one = |worker_repo: &gix::Repository, item: &UnstagedRender| -> Option<FileDiffOutput> {
        match item {
            UnstagedRender::Removed { path, index_id } => {
                let old = worker_repo.find_object(*index_id).ok()?;
                Some(file_deletion_output(path, &old.data))
            }
            UnstagedRender::Modified { path, index_id } => {
                let new = workdir.as_ref().and_then(|wd| std::fs::read(wd.join(path)).ok()).unwrap_or_default();
                let old = worker_repo.find_object(*index_id).ok()?;
                Some(file_modification_output(path, &old.data, &new))
            }
        }
    };

    let outputs: Vec<FileDiffOutput> = if renderable.len() >= crate::git::PARALLEL_FILE_THRESHOLD {
        let sync_repo = crate::git::sync_repo_without_object_cache(repo);
        renderable
            .par_iter()
            .map_init(|| sync_repo.to_thread_local(), |worker_repo, item| render_one(worker_repo, item))
            .flatten()
            .collect()
    } else {
        renderable.iter().filter_map(|item| render_one(repo, item)).collect()
    };

    let mut emitted = 0usize;
    for out in outputs {
        if sink.guardrail_exceeded() {
            sink.account_skipped_file(out.path);
        } else {
            merge_file_output_into_sink(sink, out);
        }
        emitted += 1;
    }
    if !skipped.is_empty() {
        let _ = sink.guardrail_exceeded();
        for item in skipped {
            let path = match item {
                UnstagedRender::Removed { path, .. } | UnstagedRender::Modified { path, .. } => path.clone(),
            };
            sink.account_skipped_file(path);
            emitted += 1;
        }
    }
    emitted
}

/// Text of the faint placeholder reserved for untracked entries while
/// the side-thread walk is in flight. Lives at `body[untracked_anchor]`
/// until `InspectMsg::UntrackedFilesUpdate` arrives.
pub const UNTRACKED_PENDING_PLACEHOLDER: &str = "(scanning for untracked files…)";

/// Shared body assembly used by both `compute_working_tree_diff` and
/// `compute_status`. Returns the rendered `body` plus the aggregates
/// the diff-view entry point needs to stuff into a `DiffDocument`
/// (`files`, `stats`, `flags`, and the placeholder anchor when one
/// was inserted).
///
/// When `reserve_untracked_anchor` is true, a faint placeholder line
/// is appended right after the short-status section and its index is
/// returned in the last tuple slot. The placeholder is the contract
/// between this renderer and the app's `UntrackedFilesUpdate` handler:
/// finding the anchor means the document is still waiting for its
/// untracked rows.
fn assemble_working_tree_body(
    repo: &gix::Repository,
    untracked_files: UntrackedFiles,
    reserve_untracked_anchor: bool,
) -> (Vec<DiffLine>, Vec<FileStat>, DiffStats, DiffFlags, Option<usize>, Vec<FileSection>) {
    let mut body: Vec<DiffLine> = Vec::new();
    let mut files: Vec<FileStat> = Vec::new();
    let mut stats = DiffStats { files: 0, insertions: 0, deletions: 0 };
    let mut flags = DiffFlags::default();
    let mut sections: Vec<FileSection> = Vec::new();

    body.push(DiffLine::new(DiffLineKind::SectionTitle, "Working Tree Status"));
    body.push(DiffLine::new(DiffLineKind::Blank, ""));

    let sweep = sweep_status(repo, untracked_files);
    match &sweep {
        Ok(items) => body.extend(render_short_status(&items.short, reserve_untracked_anchor)),
        Err(e) => body.push(DiffLine::new(DiffLineKind::Faint, format!("Status query failed: {e}"))),
    }

    // Park the placeholder at the end of the short-status block so
    // splicing it later doesn't shift the staged/unstaged section
    // headers by more than the count of untracked rows that land.
    let untracked_anchor = if reserve_untracked_anchor {
        let idx = body.len();
        body.push(DiffLine::new(DiffLineKind::Faint, UNTRACKED_PENDING_PLACEHOLDER));
        Some(idx)
    } else {
        None
    };

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
        let mut sink = DiffSink {
            lines: &mut body,
            stats: &mut stats,
            files: &mut files,
            flags: &mut flags,
            sections: &mut sections,
        };
        if render_staged_section(repo, &mut sink, staged) == 0 {
            sink.lines.push(DiffLine::new(DiffLineKind::Faint, "(no staged changes)"));
        }
    }

    body.push(DiffLine::new(DiffLineKind::Blank, ""));
    body.push(DiffLine::new(DiffLineKind::SectionUnstaged, "── Unstaged ────────────────────────────────────────────"));
    body.push(DiffLine::new(DiffLineKind::Blank, ""));
    {
        let mut sink = DiffSink {
            lines: &mut body,
            stats: &mut stats,
            files: &mut files,
            flags: &mut flags,
            sections: &mut sections,
        };
        if render_unstaged_section(repo, &mut sink, unstaged) == 0 {
            sink.lines.push(DiffLine::new(DiffLineKind::Faint, "(no unstaged changes)"));
        }
    }

    (body, files, stats, flags, untracked_anchor, sections)
}

/// Assemble the working-tree diff document with the untracked walk
/// deferred to a side thread. The returned document has a
/// `untracked_anchor` pointing at a faint placeholder in `body`; the
/// worker spawns `collect_untracked_paths` separately and an
/// `InspectMsg::UntrackedFilesUpdate` replaces the placeholder once
/// the walk finishes. On a wide-checkout monorepo, skipping the dir
/// walk here is what closes the gap with `git diff`'s wall clock.
pub fn compute_working_tree_diff(repo: &gix::Repository, target: DiffTarget) -> DiffDocument {
    let (body, files, stats, flags, untracked_anchor, sections) =
        assemble_working_tree_body(repo, UntrackedFiles::None, /* placeholder */ true);
    DiffDocument { target, header: Vec::new(), body, files, stats, flags, untracked_anchor, sections }
}

/// Full-context single-file diff for the working tree: HEAD's blob
/// (empty when the file is new) against the on-disk bytes (empty when
/// deleted). The isolate view deliberately collapses the staged /
/// unstaged split — it answers "how does this file differ from HEAD",
/// with the whole file as context.
pub fn compute_worktree_file_diff(repo: &gix::Repository, path: &str) -> DiffDocument {
    compute_worktree_file_diff_inner(repo, path).unwrap_or_else(|e| empty_error_document(DiffTarget::WorkingTree, e))
}

fn compute_worktree_file_diff_inner(repo: &gix::Repository, path: &str) -> Result<DiffDocument> {
    let old = head_blob_bytes(repo, path)?.unwrap_or_default();
    let workdir = repo.workdir().ok_or_else(|| anyhow::anyhow!("repository has no worktree"))?;
    let new = std::fs::read(workdir.join(path)).unwrap_or_default();
    if old.is_empty() && new.is_empty() {
        anyhow::bail!("{path}: not in HEAD and not on disk");
    }
    let mode_meta = if old.is_empty() {
        Some("new file")
    } else if new.is_empty() {
        Some("deleted file")
    } else {
        None
    };
    let output = file_full_context_output(path, &old, &new, mode_meta);
    Ok(single_file_document(DiffTarget::WorkingTree, output))
}

/// Bytes of `path`'s blob at HEAD, `None` when the path doesn't exist
/// there (new file) or HEAD is unborn.
fn head_blob_bytes(repo: &gix::Repository, path: &str) -> Result<Option<Vec<u8>>> {
    let Ok(head) = repo.head_commit() else {
        return Ok(None);
    };
    match head.tree()?.lookup_entry_by_path(path)? {
        Some(entry) if entry.mode().is_blob() => Ok(Some(entry.object()?.data.clone())),
        _ => Ok(None),
    }
}

/// Walk just the untracked-files dimension of `gix::status`, returning
/// the entries the dir walk surfaces. Used by the worker's side thread
/// to fill in a `DiffDocument`'s placeholder anchor without blocking
/// the diff-view's first paint.
///
/// We re-enter `repo.status(...)` here rather than reusing the
/// fast-path sweep because `gix::status` exposes one combined
/// iterator: there's no public knob to ask for just the dir walk. The
/// extra index-walk this implies is the same constant cost the
/// fast-path already paid, run concurrently on the side thread. Net
/// CPU goes up; wall clock for the user-visible diff goes down, which
/// is the trade we want.
pub fn collect_untracked_paths(repo: &gix::Repository) -> Vec<String> {
    use gix::status::{Item, index_worktree};

    let Ok(platform) = repo.status(gix::progress::Discard) else {
        return Vec::new();
    };
    let Ok(iter) = platform.untracked_files(UntrackedFiles::Collapsed).into_iter(Vec::new()) else {
        return Vec::new();
    };

    let mut paths: Vec<String> = Vec::new();
    for item in iter.flatten() {
        if let Item::IndexWorktree(index_worktree::Item::DirectoryContents { entry, .. }) = item
            && matches!(entry.status, gix::dir::entry::Status::Untracked)
        {
            paths.push(entry.rela_path.to_string());
        }
    }
    paths.sort();
    paths
}

#[cfg(test)]
mod tests {
    use super::{
        UNTRACKED_PENDING_PLACEHOLDER, collect_untracked_paths, compute_status, compute_working_tree_diff,
        compute_worktree_file_diff,
    };
    use crate::{
        model::{DiffLineKind, DiffTarget},
        test_support::{commit_all, make_fixture_repo, run_git, write_file},
    };

    #[test]
    fn worktree_file_diff_shows_full_context_against_head() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();
        let base: String = (0..10).map(|i| format!("row{i}\n")).collect();
        write_file(path, "a.txt", &base);
        commit_all(path, "base");
        let mut changed: Vec<String> = (0..10).map(|i| format!("row{i}\n")).collect();
        changed[5] = "EDITED\n".to_string();
        write_file(path, "a.txt", &changed.concat());

        let doc = compute_worktree_file_diff(&repo, "a.txt");
        assert_eq!(doc.stats.files, 1);
        assert!(doc.body.iter().any(|l| matches!(l.kind, DiffLineKind::Add) && l.text == "EDITED"));
        assert!(
            doc.body.iter().any(|l| matches!(l.kind, DiffLineKind::Context) && l.text == "row0"),
            "context spans the whole file"
        );
        assert!(doc.body.iter().any(|l| matches!(l.kind, DiffLineKind::Context) && l.text == "row9"));
    }

    #[test]
    fn worktree_file_diff_handles_untracked_and_missing_paths() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();
        write_file(path, "tracked.txt", "x\n");
        commit_all(path, "base");

        // Untracked file: rendered as a pure addition against HEAD.
        std::fs::write(path.join("new.txt"), "fresh\n").expect("write");
        let doc = compute_worktree_file_diff(&repo, "new.txt");
        assert!(doc.body.iter().any(|l| matches!(l.kind, DiffLineKind::Add) && l.text == "fresh"));

        // Neither in HEAD nor on disk: error document.
        let doc = compute_worktree_file_diff(&repo, "missing.txt");
        assert!(doc.header.iter().any(|l| l.text.starts_with("Error:")));
    }

    #[test]
    fn working_tree_diff_renders_staged_and_unstaged_sections() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();

        write_file(path, "staged.txt", "one\ntwo\nthree\n");
        write_file(path, "unstaged.txt", "alpha\nbeta\n");
        commit_all(path, "baseline");

        write_file(path, "staged.txt", "one\nTWO\nthree\nfour\n");
        run_git(path, &["add", "staged.txt"]);

        write_file(path, "unstaged.txt", "alpha\nBETA\n");

        let doc = compute_working_tree_diff(&repo, DiffTarget::WorkingTree);

        // Diff body lines store text *without* the unified-diff prefix
        // (`+`/`-`/` `); the renderer adds it at draw time.
        let body_text: String = doc.body.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        assert!(body_text.contains("── Staged"), "staged section title present");
        assert!(body_text.contains("diff --git a/staged.txt b/staged.txt"));
        let has_four_add = doc.body.iter().any(|l| matches!(l.kind, DiffLineKind::Add) && l.text == "four");
        assert!(has_four_add, "staged diff contains Add line 'four'");
        assert!(body_text.contains("── Unstaged"), "unstaged section title present");
        assert!(body_text.contains("diff --git a/unstaged.txt b/unstaged.txt"));
        let has_beta_add = doc.body.iter().any(|l| matches!(l.kind, DiffLineKind::Add) && l.text == "BETA");
        assert!(has_beta_add, "unstaged diff contains Add line 'BETA'");

        let paths: Vec<&str> = doc.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"staged.txt"));
        assert!(paths.contains(&"unstaged.txt"));
        assert!(doc.stats.files >= 2);
        assert!(doc.stats.insertions > 0);

        // Untracked walk is deferred: a placeholder sits at the
        // anchor index, and `?? path` rows have not been emitted yet.
        let anchor = doc.untracked_anchor.expect("working-tree diff defers untracked walk");
        assert_eq!(doc.body[anchor].text, UNTRACKED_PENDING_PLACEHOLDER);
        assert!(!doc.body.iter().any(|l| l.text.starts_with("?? ")), "no untracked rows on the diff critical path");
    }

    #[test]
    fn working_tree_diff_empty_sections_show_placeholder_text() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();
        write_file(path, "a.txt", "hi\n");
        commit_all(path, "baseline");

        let doc = compute_working_tree_diff(&repo, DiffTarget::WorkingTree);
        let body_text: String = doc.body.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");

        // The "Nothing to commit" line is now chosen at apply-time by
        // the app when it learns the untracked list is also empty —
        // here we just assert the staged/unstaged placeholders and
        // the pending untracked placeholder are present.
        assert!(body_text.contains(UNTRACKED_PENDING_PLACEHOLDER));
        assert!(body_text.contains("(no staged changes)"));
        assert!(body_text.contains("(no unstaged changes)"));
        assert!(doc.untracked_anchor.is_some(), "anchor reserved for the pending untracked walk");
        assert_eq!(doc.stats.files, 0);
        assert_eq!(doc.stats.insertions, 0);
        assert_eq!(doc.stats.deletions, 0);
    }

    #[test]
    fn collect_untracked_paths_returns_only_untracked_entries() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();
        write_file(path, "tracked.txt", "hi\n");
        commit_all(path, "baseline");
        // Modify tracked, add a new untracked file, stage one change.
        write_file(path, "tracked.txt", "hi\nthere\n");
        write_file(path, "untracked_a.txt", "x\n");
        write_file(path, "untracked_b.txt", "y\n");

        let paths = collect_untracked_paths(&repo);
        assert_eq!(paths, vec!["untracked_a.txt".to_string(), "untracked_b.txt".to_string()]);
    }

    #[test]
    fn compute_status_includes_untracked_inline() {
        // The standalone status pane stays synchronous: untracked
        // rows are in the document by the time it returns.
        let (td, repo) = make_fixture_repo();
        let path = td.path();
        write_file(path, "tracked.txt", "hi\n");
        commit_all(path, "baseline");
        write_file(path, "new_untracked.txt", "x\n");

        let doc = compute_status(&repo);
        let body_text: String = doc.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        assert!(body_text.contains("?? new_untracked.txt"));
        assert!(
            !body_text.contains(UNTRACKED_PENDING_PLACEHOLDER),
            "status pane should not emit the async placeholder"
        );
        let _ = DiffLineKind::Faint;
    }
}
