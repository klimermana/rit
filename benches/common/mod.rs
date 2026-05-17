//! Fixture-repo helpers shared between bench files. Spins up a fresh
//! `tempfile::TempDir`, runs `git init` + a configurable number of
//! commits, and hands back a path the caller can pass into rit's worker.
//!
//! Kept minimal on purpose — benches are scaffolding for measurement,
//! not exhaustive perf coverage. Each helper here is also duplicated in
//! intent (not in code) by `src/git/mod.rs`'s test fixtures.
//!
//! `#[allow(dead_code)]`: each bench file gets its own compile of this
//! module via `mod common;`, so any helper a particular bench doesn't
//! use looks "dead" to that bench's compilation unit.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// A fixture repo that cleans up when dropped.
pub struct FixtureRepo {
    pub td: tempfile::TempDir,
}

impl FixtureRepo {
    pub fn new() -> Self {
        let td = tempfile::tempdir().expect("create temp dir");
        let p = td.path();
        run_git(p, &["init", "-q", "-b", "main"]);
        run_git(p, &["config", "user.email", "bench@example.com"]);
        run_git(p, &["config", "user.name", "Bench"]);
        run_git(p, &["config", "commit.gpgsign", "false"]);
        FixtureRepo { td }
    }

    pub fn path(&self) -> &Path {
        self.td.path()
    }

    pub fn path_buf(&self) -> PathBuf {
        self.td.path().to_path_buf()
    }

    /// Add `count` trivial commits, each touching a fresh file. Useful
    /// for indexing / search benches that want a reasonably-sized history
    /// without paying the cost of a real repo.
    pub fn seed_commits(&self, count: usize) {
        for i in 0..count {
            let path = self.path().join(format!("file_{i:04}.txt"));
            std::fs::write(&path, format!("seed line {i}\n")).expect("write seed file");
            run_git(self.path(), &["add", "-A"]);
            run_git(self.path(), &["commit", "-q", "-m", &format!("seed commit {i}")]);
        }
    }

    /// Add one commit that touches a single file with the given line count.
    /// Used by the diff-generation bench to produce small + large diffs.
    pub fn commit_with_n_lines(&self, name: &str, lines: usize) {
        let path = self.path().join(name);
        let mut content = String::with_capacity(lines * 8);
        for i in 0..lines {
            content.push_str(&format!("line {i}\n"));
        }
        std::fs::write(&path, content).expect("write");
        run_git(self.path(), &["add", "-A"]);
        run_git(self.path(), &["commit", "-q", "-m", &format!("add {name}")]);
    }
}

pub fn run_git(cwd: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("git binary on PATH");
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}
