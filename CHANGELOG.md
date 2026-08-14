# Changelog

All notable user-visible changes to `rit` are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project (loosely) follows [Semantic
Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Copying from the diff pane now keeps the `+`/`-`/space prefixes, so
  a multi-line copy still reads as a diff instead of an ambiguous
  blob of lines. The prefix is a real selectable cell right after the
  line-number gutter: full-line and multi-line selections include it,
  while a drag that starts mid-line (or a double-clicked word) still
  copies just the content. Line numbers are never copied.

- Copying (drag-to-copy or `y`) on a machine without a display no
  longer prints `Error: Can't open display: (null)` into the UI:
  clipboard helper processes now run with their output discarded, X11
  helpers (`xclip`/`xsel`) are only tried when `DISPLAY` is set, and
  `wl-copy` is used on Wayland. When no helper is available — e.g.
  over SSH with no display — the copy falls back to an OSC 52 escape
  sequence, which asks the terminal emulator itself to set the
  clipboard (most terminals support it; some gate it behind a
  "allow clipboard access" setting).

### Changed

- Renamed files are now shown as renames instead of disappearing or
  splitting in two. Commit diffs pair a deletion and an addition with
  identical content into one `rename from` / `rename to` entry (exact
  renames — a rename with edits in the same commit still shows as
  delete + add). The staged section uses git's own rename detection,
  so `git mv` — even followed by edits — renders rename headers plus
  the real hunks, instead of a bare 0-line modification at the new
  path that never mentioned the old one; the short-status block
  spells it `R old -> new` like `git status --short`. The single-file
  view (`o`) on a rename destination shows the file as unchanged
  content with a `renamed from` note rather than as all-new.
  Interleaved A/B benchmarks show a ~7–16% slowdown in the commit
  history walk (indexing) from the gix bump; diff generation, search,
  and scrolling are unaffected.

### Added

- "Staged changes" row in the log: when the index differs from HEAD, a
  second pseudo-row appears under "Uncommitted changes". Selecting it
  shows a staged-only diff (index vs HEAD) — worktree edits that
  aren't staged and untracked files are excluded, so it answers
  "what exactly will `git commit` pick up". The single-file view (`o`)
  inside it diffs HEAD's blob against the index blob. The row appears
  and disappears with the same background check that drives the
  dirty indicator.
- Mouse support. The terminal's mouse is now captured: the wheel
  scrolls whichever pane is under the pointer (and the status, refs,
  and blame views), clicking a pane focuses it (clicking a log row
  also selects that commit), and dragging in the diff pane makes a
  character-precise text selection *confined to that pane* — releasing
  the button copies it to the clipboard with the gutter (line numbers,
  `+`/`-` markers) stripped from every fully-selected line.
  Double-clicking selects the word under the cursor (word characters
  follow iTerm's default, so paths, flags, and versions come out
  whole); triple-clicking selects the whole line. This fixes copying
  out of a side-by-side layout,
  where the terminal's own selection would sweep up the log pane too.
  The terminal-native selection remains available via Option/Alt+drag.
- Single-file diff view: `o` in the diff pane takes over the pane with
  the full diff of the file under the viewport — rendered as the
  *entire file* with added/removed lines highlighted inline, so every
  change is visible in its surrounding context instead of ±3-line
  hunks. It uses a much higher size cap (8 MiB vs the inline 256 KiB),
  so files whose diffs were suppressed as "large" are now viewable.
  The view opens scrolled to the first changed line (with a few lines
  of leading context) instead of the top of the file. Search,
  horizontal scroll, line numbers, and `b` (blame) all work inside
  it; `q`/`Esc` returns to the multi-file diff exactly where you left
  it. Works for commit diffs and the working tree (where it shows the
  file against HEAD).

- File picker: `t` in the diff pane turns the diffstat into a
  navigable file list — `j`/`k` select a file (wrapping past either
  end), `/` filters by filename (non-matching rows dim and `j`/`k`
  move only between matches; `Esc` clears the filter before closing
  the picker), `Enter` jumps to its diff section, `o` opens its
  single-file view, `q`/`Esc` cancels. Files whose diffs were
  suppressed by the file/line guardrails open the single-file view
  directly on `Enter`, so every file in a huge commit is now
  reachable. The status bar shows a `PICK` mode chip while the picker
  is active.

- The diff guardrail notices now say how to see the suppressed
  content (`t` to list files, `o` to open one) instead of being dead
  ends.

- Pathspec-scoped diffs: when `rit` is launched with a path/pathspec
  (`rit src/ui`), the diff pane now shows only the hunks for files
  matching that pathspec, with a faint note counting the hidden files.
  `f` toggles between the scoped and full diff, and the pane title
  shows `[only <pathspec>]` while scoping is active. Filtering happens
  before any blob is loaded, so scoped diffs of wide commits skip the
  render cost of unrelated files entirely (`diff_generation` benches
  for the unscoped path are unchanged — all cases "no change
  detected").

- Mode indicator: the status bar now starts with a colored, bold chip
  naming the active keymap — `LOG`, `DIFF`, `SEARCH`, `STATUS`,
  `REFS`, `BLAME`, or `HELP` — so it's always clear which mode you're
  in. The chip's precedence mirrors the input dispatcher exactly.

### Changed

- Release builds now use fat LTO (was thin). Working-tree diffs on a
  wide checkout (5000 tracked files) run ~4-7× faster — the
  `gix::status` sweep benefits from cross-crate inlining — with every
  other benchmark unchanged within noise. Clean release builds take
  ~28 s longer; dev builds are unaffected.

### Fixed

- Reopening the working-tree diff while a previous untracked-files
  scan was still running could resolve the "(scanning for untracked
  files…)" placeholder with the *old* scan's results and silently drop
  the fresh scan. Diff requests and their follow-ups now carry a
  sequence number, so stale replies are discarded instead of racing
  the current document.
- `Ctrl+C` now quits from every mode — previously it was ignored while
  a search prompt, the status view, or the help popup was open.
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

- Which pane is focused is now much more legible: the focused pane
  gets a thick cyan border and a bold title (unfocused panes drop to a
  plain dark-gray border and dim title), and the log's bright
  selection bar follows focus — when the diff pane is driving, the
  log's cursor row fades to a faint bar so only one pane shows the
  active highlight.
- The mode chip's color now doubles as the focused border's accent:
  the active pane (and the full-screen status/refs/blame views and
  help popup) borders in the same color as the chip, so one color
  consistently means one mode — blue LOG, cyan DIFF, magenta BLAME,
  green REFS, yellow SEARCH, white HELP.
- Indexing is 27–58% faster: the git worker now configures gix's
  decoded-object cache (unset by default), sized for tree diffs
  against the checkout with a 4 MiB floor. The history walk and its
  pathspec filter revisit shared subtrees constantly, so the cache
  removes most repeat object decoding — `indexing/with_pathspec/200`
  drops from 24 ms to 9.9 ms (−58%), plain `indexing/200` from 7.3 ms
  to 4.1 ms (−44%).
- Scrubbing through commits with the diff pane open no longer computes
  a diff for every commit passed over. The git worker now coalesces
  queued diff requests to the newest one before doing any work (only
  the newest result would ever be shown), so the diff you land on
  appears immediately instead of waiting behind every intermediate
  one — and background indexing is no longer starved while the queue
  drains. The request channel is also unbounded now, so the UI thread
  can never block on a send mid-keystroke.
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

- Blame view. `b` in the diff pane annotates the file at the top of
  the viewport (at that commit, or HEAD for the working-tree diff);
  `rit blame <path>` launches straight into it. Every line shows the
  commit, author, and age that introduced it (gix-native blame — no
  git binary needed, runs off-thread with a loading indicator).
  `Enter` opens the selected line's commit diff, `,` re-blames at that
  commit's parent (following renames), Backspace walks back, `y`
  yanks the line's commit hash.
- Refs view: `r` opens a full-screen browser of local branches,
  remote branches, and tags (annotated tags peeled to their commit),
  each with the target commit's summary and relative date. `Enter`
  re-roots the log at the selected ref — `R` afterwards reloads from
  that same root.
- `]` / `[` jump to the next / previous file in the diff pane, so a
  many-file commit can be skimmed without scrolling through every
  hunk. `[` above the first file returns to the commit header.
- Display toggles in the log view, tig-style: `D` cycles the date
  column (relative → absolute local time → hidden), `A` cycles the
  author column (full → abbreviated → hidden), and `X` switches
  between the short and full 40-char commit hash.
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
