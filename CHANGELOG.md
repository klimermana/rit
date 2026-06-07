# Changelog

All notable user-visible changes to `rit` are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project (loosely) follows [Semantic
Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

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
