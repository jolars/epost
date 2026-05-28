use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::Config;
use crate::ui::app::{App, Mode, Pane};
use crate::ui::cmdline;

pub fn handle(app: &mut App, cfg: &Config, k: KeyEvent) {
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        app.quit = true;
        return;
    }

    match app.mode {
        Mode::Normal => normal(app, k),
        Mode::Command => command(app, cfg, k),
        Mode::LinkPick => link_pick(app, cfg, k),
    }
}

fn normal(app: &mut App, k: KeyEvent) {
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

    // Universal keys (work in any focus).
    match k.code {
        KeyCode::Char('q') => {
            app.quit = true;
            return;
        }
        KeyCode::Tab => {
            app.cycle_focus(true);
            return;
        }
        KeyCode::BackTab => {
            app.cycle_focus(false);
            return;
        }
        KeyCode::Char(':') => {
            enter_command(app);
            return;
        }
        _ => {}
    }

    // Focus-routed keys: `j`/`k` are the obvious case — list nav when List
    // has focus, reader scroll when Reader does. `f` only makes sense in
    // the reader (link picker over the rendered body). The flag toggles
    // (`m`/`*`/`d`) only apply to a selected row, so they're List-only.
    match app.focus {
        Pane::List => match k.code {
            KeyCode::Char('j') => app.select_next(),
            KeyCode::Char('k') => app.select_prev(),
            KeyCode::Char('m') => app.toggle_flag_selected('S'),
            KeyCode::Char('*') => app.toggle_flag_selected('F'),
            KeyCode::Char('d') => app.toggle_flag_selected('T'),
            KeyCode::Char('l') | KeyCode::Enter if app.reader_visible => {
                app.focus = Pane::Reader;
            }
            _ => {}
        },
        Pane::Reader => match k.code {
            KeyCode::Char('j') => app.reader_scroll = app.reader_scroll.saturating_add(1),
            KeyCode::Char('k') => app.reader_scroll = app.reader_scroll.saturating_sub(1),
            KeyCode::Char('f') => {
                app.link_pick_buf.clear();
                app.mode = Mode::LinkPick;
            }
            KeyCode::Esc => {
                app.focus = if app.list_visible {
                    Pane::List
                } else if app.sidebar_visible {
                    Pane::Folders
                } else {
                    Pane::Reader
                };
            }
            _ => {}
        },
        Pane::Folders => {}
    }
}

fn command(app: &mut App, cfg: &Config, k: KeyEvent) {
    match k.code {
        KeyCode::Esc => exit_command(app),
        KeyCode::Backspace if app.cmdline_buf.pop().is_none() => exit_command(app),
        KeyCode::Backspace => {}
        KeyCode::Enter => {
            let buf = std::mem::take(&mut app.cmdline_buf);
            app.mode = Mode::Normal;
            cmdline::dispatch(buf.trim(), app, cfg);
        }
        KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.cmdline_buf.push(c);
        }
        _ => {}
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
    app.cmdline_buf.clear();
    app.status_error = None;
    app.mode = Mode::Command;
}

fn exit_command(app: &mut App) {
    app.cmdline_buf.clear();
    app.mode = Mode::Normal;
}

fn follow_link(app: &mut App, cfg: &Config, buf: &str) {
    let Ok(id) = buf.parse::<u32>() else {
        if !buf.is_empty() {
            app.status_error = Some(format!("link: not a number: {buf:?}"));
        }
        return;
    };
    let Some(parsed) = app.parsed.as_ref() else {
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
