//! Diff-generation latency bench.
//!
//! Drives the git worker through a `LoadDiff` request for the HEAD
//! commit on two fixtures: one small (10 lines changed), one large
//! (5000 lines changed). Round-trip time from request to
//! `DiffLoaded` is the metric. Baseline for changes to compute_commit_diff
//! (binary detection, guardrail cutoffs, gix-diff porting).

mod common;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use crossbeam_channel::{RecvTimeoutError, bounded};
use rit::{
    git::{self, GitMsg, GitReq, HistoryMsg, InspectMsg, InspectReq},
    model::DiffTarget,
};
use std::time::Duration;

/// Spin up a worker, drain the initial bootstrap messages (RepoInfo,
/// WorkingTreeMeta, the first Commits batch, RefsLoaded, WalkDone for
/// these small repos), and return the channels + thread handle.
///
/// Note: `run_git_thread` discovers the repo from the current dir. Each
/// call here chdirs the process; that's acceptable because previously
/// spawned workers hold their own `gix::Repository` handle that doesn't
/// depend on cwd anymore.
fn boot_worker(
    repo_path: &std::path::Path,
) -> (
    crossbeam_channel::Sender<GitReq>,
    crossbeam_channel::Receiver<GitMsg>,
    Option<gix::ObjectId>,
    std::thread::JoinHandle<()>,
) {
    std::env::set_current_dir(repo_path).expect("cd into fixture");

    let (req_tx, req_rx) = bounded::<GitReq>(64);
    let (msg_tx, msg_rx) = bounded::<GitMsg>(2048);
    let handle = std::thread::spawn(move || {
        git::run_git_thread(req_rx, msg_tx, None, false);
    });

    // Wait until indexing is done (it's tiny -- 1 commit).
    let mut head: Option<gix::ObjectId> = None;
    let timeout = Duration::from_secs(10);
    loop {
        match msg_rx.recv_timeout(timeout) {
            Ok(GitMsg::History(HistoryMsg::Commits { commits, .. })) => {
                if head.is_none() {
                    head = commits.first().map(|c| c.id);
                }
            }
            Ok(GitMsg::History(HistoryMsg::WalkDone { .. })) => break,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => panic!("worker stalled before WalkDone"),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    (req_tx, msg_rx, head, handle)
}

fn await_diff(rx: &crossbeam_channel::Receiver<GitMsg>) {
    let timeout = Duration::from_secs(30);
    loop {
        match rx.recv_timeout(timeout) {
            Ok(GitMsg::Inspect(InspectMsg::DiffLoaded(_))) => return,
            Ok(_) => {}
            Err(_) => panic!("worker stalled before DiffLoaded"),
        }
    }
}

fn bench_diff_generation(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff_generation");
    group.sample_size(20);

    for &(label, lines) in &[("small_10_lines", 10usize), ("large_5000_lines", 5000)] {
        let fix = common::FixtureRepo::new();
        fix.commit_with_n_lines("payload.txt", lines);
        let path = fix.path_buf();

        let (req_tx, msg_rx, head, handle) = boot_worker(&path);
        let head = head.expect("must have at least one commit");

        group.bench_with_input(BenchmarkId::from_parameter(label), &head, |b, head| {
            b.iter(|| {
                req_tx.send(GitReq::Inspect(InspectReq::LoadDiff(DiffTarget::Commit(*head)))).expect("send");
                await_diff(&msg_rx);
            });
        });

        drop(req_tx);
        _ = handle.join();
    }
    group.finish();
}

/// Working-tree diff bench. Exercises the gix-native staged + unstaged
/// diff render code path that replaced the `git diff` / `git diff --cached`
/// shellouts. Setup: a fixture repo with one baseline commit, then one
/// staged modification and one unstaged modification — so both renderers
/// have content to produce.
fn bench_working_tree_diff(c: &mut Criterion) {
    let mut group = c.benchmark_group("working_tree_diff");
    group.sample_size(20);

    let fix = common::FixtureRepo::new();
    let path = fix.path();
    // Baseline so the index has prior content.
    std::fs::write(path.join("staged.txt"), "alpha\nbeta\ngamma\n").expect("write");
    std::fs::write(path.join("unstaged.txt"), "one\ntwo\nthree\n").expect("write");
    common::run_git(path, &["add", "-A"]);
    common::run_git(path, &["commit", "-q", "-m", "baseline"]);
    // Stage one change, leave another unstaged.
    std::fs::write(path.join("staged.txt"), "alpha\nBETA\ngamma\nfour\n").expect("write");
    common::run_git(path, &["add", "staged.txt"]);
    std::fs::write(path.join("unstaged.txt"), "one\nTWO\nthree\n").expect("write");

    let repo_path = fix.path_buf();
    let (req_tx, msg_rx, _, handle) = boot_worker(&repo_path);

    group.bench_function("staged_plus_unstaged", |b| {
        b.iter(|| {
            req_tx.send(GitReq::Inspect(InspectReq::LoadDiff(DiffTarget::WorkingTree))).expect("send");
            await_diff(&msg_rx);
        });
    });

    drop(req_tx);
    _ = handle.join();
    group.finish();
}

criterion_group!(benches, bench_diff_generation, bench_working_tree_diff);
criterion_main!(benches);
