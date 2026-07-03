# Changelog

All notable user-visible changes to `rit` are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project (loosely) follows [Semantic
Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Opening a commit no longer waits for background indexing. The git
  worker's history walk is now preemptible: it polls for queued
  requests during a batch and yields immediately, so a diff (or
  reload) request is serviced right away instead of blocking behind
  the in-flight sweep. Back-to-back `indexing` benches show this is
  performance-neutral (indexing/50 −5.5%, indexing/200 −1.9%, pathspec
  cases within noise).
- Single-file / pathspec history streams in as it's found. Each walk
  batch now examines a bounded window of commits and emits whatever
  matched instead of tree-diffing up to thousands of commits to fill a
  full batch first, so the first matching commits appear right away
  rather than after a long up-front scan.

### Changed

- Single-file / literal-path history now decides whether each commit
  touches the path by comparing the tree-entry oid at that path against
  the first parent, instead of running a full per-commit tree-diff and
  pathspec match. A directory's tree oid changes iff anything under it
  changed, so this is exact for files and directories alike, and it
  skips the tree-diff record allocation, the bounded-LRU cache churn,
  and the pathspec match loop the old path paid per commit. Glob/magic
  pathspecs (`*.rs`, `:!target`, …) still use the full pathspec matcher.
  On the flat-file `indexing/with_literal_path` bench this is ~3.7–3.9%
  faster; the gain is larger on commits that touch many files.
- Working-tree diff: the untracked-files walk now runs on a side
  thread instead of the diff critical path. The diff pane shows
  staged/unstaged content as soon as the index walk finishes and the
  `?? path` rows fold in once the dir walk completes. On a 5000-file
  flat checkout the bench `working_tree_diff/wide_checkout_5000_tracked`
  drops from 68 ms to 6.0 ms (~11× faster). A faint
  "(scanning for untracked files…)" placeholder sits in the spot
  until the result arrives.
- Per-file diff rendering swapped from the `similar` crate to
  `imara-diff`'s Histogram algorithm (a port of git's libxdiff).
  Matches `git diff -U3`'s hunk grouping and produces identical
  headers. Indent-based slider postprocessing keeps hunks aligned on
  syntactic boundaries.
- Per-file diff work (commit diffs, staged section, unstaged section)
  is parallelized via rayon above a small file-count threshold. On a
  200-file commit, the bench `diff_generation/many_files_200` drops
  from 3.0 ms to 1.83 ms (-39%). Below the threshold the renderer
  stays serial so 1–2-file diffs don't pay scheduler overhead.

- Log search now also matches against ref names attached to a commit
  (tags, local branches, remote branches, HEAD) alongside the existing
  message and author fields. Useful for jumping to a release commit by
  typing the tag (`v0.2.0`) or to a feature head by typing the branch.

### Added

- Horizontal scroll in the diff pane. `h` / `l` (or `←` / `→`) shift
  the view 4 columns at a time so truncated long lines can be read in
  full; `0` snaps back to column 0. Reset automatically when a new
  diff is loaded.
- Positional CLI arg now accepts a commit revision (full or short hash,
  branch, tag, `HEAD~3`, …) and walks the log from that commit instead
  of HEAD. The arg is tried as a revision first via `gix` rev-parse and
  falls back to a pathspec if it can't be peeled to a commit, so the
  existing `rit path/to/file.rs` form still works. To disambiguate a
  path that happens to look like a hash, prefix it: `rit ./abc1234`.
