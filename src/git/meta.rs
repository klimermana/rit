//! Standalone helpers that don't fit the walk / diff / status split:
//! repo identity, working-tree author lookup, the quick dirty check,
//! the ref-table loader, and the two time formatters used by the
//! header / log row renderers.

use crate::model::{RefKind, RefLabel, RepoInfo};
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
    RepoInfo { name, branch }
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
