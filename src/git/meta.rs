//! Standalone helpers that don't fit the walk / diff / status split:
//! repo identity, working-tree author lookup, the quick dirty check,
//! the ref-table loader, and the two time formatters used by the
//! header / log row renderers.

use crate::model::{RefEntry, RefKind, RefLabel, RepoInfo};
use chrono::{TimeZone, Utc};
use compact_str::CompactString;
use gix::{ObjectId, bstr::ByteSlice};
use std::collections::HashMap;

pub fn load_refs(repo: &gix::Repository) -> HashMap<ObjectId, Vec<RefLabel>> {
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

/// Build the refs-view listing: every local branch, remote branch, and
/// tag, peeled to a commit (annotated tags follow their target chain)
/// and annotated with that commit's summary + relative author time.
/// Sorted by kind (branches, remotes, tags), then name. Refs that
/// don't peel to a commit (e.g. a tag of a blob) are skipped.
pub fn load_ref_entries(repo: &gix::Repository) -> Vec<RefEntry> {
    let mut entries: Vec<RefEntry> = Vec::new();
    let Ok(platform) = repo.references() else {
        return entries;
    };
    let Ok(all_refs) = platform.all() else { return entries };

    for mut reference in all_refs.flatten() {
        let full_name = reference.name().as_bstr().to_str_lossy().into_owned();
        // The refs view lists real refs only — the HEAD pseudo-ref is
        // already visible as the checked-out branch decoration.
        let (name, kind): (CompactString, RefKind) = if let Some(b) = full_name.strip_prefix("refs/heads/") {
            (b.into(), RefKind::LocalBranch)
        } else if let Some(r) = full_name.strip_prefix("refs/remotes/") {
            (r.into(), RefKind::RemoteBranch)
        } else if let Some(t) = full_name.strip_prefix("refs/tags/") {
            (t.into(), RefKind::Tag)
        } else {
            continue;
        };

        let Ok(target) = reference.peel_to_id() else {
            continue;
        };
        let Ok(object) = target.object() else { continue };
        let Ok(commit) = object.try_into_commit() else { continue };
        let (summary, authored_relative) = match commit.decode() {
            Ok(decoded) => {
                let summary = decoded.message().title.to_str_lossy().trim_end().to_owned();
                let when = decoded.author().ok().and_then(|a| a.time().ok()).map(|t| t.seconds).unwrap_or(0);
                (summary, relative_time(when))
            }
            Err(_) => (String::new(), "".into()),
        };
        entries.push(RefEntry { name, kind, target: target.detach(), summary, authored_relative });
    }

    let kind_rank = |k: RefKind| match k {
        RefKind::Head | RefKind::LocalBranch => 0u8,
        RefKind::RemoteBranch => 1,
        RefKind::Tag => 2,
    };
    entries.sort_by(|a, b| kind_rank(a.kind).cmp(&kind_rank(b.kind)).then_with(|| a.name.cmp(&b.name)));
    entries
}

pub fn repo_info_for(repo: &gix::Repository) -> RepoInfo {
    let name = repo
        .workdir()
        .and_then(|p| p.canonicalize().ok())
        .or_else(|| std::env::current_dir().ok().and_then(|p| p.canonicalize().ok()))
        .map(|p| {
            let display = p.to_string_lossy().into_owned();
            if let Ok(home) = std::env::var("HOME") {
                if display == home {
                    "~".to_string()
                } else if let Some(rest) = display.strip_prefix(&(home.clone() + "/")) {
                    format!("~/{rest}")
                } else {
                    display
                }
            } else {
                display
            }
        })
        .unwrap_or_else(|| "unknown".to_string());
    let branch = repo.head_name().ok().flatten().map(|n| n.shorten().to_string()).unwrap_or_else(|| "HEAD".to_string());
    RepoInfo { name, branch, path_filter: None }
}

pub fn working_tree_author(repo: &gix::Repository) -> String {
    // Prefer git config user.name; fall back to env, then a literal.
    if let Some(name) = repo.config_snapshot().string("user.name") {
        let s = name.to_string();
        if !s.trim().is_empty() {
            return s;
        }
    }
    std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "you".to_string())
}

/// Fast at-a-glance dirty check: walk `gix::status` and return on the
/// first observed change (staged, unstaged, or untracked). Returns `None`
/// when the status query itself errors — the UI keeps its previous
/// indicator in that case rather than flashing to "clean".
pub fn quick_is_dirty(repo: &gix::Repository) -> Option<bool> {
    use gix::status::{Item, UntrackedFiles, index_worktree, plumbing::index_as_worktree};
    let platform = repo.status(gix::progress::Discard).ok()?;
    let iter = platform.untracked_files(UntrackedFiles::Collapsed).into_iter(Vec::new()).ok()?;
    for item in iter.flatten() {
        match item {
            Item::TreeIndex(_) => return Some(true),
            Item::IndexWorktree(iw) => match iw {
                index_worktree::Item::Modification { status, .. } => {
                    // NeedsUpdate is a stat-cache refresh hint, not a user-visible change.
                    if !matches!(status, index_as_worktree::EntryStatus::NeedsUpdate(_)) {
                        return Some(true);
                    }
                }
                index_worktree::Item::DirectoryContents { entry, .. } => {
                    if matches!(entry.status, gix::dir::entry::Status::Untracked) {
                        return Some(true);
                    }
                }
                index_worktree::Item::Rewrite { .. } => return Some(true),
            },
        }
    }
    Some(false)
}

pub fn relative_time(unix_secs: i64) -> CompactString {
    let now = Utc::now();
    let t = Utc.timestamp_opt(unix_secs, 0).single().unwrap_or(now);
    let s = now.signed_duration_since(t).num_seconds();
    // Clock-skewed or future-dated commits collapse to "now" — otherwise
    // the < 60 branch would print "-12s ago" and similar.
    if s < 0 {
        "now".into()
    } else if s < 60 {
        format!("{s}s ago").into()
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

pub fn format_timestamp(unix_secs: i64) -> String {
    Utc.timestamp_opt(unix_secs, 0).single().unwrap_or_else(Utc::now).format("%a %b %e %T %Y +0000").to_string()
}

#[cfg(test)]
mod tests {
    use super::load_ref_entries;
    use crate::{
        model::RefKind,
        test_support::{commit_all, make_fixture_repo, run_git, write_file},
    };

    #[test]
    fn load_ref_entries_lists_branches_and_peeled_tags_sorted() {
        let (td, repo) = make_fixture_repo();
        let path = td.path();
        write_file(path, "a.txt", "one\n");
        commit_all(path, "first commit");
        run_git(path, &["branch", "feature"]);
        run_git(path, &["tag", "-a", "v1.0", "-m", "release"]); // annotated → needs peeling
        run_git(path, &["tag", "lightweight"]);

        let entries = load_ref_entries(&repo);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["feature", "main", "lightweight", "v1.0"], "branches first, then tags, name order");

        let head = repo.head_id().expect("head").detach();
        for e in &entries {
            assert_eq!(e.target, head, "every ref (incl. annotated tag) peels to the commit");
            assert_eq!(e.summary, "first commit");
        }
        assert_eq!(entries[0].kind, RefKind::LocalBranch);
        assert_eq!(entries[3].kind, RefKind::Tag);
    }
}
