use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::Config;
use crate::ui::app::{App, Mode, Pane, Screen};
use crate::ui::{cmdline, compose};

pub fn handle(app: &mut App, cfg: &Config, k: KeyEvent) {
    // Tab-switch chords stay global so the user can navigate away
    // even when an editor session is intercepting everything else.
    if global(app, k) {
        return;
    }

    // If the active tab hosts a live `$EDITOR` session, every other
    // key forwards to the pty — including `:`, `q`, and Ctrl-C, which
    // vim/nvim/emacs all need for their own commands (`:wq` to save,
    // `q` to record/quit a buffer, Ctrl-C to cancel a partial input).
    // The user exits the editor through the editor's own quit, which
    // ends the session and returns the form.
    if let Some(Screen::Compose(c)) = app.screens.get_mut(app.active)
        && let Some(ed) = c.editor.as_mut()
    {
        ed.forward_key(k);
        return;
    }

    // Ctrl-C quits the app — only reachable when no editor is live,
    // so a long-running edit isn't an accidental ^C away from losing
    // the draft tempfile.
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        app.quit = true;
        return;
    }

    match app.mode {
        Mode::Normal => normal(app, cfg, k),
        Mode::Command => command(app, cfg, k),
        Mode::LinkPick => link_pick(app, cfg, k),
    }
}

/// Tab-switch chords: dispatched before the per-mode keymap so they
/// work in any mode. Returns true when the key was consumed.
fn global(app: &mut App, k: KeyEvent) -> bool {
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        match k.code {
            KeyCode::PageDown => {
                app.next_tab();
                return true;
            }
            KeyCode::PageUp => {
                app.prev_tab();
                return true;
            }
            _ => {}
        }
    }
    if k.modifiers.contains(KeyModifiers::ALT)
        && let KeyCode::Char(c) = k.code
        && let Some(d) = c.to_digit(10)
        && (1..=9).contains(&d)
    {
        app.set_tab(d as usize - 1);
        return true;
    }
    false
}

fn normal(app: &mut App, cfg: &Config, k: KeyEvent) {
    // `q` and `:` are global in Normal mode, regardless of active screen.
    if k.code == KeyCode::Char('q') && !k.modifiers.contains(KeyModifiers::CONTROL) {
        // In a compose tab, `q` should type a literal `q` into the
        // focused field — never quit the app. Inbox keeps the old
        // semantics.
        if !matches!(app.screens.get(app.active), Some(Screen::Compose(_))) {
            app.quit = true;
            return;
        }
    }
    if k.code == KeyCode::Char(':') {
        enter_command(app);
        return;
    }

    // Route to per-screen handlers.
    if let Some(Screen::Compose(c)) = app.screens.get_mut(app.active) {
        compose::handle_key(c, k);
        return;
    }

    inbox_normal(app, cfg, k);
}

