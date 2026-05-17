# rit hardening + cleanup plan

## Progress log

| Stage | Commit | Description |
|---|---|---|
| 0 | `fb5a6be` | Prep + baselines: pathspec bench, criterion `pre-cleanup` saved, 2 vs 51 clippy warnings recorded |
| 1 | `cc37902` | Move `[workspace.lints.clippy]` → `[lints.clippy]`. 2 → 60 warnings now firing. CLAUDE.md CI note added. |
| 2 | `b85a710` | Resolve all 60 violations. `cargo clippy --no-deps` is silent. No `#[allow]` left — every dead-code site is either truly used (annotation removed), kept via `pub` field, or `#[expect(..., reason = "…")]`. |
| 3 | _pending_ | Edition 2024 migration |

Project plan derived from the 2026-05-17 code review. Implements every
item raised in the review (correctness, perf, refactor) and turns the
already-written-but-dormant clippy config into something that actually
fires.

## Locked-in decisions

- **Lint level**: keep `warn` in `[lints.clippy]`; document
  `cargo clippy --no-deps -- -D warnings` as the CI invocation in
  `CLAUDE.md` so a future CI hook can adopt it.
- **5g DiffLine prefix refactor**: include, accepting new search
  semantics. Diff search of `+foo` will no longer match the leading `+`.
  README "Known limitations" gets a one-line note.
- **5h pathspec tree-diff cache**: include now. A new
  `indexing/with_pathspec` bench is added at Stage 0 so we have a
  baseline.

## Branch / commit strategy

- Single feature branch `cleanup-2026-05`.
- One commit per logical change so any stage is reverable / bisectable.
- Final merge as a single PR with a bench-delta table in the body.

---

## Stage 0 — Prep & baselines ✅

1. ✅ Verify clean tree.
2. ✅ Add a new bench parameter `indexing/with_pathspec` to
   `benches/indexing.rs` so the Stage 5h change has something to compare
   against.
3. ✅ Save the baseline every perf commit will compare against:
   ```
   cargo bench -- --save-baseline pre-cleanup
   ```
4. ✅ Record the current clippy-warning counts:
   - As-shipped (defaults only): **2** warnings
   - Intended config preview (the curated list from `Cargo.toml`): **51** warnings

**Baseline numbers** (`pre-cleanup`):

| Bench | Mean |
|---|---|
| `indexing/50` | 2.54 ms |
| `indexing/200` | 7.60 ms |
| `indexing_with_pathspec/50` | 5.83 ms |
| `indexing_with_pathspec/200` | 23.96 ms |
| `diff_generation/small_10_lines` | 58.6 µs |
| `diff_generation/large_5000_lines` | 296.3 µs |
| `working_tree_diff/staged_plus_unstaged` | 869.7 µs |

**Gate**

- ✅ `git status` clean.
- ✅ `target/criterion/*/pre-cleanup/` directories exist for every bench
  (including the new pathspec one).
- ✅ Both warning counts recorded above for later comparison.

---

## Stage 1 — Fix the lint config (no functional changes) ✅

`Cargo.toml`: move `[workspace.lints.clippy]` → `[lints.clippy]`. **No
code changes yet** — the goal is to let the curated lints actually fire
so Stage 2 has a real worklist.

Also add a one-line note to `CLAUDE.md` recording the CI command
(`cargo clippy --no-deps -- -D warnings`).

**Gate**

- `cargo build` clean.
- `cargo clippy --no-deps 2>&1 | grep -c '^warning:'` ≈ the count
  recorded in Stage 0's preview.
- `cargo test --tests` passes (28 tests).
- Commit body lists the violation count surfaced.

---

## Stage 2 — Address every lint violation ✅

Walk the now-active warnings:

- **`indexing_slicing` / `string_slice`** (17 + 2 sites): restructure to
  iterator / `.get()` where possible; otherwise annotate
  `#[expect(clippy::indexing_slicing, reason = "guarded by is_empty check at line N")]`
  with a real justification. No bare `#[allow]`.
- **`allow_attributes_without_reason`**: every `#[allow(dead_code)]`
  becomes `#[expect(dead_code, reason = "…")]` — or the dead code goes
  if it really is dead. Note: `last_query` / `last_generation` are
  *used* by `should_narrow`, so they need `expect`, not deletion.
- **`new_without_default`** (default clippy lint, already firing): add
  `Default` impls for `CommitSearchState` / `DiffSearchState`.
- Other lints (`unwrap_used`, `panic`, …): convert to `?` / `.ok_or(...)`
  where possible, `expect` with reason otherwise.

Run `./fix` at the end.

