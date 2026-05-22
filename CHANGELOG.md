# Changelog

All notable user-visible changes to `rit` are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project (loosely) follows [Semantic
Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
