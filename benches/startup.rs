//! Startup-latency bench.
//!
//! The existing `indexing` bench measures the spawn → `WalkDone` round
//! trip — the right metric for total throughput, but it tells us
//! nothing about the perceived "staring at an empty log" delay, which
//! is the time from spawn until the first batch of commits arrives.
//!
//! This bench reports both:
//!
//!   - `time_to_first_batch` — spawn → first `Commits` message.
//!   - `time_to_walk_done`   — spawn → `WalkDone`.
//!
//! Each metric is parameterized along three orthogonal dimensions so we
//! can isolate where the startup cost actually lives:
//!
//!   - `commits/{n}`         — varies history depth (worktree files /
//!                              refs held at 1). Exercises the walker
//!                              and per-commit decode.
//!   - `worktree_files/{n}`  — varies HEAD-tree width (commits fixed at
//!                              10k). Exercises `meta::quick_is_dirty`,
//!                              which scans the whole worktree before
//!                              the first commit batch can be emitted.
//!   - `refs/{n}`            — varies ref count (commits fixed at 10k).
//!                              Exercises `meta::load_refs`, which runs
//!                              after the first batch and gates the
//!                              second.

mod common;

use criterion::{BenchmarkGroup, BenchmarkId, Criterion, criterion_group, criterion_main, measurement::WallTime};
use crossbeam_channel::{Receiver, RecvTimeoutError, bounded};
use rit::git::{self, GitMsg, GitReq, HistoryMsg};
use std::{
    path::Path,
    time::{Duration, Instant},
};

const RECV_TIMEOUT: Duration = Duration::from_secs(60);

fn drain_until_first_commits(rx: &Receiver<GitMsg>) {
    loop {
        match rx.recv_timeout(RECV_TIMEOUT) {
            Ok(GitMsg::History(HistoryMsg::Commits { .. })) => return,
            // Empty-repo edge case: no commits, only WalkDone.
            Ok(GitMsg::History(HistoryMsg::WalkDone { .. })) => return,
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => panic!("worker stalled before first batch"),
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn drain_until_walk_done(rx: &Receiver<GitMsg>) {
    loop {
        match rx.recv_timeout(RECV_TIMEOUT) {
            Ok(GitMsg::History(HistoryMsg::WalkDone { .. })) => return,
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => panic!("worker stalled before WalkDone"),
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Run the worker against `repo_path` and time how long until `drain`
/// observes the message it cares about. Channel teardown after timing
/// stops is excluded by virtue of being outside the `Instant::now()` /
/// `elapsed()` pair.
fn timed_iter(iters: u64, repo_path: &Path, drain: fn(&Receiver<GitMsg>)) -> Duration {
    let mut total = Duration::ZERO;
    for _ in 0..iters {
        // `run_git_thread` calls `gix::discover(".")`, so chdir into
        // the fixture for the duration of the run. Sequential by virtue
        // of criterion running benches sequentially within a group.
        let prev = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(repo_path).expect("cd into fixture");

        let (req_tx, req_rx) = bounded::<GitReq>(64);
        // Generous enough that the worker never blocks on send even if
        // we stop draining after the first batch — worst case is ~200
        // messages for 50k commits.
        let (msg_tx, msg_rx) = bounded::<GitMsg>(4096);

        let start = Instant::now();
        let handle = std::thread::spawn(move || {
            git::run_git_thread(req_rx, msg_tx, None, false);
        });
        drain(&msg_rx);
        total += start.elapsed();

        // Worker exits on the next try_recv after req_tx drops; join
        // cleans up the thread before the next iteration.
        drop(req_tx);
        _ = handle.join();
        std::env::set_current_dir(prev).expect("cd back");
    }
    total
}

fn add_startup_benches(group: &mut BenchmarkGroup<'_, WallTime>, repo_path: &Path, id: &str) {
    group.bench_with_input(BenchmarkId::new("time_to_first_batch", id), &(), |b, _| {
        b.iter_custom(|iters| timed_iter(iters, repo_path, drain_until_first_commits));
    });
    group.bench_with_input(BenchmarkId::new("time_to_walk_done", id), &(), |b, _| {
        b.iter_custom(|iters| timed_iter(iters, repo_path, drain_until_walk_done));
    });
}

fn bench_startup(c: &mut Criterion) {
    let mut group = c.benchmark_group("startup");
    group.sample_size(10);

    // Dimension 1: commit count, with worktree / refs held at the
    // baseline. Isolates the per-commit walker cost.
    for &commits in &[1_000usize, 10_000, 50_000] {
        let fix = common::FixtureRepo::new();
        fix.seed_commits_fast(commits);
        add_startup_benches(&mut group, fix.path(), &format!("commits/{commits}"));
    }

    // Dimension 2: HEAD-tree width at fixed history depth. Isolates the
    // `quick_is_dirty` scan that runs before the first commit batch.
    for &files in &[100usize, 1_000, 10_000] {
        let fix = common::FixtureRepo::new();
        fix.seed_commits_fast_with_files(10_000, files);
        add_startup_benches(&mut group, fix.path(), &format!("worktree_files/{files}"));
    }

    // Dimension 3: ref count at fixed history depth. Isolates the
    // `load_refs` enumeration that gates batch 2 (so it shows up in TTW
    // but, on the current worker, not in TTFB).
    for &refs in &[10usize, 1_000, 10_000] {
        let fix = common::FixtureRepo::new();
        fix.seed_commits_fast(10_000);
        fix.add_packed_refs(refs);
        add_startup_benches(&mut group, fix.path(), &format!("refs/{refs}"));
    }

    group.finish();
}

criterion_group!(benches, bench_startup);
criterion_main!(benches);