**Gate**

- `cargo clippy --no-deps` returns **zero** warnings.
- `cargo test --tests` passes.
- Diff is annotation-heavy and behaviour-neutral.

---

## Stage 3 — Edition 2024 migration

1. `cargo fix --edition --allow-dirty --all-targets`.
2. Bump `edition = "2024"` in `Cargo.toml`.
3. Rename `gen` field on `Walker` and `gen:` fields in `HistoryMsg`
   variants → **`generation`** (consistent with `App::walk_gen` /
   `CommitSearchState::last_generation`).
4. `./fix`.

**Gate**

- `cargo build`, `cargo build --benches`, `cargo test --tests`,
  `cargo clippy --no-deps` all clean.
- `grep -rn '\bgen\b' src/ benches/` returns zero non-comment matches.
- Commit body quotes any non-trivial cargo-fix changes (most likely:
  lifetime-scope tweaks in `gix` borrow patterns).

---

## Stage 4 — Correctness fixes (one commit each)

### 4a — `hunk_header` empty-range fix

`git/mod.rs:779`. Display start = `0` when length is `0`, else
`start + 1`.

- Add tests for pure-insertion-at-start (`from_lines("", "x\n")`) and
  pure-deletion (`from_lines("x\n", "")`).

**Gate**: new tests pass, existing hunk-header tests unchanged.

### 4b — Status query error reported, not silently "clean"

`git/mod.rs:949`. `compute_short_status_lines_gix` returns a `Result`
(or an enum with an `Errored` variant) and the caller renders an
explicit "status query failed" line instead of the clean message.

**Gate**: 28 existing tests pass; add one test covering an error path
if practical, otherwise a manual note in the commit body.

### 4c — Drop `Option` from `change_path` / `staged_change_path`

`git/mod.rs:765`, `:888`. Return `String` directly. Update 3 callers.

**Gate**: build clean, tests pass.

### 4d — `relative_time` clamps future dates

`git/mod.rs:461`. Negative `s` → return `"now"`.

**Gate**: new unit test for `unix_secs > now`.

---

## Stage 5 — Performance fixes (one commit each)

Workflow per commit: edit → `./fix` →
`cargo bench --bench <relevant> -- --baseline pre-cleanup` → quote the
criterion lines in the commit body.

### 5a — `commits_len()` O(1)

`app.rs:931`. Replace the filter+count with `rows.len() - 1`.

Per-frame call; no direct bench but the simplification stands on its
own.

### 5b — Drop `o.data.clone()` (9 sites)

`git/mod.rs:571,576,581-582,858,864,870-871,925,931-932`. Borrow the
blob directly: `if let Ok(o) = repo.find_object(*oid) { render_*(sink, &p, &o.data); }`.

**Bench**: `diff_generation` (both `large_5000_lines` and
`working_tree_diff/staged_plus_unstaged`).

### 5c — Single-pass `repo.status` in `compute_working_tree_diff`

`git/mod.rs:1036`. Iterate status once, classify each item into
short-status / staged / unstaged buckets. Refactor `render_staged_diff`
and `render_unstaged_diff` to consume pre-classified items rather than
each starting their own status pass.

This is the biggest expected perf win.

**Gate / bench**: existing `working_tree_diff_*` integration tests at
`git/mod.rs:1333,1374` still pass; add one more case for "file modified
in both stage and worktree" if not already covered. Bench:
`working_tree_diff/staged_plus_unstaged`.

### 5d — `Arc<HashMap<...>>` for `RefsLoaded`

Wrap the refs map so the walker, channel message, and app share one
allocation.

**Bench**: `indexing` (modest win on ref-heavy repos).

### 5e — Backfill `RefsLoaded` only over first-batch rows

Worker tags `RefsLoaded` with the row count where refs became live;
app's backfill loop is bounded by that.

**Gate**: rendering still shows refs on the first batch in tests.

### 5f — `extend_commit_matches` reuses lowercased `last_query`

Avoid re-`to_lowercase()` on every batch.

**Bench**: `search/hot_full_scan` (small win).

### 5g — DiffLine prefix encoding refactor

Move `+` / `-` / ` ` out of `DiffLine.text`, prepend at render time
based on `kind`.

- **Behaviour change**: diff search of `+foo` will no longer match the
  leading `+` in the diff pane (decision locked in).
- README "Known limitations" gets a one-line note in this same commit.

**Bench**: `diff_generation/large_5000_lines` (large win — 40k+
`format!` allocations dropped on a 20k-line diff).

### 5h — Pathspec tree-diff cache

