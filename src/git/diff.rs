//! Commit-diff rendering: the entry points called from the worker's
//! Inspect request handler, plus the per-file render helpers shared
//! with the working-tree status path. Independent of any walk state.

use crate::{
    git::{
        MAX_FILE_VIEW_DIFF_BYTES, MAX_INLINE_DIFF_BYTES, MAX_INLINE_DIFF_FILES, MAX_INLINE_DIFF_LINES,
        PARALLEL_FILE_THRESHOLD, TreeDiffCache, compute_tree_diff_records, meta::format_timestamp,
    },
    model::{DiffDocument, DiffFlags, DiffLine, DiffLineKind, DiffStats, DiffTarget, FileSection, FileStat},
};
use anyhow::Result;
use gix::{ObjectId, bstr::ByteSlice};
use imara_diff::{Algorithm, Diff, Hunk, InternedInput};
use rayon::prelude::*;

/// Lines of context preserved on either side of a hunk. Matches both
/// `git diff -U3` (the unified-diff default) and the value used by
/// `imara_diff::UnifiedDiffConfig::default()`. Hunks closer than
/// `2 * HUNK_CONTEXT_LEN` lines are merged into one block — same rule
/// `git diff` and `imara-diff`'s built-in unified printer use.
const HUNK_CONTEXT_LEN: u32 = 3;

pub fn empty_error_document(target: DiffTarget, e: anyhow::Error) -> DiffDocument {
    DiffDocument {
        target,
        header: vec![DiffLine::new(DiffLineKind::Faint, format!("Error: {e}"))],
        body: Vec::new(),
        files: Vec::new(),
        stats: DiffStats { files: 0, insertions: 0, deletions: 0 },
        flags: DiffFlags::default(),
        untracked_anchor: None,
        sections: Vec::new(),
    }
}

/// `scope` restricts the rendered diff to files matching the CLI
/// pathspec (`rit <path>` + the diff pane's `f` toggle). `None` renders
/// the commit's full diff. Filtering happens on the tree-diff records —
/// before any blob is fetched — so a scoped diff of a wide commit only
/// pays for the files it shows.
pub fn compute_commit_diff(
    repo: &gix::Repository,
    id: ObjectId,
    cache: &mut TreeDiffCache,
    scope: Option<&mut gix::pathspec::Search>,
) -> DiffDocument {
    let target = DiffTarget::Commit(id);
    compute_commit_diff_inner(repo, id, target, cache, scope).unwrap_or_else(|e| empty_error_document(target, e))
}

fn compute_commit_diff_inner(
    repo: &gix::Repository,
    id: ObjectId,
    target: DiffTarget,
    cache: &mut TreeDiffCache,
    scope: Option<&mut gix::pathspec::Search>,
) -> Result<DiffDocument> {
    let commit_obj = repo.find_object(id)?.try_into_commit()?;
    let decoded = commit_obj.decode()?;
    let mut header: Vec<DiffLine> = Vec::new();
    let mut body: Vec<DiffLine> = Vec::new();
    let mut stats = DiffStats { files: 0, insertions: 0, deletions: 0 };
    let mut files: Vec<FileStat> = Vec::new();

    header.push(DiffLine::new(DiffLineKind::CommitHeader, format!("commit {id}")));
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
    let message = decoded.message();
    header.push(DiffLine::new(DiffLineKind::Message, format!("    {}", message.title.to_str_lossy())));
    if let Some(body) = message.body {
        for line in body.to_str_lossy().lines() {
            header.push(DiffLine::new(DiffLineKind::Message, format!("    {line}")));
        }
    }
    header.push(DiffLine::new(DiffLineKind::Blank, ""));

    let parent_ids: Vec<ObjectId> = decoded.parents().collect();

    // Cached tree-diff records. The pathspec filter populated this
    // during walking for every commit it considered; on a pathspec
    // walk this is a cache hit.
    let mut records = compute_tree_diff_records(repo, parent_ids.first().copied(), id, cache)?.to_vec();

    // The recorder emits records for intermediate trees as well as
    // blobs; only blob records become rendered file diffs, so the
    // hidden-file count must ignore the tree entries or it would
    // overstate what scoping removed.
    let blob_count =
        |records: &[gix::diff::tree::recorder::Change]| records.iter().filter(|c| change_is_blob(c)).count();
    let mut scoped_out = 0usize;
    if let Some(search) = scope {
        // Attribute-driven pathspec magic is unsupported, same as the
        // walker's matcher — the stub satisfies the signature.
        let mut attrs = |_: &_, _: _, _: _, _: &mut _| true;
        let before = blob_count(&records);
        records.retain(|change| {
            let path = change_path_bstr(change);
            search.pattern_matching_relative_path(path, Some(false), &mut attrs).is_some()
        });
        scoped_out = before - blob_count(&records);
    }

    let mut flags = DiffFlags::default();
    let mut sections: Vec<FileSection> = Vec::new();
    {
        let mut sink = DiffSink {
            lines: &mut body,
            stats: &mut stats,
            files: &mut files,
            flags: &mut flags,
            sections: &mut sections,
        };
        render_diff_records(repo, &records, &mut sink);
    }

    if scoped_out > 0 {
        body.push(DiffLine::new(
            DiffLineKind::Faint,
            format!(
                "… {scoped_out} changed file{} outside the path filter hidden (f shows the full diff)",
                if scoped_out == 1 { "" } else { "s" },
            ),
        ));
    }

    Ok(DiffDocument { target, header, body, files, stats, flags, untracked_anchor: None, sections })
}

