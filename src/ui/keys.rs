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
        Mode::Reader => reader(app, cfg, k),
        Mode::Command => command(app, cfg, k),
        Mode::LinkPick => link_pick(app, cfg, k),
    }
}

fn normal(app: &mut App, k: KeyEvent) {
    match k.code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Tab => app.cycle_focus(true),
        KeyCode::BackTab => app.cycle_focus(false),
        KeyCode::Char('j') if app.focus == Pane::List => app.select_next(),
        KeyCode::Char('k') if app.focus == Pane::List => app.select_prev(),
        KeyCode::Char('l') | KeyCode::Enter if app.focus == Pane::Reader && app.reader_visible => {
            app.mode = Mode::Reader;
        }
        KeyCode::Char(':') => enter_command(app),
        _ => {}
    }
}

fn reader(app: &mut App, _cfg: &Config, k: KeyEvent) {
    match k.code {
        KeyCode::Char('j') => app.reader_scroll = app.reader_scroll.saturating_add(1),
        KeyCode::Char('k') => app.reader_scroll = app.reader_scroll.saturating_sub(1),
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.focus = if app.list_visible {
                Pane::List
            } else if app.sidebar_visible {
                Pane::Folders
            } else {
                Pane::Reader
            };
        }
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('f') => {
            app.link_pick_buf.clear();
            app.mode = Mode::LinkPick;
        }
        KeyCode::Char(':') => enter_command(app),
        _ => {}
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
            app.mode = Mode::Reader;
        }
        KeyCode::Backspace => {
            app.link_pick_buf.pop();
        }
        KeyCode::Char(c) if c.is_ascii_digit() => {
            app.link_pick_buf.push(c);
        }
        KeyCode::Enter => {
            let buf = std::mem::take(&mut app.link_pick_buf);
            app.mode = Mode::Reader;
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