Add a small LRU keyed by `(parent_oid, commit_oid)` so
`commit_touches_pathspec_inner`'s records can be reused when
`compute_commit_diff_inner` runs on the same commit. Size cap kept
small (say 64 entries) so memory growth on long walks is bounded.

**Bench**: the new `indexing/with_pathspec` baseline saved at Stage 0.

---

## Stage 6 — Structural refactor (zero behaviour change)

### 6a — Split `src/git/mod.rs`

```
git/
  mod.rs          re-exports + GitReq/GitMsg + constants
  worker.rs       run_git_thread, process_request, batch loop
  walk.rs         Walker, build_commit_info, pathspec helpers
  diff.rs         commit-diff renderer, DiffSink, hunk_header,
                  classify_skip, render_file_*
  status.rs       working-tree diff, short-status renderer
  meta.rs         repo_info_for, working_tree_author, quick_is_dirty,
                  load_refs, relative_time, format_timestamp
```

### 6b — Split `src/app.rs`

```
app/
  mod.rs          App + run loop + dispatch
  state.rs        LogState, DiffState, StatusState, YankFeedback,
                  LogRow, WorkingTreeRow, Focus
  search.rs       shared SearchState + should_narrow + cycle +
                  commit_matches
  input.rs        handle_input + handle_*_key variants
  clipboard.rs    yank_to_clipboard with platform cfgs
```

### 6c — Unify `CommitSearchState` / `DiffSearchState`

Shared `SearchState` struct with `active`/`query`/`matches`/`current`
and the shared methods. `CommitSearchState` becomes a wrapper that
adds the narrowing fields.

### 6d — Unify `*_jump_first_at_or_after_cursor`

Free function
`jump_first_at_or_after(&[usize], &mut usize, cursor: usize) -> Option<usize>`.
Two callers.

### 6e — Consolidate test fixtures

`src/test_support.rs` gated on
`#[cfg(any(test, feature = "test-support"))]`. Re-export from `lib.rs`
so benches can use it. `benches/common/mod.rs` either deletes its
copies or thinly delegates.

**Gate for every 6.x**

- `cargo build`, `cargo build --benches`, `cargo test --tests`,
  `cargo clippy --no-deps` all clean.
- `cargo bench --quick` runs to completion (no algorithmic regression).
- Diff is purely structural — `git log --stat` should show file moves,
  not large content rewrites.

---

## Stage 7 — Final validation

- `./fix && cargo build && cargo build --benches && cargo clippy --no-deps && cargo test --tests`.
- `cargo bench -- --baseline pre-cleanup` — capture criterion output as
  a markdown table for the merge commit body.
- Manual UI smoke (run in this repo's directory):
  - log scrolls: ↓ / ↑ / Ctrl-D / Ctrl-U / g / G
  - `/` search in both panes; `n` / `N` cycle; Esc clears
  - `Enter` opens diff; `Tab` swaps focus; `q` / Esc closes
  - `s` opens status pane; `q` closes
  - `R` reloads; `y` yanks (verify clipboard)
  - `?` help; `#` toggle line numbers; `v` toggle hunks
- Final commit: summary + bench delta table + manual checklist.

---

## Validation rubric (applies to every stage)

| Check | Command |
|---|---|
| Formatting | `./fix` (`cargo +nightly fmt`) |
| Debug build | `cargo build` |
| Bench compile | `cargo build --benches` |
| Lints (must be zero) | `cargo clippy --no-deps` |
| Tests | `cargo test --tests` |
| Perf (Stage 5 commits) | `cargo bench --bench <name> -- --baseline pre-cleanup` |
| Manual UI (Stage 7) | `cargo run --release` |

---

## Risks / things to watch

- **Edition 2024 lifetime-scope changes**: `cargo fix --edition` usually
  handles these, but the `Walker<'r>` borrow of `gix::Repository` is
  exactly the shape that can need a tweak. Review the migrator's diff
  carefully before committing.
- **Stage 5c** (single status pass) is the largest perf rewrite and the
  most likely to surface a behaviour difference vs the current 3-pass
  implementation. Existing tests should catch divergence; add one more
  case for "file modified in both stage and worktree" if not already
  covered.
- **`compute_short_status_lines_gix` returning `Result`** ripples to its
  caller in `compute_working_tree_diff`. Single internal caller, but
  also a planned area in `inspect.rs:23`; align error-variant naming.
- **Stage 6 file moves**: criterion baselines are keyed by bench name
  not source path, so the file moves don't invalidate them. Re-confirm
  before relying on it.
- **Stage 5g** is a user-visible behaviour change. README update lands
  in the same commit so docs don't lag.