/// Render previously-computed tree-diff records into the sink. Split
/// out of the old `diff_trees` so the cache lookup can sit outside the
/// renderer.
///
/// Per-file diff work is independent — same blobs in, same hunks out
/// — so the records that survive the file-count guardrail are
/// rendered in parallel via rayon and concatenated serially into the
/// sink. The serial concat enforces the line-count guardrail (which
/// depends on cumulative output size and can't be applied during
/// parallel rendering).
pub fn render_diff_records(
    repo: &gix::Repository,
    records: &[gix::diff::tree::recorder::Change],
    sink: &mut DiffSink<'_>,
) {
    use gix::diff::tree::recorder::Change;

    // File-count cap kicks in before any blob is fetched. `headroom`
    // is how many more files the sink can absorb full diffs for;
    // anything past that is accounted as a skipped file with the
    // truncation note pinned to the document.
    let headroom = MAX_INLINE_DIFF_FILES.saturating_sub(sink.stats.files);
    let (render_records, skip_records) = if records.len() > headroom {
        let (a, b) = records.split_at(headroom);
        (a, Some(b))
    } else {
        (records, None)
    };

    // Render every surviving record. Above `PARALLEL_FILE_THRESHOLD`
    // we go through rayon's `map_init`: `gix::Repository` is `Send`
    // but `!Sync` (interior `RefCell`s for pack caches, inflate
    // buffers, etc.) so each worker needs its own clone — gix's
    // `ThreadSafeRepository` (`into_sync()` + `to_thread_local()`)
    // does that without re-opening the repo. Below the threshold the
    // per-call rayon setup costs more than it saves on a 1- or
    // 2-file diff, so we stay serial.
    let render_one = |worker_repo: &gix::Repository, change: &Change| -> Option<FileDiffOutput> {
        match change {
            Change::Addition { entry_mode, oid, path, .. } if entry_mode.is_blob() => {
                let p = path.to_str_lossy().into_owned();
                let new = worker_repo.find_object(*oid).ok()?;
                Some(file_addition_output(&p, &new.data))
            }
            Change::Deletion { entry_mode, oid, path, .. } if entry_mode.is_blob() => {
                let p = path.to_str_lossy().into_owned();
                let old = worker_repo.find_object(*oid).ok()?;
                Some(file_deletion_output(&p, &old.data))
            }
            Change::Modification { entry_mode, previous_oid, oid, path, .. } if entry_mode.is_blob() => {
                let p = path.to_str_lossy().into_owned();
                let old = worker_repo.find_object(*previous_oid).ok()?;
                let new = worker_repo.find_object(*oid).ok()?;
                Some(file_modification_output(&p, &old.data, &new.data))
            }
            _ => None,
        }
    };

    let outputs: Vec<FileDiffOutput> = if render_records.len() >= PARALLEL_FILE_THRESHOLD {
        let sync_repo = crate::git::sync_repo_without_object_cache(repo);
        render_records
            .par_iter()
            .map_init(|| sync_repo.to_thread_local(), |worker_repo, change| render_one(worker_repo, change))
            .flatten()
            .collect()
    } else {
        render_records.iter().filter_map(|change| render_one(repo, change)).collect()
    };

    for out in outputs {
        if sink.guardrail_exceeded() {
            sink.account_skipped_file(out.path);
            continue;
        }
        merge_file_output_into_sink(sink, out);
    }

    if let Some(skipped) = skip_records {
        // Trip the truncation note once before draining the tail so
        // the message lines up with the file-count guardrail caller.
        let _ = sink.guardrail_exceeded();
        for change in skipped {
            sink.account_skipped_file(change_path(change));
        }
    }
}

/// Mutable accumulator threaded through every file-render call. Keeps the
/// four output collections (`lines`, `stats`, `files`, `flags`) together
/// so call sites just pass `sink` instead of four borrows, and so the
/// guardrail logic stays in one place.
pub struct DiffSink<'a> {
    pub lines: &'a mut Vec<DiffLine>,
    pub stats: &'a mut DiffStats,
    pub files: &'a mut Vec<FileStat>,
    pub flags: &'a mut DiffFlags,
    /// Per-file section starts, recorded as each file's lines are
    /// merged. Ends up on `DiffDocument::sections`.
    pub sections: &'a mut Vec<FileSection>,
}

