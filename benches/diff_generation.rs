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
            Ok(GitMsg::Inspect(InspectMsg::DiffLoaded { .. })) => return,
            Ok(_) => {}
            Err(_) => panic!("worker stalled before DiffLoaded"),
        }
    }
}

/// Drain until the off-thread untracked-files walk reports back.
/// Used by `working_tree_diff` benches as untimed teardown between
/// iterations so the next iteration's status sweep doesn't contend
/// with the previous side-thread scan (which would conflate
/// concurrent FS access into the measurement). Returns silently if
/// no untracked update arrives — commit-diff requests don't spawn
/// one.
fn drain_untracked_update(rx: &crossbeam_channel::Receiver<GitMsg>) {
    let timeout = Duration::from_secs(30);
    loop {
        match rx.recv_timeout(timeout) {
            Ok(GitMsg::Inspect(InspectMsg::UntrackedFilesUpdate { .. })) => return,
            Ok(_) => {}
            Err(_) => return,
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
                req_tx
                    .send(GitReq::Inspect(InspectReq::LoadDiff {
                        target: DiffTarget::Commit(*head),
                        seq: 0,
                        scoped: false,
                    }))
                    .expect("send");
                await_diff(&msg_rx);
            });
        });

        drop(req_tx);
        _ = handle.join();
    }

    // Many-file commit (root commit creating 200 files). Targets the
    // rayon-parallelized per-file render path: each file is an
    // independent diff job, so a single-thread renderer scales
    // linearly with file count while a worker-pool renderer doesn't.
    {
        let fix = common::FixtureRepo::new();
        fix.seed_commits_fast_with_files(1, 200);
        let path = fix.path_buf();
        let (req_tx, msg_rx, head, handle) = boot_worker(&path);
        let head = head.expect("must have at least one commit");

        group.bench_with_input(BenchmarkId::from_parameter("many_files_200"), &head, |b, head| {
            b.iter(|| {
                req_tx
                    .send(GitReq::Inspect(InspectReq::LoadDiff {
                        target: DiffTarget::Commit(*head),
                        seq: 0,
                        scoped: false,
                    }))
                    .expect("send");
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
///
/// Two scenarios, sharing a group so criterion compares them side by side:
///
/// - `staged_plus_unstaged` — 2 tracked files, exercises just the
///   per-file diff render.
/// - `wide_checkout_5000_tracked` — 5000 tracked files at the worktree
///   root, two of them modified. The dir-walk cost of `gix::status`
///   dominates here; this is the scenario monorepos look like and the
///   one the untracked-files-async change targets.
fn bench_working_tree_diff(c: &mut Criterion) {
    let mut group = c.benchmark_group("working_tree_diff");
    group.sample_size(20);

    {
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
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let start = std::time::Instant::now();
                    req_tx
                        .send(GitReq::Inspect(InspectReq::LoadDiff {
                            target: DiffTarget::WorkingTree,
                            seq: 0,
                            scoped: false,
                        }))
                        .expect("send");
                    await_diff(&msg_rx);
                    total += start.elapsed();
                    // Untimed: let the previous iter's side-thread
                    // untracked walk finish so the next iter's status
                    // sweep doesn't share FS bandwidth with it.
                    drain_untracked_update(&msg_rx);
                }
                total
            });
        });

        drop(req_tx);
        _ = handle.join();
    }

    {
        let fix = common::FixtureRepo::new();
        // 5000 tracked files at the worktree root. `seed_commits_fast_with_files`
        // leaves the worktree synced to HEAD, so the index walk has plenty
        // of tracked entries to stat and the dir walk has many on-disk
        // entries to compare against the index.
        fix.seed_commits_fast_with_files(1, 5000);
        let path = fix.path();

        // Stage one change, leave one unstaged, both on existing tracked
        // files (so the staged/unstaged sections have a tiny amount of
        // content to render — the work being measured is the status walk,
        // not the diff body).
        std::fs::write(path.join("file_0000000.txt"), "modified-staged\n").expect("write");
        common::run_git(path, &["add", "file_0000000.txt"]);
        std::fs::write(path.join("file_0000001.txt"), "modified-unstaged\n").expect("write");

        let repo_path = fix.path_buf();
        let (req_tx, msg_rx, _, handle) = boot_worker(&repo_path);

        group.bench_function("wide_checkout_5000_tracked", |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let start = std::time::Instant::now();
                    req_tx
                        .send(GitReq::Inspect(InspectReq::LoadDiff {
                            target: DiffTarget::WorkingTree,
                            seq: 0,
                            scoped: false,
                        }))
                        .expect("send");
                    await_diff(&msg_rx);
                    total += start.elapsed();
                    drain_untracked_update(&msg_rx);
                }
                total
            });
        });

        drop(req_tx);
        _ = handle.join();
    }

    group.finish();
}

