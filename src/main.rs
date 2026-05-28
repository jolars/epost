use std::io::{self, Stdout};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

mod config;
mod mail;
mod store;
mod ui;

use crate::config::Config;
use crate::ui::app::App;

const POLL_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Parser)]
#[command(name = "epost", about = "Linux maildir email reader/composer")]
struct Args {
    /// Path to the TOML config file (default: $XDG_CONFIG_HOME/epost/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Path to the SQLite index cache file (default:
    /// $XDG_CACHE_HOME/epost/index.sqlite). The index is rebuildable from the
    /// configured maildirs, so deleting this file is safe.
    #[arg(long)]
    cache: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let path = args.config.unwrap_or_else(config::default_path);

    let cfg = match config::load(&path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("epost: failed to load config: {e:#}");
            return ExitCode::from(2);
        }
    };

    let cache_path = args.cache.unwrap_or_else(config::default_cache_path);

    install_panic_hook();

    let mut terminal = match enter_tui() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("epost: failed to enter raw mode: {e:#}");
            return ExitCode::from(2);
        }
    };

    // Picker probes stdio for kitty/iTerm2/sixel capabilities; per the
    // crate docs this must happen after EnterAlternateScreen but before
    // the event loop starts reading keypresses.
    let (picker, picker_warning) = ui::images::build_picker(&cfg.images);

    let result = run(&mut terminal, &cfg, cache_path, picker, picker_warning);
    let restore = leave_tui();

    if let Err(e) = result {
        eprintln!("epost: {e:#}");
        return ExitCode::from(1);
    }
    if let Err(e) = restore {
        eprintln!("epost: failed to restore terminal: {e:#}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    cfg: &Config,
    cache_path: PathBuf,
    picker: Option<ratatui_image::picker::Picker>,
    picker_warning: Option<String>,
) -> Result<()> {
    let mut app = App::new(cfg, cache_path, picker);
    if let Some(w) = picker_warning {
        app.status_error = Some(w);
    }
    while !app.quit {
        app.poll_scan();
        app.ensure_body_for_selection();

        terminal
            .draw(|f| ui::app::draw(f, &mut app))
            .context("drawing frame")?;

        if event::poll(POLL_TIMEOUT).context("polling input")?
            && let Event::Key(k) = event::read().context("reading input")?
            && k.kind == KeyEventKind::Press
        {
            ui::keys::handle(&mut app, cfg, k);
        }
    }
    Ok(())
}

fn enter_tui() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enabling raw mode")?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen).context("entering alternate screen")?;
    Terminal::new(CrosstermBackend::new(out)).context("constructing terminal")
}

fn leave_tui() -> Result<()> {
    let mut out = io::stdout();
    execute!(out, LeaveAlternateScreen).context("leaving alternate screen")?;
    disable_raw_mode().context("disabling raw mode")?;
    Ok(())
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
        original(info);
    }));
}