impl DiffSink<'_> {
    /// True once a file-count or line-count guardrail has fired. Callers
    /// should stop materialising hunks but still call `account_skipped_file`
    /// so the diffstat counts every changed file.
    pub fn guardrail_exceeded(&mut self) -> bool {
        if self.stats.files >= MAX_INLINE_DIFF_FILES {
            self.note_truncation(format!(
                "… {} files changed; remaining file diffs suppressed (>{} files; t lists them, o opens one)",
                self.stats.files, MAX_INLINE_DIFF_FILES,
            ));
            return true;
        }
        if self.lines.len() >= MAX_INLINE_DIFF_LINES {
            self.note_truncation(format!(
                "… diff truncated at {MAX_INLINE_DIFF_LINES} lines; remaining files summarised (t lists files, o opens one)",
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

    pub fn account_skipped_file(&mut self, path: String) {
        self.stats.files += 1;
        self.files.push(FileStat { path, additions: 0, deletions: 0 });
    }

    fn record_file(&mut self, path: String, additions: usize, deletions: usize) {
        self.stats.files += 1;
        self.files.push(FileStat { path, additions, deletions });
    }
}

/// One file's worth of diff output, ready to fold into a `DiffSink`.
/// The split between this pure-data variant and the sink-pushing
/// renderers (`render_file_addition` / `…_deletion` / `…_modification`)
/// is what lets the commit-diff render and the staged/unstaged
/// sections parallelize per-file work via rayon: each task produces a
/// `FileDiffOutput`, and the final merge into the sink happens
/// serially so guardrail bookkeeping and ordering stay deterministic.
pub struct FileDiffOutput {
    pub path: String,
    pub lines: Vec<DiffLine>,
    pub additions: usize,
    pub deletions: usize,
    /// `Some` when the file was suppressed by `classify_skip` —
    /// merge code bumps the matching counter on `DiffFlags`.
    pub skip_reason: Option<SkipReason>,
}

/// Pure variant of `render_file_addition` — produces a `FileDiffOutput`
/// instead of mutating a sink. Safe to call from a rayon worker.
pub fn file_addition_output(path: &str, new: &[u8]) -> FileDiffOutput {
    let mut lines: Vec<DiffLine> = Vec::new();
    push_file_headers(&mut lines, path, Some("new file"));
    let (additions, deletions, skip_reason) = match classify_skip(&[], new) {
        Some(reason) => {
            lines.push(DiffLine::new(DiffLineKind::Faint, skip_summary_text(&reason, new.len())));
            (0, 0, Some(reason))
        }
        None => {
            let mut count = 0usize;
            for line in new.to_str_lossy().lines() {
                count += 1;
                lines.push(DiffLine::new(DiffLineKind::Add, line));
            }
            (count, 0, None)
        }
    };
    FileDiffOutput { path: path.to_string(), lines, additions, deletions, skip_reason }
}

/// Pure variant of `render_file_deletion`.
pub fn file_deletion_output(path: &str, old: &[u8]) -> FileDiffOutput {
    let mut lines: Vec<DiffLine> = Vec::new();
    push_file_headers(&mut lines, path, Some("deleted file"));
    let (additions, deletions, skip_reason) = match classify_skip(old, &[]) {
        Some(reason) => {
            lines.push(DiffLine::new(DiffLineKind::Faint, skip_summary_text(&reason, old.len())));
            (0, 0, Some(reason))
        }
        None => {
            let mut count = 0usize;
            for line in old.to_str_lossy().lines() {
                count += 1;
                lines.push(DiffLine::new(DiffLineKind::Del, line));
            }
            (0, count, None)
        }
    };
    FileDiffOutput { path: path.to_string(), lines, additions, deletions, skip_reason }
}

/// Pure variant of `render_file_modification`.
pub fn file_modification_output(path: &str, old: &[u8], new: &[u8]) -> FileDiffOutput {
    let mut lines: Vec<DiffLine> = Vec::new();
    push_file_headers(&mut lines, path, None);
    let (additions, deletions, skip_reason) = match classify_skip(old, new) {
        Some(reason) => {
            lines.push(DiffLine::new(DiffLineKind::Faint, skip_summary_text(&reason, old.len().max(new.len()))));
            (0, 0, Some(reason))
        }
        None => {
            let (file_add, file_del) = render_unified_into(&mut lines, old, new);
            (file_add, file_del, None)
        }
    };
    FileDiffOutput { path: path.to_string(), lines, additions, deletions, skip_reason }
}

/// Full-context variant of the per-file renderers: emits the *entire*
/// file with changed lines interleaved in place, instead of ±3-line
/// hunks. Powers the single-file view (`o` in the diff pane), where the
/// point is reading a change in the context of the whole file. Runs
/// with the raised `MAX_FILE_VIEW_DIFF_BYTES` cap — the view is
/// on-demand and single-file, so the inline guardrails don't apply.
pub fn file_full_context_output(path: &str, old: &[u8], new: &[u8], mode_meta: Option<&str>) -> FileDiffOutput {
    let mut lines: Vec<DiffLine> = Vec::new();
    push_file_headers(&mut lines, path, mode_meta);
    let (additions, deletions, skip_reason) = match classify_skip_with_cap(old, new, MAX_FILE_VIEW_DIFF_BYTES) {
        Some(reason) => {
            lines.push(DiffLine::new(DiffLineKind::Faint, file_view_skip_text(&reason, old.len().max(new.len()))));
            (0, 0, Some(reason))
        }
        None => {
            let (file_add, file_del) = render_full_context_into(&mut lines, old, new);
            (file_add, file_del, None)
        }
    };
    FileDiffOutput { path: path.to_string(), lines, additions, deletions, skip_reason }
}

/// `render_unified_into` without the context clamp: every unchanged
/// line is emitted as `Context`, so no hunk headers are needed — the
/// output *is* the file, with deletions inlined where they were
/// removed and additions where they landed.
fn render_full_context_into(lines: &mut Vec<DiffLine>, old: &[u8], new: &[u8]) -> (usize, usize) {
    let input = InternedInput::new(old, new);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    // Same slider postprocessing as the hunk renderer, so the isolated
    // view of a file shows the same add/del lines the inline diff did.
    diff.postprocess_lines(&input);

    let before_total = input.before.len() as u32;
    let mut file_add = 0usize;
    let mut file_del = 0usize;
    let mut cursor = 0u32;
    for h in diff.hunks() {
        while cursor < h.before.start {
            push_token_line(lines, DiffLineKind::Context, &input, cursor, /* before */ true);
            cursor += 1;
        }
        for i in h.before.start..h.before.end {
            push_token_line(lines, DiffLineKind::Del, &input, i, /* before */ true);
            file_del += 1;
        }
        cursor = h.before.end;
        for i in h.after.start..h.after.end {
            push_token_line(lines, DiffLineKind::Add, &input, i, /* before */ false);
            file_add += 1;
        }
    }
    while cursor < before_total {
        push_token_line(lines, DiffLineKind::Context, &input, cursor, /* before */ true);
        cursor += 1;
    }
    (file_add, file_del)
}

/// Render one file of `id`'s diff with full file context — the
/// document behind `InspectReq::LoadFileDiff` for commit targets. The
/// tree-diff records are a cache hit when the user is isolating a file
/// from the commit diff already on screen.
pub fn compute_file_diff(repo: &gix::Repository, id: ObjectId, path: &str, cache: &mut TreeDiffCache) -> DiffDocument {
    let target = DiffTarget::Commit(id);
    compute_file_diff_inner(repo, id, target, path, cache).unwrap_or_else(|e| empty_error_document(target, e))
}

fn compute_file_diff_inner(
    repo: &gix::Repository,
    id: ObjectId,
    target: DiffTarget,
    path: &str,
    cache: &mut TreeDiffCache,
) -> Result<DiffDocument> {
    use gix::diff::tree::recorder::Change;

    let commit_obj = repo.find_object(id)?.try_into_commit()?;
    let parent_id = commit_obj.decode()?.parents().next();
    let records = compute_tree_diff_records(repo, parent_id, id, cache)?;
    let change = records
        .iter()
        .find(|c| change_is_blob(c) && change_path_bstr(c).to_str_lossy() == path)
        .ok_or_else(|| anyhow::anyhow!("{path} was not changed in this commit"))?;

    let output = match change {
        Change::Addition { oid, .. } => {
            let new = repo.find_object(*oid)?;
            file_full_context_output(path, &[], &new.data, Some("new file"))
        }
        Change::Deletion { oid, .. } => {
            let old = repo.find_object(*oid)?;
            file_full_context_output(path, &old.data, &[], Some("deleted file"))
        }
        Change::Modification { previous_oid, oid, .. } => {
            let old = repo.find_object(*previous_oid)?;
            let new = repo.find_object(*oid)?;
            file_full_context_output(path, &old.data, &new.data, None)
        }
    };

    Ok(single_file_document(target, output))
}

/// Wrap one `FileDiffOutput` into a standalone `DiffDocument` — shared
/// by the commit and working-tree single-file paths. No commit header:
/// the takeover view's title already names the commit and path, so the
/// file content starts at the top of the pane.
pub fn single_file_document(target: DiffTarget, output: FileDiffOutput) -> DiffDocument {
    let mut body: Vec<DiffLine> = Vec::new();
    let mut stats = DiffStats { files: 0, insertions: 0, deletions: 0 };
    let mut files: Vec<FileStat> = Vec::new();
    let mut flags = DiffFlags::default();
    let mut sections: Vec<FileSection> = Vec::new();
    {
        let mut sink = DiffSink {
            lines: &mut body,
            stats: &mut stats,
            files: &mut files,
            flags: &mut flags,
            sections: &mut sections,
        };
        merge_file_output_into_sink(&mut sink, output);
    }
    DiffDocument { target, header: Vec::new(), body, files, stats, flags, untracked_anchor: None, sections }
}

/// Fold a per-file output into a sink, applying its skip/flag updates
/// and recording the file in the diffstat. Used by both the
/// sink-pushing wrappers below and the parallel renderers.
pub fn merge_file_output_into_sink(sink: &mut DiffSink<'_>, output: FileDiffOutput) {
    if let Some(reason) = output.skip_reason {
        match reason {
            SkipReason::Binary => sink.flags.skipped_binary_files += 1,
            SkipReason::Oversize => sink.flags.skipped_large_files += 1,
        }
        sink.flags.truncated = true;
    }
    sink.stats.insertions += output.additions;
    sink.stats.deletions += output.deletions;
    sink.sections.push(FileSection { path: output.path.clone(), body_start: sink.lines.len() });
    sink.lines.extend(output.lines);
    sink.record_file(output.path, output.additions, output.deletions);
}

/// Sink-pushing wrapper around `file_addition_output`. Kept so the
/// working-tree status renderer (which is already serial) doesn't
/// have to construct + merge by hand.
pub fn render_file_addition(sink: &mut DiffSink<'_>, path: &str, new: &[u8]) {
    merge_file_output_into_sink(sink, file_addition_output(path, new));
}

/// Sink-pushing wrapper around `file_deletion_output`.
pub fn render_file_deletion(sink: &mut DiffSink<'_>, path: &str, old: &[u8]) {
    merge_file_output_into_sink(sink, file_deletion_output(path, old));
}

/// Sink-pushing wrapper around `file_modification_output`. Shared
/// between commit diffs (HEAD vs HEAD~1), staged diffs (HEAD vs
/// index), and unstaged diffs (index vs worktree).
///
/// Uses `imara-diff`'s Histogram algorithm (a port of git's libxdiff).
/// On text-heavy real-world inputs it consistently outperforms the
/// `similar` crate's Myers implementation, which was previously the
/// algorithmic gap between `rit`'s diff render and `git diff`.
pub fn render_file_modification(sink: &mut DiffSink<'_>, path: &str, old: &[u8], new: &[u8]) {
    merge_file_output_into_sink(sink, file_modification_output(path, old, new));
}

/// Text used by the "skipped" placeholder when `classify_skip` fires.
/// Shared between the pure and sink-pushing render paths so they
/// produce byte-identical lines.
fn skip_summary_text(reason: &SkipReason, biggest_side_bytes: usize) -> String {
    match reason {
        SkipReason::Binary => "Binary file — diff suppressed".to_string(),
        SkipReason::Oversize => {
            let kib = biggest_side_bytes / 1024;
            format!(
                "Large file ({kib} KiB) — diff suppressed (>{} KiB; o opens the full file)",
                MAX_INLINE_DIFF_BYTES / 1024
            )
        }
    }
}

/// Skip text for the single-file view, which runs with the raised
/// `MAX_FILE_VIEW_DIFF_BYTES` cap and where "press o" would be
/// circular advice.
fn file_view_skip_text(reason: &SkipReason, biggest_side_bytes: usize) -> String {
    match reason {
        SkipReason::Binary => "Binary file — diff suppressed".to_string(),
        SkipReason::Oversize => {
            let kib = biggest_side_bytes / 1024;
            format!(
                "Large file ({kib} KiB) — exceeds even the single-file cap (>{} KiB)",
                MAX_FILE_VIEW_DIFF_BYTES / 1024
            )
        }
    }
}

/// Drive `imara-diff` over `old` / `new` byte slices and emit hunks
/// into the provided `lines` buffer. Returns `(additions, deletions)`
/// for the file; the caller folds them into stats. Mirrors the
/// structure of `imara_diff::UnifiedDiff` (so hunk grouping and
/// headers match `git diff -U3`) but pushes structured `DiffLine`s
/// rather than a unified-diff string.
///
/// Takes `&mut Vec<DiffLine>` rather than `&mut DiffSink` so the
/// pure file-output builders can call it from a rayon worker without
/// touching the shared sink.
fn render_unified_into(lines: &mut Vec<DiffLine>, old: &[u8], new: &[u8]) -> (usize, usize) {
    let input = InternedInput::new(old, new);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    // Indent-based slider postprocessing — same default `git diff` uses
    // — produces hunks that line up on syntactic boundaries instead of
    // sliding into adjacent equal lines. Skipping it would produce
    // technically-correct but visually noisier diffs.
    diff.postprocess_lines(&input);

    let before_total = input.before.len() as u32;
    let after_total = input.after.len() as u32;

    let mut file_add = 0usize;
    let mut file_del = 0usize;
    let mut hunks = diff.hunks().peekable();
    while let Some(first) = hunks.next() {
        // Collect a chain of hunks whose context windows overlap. The
        // merge condition `gap <= 2 * context_len` matches
        // `imara_diff::UnifiedDiff`'s rule, which in turn matches
        // `git diff -U3`.
        let mut group: Vec<Hunk> = vec![first];
        while let Some(next) = hunks.peek() {
            let last_end = group.last().map(|h| h.before.end).unwrap_or(0);
            let gap = next.before.start.saturating_sub(last_end);
            if gap <= 2 * HUNK_CONTEXT_LEN {
                // The `peek().is_some()` immediately above guarantees
                // `next()` returns Some; `unwrap_or_default()` keeps
                // the `unwrap_used` lint quiet without panicking even
                // in the (impossible) None case.
                group.push(hunks.next().unwrap_or_default());
            } else {
                break;
            }
        }

        let group_before_start = group.first().map(|h| h.before.start).unwrap_or(0).saturating_sub(HUNK_CONTEXT_LEN);
        let group_before_end = (group.last().map(|h| h.before.end).unwrap_or(0) + HUNK_CONTEXT_LEN).min(before_total);
        let group_after_start = group.first().map(|h| h.after.start).unwrap_or(0).saturating_sub(HUNK_CONTEXT_LEN);
        let group_after_end = (group.last().map(|h| h.after.end).unwrap_or(0) + HUNK_CONTEXT_LEN).min(after_total);

        lines.push(DiffLine::new(
            DiffLineKind::HunkHeader,
            hunk_header_str(
                group_before_start,
                group_before_end.saturating_sub(group_before_start),
                group_after_start,
                group_after_end.saturating_sub(group_after_start),
            ),
        ));

        // Walk the before side as a cursor; for each hunk emit the
        // intervening context (lines `cursor..hunk.before.start`),
        // then the hunk's deletions, then the hunk's additions, then
        // advance the cursor past the deletions.
        let mut cursor = group_before_start;
        for h in &group {
            while cursor < h.before.start {
                push_token_line(lines, DiffLineKind::Context, &input, cursor, /* before */ true);
                cursor += 1;
            }
            for i in h.before.start..h.before.end {
                push_token_line(lines, DiffLineKind::Del, &input, i, /* before */ true);
                file_del += 1;
            }
            cursor = h.before.end;
            for i in h.after.start..h.after.end {
                push_token_line(lines, DiffLineKind::Add, &input, i, /* before */ false);
                file_add += 1;
            }
        }
        while cursor < group_before_end {
            push_token_line(lines, DiffLineKind::Context, &input, cursor, /* before */ true);
            cursor += 1;
        }
    }

    (file_add, file_del)
}

/// Pull line bytes out of the interner for a given token offset on
/// either side and push them as a `DiffLine` of `kind`. Trailing
/// newline (kept by `imara-diff`'s tokenizer to detect EOL changes)
/// is stripped — the renderer prepends its own marker at draw time.
///
/// `idx` is always a valid offset into the chosen side: both callers
/// (`render_unified_into` and `render_full_context_into`) only ever feed
/// indices pulled from `Diff::hunks()` on this same `InternedInput`, or a
/// cursor clamped to the side's length. Likewise for the after side
/// (`h.after.start..h.after.end` ⊆ `0..input.after.len()`).
fn push_token_line(
    lines: &mut Vec<DiffLine>,
    kind: DiffLineKind,
    input: &InternedInput<&[u8]>,
    idx: u32,
    before: bool,
) {
    #[expect(clippy::indexing_slicing, reason = "idx is bounded by render_unified_into's hunk walk; see fn doc")]
    let token = if before { input.before[idx as usize] } else { input.after[idx as usize] };
    let bytes = input.interner[token];
    let trimmed = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    lines.push(DiffLine::new(kind, String::from_utf8_lossy(trimmed)));
}

/// Format a unified-diff hunk header from already-computed ranges.
/// Inputs are 0-based line indices; output uses the unified-diff
/// convention of `0` for the start of an empty range and otherwise
/// `start + 1` (1-based).
fn hunk_header_str(or_start: u32, or_len: u32, nr_start: u32, nr_len: u32) -> String {
    let or_display_start = if or_len == 0 { 0 } else { or_start + 1 };
    let nr_display_start = if nr_len == 0 { 0 } else { nr_start + 1 };
    format!("@@ -{or_display_start},{or_len} +{nr_display_start},{nr_len} @@")
}

/// Reason a file's inline diff was omitted. Each maps to a single
/// faint-rendered line in the output and bumps a counter on `DiffFlags`.
pub enum SkipReason {
    Binary,
    Oversize,
}

/// Decide whether a file should be summarised rather than fully diffed.
/// `old` and `new` are raw blob bytes (either may be empty for pure
/// add/delete).
pub fn classify_skip(old: &[u8], new: &[u8]) -> Option<SkipReason> {
    classify_skip_with_cap(old, new, MAX_INLINE_DIFF_BYTES)
}

/// `classify_skip` with a caller-chosen byte cap — the single-file view
/// runs the same binary sniff but with `MAX_FILE_VIEW_DIFF_BYTES`.
pub fn classify_skip_with_cap(old: &[u8], new: &[u8], max_bytes: usize) -> Option<SkipReason> {
    // NUL-byte sniff matches the convention used in compute_numstat_gix.
    if old.contains(&0) || new.contains(&0) {
        return Some(SkipReason::Binary);
    }
    if old.len() > max_bytes || new.len() > max_bytes {
        return Some(SkipReason::Oversize);
    }
    None
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
    change_path_bstr(change).to_str_lossy().into_owned()
}

fn change_is_blob(change: &gix::diff::tree::recorder::Change) -> bool {
    use gix::diff::tree::recorder::Change;
    match change {
        Change::Addition { entry_mode, .. }
        | Change::Deletion { entry_mode, .. }
        | Change::Modification { entry_mode, .. } => entry_mode.is_blob(),
    }
}

fn change_path_bstr(change: &gix::diff::tree::recorder::Change) -> &gix::bstr::BStr {
    use gix::diff::tree::recorder::Change;
    match change {
        Change::Addition { path, .. } | Change::Deletion { path, .. } | Change::Modification { path, .. } => {
            path.as_ref()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DiffSink, MAX_INLINE_DIFF_BYTES, SkipReason, classify_skip, compute_commit_diff, compute_file_diff,
        file_full_context_output, render_file_modification,
    };
    use crate::{
        git::{TreeDiffCache, walk::build_pathspec_search},
        model::{DiffFlags, DiffLine, DiffLineKind, DiffStats, FileStat, PathFilter},
        test_support::{commit_all, make_fixture_repo, write_file},
    };

    /// Drive `render_file_modification` and pull out the hunk-header
    /// strings it emitted. The header math is most robustly checked
    /// against the public renderer; that way the imara-diff swap is
    /// also exercised end-to-end.
    fn headers_for(old: &[u8], new: &[u8]) -> Vec<String> {
        let mut lines: Vec<DiffLine> = Vec::new();
        let mut stats = DiffStats { files: 0, insertions: 0, deletions: 0 };
        let mut files: Vec<FileStat> = Vec::new();
        let mut flags = DiffFlags::default();
        let mut sections = Vec::new();
        {
            let mut sink = DiffSink {
                lines: &mut lines,
                stats: &mut stats,
                files: &mut files,
                flags: &mut flags,
                sections: &mut sections,
            };
            render_file_modification(&mut sink, "x.txt", old, new);
        }
        lines
            .into_iter()
            .filter_map(|l| if matches!(l.kind, DiffLineKind::HunkHeader) { Some(l.text) } else { None })
            .collect()
    }

    #[test]
    fn header_for_single_replace_in_middle() {
        let headers = headers_for(b"a\nb\nc\nd\ne\n", b"a\nb\nX\nd\ne\n");
        assert_eq!(headers, vec!["@@ -1,5 +1,5 @@".to_string()]);
    }

    #[test]
    fn header_for_pure_insertion_at_end() {
        let headers = headers_for(b"a\nb\n", b"a\nb\nc\nd\n");
        assert_eq!(headers, vec!["@@ -1,2 +1,4 @@".to_string()]);
    }

    #[test]
    fn header_with_two_disjoint_groups() {
        let old: String = (0..30).map(|i| format!("line{i}\n")).collect();
        let mut new_lines: Vec<String> = (0..30).map(|i| format!("line{i}\n")).collect();
        new_lines[2] = "CHANGED\n".to_string();
        new_lines[27] = "CHANGED\n".to_string();
        let new: String = new_lines.concat();
        let headers = headers_for(old.as_bytes(), new.as_bytes());
        assert_eq!(headers, vec!["@@ -1,6 +1,6 @@".to_string(), "@@ -25,6 +25,6 @@".to_string()]);
    }

    #[test]
    fn header_uses_zero_start_for_empty_old_range() {
        let headers = headers_for(b"", b"x\ny\n");
        assert_eq!(headers, vec!["@@ -0,0 +1,2 @@".to_string()]);
    }

    #[test]
    fn header_uses_zero_start_for_empty_new_range() {
        let headers = headers_for(b"x\ny\n", b"");
        assert_eq!(headers, vec!["@@ -1,2 +0,0 @@".to_string()]);
    }

    #[test]
    fn modification_emits_add_and_del_lines_at_expected_kinds() {
        let mut lines: Vec<DiffLine> = Vec::new();
        let mut stats = DiffStats { files: 0, insertions: 0, deletions: 0 };
        let mut files: Vec<FileStat> = Vec::new();
        let mut flags = DiffFlags::default();
        let mut sections = Vec::new();
        {
            let mut sink = DiffSink {
                lines: &mut lines,
                stats: &mut stats,
                files: &mut files,
                flags: &mut flags,
                sections: &mut sections,
            };
            render_file_modification(&mut sink, "x.txt", b"a\nb\nc\n", b"a\nB\nc\n");
        }
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].path, "x.txt");
        assert_eq!(sections[0].body_start, 0);
        let adds: Vec<&str> = lines
            .iter()
            .filter_map(|l| if matches!(l.kind, DiffLineKind::Add) { Some(l.text.as_str()) } else { None })
            .collect();
        let dels: Vec<&str> = lines
            .iter()
            .filter_map(|l| if matches!(l.kind, DiffLineKind::Del) { Some(l.text.as_str()) } else { None })
            .collect();
        assert_eq!(adds, vec!["B"]);
        assert_eq!(dels, vec!["b"]);
        assert_eq!(stats.insertions, 1);
        assert_eq!(stats.deletions, 1);
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn scoped_commit_diff_renders_only_matching_files() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();
        write_file(path, "src/a.rs", "fn a() {}\n");
        write_file(path, "docs/b.md", "hello\n");
        commit_all(path, "both");
        let head = repo.head_id().expect("head").detach();
        let mut cache = TreeDiffCache::new();

        let full = compute_commit_diff(&repo, head, &mut cache, None);
        assert_eq!(full.stats.files, 2, "unscoped diff renders every file");
        assert!(!full.body.iter().any(|l| l.text.contains("outside the path filter")));

        let mut search = build_pathspec_search(&PathFilter::new("src")).expect("spec");
        let scoped = compute_commit_diff(&repo, head, &mut cache, Some(&mut search));
        assert_eq!(scoped.stats.files, 1);
        assert_eq!(scoped.files[0].path, "src/a.rs");
        assert_eq!(scoped.sections.len(), 1, "hidden files must not leave sections behind");
        assert!(
            scoped.body.iter().any(|l| l.text.contains("1 changed file outside the path filter")),
            "scoped diff should note the hidden file",
        );
    }

    #[test]
    fn scoped_commit_diff_with_all_files_matching_adds_no_note() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();
        write_file(path, "src/a.rs", "fn a() {}\n");
        commit_all(path, "only src");
        let head = repo.head_id().expect("head").detach();
        let mut cache = TreeDiffCache::new();

        let mut search = build_pathspec_search(&PathFilter::new("src")).expect("spec");
        let scoped = compute_commit_diff(&repo, head, &mut cache, Some(&mut search));
        assert_eq!(scoped.stats.files, 1);
        assert!(!scoped.body.iter().any(|l| l.text.contains("outside the path filter")));
    }

    #[test]
    fn full_context_emits_every_line_without_hunk_headers() {
        let old: String = (0..30).map(|i| format!("line{i}\n")).collect();
        let mut new_lines: Vec<String> = (0..30).map(|i| format!("line{i}\n")).collect();
        new_lines[15] = "CHANGED\n".to_string();
        let new: String = new_lines.concat();

        let out = file_full_context_output("x.txt", old.as_bytes(), new.as_bytes(), None);

        assert!(out.skip_reason.is_none());
        assert_eq!(out.additions, 1);
        assert_eq!(out.deletions, 1);
        assert!(
            !out.lines.iter().any(|l| matches!(l.kind, DiffLineKind::HunkHeader)),
            "full-context output is the whole file — no hunk headers"
        );
        let context = out.lines.iter().filter(|l| matches!(l.kind, DiffLineKind::Context)).count();
        assert_eq!(context, 29, "every unchanged line is present as context");
        assert!(out.lines.iter().any(|l| l.text == "line0"), "lines far outside ±3 context are included");
        assert!(out.lines.iter().any(|l| l.text == "line29"));
    }

    #[test]
    fn full_context_renders_files_past_the_inline_cap() {
        let big = vec![b'a'; MAX_INLINE_DIFF_BYTES + 1];
        assert!(matches!(classify_skip(&big, b""), Some(SkipReason::Oversize)), "inline path suppresses this file");

        let out = file_full_context_output("big.txt", &big, b"", Some("deleted file"));
        assert!(out.skip_reason.is_none(), "single-file view renders past the inline byte cap");
        assert_eq!(out.deletions, 1, "the blob is one giant line");
    }

    #[test]
    fn compute_file_diff_isolates_one_file_with_full_context() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();
        let base: String = (0..12).map(|i| format!("alpha{i}\n")).collect();
        write_file(path, "src/a.rs", &base);
        write_file(path, "docs/b.md", "hello\n");
        commit_all(path, "base");
        let mut changed: Vec<String> = (0..12).map(|i| format!("alpha{i}\n")).collect();
        changed[6] = "BETA\n".to_string();
        write_file(path, "src/a.rs", &changed.concat());
        write_file(path, "docs/b.md", "hello world\n");
        commit_all(path, "change");
        let head = repo.head_id().expect("head").detach();
        let mut cache = TreeDiffCache::new();

        let doc = compute_file_diff(&repo, head, "src/a.rs", &mut cache);
        assert_eq!(doc.stats.files, 1);
        assert_eq!(doc.files[0].path, "src/a.rs");
        assert_eq!(doc.sections.len(), 1);
        assert!(doc.body.iter().any(|l| matches!(l.kind, DiffLineKind::Add) && l.text == "BETA"));
        assert!(
            doc.body.iter().any(|l| matches!(l.kind, DiffLineKind::Context) && l.text == "alpha0"),
            "context spans the whole file, not ±3 lines around the change"
        );
        assert!(doc.body.iter().any(|l| matches!(l.kind, DiffLineKind::Context) && l.text == "alpha11"));
        assert!(!doc.body.iter().any(|l| l.text.contains("b.md")), "the other changed file stays out");
    }

    #[test]
    fn compute_file_diff_unknown_path_yields_error_document() {
        let (td, repo) = make_fixture_repo();
        write_file(td.path(), "a.txt", "x\n");
        commit_all(td.path(), "first");
        let head = repo.head_id().expect("head").detach();
        let mut cache = TreeDiffCache::new();

        let doc = compute_file_diff(&repo, head, "nope.rs", &mut cache);
        assert!(doc.header.iter().any(|l| l.text.starts_with("Error:")));
        assert!(doc.body.is_empty());
    }

    #[test]
    fn classify_skip_flags_binary_by_nul_byte() {
        assert!(matches!(classify_skip(b"hello\0world", b"new"), Some(SkipReason::Binary)));
        assert!(matches!(classify_skip(b"old", b"\x00binary"), Some(SkipReason::Binary)));
    }

    #[test]
    fn classify_skip_flags_oversize() {
        let big = vec![b'a'; MAX_INLINE_DIFF_BYTES + 1];
        assert!(matches!(classify_skip(&big, b""), Some(SkipReason::Oversize)));
        assert!(matches!(classify_skip(b"", &big), Some(SkipReason::Oversize)));
    }

    #[test]
    fn classify_skip_passes_normal_text_pair() {
        assert!(classify_skip(b"hello\n", b"world\n").is_none());
        let at_cap = vec![b'a'; MAX_INLINE_DIFF_BYTES];
        assert!(classify_skip(&at_cap, b"").is_none());
    }

    #[test]
    fn classify_skip_binary_wins_over_size() {
        let mut big_binary = vec![b'a'; MAX_INLINE_DIFF_BYTES + 1];
        big_binary[0] = 0;
        assert!(matches!(classify_skip(&big_binary, b""), Some(SkipReason::Binary)));
    }
}
