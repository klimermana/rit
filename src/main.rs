mod app;
mod git;
mod model;
mod ui;

use anyhow::Result;
use app::App;
use clap::Parser;
use crossbeam_channel::{bounded, Sender};
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use model::PathFilter;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    io::{self, BufWriter},
    panic,
    time::Duration,
};

/// Interactive terminal git log/diff viewer.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Limit the log to commits touching this pathspec. Matches
    /// `git log -- <pathspec>` semantics (supports globs and
    /// `:!exclude` magic).
    path: Option<String>,

    /// Render the ASCII commit graph column. Off by default because the
    /// per-commit lane bookkeeping is non-trivial on deep monorepos.
    #[arg(short = 'g', long = "graph")]
    graph: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path_filter = cli.path.map(PathFilter::new);
    let graph_enabled = cli.graph;

    // Restore terminal on panic.
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stderr(), LeaveAlternateScreen);
        original_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // Wrap stdout in BufWriter so ratatui's many small per-cell writes
    // coalesce into far fewer syscalls. CrosstermBackend calls flush() at
    // the end of each frame, so latency is not affected.
    let backend = CrosstermBackend::new(BufWriter::new(stdout));
    let mut terminal = Terminal::new(backend)?;

    let (req_tx, req_rx) = bounded::<git::GitReq>(64);
    let (msg_tx, msg_rx) = bounded::<git::GitMsg>(256);
    let (input_tx, input_rx) = bounded::<Event>(64);

    let git_tx = msg_tx.clone();
    std::thread::spawn(move || {
        git::run_git_thread(req_rx, git_tx, path_filter, graph_enabled);
    });

    std::thread::spawn(move || {
        input_thread(input_tx);
    });

    let mut app = App::new(req_tx, msg_rx, input_rx);
    let result = app.run(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn input_thread(tx: Sender<Event>) {
    loop {
        match event::poll(Duration::from_millis(200)) {
            Ok(true) => match event::read() {
                Ok(evt) => {
                    if tx.send(evt).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            },
            Ok(false) => {}
            Err(_) => return,
        }
    }
}
