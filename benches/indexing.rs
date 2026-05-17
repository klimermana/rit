//! Indexing throughput bench.
//!
//! Measures end-to-end time from spawning the git worker on a fixture
//! repo to draining the final `WalkDone` message. Baseline for future
//! continuous-indexing changes (commit caching, lazier ref hydration,
//! etc.).

mod common;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use crossbeam_channel::{bounded, RecvTimeoutError};
use rit::{
    git::{self, GitMsg, GitReq, HistoryMsg},
    model::PathFilter,
};
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

                let _ = drain_until_walk_done(&msg_rx);

                drop(req_tx);
                let _ = handle.join();
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
                    git::run_git_thread(req_rx, msg_tx, Some(PathFilter::new("file_*.txt")), false);
                });

                let _ = drain_until_walk_done(&msg_rx);

                drop(req_tx);
                let _ = handle.join();
                std::env::set_current_dir(prev).expect("cd back");
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_full_history_indexing, bench_pathspec_history_indexing);
criterion_main!(benches);
