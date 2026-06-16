use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::Config;
use crate::mail::text;
use crate::ui::app::{App, Mode, Pane, Screen};
use crate::ui::clipboard::{self, YankOutcome};
use crate::ui::motion::{self, Motion, Region};
use crate::ui::reader::{LaidOutBody, YankHighlight};
use crate::ui::textobj::{self, TextObjKind};
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
        Mode::AttachmentPick => attachment_pick(app, cfg, k),
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
    // `/` and `g/` enter Search mode (global / local). Handled before
    // the `q` / `:` early-returns so the pending-`g` state can settle
    // on the next key. Only applies to the inbox screen. `/` is the broad
    // default (spans `[search].global_folders`, i.e. inbox + archive out of
    // the box); `g/` narrows to the current folder.
    if matches!(app.screens.get(app.active), Some(Screen::Inbox(_))) {
        if app.pending_g {
            app.pending_g = false;
            if k.code == KeyCode::Char('/') {
                app.enter_search_local();
                return;
            }
            if k.code == KeyCode::Char('g') {
                // `gg` → top in whichever pane supports it. Reader: cursor
                // to row 0. List: first message in the folder.
                let inbox = app.inbox_mut();
                match inbox.focus {
                    Pane::Reader => inbox.move_reader_cursor_to_top(),
                    Pane::List => inbox.select_first(),
                    _ => {}
                }
                return;
            }
            // `g`-prefixed verbs, Reader focus only: `gx` opens the link
            // under the cursor (falling back to the attachment under the
            // cursor) in the external opener, `gs` saves the attachment,
            // `gd` drags it, `gf` opens the numeric attachment picker.
            // Anything else falls through.
            if app.inbox().focus == Pane::Reader {
                match k.code {
                    KeyCode::Char('x') => {
                        // Vim `gx`: a link under the cursor wins; otherwise
                        // fall through to the attachment opener.
                        if !open_link_under_cursor(app, cfg) {
                            cmdline::open_attachment_under_cursor(app, cfg);
                        }
                        return;
                    }
                    KeyCode::Char('s') => {
                        cmdline::save_attachment_under_cursor(app);
                        return;
                    }
                    KeyCode::Char('d') => {
                        cmdline::drag_attachment_under_cursor(app, cfg);
                        return;
                    }
                    KeyCode::Char('f') => {
                        if app.inbox().attachment_count() > 0 {
                            app.attachment_pick_buf.clear();
                            app.mode = Mode::AttachmentPick;
                        }
                        return;
                    }
                    // `ge` / `gE` — end of the previous word.
                    KeyCode::Char('e') => {
                        let n = app.pending_count.take().unwrap_or(1);
                        for _ in 0..n {
                            app.inbox_mut()
                                .reader_word(crate::ui::words::WordMotion::EndBack, false);
                        }
                        return;
                    }
                    KeyCode::Char('E') => {
                        let n = app.pending_count.take().unwrap_or(1);
                        for _ in 0..n {
                            app.inbox_mut()
                                .reader_word(crate::ui::words::WordMotion::EndBack, true);
                        }
                        return;
                    }
                    _ => {}
                }
            }
            // Any other key falls through to the rest of Normal mode
            // (so e.g. `g` then `j` still moves selection).
        }
        if k.code == KeyCode::Char('/') && !k.modifiers.intersects(KeyModifiers::CONTROL) {
            app.enter_search_global(cfg);
            return;
        }
        // `z`-prefix scroll positioning (`zz` / `zt` / `zb`), Reader focus.
        if app.pending_z {
            app.pending_z = false;
            if app.inbox().focus == Pane::Reader {
                use crate::ui::app::ViewportPos;
                match k.code {
                    KeyCode::Char('z') => {
                        app.inbox_mut().reader_scroll_cursor(ViewportPos::Middle);
                        return;
                    }
                    KeyCode::Char('t') => {
                        app.inbox_mut().reader_scroll_cursor(ViewportPos::Top);
                        return;
                    }
                    KeyCode::Char('b') => {
                        app.inbox_mut().reader_scroll_cursor(ViewportPos::Bottom);
                        return;
                    }
                    _ => {}
                }
            }
        }
        // Count prefix: digits accumulate into `pending_count`, consumed by
        // the next motion. `0` is the line-start motion unless a count is
        // already in progress. Only meaningful when not modified.
        if let KeyCode::Char(c) = k.code {
            let counting = app.pending_count.is_some();
            if !k
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                && c.is_ascii_digit()
                && (c != '0' || counting)
            {
                let d = c as usize - '0' as usize;
                app.pending_count = Some(app.pending_count.unwrap_or(0) * 10 + d);
                return;
            }
        }
        if k.code == KeyCode::Char('z')
            && !k.modifiers.intersects(KeyModifiers::CONTROL)
            && app.inbox().focus == Pane::Reader
        {
            app.pending_z = true;
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
            compose::KeyOutcome::Consumed => {
                // After every consumed compose key, re-derive the
                // address-completion popup state. This is where the
                // popup opens (token grew past min_chars), closes
                // (focus moved off a recipient field), or rearms its
                // external debounce.
                app.refresh_address_complete(cfg);
                return;
            }
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
            // Vim page navigation, focus-routed (reader scrolls, list
            // moves selection): Ctrl-d/u half page, Ctrl-f/b full page.
            KeyCode::Char('d') => app.page_move(true, false),
            KeyCode::Char('u') => app.page_move(false, false),
            KeyCode::Char('f') => app.page_move(true, true),
            KeyCode::Char('b') => app.page_move(false, true),
            // Ctrl-e/y scroll the reader viewport one line without moving
            // the cursor's intent; the draw-time clamp slides the cursor
            // to the viewport edge if the scroll would push it off-screen.
            KeyCode::Char('e') => app.scroll_reader_line(true),
            KeyCode::Char('y') => app.scroll_reader_line(false),
            // Ctrl-V enters block-wise visual selection in the reader.
            // Guarded on Reader focus so it doesn't fire over the list /
            // folder panes (where there's no body cursor to anchor).
            KeyCode::Char('v') if app.inbox().focus == Pane::Reader => {
                app.enter_visual(crate::ui::app::VisualKind::Block);
            }
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
    // has focus, reader scroll when Reader does. `f` forwards the selected
    // message when List has focus (alongside `F`), but is the link picker
    // over the rendered body when Reader does. The flag toggles
    // (`m`/`*`/`x`) only apply to a selected row, so they're List-only.
    let focus = app.inbox().focus;
    match focus {
        Pane::List => match k.code {
            KeyCode::Char('j') => app.select_next(),
            KeyCode::Char('k') => app.select_prev(),
            KeyCode::Char('G') => app.select_last(),
            // `v` / `V` arm (or cancel) a vim-style visual-line selection
            // over the list. There's no char/line distinction in a row
            // list, so both keys do the same thing; `j`/`k` then extend
            // the range and the action keys below operate on all of it.
            KeyCode::Char('v') | KeyCode::Char('V') => app.toggle_list_visual(),
            KeyCode::Char('m') => cmdline::flag_selection(app, 'S'),
            KeyCode::Char('*') => cmdline::flag_selection(app, 'F'),
            KeyCode::Char('x') => cmdline::flag_selection(app, 'T'),
            KeyCode::Char('a') => cmdline::archive_selected(app, cfg),
            KeyCode::Char('d') => cmdline::trash_selected(app, cfg),
            KeyCode::Char('D') => cmdline::trash_thread_selected(app, cfg),
            KeyCode::Char('c') => cmdline::open_blank_compose_external(app, cfg),
            KeyCode::Char('r') => cmdline::open_reply(app, cfg, cmdline::ReplyKind::Reply),
            KeyCode::Char('R') => cmdline::open_reply(app, cfg, cmdline::ReplyKind::ReplyAll),
            KeyCode::Char('f') | KeyCode::Char('F') => {
                cmdline::open_reply(app, cfg, cmdline::ReplyKind::Forward)
            }
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
            KeyCode::Esc if app.inbox().list_visual.is_some() => {
                app.inbox_mut().list_visual = None;
            }
            KeyCode::Esc if app.inbox().search.is_some() => app.clear_search(),
            KeyCode::Esc if app.inbox().sidebar_visible => {
                app.inbox_mut().focus = Pane::Folders;
            }
            _ => {}
        },
        Pane::Reader => {
            // `y` sequence in Reader: `yl` yank-link, `yip` yank inner
            // paragraph, `yap` yank a paragraph (block + trailing
            // newline). Armed on `y` with an empty buffer; each key is
            // appended and matched. `yi` / `ya` stay live as prefixes;
            // anything unrecognised clears the buffer and falls through to
            // the rest of the Reader keymap so e.g. `y` then `j` scrolls.
            if let Some(buf) = app.pending_y.clone() {
                match k.code {
                    KeyCode::Esc => {
                        app.pending_y = None;
                        return;
                    }
                    KeyCode::Char(c) => {
                        let mut seq = buf;
                        seq.push(c);
                        match seq.as_str() {
                            // `yy` — yank the current line (vim line-wise).
                            "y" => {
                                app.pending_y = None;
                                yank_line(app, cfg);
                                return;
                            }
                            "l" => {
                                app.pending_y = None;
                                yank_link(app, cfg);
                                return;
                            }
                            "ip" => {
                                app.pending_y = None;
                                yank_paragraph(app, cfg, false);
                                return;
                            }
                            "ap" => {
                                app.pending_y = None;
                                yank_paragraph(app, cfg, true);
                                return;
                            }
                            // `yie` / `yae` — whole body (vim-textobj-entire
                            // convention: "inner / around entire"). `yae`
                            // adds a trailing newline like `yap`.
                            "ie" => {
                                app.pending_y = None;
                                yank_body(app, cfg, false);
                                return;
                            }
                            "ae" => {
                                app.pending_y = None;
                                yank_body(app, cfg, true);
                                return;
                            }
                            // Operator-yanks: `yw`/`yb`/`ye` (+ WORD) yank
                            // the motion's span; `y}`/`y{` yank to the
                            // paragraph boundary.
                            "w" => {
                                app.pending_y = None;
                                yank_motion(app, cfg, Motion::WordForward, 1);
                                return;
                            }
                            "b" => {
                                app.pending_y = None;
                                yank_motion(app, cfg, Motion::WordBack, 1);
                                return;
                            }
                            "e" => {
                                app.pending_y = None;
                                yank_motion(app, cfg, Motion::WordEnd, 1);
                                return;
                            }
                            "W" => {
                                app.pending_y = None;
                                yank_motion(app, cfg, Motion::WordForwardBig, 1);
                                return;
                            }
                            "B" => {
                                app.pending_y = None;
                                yank_motion(app, cfg, Motion::WordBackBig, 1);
                                return;
                            }
                            "E" => {
                                app.pending_y = None;
                                yank_motion(app, cfg, Motion::WordEndBig, 1);
                                return;
                            }
                            "}" => {
                                app.pending_y = None;
                                yank_to_paragraph(app, cfg, true);
                                return;
                            }
                            "{" => {
                                app.pending_y = None;
                                yank_to_paragraph(app, cfg, false);
                                return;
                            }
                            // Still a valid prefix — keep collecting.
                            "i" | "a" => {
                                app.pending_y = Some(seq);
                                return;
                            }
                            // Two-char text object: `yiw`/`yaw`/`yi"`/`yi(`
                            // … (the `ip`/`ap`/`ie`/`ae` cases matched
                            // above).
                            s if s.len() == 2 && (s.starts_with('i') || s.starts_with('a')) => {
                                app.pending_y = None;
                                let around = s.starts_with('a');
                                yank_text_object(app, cfg, c, around);
                                return;
                            }
                            // Dead end — drop the sequence and let the key
                            // fall through to the normal Reader keymap.
                            _ => app.pending_y = None,
                        }
                    }
                    // Non-char key (arrows, etc.): abandon the sequence and
                    // fall through.
                    _ => app.pending_y = None,
                }
            }
            // Count prefix applies to the motions below; non-motion keys
            // (yank, visual, reply) ignore it and it's dropped after.
            let count = app.pending_count.take().unwrap_or(1);
            let repeat_word = |app: &mut App, m: crate::ui::words::WordMotion, big: bool| {
                for _ in 0..count {
                    app.inbox_mut().reader_word(m, big);
                }
            };
            match k.code {
                // Cursor motion: `j`/`k` walk the body-relative cursor and
                // let `follow_cursor` drag the viewport along, same as
                // visual mode. `gg` (top) lives in the `pending_g` block.
                KeyCode::Char('j') | KeyCode::Down => {
                    app.inbox_mut().move_reader_cursor(count as i32, 0);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    app.inbox_mut().move_reader_cursor(-(count as i32), 0);
                }
                KeyCode::Char('h') | KeyCode::Left => {
                    app.inbox_mut().move_reader_cursor(0, -(count as i32));
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    app.inbox_mut().move_reader_cursor(0, count as i32);
                }
                KeyCode::Char('0') | KeyCode::Home => {
                    app.inbox_mut().move_reader_cursor_to_line_start();
                }
                KeyCode::Char('$') | KeyCode::End => {
                    app.inbox_mut().move_reader_cursor_to_line_end();
                }
                KeyCode::Char('G') => app.inbox_mut().move_reader_cursor_to_bottom(),
                // `{` / `}` paragraph motions; `H` / `M` / `L` viewport
                // positions.
                KeyCode::Char('}') => app.inbox_mut().reader_paragraph(true),
                KeyCode::Char('{') => app.inbox_mut().reader_paragraph(false),
                KeyCode::Char('H') => app
                    .inbox_mut()
                    .reader_cursor_to_viewport(crate::ui::app::ViewportPos::Top),
                KeyCode::Char('M') => app
                    .inbox_mut()
                    .reader_cursor_to_viewport(crate::ui::app::ViewportPos::Middle),
                KeyCode::Char('L') => app
                    .inbox_mut()
                    .reader_cursor_to_viewport(crate::ui::app::ViewportPos::Bottom),
                KeyCode::Char('w') => {
                    repeat_word(app, crate::ui::words::WordMotion::Forward, false)
                }
                KeyCode::Char('b') => repeat_word(app, crate::ui::words::WordMotion::Back, false),
                KeyCode::Char('e') => repeat_word(app, crate::ui::words::WordMotion::End, false),
                KeyCode::Char('W') => repeat_word(app, crate::ui::words::WordMotion::Forward, true),
                KeyCode::Char('B') => repeat_word(app, crate::ui::words::WordMotion::Back, true),
                KeyCode::Char('E') => repeat_word(app, crate::ui::words::WordMotion::End, true),
                KeyCode::Char('f') => {
                    app.link_pick_buf.clear();
                    app.mode = Mode::LinkPick;
                }
                KeyCode::Char('y') => {
                    app.pending_y = Some(String::new());
                }
                KeyCode::Char('Y') => yank_line(app, cfg),
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
    // Ctrl-V (block) swap/exit. Must precede the plain `Char('v')` arm
    // below, which doesn't inspect modifiers and would otherwise swallow
    // it as char-wise.
    if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('v')) {
        let cur_kind = app.inbox().visual.as_ref().map(|v| v.kind);
        if cur_kind == Some(crate::ui::app::VisualKind::Block) {
            app.exit_visual();
        } else {
            app.inbox_mut()
                .set_visual_kind(crate::ui::app::VisualKind::Block);
        }
        return;
    }
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
            let laid = crate::ui::reader::layout(
                &p.blocks,
                width,
                &p.attachments,
                p.plain_fallback.as_deref(),
                None,
                None,
            );
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
        crate::ui::app::VisualKind::Block => "block",
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

/// `Mode::AttachmentPick` keymap (`gf`). Mirrors `link_pick`: digits
/// accumulate, `<Enter>` opens the chosen attachment in the external
/// viewer, `Esc` cancels.
fn attachment_pick(app: &mut App, cfg: &Config, k: KeyEvent) {
    match k.code {
        KeyCode::Esc => {
            app.attachment_pick_buf.clear();
            app.mode = Mode::Normal;
        }
        KeyCode::Backspace => {
            app.attachment_pick_buf.pop();
        }
        KeyCode::Char(c) if c.is_ascii_digit() => {
            app.attachment_pick_buf.push(c);
        }
        KeyCode::Enter => {
            let buf = std::mem::take(&mut app.attachment_pick_buf);
            app.mode = Mode::Normal;
            if let Ok(n) = buf.trim().parse::<usize>() {
                cmdline::open_attachment_index(app, n, cfg);
            } else if !buf.is_empty() {
                app.status_error = Some(format!("attachment: not a number: {buf}"));
            }
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
    // A `:` opened from a list-visual selection keeps the range alive so
    // the command can act on it (vim's `:'<,'>`). Cancelling the command
    // drops the selection too, matching vim's Esc-from-cmdline.
    app.inbox_mut().list_visual = None;
}

/// Yank the entire parsed body. `yie` (inner entire) / `yae` (a entire)
/// in the Reader pane — `trailing` appends a newline for `yae`, mirroring
/// `yap`. The whole body is too large to flash meaningfully, so no
/// highlight is armed.
fn yank_body(app: &mut App, cfg: &Config, trailing: bool) {
    let mut text = match app.inbox_parsed() {
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
    if trailing {
        text.push('\n');
    }
    dispatch_yank(app, cfg, text, "yanked entire body".to_string());
}

/// Yank the current reader line. `Y` / `yy` in the Reader pane, matching
/// vim's line-wise yank (the line plus its trailing newline). Reads the
/// per-frame `last_reader_body_line_text` so it lines up exactly with
/// what's on screen, and flashes the whole row.
fn yank_line(app: &mut App, cfg: &Config) {
    let inbox = app.inbox();
    let cursor = inbox.reader_cursor_line as usize;
    let Some(line) = inbox.last_reader_body_line_text.get(cursor).cloned() else {
        app.status_error = Some("yank: no line at cursor".into());
        return;
    };
    let width = crate::ui::reader::cells(&line);
    let ranges = if width > 0 {
        vec![(cursor as u16, 0, width)]
    } else {
        Vec::new()
    };
    set_yank_highlight(app, cfg, ranges);
    dispatch_yank(app, cfg, format!("{line}\n"), "yanked line".to_string());
}

/// Yank the top-level block at the reader cursor: `yip` (inner
/// paragraph) when `a_paragraph` is false, `yap` (a paragraph) when
/// true. Both yank the same block; `yap` appends a trailing newline so
/// pasting separates it from following text, matching vim's `ap`/`ip`
/// distinction. The cursor lives in body-relative coords, so this acts
/// on whichever top-level block the reader cursor sits in.
fn yank_paragraph(app: &mut App, cfg: &Config, a_paragraph: bool) {
    let inbox = app.inbox();
    let width = inbox.last_reader_inner_width.max(8);
    let cursor = inbox.reader_cursor_line;
    let (mut text, ranges) = match app.inbox_parsed() {
        Some(p) if !p.blocks.is_empty() => {
            let laid = crate::ui::reader::layout(
                &p.blocks,
                width,
                &p.attachments,
                p.plain_fallback.as_deref(),
                None,
                None,
            );
            match laid.block_at(cursor) {
                Some(idx) => (text::extract_block(&p.blocks[idx]), laid.block_ranges(idx)),
                None => (String::new(), Vec::new()),
            }
        }
        Some(_) => (String::new(), Vec::new()),
        None => {
            app.status_error = Some("yank: no parsed body".into());
            return;
        }
    };
    if text.is_empty() {
        app.status_error = Some("yank: no paragraph at cursor".into());
        return;
    }
    let label = if a_paragraph {
        text.push('\n');
        "yanked a paragraph"
    } else {
        "yanked paragraph"
    };
    set_yank_highlight(app, cfg, ranges);
    dispatch_yank(app, cfg, text, label.to_string());
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
            let laid = crate::ui::reader::layout(
                &p.blocks,
                width,
                &p.attachments,
                p.plain_fallback.as_deref(),
                None,
                None,
            );
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

/// Clamp the reader cursor to a real `(row, col)` over the laid-out body
/// (the live column can sit past EOL or at the `u16::MAX` sentinel).
fn reader_cursor_clamped(lines: &[String], line: u16, col: u16) -> (usize, usize) {
    let row = (line as usize).min(lines.len().saturating_sub(1));
    let len = lines.get(row).map(|l| l.chars().count()).unwrap_or(0);
    let col = if len == 0 {
        0
    } else {
        (col as usize).min(len - 1)
    };
    (row, col)
}

/// `y{motion}` in the reader: resolve the motion's span over the laid-out
/// body and yank the covered text (read-only — no buffer mutation).
fn yank_motion(app: &mut App, cfg: &Config, m: Motion, count: usize) {
    let lines = app.inbox().last_reader_body_line_text.clone();
    if lines.is_empty() {
        app.status_error = Some("yank: empty body".into());
        return;
    }
    let inbox = app.inbox();
    let cursor = reader_cursor_clamped(&lines, inbox.reader_cursor_line, inbox.reader_cursor_col);
    let Some(span) = motion::resolve_motion_span(&lines, cursor, m, count) else {
        return;
    };
    let region = motion::span_to_region(&lines, span);
    finish_reader_region_yank(app, cfg, &lines, &region);
}

/// `y{textobj}` in the reader (`yiw`, `yi"`, `yap` already handled
/// separately, etc.).
fn yank_text_object(app: &mut App, cfg: &Config, c: char, around: bool) {
    let lines = app.inbox().last_reader_body_line_text.clone();
    if lines.is_empty() {
        return;
    }
    let Some(obj) = textobj::key_to_text_object(c) else {
        return;
    };
    let inbox = app.inbox();
    let cursor = reader_cursor_clamped(&lines, inbox.reader_cursor_line, inbox.reader_cursor_col);
    let kind = if around {
        TextObjKind::Around
    } else {
        TextObjKind::Inner
    };
    let Some(span) = textobj::resolve_text_object(&lines, cursor, obj, kind) else {
        app.status_error = Some("yank: no text object at cursor".into());
        return;
    };
    let region = motion::span_to_region(&lines, span);
    finish_reader_region_yank(app, cfg, &lines, &region);
}

/// `y}` / `y{` — yank from the cursor to the next/previous paragraph
/// boundary (linewise).
fn yank_to_paragraph(app: &mut App, cfg: &Config, forward: bool) {
    let lines = app.inbox().last_reader_body_line_text.clone();
    if lines.is_empty() {
        return;
    }
    let is_blank = |r: usize| lines.get(r).map(|l| l.trim().is_empty()).unwrap_or(true);
    let cur = app.inbox().reader_cursor_line as usize;
    let other = if forward {
        let mut r = cur + 1;
        while r < lines.len() && !is_blank(r) {
            r += 1;
        }
        r.min(lines.len().saturating_sub(1))
    } else if cur == 0 {
        0
    } else {
        let mut r = cur - 1;
        while r > 0 && !is_blank(r) {
            r -= 1;
        }
        r
    };
    let region = Region {
        start: (cur.min(other), 0),
        end: (cur.max(other), 0),
        linewise: true,
    };
    finish_reader_region_yank(app, cfg, &lines, &region);
}

fn finish_reader_region_yank(app: &mut App, cfg: &Config, lines: &[String], region: &Region) {
    let (text, ranges) = reader_region_text(lines, region);
    if text.is_empty() {
        app.status_error = Some("yank: nothing at cursor".into());
        return;
    }
    set_yank_highlight(app, cfg, ranges);
    dispatch_yank(app, cfg, text, "yanked".to_string());
}

/// Extract a region's text from the laid-out body and the matching
/// highlight ranges. Char columns double as cell columns here (close
/// enough for the transient flash; exact for ASCII bodies).
fn reader_region_text(lines: &[String], region: &Region) -> (String, Vec<(u16, u16, u16)>) {
    if region.linewise {
        let mut text = String::new();
        let mut ranges = Vec::new();
        for r in region.start.0..=region.end.0 {
            if let Some(l) = lines.get(r) {
                text.push_str(l);
                text.push('\n');
                let w = crate::ui::reader::cells(l);
                if w > 0 {
                    ranges.push((r as u16, 0, w));
                }
            }
        }
        (text, ranges)
    } else {
        let (sr, sc) = region.start;
        let (er, ec) = region.end;
        let text = reader_extract_charwise(lines, region.start, region.end);
        let mut ranges = Vec::new();
        for r in sr..=er {
            if let Some(l) = lines.get(r) {
                let w = l.chars().count() as u16;
                let start = if r == sr { sc as u16 } else { 0 };
                let end = (if r == er { ec as u16 } else { w }).min(w);
                if end > start {
                    ranges.push((r as u16, start, end));
                }
            }
        }
        (text, ranges)
    }
}

fn reader_extract_charwise(lines: &[String], start: (usize, usize), end: (usize, usize)) -> String {
    if start.0 == end.0 {
        let chars: Vec<char> = lines[start.0].chars().collect();
        let lo = start.1.min(chars.len());
        let hi = end.1.min(chars.len());
        return chars[lo..hi].iter().collect();
    }
    let mut out = String::new();
    let first: Vec<char> = lines[start.0].chars().collect();
    out.extend(first[start.1.min(first.len())..].iter());
    out.push('\n');
    for line in lines.iter().take(end.0).skip(start.0 + 1) {
        out.push_str(line);
        out.push('\n');
    }
    let last: Vec<char> = lines[end.0].chars().collect();
    out.extend(last[..end.1.min(last.len())].iter());
    out
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

/// `gx` on the reader: open whatever sits under the cursor in the
/// external opener. A link directly under the cursor wins (vim's `gx`
/// semantics); otherwise we fall through to the attachment verb so a
/// cursor parked on an attachment chip — or the lone-attachment
/// fallback — still works. Returns `true` once a link was opened.
fn open_link_under_cursor(app: &mut App, cfg: &Config) -> bool {
    let inbox = app.inbox();
    let width = inbox.last_reader_inner_width.max(8);
    let line = inbox.reader_cursor_line;
    let col = inbox.reader_cursor_col;
    let Some(p) = app.inbox_parsed() else {
        return false;
    };
    let laid = crate::ui::reader::layout(
        &p.blocks,
        width,
        &p.attachments,
        p.plain_fallback.as_deref(),
        None,
        None,
    );
    let Some(href) = laid.link_at(line, col).map(|s| s.href.clone()) else {
        return false;
    };
    if let Err(e) = crate::ui::browser::open_url(&href, &cfg.reader.browser) {
        app.status_error = Some(format!("open: {e:#}"));
    }
    true
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
    let laid = crate::ui::reader::layout(
        &parsed.blocks,
        80,
        &[],
        parsed.plain_fallback.as_deref(),
        None,
        None,
    );
    let Some(slot) = laid.links.iter().find(|s| s.id == id) else {
        app.status_error = Some(format!("link: no such id: {id}"));
        return;
    };
    if let Err(e) = crate::ui::browser::open_url(&slot.href, &cfg.reader.browser) {
        app.status_error = Some(format!("open: {e:#}"));
    }
}
