# rit

A terminal git log/diff viewer in the spirit of [tig](https://github.com/jonas/tig). Browse commits, view diffs, search, and inspect working tree status without leaving the terminal.

> **This project was entirely vibe-coded by Claude.** Every line of Rust here was written by an LLM (Anthropic's Claude). Treat it accordingly: it works for me, it has not been hardened, audited, or maintained in any traditional sense, and the architecture is whatever the model felt like at the time. Issues and PRs are welcome but no support is implied.

## Features

- Browse commit log with ASCII graph, refs, author, relative date
- Side-by-side or stacked diff view, depending on terminal aspect ratio
- Live incremental search by commit message or author
- Working-tree status view (staged / unstaged diffs)
- Path filter: limit the log to commits touching a given path
- Lazy commit walking (handles large histories without blocking the UI)
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

Diff view
  Enter                Open diff for the selected commit
  Tab                  Switch focus between log and diff
  q / Esc              Close diff (quit from log if no diff open)

Search
  /                    Start search (commit message or author)
  n / N                Next / previous match
  Esc                  Clear search

Actions
  y                    Yank commit hash to clipboard
  s                    Toggle working-tree status view
  R                    Reload log from HEAD
  #                    Toggle line numbers in diff view
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

Three threads communicate via bounded crossbeam channels:

- **Main / UI thread** — runs the ratatui draw loop, parked in `select!` on input + git messages + a yank-feedback timer. Redraws only when something changes.
- **Git worker thread** — owns the `gix::Repository`, walks history lazily in response to `LoadMore` requests, services `FetchDiff` / `FetchStatus` / `Reload` on demand. Tags emitted commits with a generation counter so stale messages from a previous walk are dropped after a reload.
- **Input thread** — polls crossterm and forwards key events.

The diff/status content is stored as a domain `DiffLine { kind, text }` and only converted to ratatui `Line` at draw time, which keeps the data model decoupled from the rendering library.

## Known limitations

- Working-tree status shells out to the `git` binary rather than using gix directly (the gix port of staged/unstaged diff formatting would be a project of its own).
- No mouse support.
- Linux/macOS only for clipboard yank; everything else is portable.

## License

[MIT](LICENSE) — public domain dedication. Do whatever you want with it. No warranty.
