//! Raw-mode enter/leave plumbing. Owned here so process startup +
//! shutdown go through a single pair of helpers; the embedded
//! `$EDITOR` flow (`ui::embed`) attaches the editor to a pty and
//! doesn't suspend the host TUI.

use std::io::{self, BufWriter, Stdout};

use anyhow::{Context, Result};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

/// Wrap stdout in a generously sized BufWriter before handing it to
/// ratatui. Without this, every cell update / cursor move ratatui emits
/// becomes its own write() syscall — on a frame that replaces most of
/// the screen (e.g. closing the embedded `$EDITOR` and repainting the
/// compose form) that's thousands of syscalls per draw and shows up as
/// a visible "regaining control" delay. Ratatui calls `flush()` once
/// per `Terminal::draw`, so the buffer is bounded to one frame.
///
/// `mouse` toggles `EnableMouseCapture` — driven by `[reader].mouse` so
/// users on terminals where they want native drag-select can opt out.
pub fn enter(mouse: bool) -> Result<Terminal<CrosstermBackend<BufWriter<Stdout>>>> {
    enable_raw_mode().context("enabling raw mode")?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen).context("entering alternate screen")?;
    if mouse {
        execute!(out, EnableMouseCapture).context("enabling mouse capture")?;
    }
    let buffered = BufWriter::with_capacity(1 << 16, out);
    Terminal::new(CrosstermBackend::new(buffered)).context("constructing terminal")
}

pub fn leave(mouse: bool) -> Result<()> {
    let mut out = io::stdout();
    if mouse {
        execute!(out, DisableMouseCapture).context("disabling mouse capture")?;
    }
    execute!(out, LeaveAlternateScreen).context("leaving alternate screen")?;
    disable_raw_mode().context("disabling raw mode")?;
    Ok(())
}
