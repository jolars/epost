//! Raw-mode enter/leave plumbing. Owned here so process startup +
//! shutdown go through a single pair of helpers; the embedded
//! `$EDITOR` flow (`ui::embed`) attaches the editor to a pty and
//! doesn't suspend the host TUI.

use std::io::{self, Stdout};

use anyhow::{Context, Result};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub fn enter() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enabling raw mode")?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen).context("entering alternate screen")?;
    Terminal::new(CrosstermBackend::new(out)).context("constructing terminal")
}

pub fn leave() -> Result<()> {
    let mut out = io::stdout();
    execute!(out, LeaveAlternateScreen).context("leaving alternate screen")?;
    disable_raw_mode().context("disabling raw mode")?;
    Ok(())
}
