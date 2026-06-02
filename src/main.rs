use std::io::{self, BufWriter, Stdout, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{DisableMouseCapture, Event, KeyEventKind};
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

    let mouse = cfg.reader.mouse;
    let mut terminal = match tty::enter(mouse) {
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
    let restore = tty::leave(mouse);

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
        // picked up even though `scan::start_inbox_worker` and
        // `start_catchup_worker` don't push to the event channel yet).
        // While a yank-highlight is active, tighten the deadline so the
        // expiry sweep in `tick` fires within the configured window
        // (default 150 ms) instead of waiting for the 250 ms idle tick.
        let timeout = yank_highlight_deadline(&app).unwrap_or(IDLE_TICK);
        match event_rx.recv_timeout(timeout) {
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

/// Clear the inbox's yank-highlight once its deadline has passed. Called
/// from `tick` before the draw so a frame after the deadline paints
/// without the REVERSED overlay.
fn expire_yank_highlight(app: &mut App) {
    let inbox = app.inbox_mut();
    if inbox
        .yank_highlight
        .as_ref()
        .is_some_and(|h| h.expires_at <= Instant::now())
    {
        inbox.yank_highlight = None;
    }
}

/// Time until the active yank-highlight expires, or `None` when no
/// highlight is active. Clamped to `IDLE_TICK` so a long-lived highlight
/// can't push the recv deadline past the heartbeat that picks up scan
/// results.
fn yank_highlight_deadline(app: &App) -> Option<Duration> {
    let hl = app.inbox().yank_highlight.as_ref()?;
    let remaining = hl
        .expires_at
        .saturating_duration_since(Instant::now())
        .min(IDLE_TICK);
    // Floor at 1 ms — `recv_timeout(Duration::ZERO)` returns
    // Disconnected on closed channels but Timeout instantly on open
    // ones; preserve the heartbeat semantics by always sleeping a beat.
    Some(remaining.max(Duration::from_millis(1)))
}

fn process_event(app: &mut App, cfg: &Config, ev: AppEvent) {
    match ev {
        AppEvent::Input(Event::Key(k)) if k.kind == KeyEventKind::Press => {
            ui::keys::handle(app, cfg, k);
        }
        AppEvent::Input(Event::Mouse(m)) => {
            ui::mouse::handle(app, cfg, m);
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
    app.poll_scan(cfg);
    app.poll_watch(cfg);
    app.poll_pending_sends();
    app.poll_sync();
    app.poll_clipboard();
    expire_yank_highlight(app);
    app.ensure_body_for_selection();

    finalize_finished_editors(app);
    let term_size = terminal
        .size()
        .unwrap_or(ratatui::layout::Size::new(80, 24));
    spawn_pending_editors(app, &cfg.compose, term_size, event_tx);

    // Drain any DECSCUSR changes the embedded editor asked for, plus
    // a one-shot reset queued by `finalize_finished_editors`. We emit
    // these inside the BSU/ESU braces below so the cursor-shape switch
    // lands in the same atomic frame as the cell repaint — without that,
    // some terminals flash the old shape between the cell draw and the
    // shape change.
    let cursor_style_bytes = collect_cursor_style_escapes(app);

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
        if !cursor_style_bytes.is_empty() {
            let _ = backend.write_all(&cursor_style_bytes);
        }
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

/// Collect any cursor-shape changes the embedded editor(s) want to push
/// to the host terminal this frame. Drains the per-session dirty flag so
/// we don't re-emit the same shape every redraw, and folds in the
/// one-shot reset queued by `finalize_finished_editors` when an editor
/// exits. The bytes are written into the BSU/ESU envelope by the caller
/// so the shape switch is part of the same atomic frame as the repaint.
fn collect_cursor_style_escapes(app: &mut App) -> Vec<u8> {
    let mut bytes = Vec::new();
    for screen in app.screens.iter_mut() {
        if let ui::app::Screen::Compose(c) = screen
            && let Some(ed) = c.editor.as_ref()
            && let Some(shape) = ed.take_cursor_style_change()
        {
            // `\x1b[N q` — DECSCUSR with the param the editor sent. 0
            // restores the terminal default; 1..=6 are the standard
            // blink/steady block/underline/bar combinations.
            bytes.extend_from_slice(format!("\x1b[{shape} q").as_bytes());
        }
    }
    // Native compose body editor: emit DECSCUSR based on the focused
    // body's mode (steady block in Normal/Visual, steady bar in Insert).
    // Only the active tab counts — tabbing away from a compose tab
    // resets the shape to terminal default. Tracked on App so we emit
    // only on transitions instead of every frame.
    let desired_native = native_body_cursor_shape(app);
    if desired_native != app.native_cursor_shape_emitted {
        match desired_native {
            Some(shape) => {
                bytes.extend_from_slice(format!("\x1b[{shape} q").as_bytes());
            }
            None => {
                bytes.extend_from_slice(b"\x1b[0 q");
            }
        }
        app.native_cursor_shape_emitted = desired_native;
    }
    if app.cursor_style_reset_pending {
        app.cursor_style_reset_pending = false;
        // The pending-reset path also clears our tracked shape so the
        // next focus into a compose body re-emits its shape from scratch.
        app.native_cursor_shape_emitted = None;
        bytes.extend_from_slice(b"\x1b[0 q");
    }
    bytes
}

/// Desired DECSCUSR param for the native compose body editor on the
/// active tab. `None` when no native body is focused (either we're on
/// the inbox, on a compose tab with a different field focused, or the
/// compose tab is running `$EDITOR` and thus drives the shape itself).
fn native_body_cursor_shape(app: &App) -> Option<u16> {
    use ui::compose::ComposeField;
    use ui::compose_body::BodyMode;
    let screen = app.screens.get(app.active)?;
    let ui::app::Screen::Compose(c) = screen else {
        return None;
    };
    if c.editor.is_some() || c.focused != ComposeField::Body {
        return None;
    }
    Some(match c.body.mode {
        BodyMode::Insert => 6,                       // steady bar
        BodyMode::Normal | BodyMode::Visual(_) => 2, // steady block
    })
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
    if to_finalize.is_empty() {
        return;
    }
    for i in to_finalize {
        if let Some(ui::app::Screen::Compose(c)) = app.screens.get_mut(i) {
            c.editor = None;
            // Pull the user's edits from the tempfile back into the
            // native body editor, then drop the tempfile. The native
            // editor is the canonical store from here on; the tempfile
            // only existed for the `$EDITOR` round-trip.
            c.reload_body_from_tempfile();
        }
    }
    // The editor most likely left the host cursor style in whatever
    // nvim's `guicursor` was using (bar for insert, etc.). Reset to the
    // terminal's user default so the rest of the app — and any process
    // launched after epost exits without going through LeaveAlternateScreen
    // first — doesn't inherit a beam.
    app.cursor_style_reset_pending = true;
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
        // Flush the native editor to a tempfile and hand the path to
        // `$EDITOR`. The tempfile lives on the screen for the duration
        // of the session; on exit, `finalize_finished_editors` reads it
        // back into the native editor and drops it.
        let path = match c.materialize_body_tempfile() {
            Ok(p) => p,
            Err(e) => {
                to_error = Some(format!("editor: {e:#}"));
                continue;
            }
        };
        match EditorSession::start(&path, &argv, rows, cols, event_tx.clone()) {
            Ok(session) => {
                c.editor = Some(session);
            }
            Err(e) => {
                // Drop the tempfile so a retry rebuilds it from the
                // current editor contents.
                c.body_tempfile = None;
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
        // Emit DisableMouseCapture unconditionally: terminals that
        // never saw EnableMouseCapture ignore the corresponding DEC
        // private-mode resets, so this is safe regardless of the
        // session's config.
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
        original(info);
    }));
}
