use std::io::{self, Stdout};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

mod config;
mod mail;
mod store;
mod ui;

use crate::config::Config;
use crate::ui::app::App;
use crate::ui::embed::EditorSession;
use crate::ui::tty;

const POLL_TIMEOUT: Duration = Duration::from_millis(250);
/// Shorter poll while a compose tab is hosting an embedded `$EDITOR`
/// session — drives the redraw cadence so typed characters appear
/// promptly in the parsed terminal grid.
const EDITOR_POLL_TIMEOUT: Duration = Duration::from_millis(30);

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

    let mut terminal = match tty::enter() {
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
    let restore = tty::leave();

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
        app.poll_compose_sends();
        app.ensure_body_for_selection();

        // Editor lifecycle handled around the draw so each frame
        // shows the freshest state: exits surface the saved body the
        // same frame; new sessions spawn against the previously-
        // recorded body rect so no transitional empty-pty frame ever
        // hits the screen.
        finalize_finished_editors(&mut app);
        spawn_pending_editors(&mut app, &cfg.compose);

        terminal
            .draw(|f| ui::app::draw(f, &mut app))
            .context("drawing frame")?;

        let timeout = if app.has_active_editor() {
            EDITOR_POLL_TIMEOUT
        } else {
            POLL_TIMEOUT
        };
        if event::poll(timeout).context("polling input")?
            && let Event::Key(k) = event::read().context("reading input")?
            && k.kind == KeyEventKind::Press
        {
            ui::keys::handle(&mut app, cfg, k);
        }
    }
    Ok(())
}

fn finalize_finished_editors(app: &mut App) {
    let mut to_finalize: Vec<usize> = Vec::new();
    for (i, screen) in app.screens.iter_mut().enumerate() {
        if let ui::app::Screen::Compose(c) = screen
            && let Some(ed) = c.editor.as_mut()
            && ed.is_done()
        {
            to_finalize.push(i);
        }
    }
    for i in to_finalize {
        if let Some(ui::app::Screen::Compose(c)) = app.screens.get_mut(i) {
            c.editor = None;
            c.reload_body_preview();
        }
    }
}

fn spawn_pending_editors(app: &mut App, cfg: &config::Compose) {
    let argv = config::resolve_editor(cfg);
    // Iterate by index so we can borrow status_error mutably alongside.
    let mut to_error: Option<String> = None;
    for screen in app.screens.iter_mut() {
        let ui::app::Screen::Compose(c) = screen else {
            continue;
        };
        if !c.editor_pending || c.editor.is_some() {
            continue;
        }
        c.editor_pending = false;
        let (rows, cols) = c.last_body_inner.unwrap_or((24, 80));
        match EditorSession::start(&c.body_path(), &argv, rows, cols) {
            Ok(session) => {
                c.editor = Some(session);
            }
            Err(e) => {
                to_error = Some(format!("editor: {e:#}"));
            }
        }
    }
    if let Some(msg) = to_error {
        app.status_error = Some(msg);
    }
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
        original(info);
    }));
}