/// Exercise the Stage 5h tree-diff cache: walk with a pathspec, then
/// open the diff for the HEAD commit. The pathspec filter pre-populates
/// the cache during walking, so the LoadDiff request is a cache hit.
fn bench_pathspec_walk_then_diff(c: &mut Criterion) {
    let mut group = c.benchmark_group("pathspec_walk_then_diff");
    group.sample_size(20);

    let fix = common::FixtureRepo::new();
    fix.seed_commits(50);
    let repo_path = fix.path_buf();

    group.bench_function("walk_50_then_diff_head", |b| {
        b.iter(|| {
            std::env::set_current_dir(&repo_path).expect("cd into fixture");
            let (req_tx, req_rx) = bounded::<GitReq>(64);
            let (msg_tx, msg_rx) = bounded::<GitMsg>(2048);
            let handle = std::thread::spawn(move || {
                rit::git::run_git_thread(req_rx, msg_tx, Some("file_*.txt".to_string()), false);
            });

            // Walk until done, capturing HEAD.
            let mut head: Option<gix::ObjectId> = None;
            let timeout = Duration::from_secs(15);
            loop {
                match msg_rx.recv_timeout(timeout) {
                    Ok(GitMsg::History(HistoryMsg::Commits { commits, .. })) => {
                        if head.is_none() {
                            head = commits.first().map(|c| c.id);
                        }
                    }
                    Ok(GitMsg::History(HistoryMsg::WalkDone { .. })) => break,
                    Ok(_) => {}
                    Err(_) => panic!("walk timed out"),
                }
            }
            let head = head.expect("at least one commit");

            // Now request the diff for HEAD. Without the cache, this
            // re-runs gix::diff::tree from scratch; with the cache, it
            // reuses what the walker already computed.
            req_tx
                .send(GitReq::Inspect(InspectReq::LoadDiff { target: DiffTarget::Commit(head), seq: 0, scoped: false }))
                .expect("send");
            await_diff(&msg_rx);

            drop(req_tx);
            _ = handle.join();
        });
    });

    group.finish();
}

/// Cursor-scrub scenario: the user holds `j` with the diff pane open,
/// emitting one LoadDiff per row traversed. Only the final target's
/// document is ever shown — the app drops the rest by target mismatch —
/// so the metric that matters is time until the *last* requested diff
/// arrives. This is the bench that motivates (and validates) worker-side
/// coalescing of superseded LoadDiff requests: without coalescing the
/// worker fully renders all N intermediate diffs first.
fn bench_cursor_scrub(c: &mut Criterion) {
    let mut group = c.benchmark_group("cursor_scrub");
    group.sample_size(10);

    const COMMITS: usize = 40;
    const LINES: usize = 300;

    let fix = common::FixtureRepo::new();
    // Each commit rewrites the same file with shifted content so every
    // per-commit diff is a real ~300-line modification, not a trivial
    // one-liner.
    for i in 0..COMMITS {
        let mut content = String::with_capacity(LINES * 24);
        for l in 0..LINES {
            content.push_str(&format!("line {} of revision {i}\n", l + i));
        }
        std::fs::write(fix.path().join("payload.txt"), content).expect("write payload");
        common::run_git(fix.path(), &["add", "-A"]);
        common::run_git(fix.path(), &["commit", "-q", "-m", &format!("revision {i}")]);
    }
    let repo_path = fix.path_buf();

    // Boot a worker and collect every commit id (newest first).
    std::env::set_current_dir(&repo_path).expect("cd into fixture");
    let (req_tx, req_rx) = bounded::<GitReq>(64);
    let (msg_tx, msg_rx) = bounded::<GitMsg>(2048);
    let handle = std::thread::spawn(move || {
        git::run_git_thread(req_rx, msg_tx, None, false);
    });
    let mut ids: Vec<gix::ObjectId> = Vec::new();
    let timeout = Duration::from_secs(10);
    loop {
        match msg_rx.recv_timeout(timeout) {
            Ok(GitMsg::History(HistoryMsg::Commits { commits, .. })) => {
                ids.extend(commits.iter().map(|c| c.id));
            }
            Ok(GitMsg::History(HistoryMsg::WalkDone { .. })) => break,
            Ok(_) => {}
            Err(_) => panic!("worker stalled before WalkDone"),
        }
    }
    assert_eq!(ids.len(), COMMITS, "walker should emit every seeded commit");
    let final_target = DiffTarget::Commit(*ids.last().expect("ids nonempty"));

    group.bench_function("scrub_40_commits_300_lines", |b| {
        b.iter(|| {
            for (i, id) in ids.iter().enumerate() {
                req_tx
                    .send(GitReq::Inspect(InspectReq::LoadDiff {
                        target: DiffTarget::Commit(*id),
                        seq: i as u64,
                        scoped: false,
                    }))
                    .expect("send");
            }
            // Wait for the *final* target's document; earlier DiffLoaded
            // replies (if any survive coalescing) drain along the way.
            // The worker processes in order, so the final target's reply
            // is always the last message of the burst.
            loop {
                match msg_rx.recv_timeout(Duration::from_secs(60)) {
                    Ok(GitMsg::Inspect(InspectMsg::DiffLoaded { document, .. })) if document.target == final_target => {
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => panic!("worker stalled before final DiffLoaded"),
                }
            }
        });
    });

    drop(req_tx);
    _ = handle.join();
    group.finish();
}

criterion_group!(
    benches,
    bench_diff_generation,
    bench_working_tree_diff,
    bench_pathspec_walk_then_diff,
    bench_cursor_scrub
);
criterion_main!(benches);
