//! Commit-diff rendering: the entry points called from the worker's
//! Inspect request handler, plus the per-file render helpers shared
//! with the working-tree status path. Independent of any walk state.

use crate::{
    git::{
        MAX_INLINE_DIFF_BYTES, MAX_INLINE_DIFF_FILES, MAX_INLINE_DIFF_LINES, TreeDiffCache, compute_tree_diff_records,
        meta::format_timestamp,
    },
    model::{DiffDocument, DiffFlags, DiffLine, DiffLineKind, DiffStats, DiffTarget, FileStat},
};
use anyhow::Result;
use gix::{ObjectId, bstr::ByteSlice};
use imara_diff::{Algorithm, Diff, Hunk, InternedInput};

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
    }
}

pub fn compute_commit_diff(repo: &gix::Repository, id: ObjectId, cache: &mut TreeDiffCache) -> DiffDocument {
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
    let records = compute_tree_diff_records(repo, parent_ids.first().copied(), id, cache)?.to_vec();

    let mut flags = DiffFlags::default();
    {
        let mut sink = DiffSink { lines: &mut body, stats: &mut stats, files: &mut files, flags: &mut flags };
        render_diff_records(repo, &records, &mut sink);
    }

    Ok(DiffDocument { target, header, body, files, stats, flags, untracked_anchor: None })
}

/// Render previously-computed tree-diff records into the sink. Split
/// out of the old `diff_trees` so the cache lookup can sit outside the
/// renderer.
pub fn render_diff_records(
    repo: &gix::Repository,
    records: &[gix::diff::tree::recorder::Change],
    sink: &mut DiffSink<'_>,
) {
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
pub struct DiffSink<'a> {
    pub lines: &'a mut Vec<DiffLine>,
    pub stats: &'a mut DiffStats,
    pub files: &'a mut Vec<FileStat>,
    pub flags: &'a mut DiffFlags,
}

impl DiffSink<'_> {
    /// True once a file-count or line-count guardrail has fired. Callers
    /// should stop materialising hunks but still call `account_skipped_file`
    /// so the diffstat counts every changed file.
    pub fn guardrail_exceeded(&mut self) -> bool {
        if self.stats.files >= MAX_INLINE_DIFF_FILES {
            self.note_truncation(format!(
                "… {} files changed; remaining file diffs suppressed (>{} files)",
                self.stats.files, MAX_INLINE_DIFF_FILES,
            ));
            return true;
        }
        if self.lines.len() >= MAX_INLINE_DIFF_LINES {
            self.note_truncation(format!(
                "… diff truncated at {MAX_INLINE_DIFF_LINES} lines; remaining files summarised",
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

/// Render a pure-addition file diff (new file in the new revision).
///
/// Body lines are stored without the leading `+` — the renderer prepends
/// the marker at draw time based on `DiffLineKind::Add`. The per-line
/// `format!` was a measurable allocation in the original implementation
/// on large diffs.
pub fn render_file_addition(sink: &mut DiffSink<'_>, path: &str, new: &[u8]) {
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
pub fn render_file_deletion(sink: &mut DiffSink<'_>, path: &str, old: &[u8]) {
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
///
/// Uses `imara-diff`'s Histogram algorithm (a port of git's libxdiff).
/// On text-heavy real-world inputs it consistently outperforms the
/// `similar` crate's Myers implementation, which was previously the
/// algorithmic gap between `rit`'s diff render and `git diff`.
pub fn render_file_modification(sink: &mut DiffSink<'_>, path: &str, old: &[u8], new: &[u8]) {
    push_file_headers(sink.lines, path, None);
    let (additions, deletions) = match classify_skip(old, new) {
        Some(reason) => {
            push_skip_summary(sink.lines, sink.flags, reason, old.len().max(new.len()));
            (0, 0)
        }
        None => render_unified(sink, old, new),
    };
    sink.record_file(path.to_string(), additions, deletions);
}

/// Drive `imara-diff` over `old` / `new` byte slices and emit hunks
/// into `sink`. Mirrors the structure of `imara_diff::UnifiedDiff` (so
/// hunk grouping and headers match `git diff -U3`) but pushes
/// structured `DiffLine`s rather than a unified-diff string, which is
/// what the TUI renderer wants downstream.
fn render_unified(sink: &mut DiffSink<'_>, old: &[u8], new: &[u8]) -> (usize, usize) {
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

        sink.lines.push(DiffLine::new(
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
                push_token_line(sink.lines, DiffLineKind::Context, &input, cursor, /* before */ true);
                cursor += 1;
            }
            for i in h.before.start..h.before.end {
                push_token_line(sink.lines, DiffLineKind::Del, &input, i, /* before */ true);
                sink.stats.deletions += 1;
                file_del += 1;
            }
            cursor = h.before.end;
            for i in h.after.start..h.after.end {
                push_token_line(sink.lines, DiffLineKind::Add, &input, i, /* before */ false);
                sink.stats.insertions += 1;
                file_add += 1;
            }
        }
        while cursor < group_before_end {
            push_token_line(sink.lines, DiffLineKind::Context, &input, cursor, /* before */ true);
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
/// `idx` is always a valid offset into the chosen side: `render_unified`
/// only ever feeds indices it pulled from `Diff::hunks()` on this same
/// `InternedInput`, and the cursor walks `[group_before_start, group_before_end)`
/// which is clamped to the side's length. Likewise for the after side
/// (`h.after.start..h.after.end` ⊆ `0..input.after.len()`).
fn push_token_line(
    lines: &mut Vec<DiffLine>,
    kind: DiffLineKind,
    input: &InternedInput<&[u8]>,
    idx: u32,
    before: bool,
) {
    #[expect(clippy::indexing_slicing, reason = "idx is bounded by render_unified's hunk walk; see fn doc")]
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

#[cfg(test)]
mod tests {
    use super::{DiffSink, MAX_INLINE_DIFF_BYTES, SkipReason, classify_skip, render_file_modification};
    use crate::model::{DiffFlags, DiffLine, DiffLineKind, DiffStats, FileStat};

    /// Drive `render_file_modification` and pull out the hunk-header
    /// strings it emitted. The header math is most robustly checked
    /// against the public renderer; that way the imara-diff swap is
    /// also exercised end-to-end.
    fn headers_for(old: &[u8], new: &[u8]) -> Vec<String> {
        let mut lines: Vec<DiffLine> = Vec::new();
        let mut stats = DiffStats { files: 0, insertions: 0, deletions: 0 };
        let mut files: Vec<FileStat> = Vec::new();
        let mut flags = DiffFlags::default();
        {
            let mut sink = DiffSink { lines: &mut lines, stats: &mut stats, files: &mut files, flags: &mut flags };
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
        {
            let mut sink = DiffSink { lines: &mut lines, stats: &mut stats, files: &mut files, flags: &mut flags };
            render_file_modification(&mut sink, "x.txt", b"a\nb\nc\n", b"a\nB\nc\n");
        }
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
