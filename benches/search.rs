//! Commit-search latency bench.
//!
//! Builds a synthetic Vec<CommitRecord> (no git fixture needed) and
//! measures how long the commit_matches-style closure takes for both
//! hot ("matches early") and cold ("no match") queries. Baseline for
//! changes to the matching loop (e.g. case-insensitive substring,
//! aho-corasick, etc.).

use compact_str::CompactString;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rit::model::{CommitRecord, CommitSearchText};

mod common;

fn fake_commits(n: usize) -> Vec<CommitRecord> {
    // Empty ObjectId is fine — we never look the commit up via the repo here.
    let zero_id = gix::ObjectId::null(gix::hash::Kind::Sha1);
    (0..n)
        .map(|i| {
            let author = format!("Author Number {i:05}");
            let summary = format!("Refactor module {i:05} -- improve clarity and reduce duplication");
            CommitRecord {
                id: zero_id,
                short_id: format!("{i:07x}").into(),
                authored_unix_secs: 0,
                authored_relative: "1d ago".into(),
                author: CompactString::from(author.as_str()),
                summary: summary.clone(),
                refs: Vec::new(),
                graph: CompactString::default(),
                search: CommitSearchText {
                    author_lower: CompactString::from(author.to_lowercase().as_str()),
                    summary_lower: summary.to_lowercase(),
                },
            }
        })
        .collect()
}

fn matches(c: &CommitRecord, q: &str) -> bool {
    c.search.summary_lower.contains(q) || c.search.author_lower.contains(q)
}

fn bench_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("search");
    for &n in &[1_000usize, 10_000, 50_000] {
        let commits = fake_commits(n);
        group.throughput(Throughput::Elements(n as u64));

        // Hot query: matches a substring present in every summary.
        group.bench_with_input(BenchmarkId::new("hot_full_scan", n), &commits, |b, commits| {
            b.iter(|| commits.iter().filter(|c| matches(c, "refactor")).count());
        });

        // Cold query: matches nothing, so worst-case full scan happens.
        group.bench_with_input(BenchmarkId::new("cold_full_scan", n), &commits, |b, commits| {
            b.iter(|| commits.iter().filter(|c| matches(c, "xyzzy_no_match")).count());
        });
    }
    group.finish();
}

criterion_group!(benches, bench_search);
criterion_main!(benches);