fn inbox_normal(app: &mut App, cfg: &Config, k: KeyEvent) {
    // Ctrl-h/j/k/l: vim-window-style spatial pane navigation. Match
    // before the plain-letter arms so e.g. Ctrl-j doesn't also fire
    // `select_next`. Any other Ctrl-letter is swallowed here so it can't
    // accidentally trigger a Normal binding either.
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        match k.code {
            KeyCode::Char('h') => app.focus_left(),
            KeyCode::Char('j') => app.focus_down(),
            KeyCode::Char('k') => app.focus_up(),
            KeyCode::Char('l') => app.focus_right(),
            _ => {}
        }
        return;
    }

    // Alt-j/k: cycle the active folder from any focus. Match before
    // the focus-routed arms so plain j/k still navigate the list /
    // scroll the reader while the Alt variant always switches folders.
    if k.modifiers.contains(KeyModifiers::ALT) {
        match k.code {
            KeyCode::Char('j') => {
                app.cycle_folder(true);
                return;
            }
            KeyCode::Char('k') => {
                app.cycle_folder(false);
                return;
            }
            _ => {}
        }
    }

    // Universal keys (work in any focus).
    match k.code {
        KeyCode::Tab => {
            app.cycle_focus(true);
            return;
        }
        KeyCode::BackTab => {
            app.cycle_focus(false);
            return;
        }
        _ => {}
    }

    // Focus-routed keys: `j`/`k` are the obvious case — list nav when List
    // has focus, reader scroll when Reader does. `f` only makes sense in
    // the reader (link picker over the rendered body). The flag toggles
    // (`m`/`*`/`d`) only apply to a selected row, so they're List-only.
    let focus = app.inbox().focus;
    match focus {
        Pane::List => match k.code {
            KeyCode::Char('j') => app.select_next(),
            KeyCode::Char('k') => app.select_prev(),
            KeyCode::Char('m') => app.toggle_flag_selected('S'),
            KeyCode::Char('*') => app.toggle_flag_selected('F'),
            KeyCode::Char('d') => app.toggle_flag_selected('T'),
            KeyCode::Char('a') => cmdline::archive_selected(app, cfg),
            KeyCode::Char('D') => cmdline::trash_selected(app, cfg),
            KeyCode::Char('c') => cmdline::open_blank_compose_external(app, cfg),
            KeyCode::Char('r') => cmdline::open_reply(app, cfg, cmdline::ReplyKind::Reply),
            KeyCode::Char('R') => cmdline::open_reply(app, cfg, cmdline::ReplyKind::ReplyAll),
            KeyCode::Char('F') => cmdline::open_reply(app, cfg, cmdline::ReplyKind::Forward),
            KeyCode::Char('l') | KeyCode::Enter if app.inbox().reader_visible => {
                app.inbox_mut().focus = Pane::Reader;
            }
            _ => {}
        },
        Pane::Reader => match k.code {
            KeyCode::Char('j') => {
                let inbox = app.inbox_mut();
                inbox.reader_scroll = inbox.reader_scroll.saturating_add(1);
            }
            KeyCode::Char('k') => {
                let inbox = app.inbox_mut();
                inbox.reader_scroll = inbox.reader_scroll.saturating_sub(1);
            }
            KeyCode::Char('f') => {
                app.link_pick_buf.clear();
                app.mode = Mode::LinkPick;
            }
            KeyCode::Char('r') => cmdline::open_reply(app, cfg, cmdline::ReplyKind::Reply),
            KeyCode::Char('R') => cmdline::open_reply(app, cfg, cmdline::ReplyKind::ReplyAll),
            KeyCode::Char('F') => cmdline::open_reply(app, cfg, cmdline::ReplyKind::Forward),
            KeyCode::Esc => {
                let inbox = app.inbox_mut();
                inbox.focus = if inbox.list_visible {
                    Pane::List
                } else if inbox.sidebar_visible {
                    Pane::Folders
                } else {
                    Pane::Reader
                };
            }
            _ => {}
        },
        Pane::Folders => match k.code {
            KeyCode::Char('j') => app.cycle_folder(true),
            KeyCode::Char('k') => app.cycle_folder(false),
            _ => {}
        },
    }
}

fn command(app: &mut App, cfg: &Config, k: KeyEvent) {
    match k.code {
        KeyCode::Esc => exit_command(app),
        KeyCode::Backspace if app.cmdline.is_empty() => exit_command(app),
        KeyCode::Enter => {
            let buf = app.cmdline.take();
            app.mode = Mode::Normal;
            cmdline::dispatch(buf.trim(), app, cfg);
        }
        _ => {
            app.cmdline.handle(k);
        }
    }
}

fn link_pick(app: &mut App, cfg: &Config, k: KeyEvent) {
    match k.code {
        KeyCode::Esc => {
            app.link_pick_buf.clear();
            app.mode = Mode::Normal;
        }
        KeyCode::Backspace => {
            app.link_pick_buf.pop();
        }
        KeyCode::Char(c) if c.is_ascii_digit() => {
            app.link_pick_buf.push(c);
        }
        KeyCode::Enter => {
            let buf = std::mem::take(&mut app.link_pick_buf);
            app.mode = Mode::Normal;
            follow_link(app, cfg, &buf);
        }
        _ => {}
    }
}

fn enter_command(app: &mut App) {
    app.cmdline.clear();
    app.status_error = None;
    app.mode = Mode::Command;
}

fn exit_command(app: &mut App) {
    app.cmdline.clear();
    app.mode = Mode::Normal;
}

fn follow_link(app: &mut App, cfg: &Config, buf: &str) {
    let Ok(id) = buf.parse::<u32>() else {
        if !buf.is_empty() {
            app.status_error = Some(format!("link: not a number: {buf:?}"));
        }
        return;
    };
    let Some(parsed) = app.inbox_parsed() else {
        app.status_error = Some("link: no parsed body".into());
        return;
    };
    // The picker re-runs layout each frame for the current pane width; since
    // the keymap doesn't know that width, rebuild the link table at a sensible
    // default to find the href. Width influences wrapping but not link
    // identity / count, so any reasonable width works.
    let laid = crate::ui::reader::layout(&parsed.blocks, 80, None);
    let Some(slot) = laid.links.iter().find(|s| s.id == id) else {
        app.status_error = Some(format!("link: no such id: {id}"));
        return;
    };
    if let Err(e) = crate::ui::browser::open_url(&slot.href, &cfg.reader.browser) {
        app.status_error = Some(format!("open: {e:#}"));
    }
}
