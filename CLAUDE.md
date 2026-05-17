# Working in this repo

## Before every commit: run `./fix`

`./fix` is `cargo +nightly fmt`. Run it before staging changes — the
nightly formatter is the source of truth for layout in this codebase
and stable rustfmt sometimes disagrees.

```
./fix && git add -u && git commit ...
```

## Perf-affecting changes: confirm with benchmarks, don't guess

This repo ships a criterion bench harness with three targets:
`indexing`, `search`, `diff_generation`. Use it whenever you touch
hot-path code (git worker internals, diff rendering, search loops,
anything that the existing benches exercise).

**Workflow:**

```
# 1. Save a baseline BEFORE the change.
cargo bench -- --save-baseline pre-<change-name>

# 2. Make the change.

# 3. Compare against the baseline.
cargo bench -- --baseline pre-<change-name>
```

Criterion prints `change: [-X% +Y%] (Improved | Within noise threshold
| Regressed)` for every case. Quote the actual numbers in the commit
message — "no change" / "Y× faster" claims should be measurable, not
asserted.

For a quick smoke run during development, use `--quick`:

```
cargo bench --bench search -- --quick
```

If a change is supposed to make something faster, the bench should
show it. If a change might regress performance, the bench should
prove it doesn't. Either way, the numbers go in the commit body.

**Don't optimize without measuring first.** The existing benches
exist precisely so caches, algorithmic rewrites, and similar work
can be evaluated against real data instead of intuition.

## Other useful commands

- `cargo build` — debug build (fast)
- `cargo test --tests` — run the 28 unit + integration tests
- `cargo build --benches` — verify bench code compiles without running benches
