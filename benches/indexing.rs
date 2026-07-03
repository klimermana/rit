//! Indexing throughput bench.
//!
//! Measures end-to-end time from spawning the git worker on a fixture
//! repo to draining the final `WalkDone` message. Baseline for future
//! continuous-indexing changes (commit caching, lazier ref hydration,
//! etc.).

mod common;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use crossbeam_channel::{RecvTimeoutError, bounded};
use rit::git::{self, GitMsg, GitReq, HistoryMsg};
use std::time::Duration;

fn drain_until_walk_done(rx: &crossbeam_channel::Receiver<GitMsg>) -> usize {
    let timeout = Duration::from_secs(30);
    let mut indexed = 0usize;
    loop {
        match rx.recv_timeout(timeout) {
            Ok(GitMsg::History(HistoryMsg::Commits { commits, .. })) => {
                indexed += commits.len();
            }
            Ok(GitMsg::History(HistoryMsg::WalkDone { .. })) => return indexed,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => panic!("worker stalled before WalkDone"),
            Err(RecvTimeoutError::Disconnected) => return indexed,
        }
    }
}

fn bench_full_history_indexing(c: &mut Criterion) {
    let mut group = c.benchmark_group("indexing");
    group.sample_size(10); // each iteration spawns a worker, keep it cheap.

    for &commit_count in &[50usize, 200] {
        let fix = common::FixtureRepo::new();
        fix.seed_commits(commit_count);
        let repo_path = fix.path_buf();

        group.throughput(Throughput::Elements(commit_count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(commit_count), &commit_count, |b, _| {
            b.iter(|| {
                // `run_git_thread` calls `gix::discover(".")`, so chdir
                // into the fixture for the duration of the worker run.
                // This is not thread-safe across parallel benches but
                // criterion runs benches sequentially within a group.
                let prev = std::env::current_dir().expect("cwd");
                std::env::set_current_dir(&repo_path).expect("cd into fixture");

                let (req_tx, req_rx) = bounded::<GitReq>(64);
                let (msg_tx, msg_rx) = bounded::<GitMsg>(2048);
                let handle = std::thread::spawn(move || {
                    git::run_git_thread(req_rx, msg_tx, None, false);
                });

                _ = drain_until_walk_done(&msg_rx);

                drop(req_tx);
                _ = handle.join();
                std::env::set_current_dir(prev).expect("cd back");
            });
        });
    }
    group.finish();
}

/// Pathspec-filtered indexing. Every walked commit pays for a tree-diff
/// to decide whether it touches the pathspec; the `file_*.txt` glob
/// matches every seed commit so the channel traffic matches the
/// unfiltered run and only the per-commit tree-diff overhead is added.
///
/// Baseline for Stage 5h (pathspec tree-diff cache).
fn bench_pathspec_history_indexing(c: &mut Criterion) {
    let mut group = c.benchmark_group("indexing/with_pathspec");
    group.sample_size(10);

    for &commit_count in &[50usize, 200] {
        let fix = common::FixtureRepo::new();
        fix.seed_commits(commit_count);
        let repo_path = fix.path_buf();

        group.throughput(Throughput::Elements(commit_count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(commit_count), &commit_count, |b, _| {
            b.iter(|| {
                let prev = std::env::current_dir().expect("cwd");
                std::env::set_current_dir(&repo_path).expect("cd into fixture");

                let (req_tx, req_rx) = bounded::<GitReq>(64);
                let (msg_tx, msg_rx) = bounded::<GitMsg>(2048);
                let handle = std::thread::spawn(move || {
                    git::run_git_thread(req_rx, msg_tx, Some("file_*.txt".to_string()), false);
                });

                _ = drain_until_walk_done(&msg_rx);

                drop(req_tx);
                _ = handle.join();
                std::env::set_current_dir(prev).expect("cd back");
            });
        });
    }
    group.finish();
}

/// Literal single-path indexing. A plain path (no glob/magic) takes the
/// tree-entry-oid fast-path in `Walker` — each walked commit is decided
/// by comparing the entry oid at the path against the first parent's,
/// instead of a full per-commit tree-diff. `seed_commits` gives each
/// commit its own `file_NNNN.txt`, so `file_0000.txt` is touched by
/// exactly one commit while every other commit still pays the (now
/// cheap) touch-check — the case the fast-path targets.
///
/// A/B this against `pre-literal-fastpath`: before the change the same
/// literal input walked the pathspec tree-diff, so the delta isolates
/// the fast-path win.
fn bench_literal_path_indexing(c: &mut Criterion) {
    let mut group = c.benchmark_group("indexing/with_literal_path");
    group.sample_size(10);

    for &commit_count in &[50usize, 200] {
        let fix = common::FixtureRepo::new();
        fix.seed_commits(commit_count);
        let repo_path = fix.path_buf();

        group.throughput(Throughput::Elements(commit_count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(commit_count), &commit_count, |b, _| {
            b.iter(|| {
                let prev = std::env::current_dir().expect("cwd");
                std::env::set_current_dir(&repo_path).expect("cd into fixture");

                let (req_tx, req_rx) = bounded::<GitReq>(64);
                let (msg_tx, msg_rx) = bounded::<GitMsg>(2048);
                let handle = std::thread::spawn(move || {
                    git::run_git_thread(req_rx, msg_tx, Some("file_0000.txt".to_string()), false);
                });

                _ = drain_until_walk_done(&msg_rx);

                drop(req_tx);
                _ = handle.join();
                std::env::set_current_dir(prev).expect("cd back");
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_full_history_indexing, bench_pathspec_history_indexing, bench_literal_path_indexing);
criterion_main!(benches);
