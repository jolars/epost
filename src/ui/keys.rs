use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::Config;
use crate::mail::text;
use crate::ui::app::{App, Mode, Pane, Screen};
use crate::ui::clipboard::{self, YankOutcome};
use crate::ui::motion;
use crate::ui::reader::{LaidOutBody, YankHighlight};
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
        Mode::Search => search(app, cfg, k),
        Mode::Visual => visual(app, cfg, k),
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
    if k.modifiers.contains(KeyModifiers::ALT) {
        match k.code {
            // Alt-h / Alt-l cycle tabs (wraparound) — same behaviour as
            // Ctrl-PageUp/PageDown but easier to hit on a laptop keyboard.
            // Handled globally so they work from any mode, including from
            // inside the compose body editor's Insert mode.
            KeyCode::Char('h') => {
                app.prev_tab();
                return true;
            }
            KeyCode::Char('l') => {
                app.next_tab();
                return true;
            }
            KeyCode::Char(c)
                if let Some(d) = c.to_digit(10)
                    && (1..=9).contains(&d) =>
            {
                app.set_tab(d as usize - 1);
                return true;
            }
            _ => {}
        }
    }
    false
}

fn normal(app: &mut App, cfg: &Config, k: KeyEvent) {
    // `/` and `g/` enter Search mode (local / global). Handled before
    // the `q` / `:` early-returns so the pending-`g` state can settle
    // on the next key. Only applies to the inbox screen.
    if matches!(app.screens.get(app.active), Some(Screen::Inbox(_))) {
        if app.pending_g {
            app.pending_g = false;
            if k.code == KeyCode::Char('/') {
                app.enter_search_global(cfg);
                return;
            }
            if k.code == KeyCode::Char('g') {
                // `gg` → scroll to top in whichever pane supports it.
                // Reader: jump body to row 0. (Lists use a different
                // convention today and aren't wired here.)
                let inbox = app.inbox_mut();
                if inbox.focus == Pane::Reader {
                    inbox.reader_scroll = 0;
                }
                return;
            }
            // Any other key falls through to the rest of Normal mode
            // (so e.g. `g` then `j` still moves selection).
        }
        if k.code == KeyCode::Char('/') && !k.modifiers.intersects(KeyModifiers::CONTROL) {
            app.enter_search_local();
            return;
        }
        if k.code == KeyCode::Char('g') && !k.modifiers.intersects(KeyModifiers::CONTROL) {
            app.pending_g = true;
            return;
        }
    }

    // `q` is global in Normal mode but never quits from a Compose tab
    // (it should type a literal `q` into whichever field has focus).
    if k.code == KeyCode::Char('q')
        && !k.modifiers.contains(KeyModifiers::CONTROL)
        && !matches!(app.screens.get(app.active), Some(Screen::Compose(_)))
    {
        app.quit = true;
        return;
    }

    // Per-screen handlers run BEFORE the `:` cmdline intercept so the
    // compose tab's native body editor (in Insert / Visual mode) can
    // swallow `:` as a literal / selection key. Normal-mode body
    // editor returns PassThrough for `:` so the cmdline still works.
    if let Some(Screen::Compose(c)) = app.screens.get_mut(app.active) {
        let outcome = compose::handle_key(c, k, cfg);
        // The inline attachment flow stages user-facing messages here so
        // the host loop can mirror them into `app.status_error` (same
        // place `:attach` writes), without plumbing `&mut App` through
        // compose-mode key dispatch.
        if let Some(msg) = c.pending_status.take() {
            app.status_error = Some(msg);
        }
        match outcome {
            compose::KeyOutcome::Consumed => return,
            compose::KeyOutcome::CloseTab => {
                // The close-confirm "Discard" arm. The prompt already
                // cleared `confirm_close`; just drop the tab. The
                // borrow check rules out an inbox tab here — `c` came
                // from `Screen::Compose(_)` above.
                let _ = app.close_active_tab();
                return;
            }
            compose::KeyOutcome::SaveAndClose => {
                // The close-confirm "Save" arm. On failure (e.g. no
                // Drafts folder configured) leave the prompt up so the
                // user can pick Discard or Cancel instead — the host
                // call below only clears the prompt + closes the tab
                // on success.
                if let Err(e) = cmdline::postpone_active(app, cfg) {
                    app.status_error = Some(format!("postpone: {e}"));
                }
                return;
            }
            compose::KeyOutcome::PassThrough => {
                // Fall through to the app-level handlers (only `:`
                // currently). Any other passthrough key is eaten by
                // the early-return below — no app-level binding
                // should fire from inside a compose tab.
            }
        }
    }

    if k.code == KeyCode::Char(':') {
        enter_command(app);
        return;
    }

    // Compose tab + body editor passed through but it's not `:`. Eat
    // the key so a stray inbox binding doesn't fire while focus is on
    // the compose form.
    if matches!(app.screens.get(app.active), Some(Screen::Compose(_))) {
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
            // Vim convention: Ctrl-r is redo. Plain `u` (undo) lives in
            // the universal section just below — both walk the same
            // stack so the user expects them to fire from any focus.
            KeyCode::Char('r') => app.redo(cfg),
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
        KeyCode::Char('n') => {
            cmdline::open_blank_compose_external(app, cfg);
            return;
        }
        // `u` walks the undo stack; the Ctrl-r redo counterpart lives in
        // the Ctrl-handler above. Both work in any focus so the user
        // doesn't have to switch panes to reverse a misfired action.
        KeyCode::Char('u') => {
            app.undo(cfg);
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
            KeyCode::Char('l') if app.inbox().reader_visible => {
                app.inbox_mut().focus = Pane::Reader;
            }
            KeyCode::Enter
                if !cmdline::resume_selected_draft_if_drafts(app, cfg)
                    && app.inbox().reader_visible =>
            {
                // Enter on a row sitting in the active account's Drafts
                // folder resumes the draft in a new composer (the
                // `resume_*` call above returns true and short-circuits
                // the guard). For any other folder, fall back to
                // focusing the reader pane.
                app.inbox_mut().focus = Pane::Reader;
            }
            KeyCode::Esc if app.inbox().search.is_some() => app.clear_search(),
            KeyCode::Esc if app.inbox().sidebar_visible => {
                app.inbox_mut().focus = Pane::Folders;
            }
            _ => {}
        },
        Pane::Reader => {
            // `y` prefix in Reader: `yp` yank-paragraph, `yl` yank-link.
            // Mirrors `pending_g` — armed on `y`, cleared on the next
            // key. Anything we don't recognise clears the prefix and
            // falls through to the rest of the Reader keymap so e.g.
            // `y` then `j` still scrolls.
            if app.pending_y {
                app.pending_y = false;
                match k.code {
                    KeyCode::Char('p') => {
                        yank_paragraph(app, cfg);
                        return;
                    }
                    KeyCode::Char('l') => {
                        yank_link(app, cfg);
                        return;
                    }
                    KeyCode::Esc => return,
                    _ => { /* fall through */ }
                }
            }
            match k.code {
                KeyCode::Char('j') => {
                    let inbox = app.inbox_mut();
                    inbox.reader_scroll = inbox.reader_scroll.saturating_add(1);
                }
                KeyCode::Char('k') => {
                    let inbox = app.inbox_mut();
                    inbox.reader_scroll = inbox.reader_scroll.saturating_sub(1);
                }
                KeyCode::Char('G') => {
                    let inbox = app.inbox_mut();
                    inbox.reader_scroll = inbox
                        .last_reader_body_lines
                        .saturating_sub(inbox.last_reader_inner_height);
                }
                KeyCode::Char('f') => {
                    app.link_pick_buf.clear();
                    app.mode = Mode::LinkPick;
                }
                KeyCode::Char('y') => {
                    app.pending_y = true;
                }
                KeyCode::Char('Y') => yank_body(app, cfg),
                KeyCode::Char('v') => {
                    app.enter_visual(crate::ui::app::VisualKind::Char);
                }
                KeyCode::Char('V') => {
                    app.enter_visual(crate::ui::app::VisualKind::Line);
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
            }
        }
        Pane::Folders => match k.code {
            KeyCode::Char('j') => app.cycle_folder(true),
            KeyCode::Char('k') => app.cycle_folder(false),
            KeyCode::Char('l') | KeyCode::Enter if app.inbox().list_visible => {
                app.inbox_mut().focus = Pane::List;
            }
            KeyCode::Esc if app.inbox().search.is_some() => app.clear_search(),
            _ => {}
        },
    }
}

/// `Mode::Visual` keymap. Cursor / anchor live on `InboxScreen`; this
/// just dispatches movement and exit. Char-wise (`v`) and line-wise
/// (`V`) share the keymap — the kind only affects what gets rendered
/// and extracted, not which keys do what.
fn visual(app: &mut App, cfg: &Config, k: KeyEvent) {
    // Bail out gracefully if the pair invariant got broken somewhere —
    // mode = Visual but no anchor on the screen. Shouldn't happen.
    if app.inbox().visual.is_none() {
        app.mode = Mode::Normal;
        return;
    }
    // Compose-specific keys first: mode exit, kind swap, yank. These
    // aren't motions, so they short-circuit `motion::apply`.
    match k.code {
        KeyCode::Esc => {
            app.exit_visual();
            return;
        }
        KeyCode::Char('v') => {
            // Same-kind exits; opposite-kind swaps.
            let cur_kind = app.inbox().visual.as_ref().map(|v| v.kind);
            if cur_kind == Some(crate::ui::app::VisualKind::Char) {
                app.exit_visual();
            } else {
                app.inbox_mut()
                    .set_visual_kind(crate::ui::app::VisualKind::Char);
            }
            return;
        }
        KeyCode::Char('V') => {
            let cur_kind = app.inbox().visual.as_ref().map(|v| v.kind);
            if cur_kind == Some(crate::ui::app::VisualKind::Line) {
                app.exit_visual();
            } else {
                app.inbox_mut()
                    .set_visual_kind(crate::ui::app::VisualKind::Line);
            }
            return;
        }
        KeyCode::Char('y') => {
            yank_visual(app, cfg);
            return;
        }
        // `gg` chord: handled here because the latch lives on `App`.
        // The motion module is stateless — first `g` arms, second `g`
        // emits FirstLine directly. `key_to_motion` returns None for
        // `g` so this branch is the only thing that consumes it.
        KeyCode::Char('g') => {
            if app.pending_g {
                app.pending_g = false;
                motion::apply(app.inbox_mut(), motion::Motion::FirstLine);
            } else {
                app.pending_g = true;
            }
            return;
        }
        _ => {}
    }
    // Any non-`g` key clears the pending-g latch so a stray `g` plus
    // an unrelated key doesn't silently chain into a later `gg`.
    app.pending_g = false;
    if let Some(m) = motion::key_to_motion(k) {
        motion::apply(app.inbox_mut(), m);
    }
}

/// Extract the current visual selection from the laid-out body and pipe
/// it through `clipboard::yank`. Exits visual mode whether or not the
/// yank succeeded — the mode's purpose is delivering the selection,
/// once it's delivered the user expects to be back in Normal.
///
/// `pub(crate)` because `ui::mouse::release` calls it: a mouse-driven
/// drag is a different way to enter the same visual state, and the
/// finalize-and-deliver path is identical.
pub(crate) fn yank_visual(app: &mut App, cfg: &Config) {
    let inbox = app.inbox();
    let Some(sel) = inbox.visual else {
        app.exit_visual();
        return;
    };
    let cursor_line = inbox.reader_cursor_line;
    let cursor_col = inbox.reader_cursor_col;
    let width = inbox.last_reader_inner_width.max(8);
    let (text, ranges) = match app.inbox_parsed() {
        Some(p) => {
            let laid = crate::ui::reader::layout(&p.blocks, width, None);
            let text = laid.extract_selection(
                sel.anchor_line,
                sel.anchor_col,
                cursor_line,
                cursor_col,
                sel.kind,
            );
            let ranges = laid.selection_cell_ranges(&sel, cursor_line, cursor_col);
            (text, ranges)
        }
        None => (String::new(), Vec::new()),
    };
    app.exit_visual();
    if text.is_empty() {
        app.status_error = Some("yank: empty selection".into());
        return;
    }
    let kind = match sel.kind {
        crate::ui::app::VisualKind::Char => "selection",
        crate::ui::app::VisualKind::Line => "lines",
    };
    set_yank_highlight(app, cfg, ranges);
    dispatch_yank(app, cfg, text, format!("yanked {kind}"));
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

/// `Mode::Search` keymap: text input edits the query (re-ranking each
/// keystroke), `Esc` cancels (restoring the prior selection), `Enter`
/// commits (returns to Normal with results pinned in the list pane).
/// Up/Down (and Ctrl-N/Ctrl-P, fzf-style) walk the result list without
/// leaving the search field — the reader pane follows automatically
/// via the next-tick `ensure_body_for_selection`.
fn search(app: &mut App, _cfg: &Config, k: KeyEvent) {
    match k.code {
        KeyCode::Esc => app.exit_search_cancel(),
        KeyCode::Enter => app.exit_search_commit(),
        KeyCode::Backspace if search_query_is_empty(app) => app.exit_search_cancel(),
        KeyCode::Down => app.select_next(),
        KeyCode::Up => app.select_prev(),
        KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => app.select_next(),
        KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => app.select_prev(),
        _ => {
            let inbox = app.inbox_mut();
            let Some(s) = inbox.search.as_mut() else {
                return;
            };
            if s.query.handle(k) {
                s.refresh();
                inbox.selected = 0;
            }
        }
    }
}

fn search_query_is_empty(app: &App) -> bool {
    app.inbox()
        .search
        .as_ref()
        .is_some_and(|s| s.query.is_empty())
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

/// Yank the entire parsed body. `Y` in Reader pane.
fn yank_body(app: &mut App, cfg: &Config) {
    let text = match app.inbox_parsed() {
        Some(p) => text::extract_body(&p.blocks),
        None => {
            app.status_error = Some("yank: no parsed body".into());
            return;
        }
    };
    if text.is_empty() {
        app.status_error = Some("yank: empty body".into());
        return;
    }
    dispatch_yank(app, cfg, text, "yanked body".to_string());
}

/// Yank the top-level block at the reader cursor. `yp` in Reader pane.
/// Cursor lives in body-relative coords (clamped into viewport each
/// draw), so this effectively yanks the topmost visible block until
/// visual mode introduces independent cursor movement.
fn yank_paragraph(app: &mut App, cfg: &Config) {
    let inbox = app.inbox();
    let width = inbox.last_reader_inner_width.max(8);
    let cursor = inbox.reader_cursor_line;
    let (text, ranges) = match app.inbox_parsed() {
        Some(p) if !p.blocks.is_empty() => {
            let laid = crate::ui::reader::layout(&p.blocks, width, None);
            match laid.block_at(cursor) {
                Some(idx) => (text::extract_block(&p.blocks[idx]), laid.block_ranges(idx)),
                None => (String::new(), Vec::new()),
            }
        }
        Some(_) => (String::new(), Vec::new()),
        None => {
            app.status_error = Some("yp: no parsed body".into());
            return;
        }
    };
    if text.is_empty() {
        app.status_error = Some("yp: no paragraph at cursor".into());
        return;
    }
    set_yank_highlight(app, cfg, ranges);
    dispatch_yank(app, cfg, text, "yanked paragraph".to_string());
}

/// Yank the URL of the first link at or after the reader cursor. `yl`
/// in Reader pane. Falls back to the first link in the body so a
/// scrolled-past-the-only-link cursor still copies it.
fn yank_link(app: &mut App, cfg: &Config) {
    let inbox = app.inbox();
    let width = inbox.last_reader_inner_width.max(8);
    let cursor = inbox.reader_cursor_line;
    let scroll_body = inbox
        .reader_scroll
        .saturating_sub(inbox.last_reader_header_offset);
    let viewport_h = inbox.last_reader_inner_height;
    let (href, visible, ranges) = match app.inbox_parsed() {
        Some(p) => {
            let laid = crate::ui::reader::layout(&p.blocks, width, None);
            let visible = laid.visible_link_count(scroll_body, viewport_h);
            match laid.first_link_at_or_after(cursor) {
                Some(slot) => (
                    slot.href.clone(),
                    visible,
                    LaidOutBody::link_segment_ranges(slot),
                ),
                None => (String::new(), visible, Vec::new()),
            }
        }
        None => {
            app.status_error = Some("yl: no parsed body".into());
            return;
        }
    };
    if href.is_empty() {
        app.status_error = Some("yl: no link in body".into());
        return;
    }
    let status = if visible > 1 {
        format!("yanked link (1 of {visible} visible)")
    } else {
        "yanked link".to_string()
    };
    set_yank_highlight(app, cfg, ranges);
    dispatch_yank(app, cfg, href, status);
}

/// Arm a transient yank highlight over `ranges` (body-relative cell
/// tuples). Honors `[reader].yank_highlight_ms = 0` as the disable
/// switch — an empty range vec also no-ops, so callers don't have to
/// pre-check before computing. Cleared on body change / scope switch
/// (see `InboxScreen`) and on expiry from `tick`.
fn set_yank_highlight(app: &mut App, cfg: &Config, ranges: Vec<(u16, u16, u16)>) {
    if cfg.reader.yank_highlight_ms == 0 || ranges.is_empty() {
        return;
    }
    let expires_at = Instant::now() + Duration::from_millis(cfg.reader.yank_highlight_ms as u64);
    app.inbox_mut().yank_highlight = Some(YankHighlight { ranges, expires_at });
}

/// Shared sink for the three yank entry points. Routes to OSC 52 vs
/// the configured `[reader].clipboard` fallback through `clipboard::yank`
/// and surfaces a one-shot status message in the cmdline row.
fn dispatch_yank(app: &mut App, cfg: &Config, text: String, ok_status: String) {
    let event_tx = app.event_tx.clone();
    match clipboard::yank(&text, cfg, event_tx.as_ref()) {
        YankOutcome::Sent => {
            app.status_error = Some(ok_status);
        }
        YankOutcome::Spawned(rx) => {
            app.clipboard_rx = Some(rx);
            app.status_error = Some(format!("{ok_status} (via fallback)"));
        }
        YankOutcome::Failed(e) => {
            app.status_error = Some(format!("yank: {e}"));
        }
    }
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
