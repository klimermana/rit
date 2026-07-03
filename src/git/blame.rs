//! gix-native blame: resolve the requested revision, run
//! `Repository::blame_file`, and flatten the outcome into the
//! render-ready `BlameDocument`. Runs on a worker side thread (see
//! `spawn_blame` in `git/mod.rs`) because blame walks history and can
//! take seconds on long-lived files.

use crate::{
    git::{inspect::BlameAt, meta::relative_time},
    model::{BlameDocument, BlameLine},
};
use compact_str::{CompactString, format_compact};
use gix::{ObjectId, bstr::ByteSlice};
use std::collections::HashMap;

/// Per-distinct-commit metadata shared by every line that commit
/// introduced. A blame outcome typically has far fewer distinct
/// commits than lines, so this is looked up once per commit.
struct CommitMeta {
    short: CompactString,
    author: CompactString,
    authored_relative: CompactString,
}

/// Resolve `at` to the blame suspect. `Err` values are display-ready.
fn resolve_suspect(repo: &gix::Repository, at: BlameAt) -> Result<ObjectId, String> {
    match at {
        BlameAt::Head => repo.head_id().map(|id| id.detach()).map_err(|e| format!("cannot resolve HEAD: {e}")),
        BlameAt::Commit(id) => Ok(id),
        BlameAt::ParentOfCommit(id) => {
            let commit = repo
                .find_object(id)
                .map_err(|e| format!("cannot read commit {id}: {e}"))?
                .try_into_commit()
                .map_err(|e| format!("{id} is not a commit: {e}"))?;
            commit
                .parent_ids()
                .next()
                .map(|p| p.detach())
                .ok_or_else(|| format!("commit {} has no parent", id.to_hex_with_len(7)))
        }
    }
}

pub fn compute_blame(repo: &gix::Repository, path: &str, at: BlameAt) -> Result<BlameDocument, String> {
    let suspect = resolve_suspect(repo, at)?;

    // Rename tracking on (like `git blame`'s default) so annotations
    // follow a file across renames; `source_file_name` then carries
    // the historical path for re-blame.
    let options =
        gix::repository::blame_file::Options { rewrites: Some(gix::diff::Rewrites::default()), ..Default::default() };
    let outcome = repo
        .blame_file(path.as_bytes().as_bstr(), suspect, options)
        .map_err(|e| format!("blame failed for {path}: {e}"))?;

    let mut meta: HashMap<ObjectId, CommitMeta> = HashMap::new();
    let mut lines: Vec<BlameLine> = Vec::new();

    for (entry, entry_lines) in outcome.entries_with_lines() {
        let commit_id = entry.commit_id;
        let m = meta.entry(commit_id).or_insert_with(|| commit_meta(repo, commit_id));
        let source_path = entry.source_file_name.as_ref().map(|n| n.to_str_lossy().into_owned()).filter(|n| n != path);

        let start = entry.start_in_blamed_file;
        for (i, text) in entry_lines.iter().enumerate() {
            lines.push(BlameLine {
                commit_id,
                commit_short: m.short.clone(),
                author: m.author.clone(),
                authored_relative: m.authored_relative.clone(),
                // gix line indices are 0-based; display is 1-based.
                line_no: start + i as u32 + 1,
                // Lines keep their trailing newline in the outcome blob.
                text: text.to_str_lossy().trim_end_matches(['\n', '\r']).to_owned(),
                source_path: source_path.clone(),
            });
        }
    }

    // entries are in blamed-file order already, but sort defensively so
    // the cursor math never sees out-of-order line numbers.
    lines.sort_by_key(|l| l.line_no);

    Ok(BlameDocument { path: path.to_owned(), at: suspect, lines })
}

/// Look up short id / author / relative time for one commit; falls back
/// to placeholders on decode errors rather than failing the blame.
fn commit_meta(repo: &gix::Repository, id: ObjectId) -> CommitMeta {
    let short: CompactString = format_compact!("{}", id.to_hex_with_len(7));
    let decoded = repo.find_object(id).ok().and_then(|o| o.try_into_commit().ok());
    let Some(commit) = decoded else {
        return CommitMeta { short, author: "?".into(), authored_relative: "?".into() };
    };
    match commit.decode() {
        Ok(c) => {
            let author = c.author().ok();
            let name: CompactString =
                author.map(|a| CompactString::from(a.name.to_str_lossy().as_ref())).unwrap_or_else(|| "?".into());
            let secs = author.and_then(|a| a.time().ok()).map(|t| t.seconds).unwrap_or(0);
            CommitMeta { short, author: name, authored_relative: relative_time(secs) }
        }
        Err(_) => CommitMeta { short, author: "?".into(), authored_relative: "?".into() },
    }
}

#[cfg(test)]
mod tests {
    use super::{BlameAt, compute_blame, resolve_suspect};
    use crate::test_support::{commit_all, make_fixture_repo, write_file};

    #[test]
    fn blame_attributes_lines_to_their_commits() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();
        write_file(path, "f.txt", "alpha\nbeta\n");
        commit_all(path, "first");
        write_file(path, "f.txt", "alpha\nBETA\ngamma\n");
        commit_all(path, "second");

        let doc = compute_blame(&repo, "f.txt", BlameAt::Head).expect("blame succeeds");
        assert_eq!(doc.lines.len(), 3);
        assert_eq!(doc.lines[0].text, "alpha");
        assert_eq!(doc.lines[1].text, "BETA");
        assert_eq!(doc.lines[2].text, "gamma");
        assert_eq!(doc.lines[0].line_no, 1);
        assert_eq!(doc.lines[2].line_no, 3);

        let head = repo.head_id().expect("head").detach();
        assert_eq!(doc.lines[1].commit_id, head, "modified line belongs to the second commit");
        assert_eq!(doc.lines[2].commit_id, head, "added line belongs to the second commit");
        assert_ne!(doc.lines[0].commit_id, head, "unchanged line still belongs to the first commit");
        assert_eq!(doc.lines[0].author.as_str(), "Test User");
    }

    #[test]
    fn reblame_at_parent_sees_the_older_content() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();
        write_file(path, "f.txt", "alpha\nbeta\n");
        commit_all(path, "first");
        write_file(path, "f.txt", "alpha\nBETA\n");
        commit_all(path, "second");

        let head = repo.head_id().expect("head").detach();
        let doc = compute_blame(&repo, "f.txt", BlameAt::ParentOfCommit(head)).expect("blame at parent");
        assert_eq!(doc.lines.len(), 2);
        assert_eq!(doc.lines[1].text, "beta", "parent revision has the pre-change content");
        assert_ne!(doc.at, head, "document records the parent as its suspect");
    }

    #[test]
    fn parent_of_root_commit_is_an_error() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();
        write_file(path, "f.txt", "x\n");
        commit_all(path, "only");
        let head = repo.head_id().expect("head").detach();

        let err = resolve_suspect(&repo, BlameAt::ParentOfCommit(head)).expect_err("root has no parent");
        assert!(err.contains("no parent"), "error is display-ready: {err}");
    }
}
