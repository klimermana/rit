# rit

A terminal git log/diff viewer in the spirit of [tig](https://github.com/jonas/tig). Browse commits, view diffs, search, and inspect working tree status without leaving the terminal.

> **This project was entirely vibe-coded by Claude.** Every line of Rust here was written by an LLM (Anthropic's Claude). Treat it accordingly: it works for me, it has not been hardened, audited, or maintained in any traditional sense, and the architecture is whatever the model felt like at the time. Issues and PRs are welcome but no support is implied.

## Features

- Browse commit log with refs, author, relative date — and an optional ASCII commit graph column (`-g`)
- Side-by-side or stacked diff view, depending on terminal aspect ratio
- Live incremental search by commit message or author
- Working-tree status view (staged / unstaged diffs)
- Path filter: limit the log to commits touching a given path
- Lazy commit walking (handles large histories without blocking the UI)
- Off-thread working-tree scan — first paint stays fast even on wide checkouts (10k+ tracked files)
- Yank commit hash to clipboard (`pbcopy` / `xclip` / `xsel`)
- Event-driven UI — idles at 0% CPU between keystrokes

## Build

```bash
cargo build --release
```

The binary lands at `target/release/rit`.

## Usage

```bash
rit                       # show full log
rit path/to/file.rs       # pathspec filter (git log -- semantics)
rit 'src/**/*.rs'         # glob pathspec
rit ':!target'            # exclusion pathspec
rit -g                    # render the ASCII commit-graph column (opt-in; -g / --graph)
rit --help
```

`rit` discovers the repo from the current directory upward, like git itself.

## Keybindings

```
Navigation
  j / ↓                Move down
  k / ↑                Move up
  g / G                Top / bottom
  Ctrl+D / Ctrl+U      Half-page down / up
  PageDown / PageUp    Full-page down / up

Diff view
  Enter                Open diff for the selected commit
  Tab                  Switch focus between log and diff
  q / Esc              Close diff (quit from log if no diff open)

Search
  /                    Start search (commit message or author)
  n / N                Next / previous match
  Esc                  Clear search

Display
  #                    Toggle line numbers in diff view
  v                    Toggle patch hunks in diff view (summary-only when off)

Actions
  y                    Yank commit hash to clipboard
  s                    Toggle working-tree status view
  R                    Reload log from HEAD
  ?                    Toggle help
  q / Ctrl+C           Quit
```

## Dependencies

- [`gix`](https://crates.io/crates/gix) — pure-Rust git implementation
- [`ratatui`](https://crates.io/crates/ratatui) + [`crossterm`](https://crates.io/crates/crossterm) — TUI rendering and input
- [`crossbeam-channel`](https://crates.io/crates/crossbeam-channel) — UI ↔ worker channels
- [`similar`](https://crates.io/crates/similar) — line-level diff algorithm
- [`clap`](https://crates.io/crates/clap) — argument parsing
- [`chrono`](https://crates.io/crates/chrono), [`anyhow`](https://crates.io/crates/anyhow), [`compact_str`](https://crates.io/crates/compact_str)

## Architecture, briefly

Three long-lived threads communicate via bounded crossbeam channels:

- **Main / UI thread** — runs the ratatui draw loop, parked in `select!` on input + git messages + a yank-feedback timer. Redraws only when something changes.
- **Git worker thread** — owns the `gix::Repository`, walks history continuously in the background (self-paced, no `LoadMore` ping required) and services `LoadDiff` / `LoadStatus` / `RefreshWorkingTreeMeta` / `Reload` on demand. Tags emitted commits with a generation counter so stale messages from a previous walk are dropped after a reload.
- **Input thread** — polls crossterm and forwards key events.

A short-lived fourth thread also gets spawned by the worker: `quick_is_dirty` runs the full `gix::status` sweep, which on a wide checkout takes long enough to block the first commit batch. Pulling it off the worker thread keeps first paint constant-time in the worktree dimension; the dirty bit lands as a follow-up `WorkingTreeMeta` message whenever the scan finishes.

The diff/status content is stored as a domain `DiffLine { kind, text }` and only converted to ratatui `Line` at draw time, which keeps the data model decoupled from the rendering library.

## Known limitations

- Diff text doesn't include git's `index <oid>..<oid> <mode>` header line, rename headers (`similarity index N%`), or `\ No newline at end of file` markers — rit renders the hunks themselves but not those metadata flourishes.
- In-diff search (`/` while the diff pane is focused) matches the body content only — the leading `+`/`-`/` ` of each line is rendered at display time, not stored, so `/+foo` won't find an addition; use `/foo`.
- No mouse support.
- Linux/macOS only for clipboard yank; everything else is portable.

## License

[MIT](LICENSE) — public domain dedication. Do whatever you want with it. No warranty.
