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

    /// Bulk-seed `count` commits via `git fast-import`. Two orders of
    /// magnitude faster than `seed_commits` because there's no per-commit
    /// subprocess; the whole history streams into one `fast-import`
    /// invocation. Used by the startup bench, which needs 50k-commit
    /// fixtures.
    ///
    /// All commits chain linearly off the previous one and share a
    /// single one-file tree, so this is appropriate for measuring walker
    /// throughput and pre-walk fixed costs but **not** for pathspec
    /// filtering (only the first commit "touches" anything).
    pub fn seed_commits_fast(&self, count: usize) {
        self.seed_commits_fast_with_files(count, 1);
    }

    /// `seed_commits_fast` with a configurable number of files in HEAD's
    /// tree. All files reference the same blob, so the worktree weighs
    /// `worktree_files` bytes plus directory metadata after `reset
    /// --hard`. Used by the startup bench's `worktree_files` dimension
    /// to isolate the cost of `gix::status` scanning a wide checkout.
    pub fn seed_commits_fast_with_files(&self, count: usize, worktree_files: usize) {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };

        let mut child = Command::new("git")
            .arg("-C")
            .arg(self.path())
            .args(["fast-import", "--quiet"])
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn git fast-import");
        {
            let stdin = child.stdin.as_mut().expect("fast-import stdin handle");
            // One blob, referenced by every tree entry.
            writeln!(stdin, "blob").expect("write blob header");
            writeln!(stdin, "mark :1").expect("write blob mark");
            writeln!(stdin, "data 2").expect("write blob data length");
            writeln!(stdin, "x").expect("write blob body");

            writeln!(stdin, "reset refs/heads/main").expect("write reset");

            let base_time: i64 = 1_700_000_000;
            for i in 0..count {
                // i64::try_from keeps the cast lint quiet; usize → i64
                // is always lossless for the commit counts we care about.
                let dt: i64 = i64::try_from(i).expect("commit count fits in i64");
                let t = base_time + dt;
                writeln!(stdin, "commit refs/heads/main").expect("write commit header");
                writeln!(stdin, "mark :{}", i + 2).expect("write commit mark");
                writeln!(stdin, "author Bench <bench@example.com> {t} +0000").expect("write author");
                writeln!(stdin, "committer Bench <bench@example.com> {t} +0000").expect("write committer");
                let msg = format!("commit {i}\n");
                writeln!(stdin, "data {}", msg.len()).expect("write data length");
                write!(stdin, "{msg}").expect("write commit message");
                if i == 0 {
                    // Establish the tree on the root commit. Every file
                    // points at the same blob, which is fine because git
                    // trees record (mode, oid, name).
                    for f in 0..worktree_files {
                        writeln!(stdin, "M 100644 :1 file_{f:07}.txt").expect("write M directive");
                    }
                } else {
                    // Chain to the previous commit; tree is inherited
                    // and no path actually changes.
                    writeln!(stdin, "from :{}", i + 1).expect("write from");
                }
            }
        }
        let status = child.wait().expect("wait fast-import");
        assert!(status.success(), "git fast-import exited unsuccessfully");

        // fast-import only writes refs + objects; the index and worktree
        // are still empty. Sync them to HEAD so the worker's `gix::status`
        // doesn't see every committed path as missing — which would skew
        // the dirty-check timing we're trying to measure.
        run_git(self.path(), &["reset", "--hard", "--quiet", "HEAD"]);
    }

    /// Drop `count` extra refs into `.git/packed-refs`, all pointing at
    /// HEAD. Used by the startup bench's `refs` dimension to isolate the
    /// cost of `meta::load_refs` enumerating a ref-heavy repo (mirrors of
    /// big upstreams routinely accumulate tens of thousands of
    /// remote-tracking refs).
    ///
    /// Writes packed-refs directly because `git update-ref --stdin` on
    /// 10k refs is itself slow enough to dominate fixture setup.
    pub fn add_packed_refs(&self, count: usize) {
        use std::{io::Write, process::Command};

        let out = Command::new("git")
            .arg("-C")
            .arg(self.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse HEAD");
        assert!(out.status.success(), "rev-parse HEAD failed");
        let oid = String::from_utf8(out.stdout).expect("rev-parse output utf-8");
        let oid = oid.trim();

        let path = self.path().join(".git").join("packed-refs");
        let mut file = std::fs::File::create(&path).expect("create packed-refs");
        writeln!(file, "# pack-refs with: peeled fully-peeled sorted").expect("write header");
        // Pack main as well so the file is self-consistent even if the
        // loose ref is later removed; gix dedupes loose/packed by name.
        writeln!(file, "{oid} refs/heads/main").expect("write main");
        for i in 0..count {
            writeln!(file, "{oid} refs/heads/bench-{i:07}").expect("write packed ref");
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
    let out = std::process::Command::new("git").arg("-C").arg(cwd).args(args).output().expect("git binary on PATH");
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}
