use std::io::{self, BufWriter, Stdout, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{Event, KeyEventKind};
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
use crate::ui::events::{self, AppEvent};
use crate::ui::tty;

/// Soft tick used as a heartbeat when the scan worker is still
/// running — once a worker exists we'll have completion-driven wakes
/// too, but the initial scan from Step 2 doesn't push to the event
/// channel yet, so we keep a slow timer to pick it up.
const IDLE_TICK: Duration = Duration::from_millis(250);

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
    terminal: &mut Terminal<CrosstermBackend<BufWriter<Stdout>>>,
    cfg: &Config,
    cache_path: PathBuf,
    picker: Option<ratatui_image::picker::Picker>,
    picker_warning: Option<String>,
) -> Result<()> {
    // Single fan-in channel: crossterm input goes through one reader
    // thread, and subsystems (editor pty, scan/send workers, the
    // maildir notify watcher) post `Wake` events when they have work
    // to surface. The main loop blocks on `recv` instead of polling,
    // so reaction time is the cost of one channel hand-off — not a
    // poll interval.
    let (event_tx, event_rx) = events::channel().context("starting input reader")?;

    let mut app = App::new(cfg, cache_path, picker, Some(event_tx.clone()));
    if let Some(w) = picker_warning {
        app.status_error = Some(w);
    }

    // Draw once before blocking so the initial UI appears even before
    // any event arrives.
    tick(terminal, &mut app, cfg, &event_tx)?;

    while !app.quit {
        // Block until something wakes us — input, editor exit, pty
        // bytes, or the idle heartbeat (so initial scan results get
        // picked up even though `scan::start_worker` doesn't push
        // to the event channel yet).
        match event_rx.recv_timeout(IDLE_TICK) {
            Ok(ev) => process_event(&mut app, cfg, ev),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Drain any events that piled up while we were sleeping so we
        // don't spend a draw per byte during fast editor output.
        while let Ok(ev) = event_rx.try_recv() {
            process_event(&mut app, cfg, ev);
        }

        tick(terminal, &mut app, cfg, &event_tx)?;
    }
    Ok(())
}

fn process_event(app: &mut App, cfg: &Config, ev: AppEvent) {
    match ev {
        AppEvent::Input(Event::Key(k)) if k.kind == KeyEventKind::Press => {
            ui::keys::handle(app, cfg, k);
        }
        AppEvent::Input(_) => {}
        AppEvent::Wake => {}
    }
}

fn tick(
    terminal: &mut Terminal<CrosstermBackend<BufWriter<Stdout>>>,
    app: &mut App,
    cfg: &Config,
    event_tx: &Sender<AppEvent>,
) -> Result<()> {
    app.poll_scan();
    app.poll_watch(cfg);
    app.poll_compose_sends();
    app.poll_sync();
    app.poll_clipboard();
    app.ensure_body_for_selection();

    finalize_finished_editors(app);
    let term_size = terminal
        .size()
        .unwrap_or(ratatui::layout::Size::new(80, 24));
    spawn_pending_editors(app, &cfg.compose, term_size, event_tx);

    // DEC Synchronized Output (mode 2026): tell the host terminal to
    // buffer everything between BSU and ESU and apply it as one atomic
    // frame instead of painting cell-by-cell. Terminals that don't
    // support it silently ignore the DEC private modes, so this is safe
    // to emit unconditionally. Major win on dense redraws (closing the
    // embedded editor, scrolling a truecolor-highlighted buffer, etc.)
    // because the host terminal does one composite update per frame
    // instead of many partial paints. Modelled after vaxis's writer.go.
    {
        let backend = terminal.backend_mut();
        let _ = backend.write_all(b"\x1b[?2026h");
    }
    // Drop the CompletedFrame here so terminal isn't still borrowed when
    // we reach for backend_mut() again to emit the ESU.
    let draw_res = terminal.draw(|f| ui::app::draw(f, app)).map(|_| ());
    {
        let backend = terminal.backend_mut();
        let _ = backend.write_all(b"\x1b[?2026l");
        let _ = backend.flush();
    }
    draw_res.context("drawing frame")?;
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

fn spawn_pending_editors(
    app: &mut App,
    cfg: &config::Compose,
    term_size: ratatui::layout::Size,
    event_tx: &Sender<AppEvent>,
) {
    let argv = config::resolve_editor(cfg);
    let mut to_error: Option<String> = None;
    for screen in app.screens.iter_mut() {
        let ui::app::Screen::Compose(c) = screen else {
            continue;
        };
        if !c.editor_pending || c.editor.is_some() {
            continue;
        }
        c.editor_pending = false;
        let (rows, cols) = c
            .last_body_inner
            .unwrap_or_else(|| compose_body_inner_size(term_size));
        match EditorSession::start(&c.body_path(), &argv, rows, cols, event_tx.clone()) {
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

/// Compute the size `ui::compose::draw` will give the body's inner
/// rect for a given terminal size. Mirrors the layout constants:
/// outer = 1 (tabs) + body + 1 (cmdline); compose = 7 (header block)
/// + body + 1 (hint); body block subtracts 2 for its border.
fn compose_body_inner_size(term_size: ratatui::layout::Size) -> (u16, u16) {
    let rows = term_size.height.saturating_sub(1 + 1 + 7 + 1 + 2).max(1);
    let cols = term_size.width.saturating_sub(2).max(1);
    (rows, cols)
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
        original(info);
    }));
}
