//! `ikit tui`: a terminal UI over the whole ikit flow, built with ratatui.
//!
//! It is a front end only. Every list it shows comes from `ikit … -o json` and every action
//! runs an ordinary `ikit` command, so the CLI and the MCP server stay the single source of
//! behavior and nothing here can drift from them.

mod app;
mod runner;
mod sections;
mod ui;

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

pub use runner::Runner;

/// What `ikit tui` was started with.
pub struct Options {
    /// The `ikit` executable to run (normally the current one).
    pub exe: PathBuf,
    /// Global flags to pass to every command (e.g. `--home DIR`).
    pub base_args: Vec<String>,
    /// Org and workload to start on; when unset, the CLI's selection is read.
    pub org: Option<String>,
    pub wl: Option<String>,
}

pub fn run(opts: Options) -> anyhow::Result<()> {
    let runner = Runner {
        exe: opts.exe,
        base_args: opts.base_args,
    };
    let mut app = app::App::new(runner, opts.org, opts.wl);
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut app::App) -> anyhow::Result<()> {
    while !app.quit {
        terminal.draw(|f| ui::draw(f, app))?;
        if event::poll(Duration::from_millis(80))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.on_key(key);
        }
        while let Ok(msg) = app.rx.try_recv() {
            app.on_msg(msg);
        }
        if let Some(attached) = app.attached.take() {
            run_attached(terminal, app, &attached.args)?;
            app.after_attached(attached.reselect);
        }
    }
    Ok(())
}

/// Suspends the TUI, runs the command on the real terminal, and waits for Enter.
fn run_attached(
    terminal: &mut DefaultTerminal,
    app: &mut app::App,
    args: &[String],
) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    println!("\n$ {}\n", app.runner.display(args));
    let summary = match app.runner.run_attached(args) {
        Ok(s) if s.success() => "done".to_string(),
        Ok(s) => match s.code() {
            Some(c) => format!("exited with status {c}"),
            None => "stopped by a signal".to_string(),
        },
        Err(e) => format!("could not start ikit: {e}"),
    };
    print!("\n[{summary}] Press Enter to return to ikit tui… ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    if summary != "done" {
        app.set_error(format!("{}: {summary}", app.runner.display(args)));
    }
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    // `clear()` would ask the terminal for the cursor position; resizing to the same size
    // clears and forces a full redraw without that round trip.
    let size = terminal.size()?;
    terminal.resize(ratatui::layout::Rect::new(0, 0, size.width, size.height))?;
    Ok(())
}
