//! Shared test fixtures: a tempdir-backed git repo plus a handful of
//! helpers that wrap shell-outs to the `git` CLI. Only compiled under
//! `#[cfg(test)]`, so this file contributes nothing to the binary.
//!
//! Used by tests in `src/git/*` and `src/app/*` (after the Stage 6
//! split). Benches don't see this module — `benches/common/mod.rs`
//! keeps its own copy because benches compile as separate crates and
//! the indirection isn't worth a feature-gated dependency cycle.
#![cfg(test)]

use crate::{
    git::{GitMsg, HistoryMsg},
    model::CommitRecord,
};
use std::path::Path;

pub fn run_git(cwd: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("git binary should be available");
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

pub fn make_fixture_repo() -> (tempfile::TempDir, gix::Repository) {
    let td = tempfile::tempdir().expect("create temp dir");
    let path = td.path();
    run_git(path, &["init", "-q", "-b", "main"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test User"]);
    run_git(path, &["config", "commit.gpgsign", "false"]);
    let repo = gix::open(path).expect("open fixture repo");
    (td, repo)
}

pub fn write_file(repo_path: &Path, rel: &str, content: &str) {
    let p = repo_path.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).expect("mkdir -p");
    }
    std::fs::write(p, content).expect("write file");
}

pub fn commit_all(repo_path: &Path, msg: &str) {
    run_git(repo_path, &["add", "-A"]);
    run_git(repo_path, &["commit", "-q", "-m", msg]);
}

pub fn commit_all_as(repo_path: &Path, msg: &str, author: &str, email: &str) {
    run_git(repo_path, &["add", "-A"]);
    run_git(
        repo_path,
        &["-c", &format!("user.name={author}"), "-c", &format!("user.email={email}"), "commit", "-q", "-m", msg],
    );
}

pub fn drain_commits(rx: &crossbeam_channel::Receiver<GitMsg>) -> Vec<CommitRecord> {
    let mut out: Vec<CommitRecord> = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if let GitMsg::History(HistoryMsg::Commits { commits, .. }) = msg {
            out.extend(commits);
        }
    }
    out
}
