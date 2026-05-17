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
use similar::ChangeTag;

pub fn empty_error_document(target: DiffTarget, e: anyhow::Error) -> DiffDocument {
    DiffDocument {
        target,
        header: vec![DiffLine::new(DiffLineKind::Faint, format!("Error: {e}"))],
        body: Vec::new(),
        files: Vec::new(),
        stats: DiffStats { files: 0, insertions: 0, deletions: 0 },
        flags: DiffFlags::default(),
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

    Ok(DiffDocument { target, header, body, files, stats, flags })
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
pub fn render_file_modification(sink: &mut DiffSink<'_>, path: &str, old: &[u8], new: &[u8]) {
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

/// Build a `@@ -old_start,old_len +new_start,new_len @@` header that covers
/// the entire grouped op set, not just its first op. The earlier
/// implementation used `group.first()` for both start and length, which
/// truncated the range on any group containing more than one op
/// (e.g. Context + Replace + Context) — the emitted header would describe
/// only the leading context.
pub fn hunk_header(group: &[similar::DiffOp]) -> String {
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

#[cfg(test)]
mod tests {
    use super::{MAX_INLINE_DIFF_BYTES, SkipReason, classify_skip, hunk_header};
    use similar::TextDiff;

    #[test]
    fn header_spans_full_group_not_just_first_op() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nb\nX\nd\ne\n";
        let diff = TextDiff::from_lines(old, new);
        let groups: Vec<_> = diff.grouped_ops(3).into_iter().collect();
        assert_eq!(groups.len(), 1, "expected one grouped hunk for this small diff");
        assert_eq!(hunk_header(&groups[0]), "@@ -1,5 +1,5 @@");
    }

    #[test]
    fn header_for_pure_insertion_at_end() {
        let old = "a\nb\n";
        let new = "a\nb\nc\nd\n";
        let diff = TextDiff::from_lines(old, new);
        let groups: Vec<_> = diff.grouped_ops(3).into_iter().collect();
        assert_eq!(groups.len(), 1);
        assert_eq!(hunk_header(&groups[0]), "@@ -1,2 +1,4 @@");
    }

    #[test]
    fn header_with_two_disjoint_groups() {
        let old: String = (0..30).map(|i| format!("line{i}\n")).collect();
        let mut new_lines: Vec<String> = (0..30).map(|i| format!("line{i}\n")).collect();
        new_lines[2] = "CHANGED\n".to_string();
        new_lines[27] = "CHANGED\n".to_string();
        let new: String = new_lines.concat();
        let diff = TextDiff::from_lines(&old, &new);
        let groups: Vec<_> = diff.grouped_ops(3).into_iter().collect();
        assert_eq!(groups.len(), 2, "expected two separate hunks");
        let headers: Vec<String> = groups.iter().map(|g| hunk_header(g)).collect();
        assert_eq!(headers[0], "@@ -1,6 +1,6 @@");
        assert_eq!(headers[1], "@@ -25,6 +25,6 @@");
    }

    #[test]
    fn header_uses_zero_start_for_empty_old_range() {
        let diff = TextDiff::from_lines("", "x\ny\n");
        let groups: Vec<_> = diff.grouped_ops(3).into_iter().collect();
        assert_eq!(groups.len(), 1);
        assert_eq!(hunk_header(&groups[0]), "@@ -0,0 +1,2 @@");
    }

    #[test]
    fn header_uses_zero_start_for_empty_new_range() {
        let diff = TextDiff::from_lines("x\ny\n", "");
        let groups: Vec<_> = diff.grouped_ops(3).into_iter().collect();
        assert_eq!(groups.len(), 1);
        assert_eq!(hunk_header(&groups[0]), "@@ -1,2 +0,0 @@");
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
