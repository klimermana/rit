//! Pure search helpers — substring match, narrow-decision predicate,
//! and the cyclic-advance routine that backs both `n`/`N` and the
//! initial first-match-at-or-after-cursor jump.

use crate::model::CommitRecord;

/// Substring-match a commit against a pre-lowercased query. The summary
/// check tends to hit first in practice, so it goes first; tags are last
/// because the blob is empty for nearly every commit and short-circuit
/// evaluation makes the cost essentially free in the no-tag case.
pub fn commit_matches(c: &CommitRecord, q: &str) -> bool {
    c.search.summary_lower.contains(q) || c.search.author_lower.contains(q) || c.search.tags_lower.contains(q)
}

/// Decide whether the current commit-search update can be served by
/// narrowing the previous match set instead of rescanning the whole
/// index. Two conditions: the walk hasn't been reloaded under us
/// (generations match), and the new query is a strict extension of the
/// previous non-empty one.
pub fn should_narrow(prev_query: &str, prev_generation: u64, query: &str, generation: u64) -> bool {
    generation == prev_generation && !prev_query.is_empty() && query.starts_with(prev_query)
}

/// Pick the first match at-or-after `cursor`, wrapping back to the
/// start when none qualifies. Mutates `*current` to the chosen index.
/// Returns the picked match position, or `None` only when `matches`
/// is empty.
pub fn jump_first_at_or_after(matches: &[usize], current: &mut usize, cursor: usize) -> Option<usize> {
    let idx = matches.iter().position(|&i| i >= cursor).unwrap_or(0);
    let &pos = matches.get(idx)?;
    *current = idx;
    Some(pos)
}

/// Cyclically advance `*current` by `delta`. Returns the new match position
/// or `None` if `matches` is empty.
pub fn cycle(matches: &[usize], current: &mut usize, delta: isize) -> Option<usize> {
    let len_i = isize::try_from(matches.len()).ok().filter(|&n| n > 0)?;
    let cur_i = isize::try_from(*current).ok()?;
    // rem_euclid on a positive divisor yields a non-negative result, so
    // `unsigned_abs` is a lossless conversion back to usize.
    let next_i = (cur_i.saturating_add(delta)).rem_euclid(len_i);
    *current = next_i.unsigned_abs();
    matches.get(*current).copied()
}

#[cfg(test)]
mod tests {
    use super::{commit_matches, should_narrow};
    use crate::model::{CommitRecord, CommitSearchText};
    use compact_str::CompactString;
    use gix::ObjectId;

    fn make_commit(summary: &str, author: &str, tags_lower: &str) -> CommitRecord {
        CommitRecord {
            id: ObjectId::null(gix::hash::Kind::Sha1),
            short_id: "0000000".into(),
            authored_unix_secs: 0,
            authored_relative: "now".into(),
            author: author.into(),
            summary: summary.to_string(),
            refs: Vec::new(),
            graph: CompactString::default(),
            search: CommitSearchText {
                author_lower: author.to_lowercase().into(),
                summary_lower: summary.to_lowercase(),
                tags_lower: tags_lower.into(),
            },
        }
    }

    #[test]
    fn commit_matches_finds_tag_substring() {
        // Query is already lowercased by the caller — match the search
        // pipeline's contract.
        let c = make_commit("unrelated message", "Someone Else", "v0.2.0 stable");
        assert!(commit_matches(&c, "v0.2"));
        assert!(commit_matches(&c, "stable"));
        assert!(!commit_matches(&c, "v0.3"));
    }

    #[test]
    fn commit_matches_skips_when_no_tag_blob_set() {
        // Empty tags projection means the commit has no tags — query
        // must not falsely hit on it.
        let c = make_commit("hello world", "alex", "");
        assert!(commit_matches(&c, "hello"));
        assert!(!commit_matches(&c, "v1"));
    }

    #[test]
    fn narrow_when_query_extends_within_same_generation() {
        assert!(should_narrow("foo", 1, "foob", 1));
        assert!(should_narrow("a", 5, "abc", 5));
    }

    #[test]
    fn rescan_when_query_shrinks() {
        assert!(!should_narrow("foobar", 1, "foo", 1));
        assert!(!should_narrow("ab", 1, "a", 1));
    }

    #[test]
    fn rescan_when_query_changes_completely() {
        assert!(!should_narrow("foo", 1, "bar", 1));
    }

    #[test]
    fn rescan_after_reload_changes_generation() {
        // Even though the new query strictly extends the old, a reload
        // means the row indices the previous match set referenced may not
        // line up anymore.
        assert!(!should_narrow("foo", 1, "foobar", 2));
    }

    #[test]
    fn rescan_when_no_previous_query() {
        // An empty previous query means there's no prior match set to
        // narrow; the new query must scan the whole index.
        assert!(!should_narrow("", 1, "f", 1));
    }
}
