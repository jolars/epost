//! Modal (Normal / Insert) editing for the compose header fields
//! (From / To / Cc / Bcc / Subject), mirroring the vim-first model the
//! body editor ([`crate::ui::compose_body`]) already uses so the whole
//! compose form reads as one vim surface.
//!
//! The fields keep storing their text in plain [`TextInput`]s — the only
//! engine that touches a `String + byte cursor`. This module adds the
//! mode layer on top: in **Insert** mode keys route straight to
//! `TextInput`'s readline editing (today's behaviour); in **Normal** mode
//! a focused single-line vim subset moves/edits the cursor and `j`/`k`
//! walk between fields as if they were lines.
//!
//! Scope is deliberately the "focused single-line set", not the body's
//! full operator engine: motions `h l 0 ^ $ w W b B e E`, mode entry
//! `i a I A`, edits `x s D C dd cc S r ~`, and `j`/`k` field navigation.
//! There are no counts, no `f`/`t` find-char, no text objects, and no
//! `d{motion}`/`y`/`p` — addresses and subjects are short, single-line
//! strings where that surface buys little. (v1 drift: `~` only flips
//! ASCII case; `^` treats the field as having no leading indent.)

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::compose::{ComposeField, ComposeScreen, KeyOutcome};
use crate::ui::text_input::TextInput;
use crate::ui::words::{WordMotion, word_motion};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeaderMode {
    /// vim Normal: motions + edits, no text insertion. The default a tab
    /// opens in, matching the body editor.
    #[default]
    Normal,
    /// vim Insert: keys flow to `TextInput`'s readline editing.
    Insert,
}

/// Where the cursor lands when entering Insert mode.
enum InsertAt {
    /// `i` — at the cursor.
    Cursor,
    /// `a` — one char to the right (append after the current char).
    After,
    /// `I` — start of line.
    Home,
    /// `A` — end of line.
    End,
}

/// Dispatch a key to the focused header field's modal editor. Only called
/// from [`crate::ui::compose::handle_key`] once the field-universal keys
/// (Tab/BackTab cycle, Enter-on-From picker) have had their crack, and
/// only when a header field (not Body / Attach) holds focus.
pub fn handle_field_key(screen: &mut ComposeScreen, k: KeyEvent) -> KeyOutcome {
    match screen.header_mode {
        HeaderMode::Insert => handle_insert(screen, k),
        HeaderMode::Normal => handle_normal(screen, k),
    }
}

fn handle_insert(screen: &mut ComposeScreen, k: KeyEvent) -> KeyOutcome {
    if k.code == KeyCode::Esc {
        screen.header_mode = HeaderMode::Normal;
        if let Some(input) = screen.focused_input_mut() {
            // vim nudges the cursor left off the just-typed char on leaving
            // Insert, then clamps onto a real char.
            input.move_left();
            input.clamp_normal();
        }
        return KeyOutcome::Consumed;
    }
    if let Some(input) = screen.focused_input_mut() {
        input.handle(k);
    }
    KeyOutcome::Consumed
}

fn handle_normal(screen: &mut ComposeScreen, k: KeyEvent) -> KeyOutcome {
    // A pending `d` / `c` / `r` latch consumes the next key first.
    if let Some(pending) = screen.header_pending.take() {
        return resolve_pending(screen, pending, k);
    }
    // Ctrl/Alt chords are handled upstream (Ctrl-J/K jumps, Alt-e/f); a
    // stray one here is swallowed so it can't fall through to the app.
    if k.modifiers.contains(KeyModifiers::CONTROL) || k.modifiers.contains(KeyModifiers::ALT) {
        return KeyOutcome::Consumed;
    }
    match k.code {
        // Field navigation: j/k walk the header rows like vim lines.
        KeyCode::Char('j') | KeyCode::Down => field_down(screen),
        KeyCode::Char('k') | KeyCode::Up => field_up(screen),
        // Enter Insert.
        KeyCode::Char('i') => enter_insert(screen, InsertAt::Cursor),
        KeyCode::Char('a') => enter_insert(screen, InsertAt::After),
        KeyCode::Char('I') => enter_insert(screen, InsertAt::Home),
        KeyCode::Char('A') => enter_insert(screen, InsertAt::End),
        // Intra-line motions.
        KeyCode::Char('h') | KeyCode::Left => with_input(screen, |i| i.move_left()),
        KeyCode::Char('l') | KeyCode::Right => with_input(screen, |i| {
            i.move_right();
            i.clamp_normal();
        }),
        KeyCode::Char('0') => with_input(screen, |i| i.move_home()),
        KeyCode::Char('^') => with_input(screen, first_non_blank),
        KeyCode::Char('$') => with_input(screen, |i| {
            i.move_end();
            i.clamp_normal();
        }),
        KeyCode::Char('w') => word(screen, WordMotion::Forward, false),
        KeyCode::Char('W') => word(screen, WordMotion::Forward, true),
        KeyCode::Char('b') => word(screen, WordMotion::Back, false),
        KeyCode::Char('B') => word(screen, WordMotion::Back, true),
        KeyCode::Char('e') => word(screen, WordMotion::End, false),
        KeyCode::Char('E') => word(screen, WordMotion::End, true),
        // Edits that stay in Normal.
        KeyCode::Char('x') => with_input(screen, |i| {
            i.delete_right();
            i.clamp_normal();
        }),
        KeyCode::Char('D') => with_input(screen, |i| {
            i.delete_to_end();
            i.clamp_normal();
        }),
        KeyCode::Char('~') => with_input(screen, toggle_ascii_case),
        // Edits that drop into Insert.
        KeyCode::Char('s') => {
            with_input(screen, |i| i.delete_right());
            screen.header_mode = HeaderMode::Insert;
        }
        KeyCode::Char('C') => {
            with_input(screen, |i| i.delete_to_end());
            screen.header_mode = HeaderMode::Insert;
        }
        KeyCode::Char('S') => {
            with_input(screen, |i| i.clear());
            screen.header_mode = HeaderMode::Insert;
        }
        // Two-key / capture sequences.
        KeyCode::Char('d') => screen.header_pending = Some('d'),
        KeyCode::Char('c') => screen.header_pending = Some('c'),
        KeyCode::Char('r') => screen.header_pending = Some('r'),
        _ => {}
    }
    KeyOutcome::Consumed
}

fn resolve_pending(screen: &mut ComposeScreen, pending: char, k: KeyEvent) -> KeyOutcome {
    match (pending, k.code) {
        // `dd` clears the line, staying in Normal.
        ('d', KeyCode::Char('d')) => with_input(screen, |i| i.clear()),
        // `cc` clears and drops into Insert (so does the discrete `S`).
        ('c', KeyCode::Char('c')) => {
            with_input(screen, |i| i.clear());
            screen.header_mode = HeaderMode::Insert;
        }
        // `r{char}` replaces the char under the cursor in place.
        ('r', KeyCode::Char(ch)) => with_input(screen, |i| i.replace_char_under_cursor(ch)),
        // Anything else cancels the pending operator (Esc, mismatched key).
        _ => {}
    }
    KeyOutcome::Consumed
}

fn enter_insert(screen: &mut ComposeScreen, at: InsertAt) {
    if let Some(i) = screen.focused_input_mut() {
        match at {
            InsertAt::Cursor => {}
            InsertAt::After => i.move_right(),
            InsertAt::Home => i.move_home(),
            InsertAt::End => i.move_end(),
        }
    }
    screen.header_mode = HeaderMode::Insert;
}

fn word(screen: &mut ComposeScreen, motion: WordMotion, big: bool) {
    if let Some(i) = screen.focused_input_mut() {
        let lines = [i.as_str().to_string()];
        let col = i.cursor_char();
        let (_, new_col) = word_motion(&lines, 0, col, motion, big);
        i.set_cursor_char(new_col);
        i.clamp_normal();
    }
}

fn field_down(screen: &mut ComposeScreen) {
    let next = match screen.focused {
        ComposeField::From => ComposeField::To,
        ComposeField::To => ComposeField::Cc,
        ComposeField::Cc => ComposeField::Bcc,
        ComposeField::Bcc => ComposeField::Subject,
        // Past the last text field, hand off to the Attach row (which owns
        // its own j/k once focused). No wrap — j at the bottom is a stop.
        ComposeField::Subject => ComposeField::Attach,
        _ => return,
    };
    screen.set_focus(next);
    if let Some(i) = screen.focused_input_mut() {
        i.clamp_normal();
    }
}

fn field_up(screen: &mut ComposeScreen) {
    let prev = match screen.focused {
        ComposeField::To => ComposeField::From,
        ComposeField::Cc => ComposeField::To,
        ComposeField::Bcc => ComposeField::Cc,
        ComposeField::Subject => ComposeField::Bcc,
        // From is the top row — k there is a stop, matching vim.
        _ => return,
    };
    screen.set_focus(prev);
    if let Some(i) = screen.focused_input_mut() {
        i.clamp_normal();
    }
}

fn with_input(screen: &mut ComposeScreen, f: impl FnOnce(&mut TextInput)) {
    if let Some(i) = screen.focused_input_mut() {
        f(i);
    }
}

fn first_non_blank(i: &mut TextInput) {
    let idx = i
        .as_str()
        .chars()
        .position(|c| !c.is_whitespace())
        .unwrap_or(0);
    i.set_cursor_char(idx);
    i.clamp_normal();
}

fn toggle_ascii_case(i: &mut TextInput) {
    if let Some(c) = i.char_under_cursor() {
        let flipped = if c.is_ascii_uppercase() {
            c.to_ascii_lowercase()
        } else if c.is_ascii_lowercase() {
            c.to_ascii_uppercase()
        } else {
            c
        };
        i.replace_char_under_cursor(flipped);
        i.move_right();
        i.clamp_normal();
    }
}

/// Short mode tag for the `Compose` block title (`Compose — NORMAL`).
pub fn mode_label(mode: HeaderMode) -> &'static str {
    match mode {
        HeaderMode::Normal => "NORMAL",
        HeaderMode::Insert => "INSERT",
    }
}

/// Bottom hint line shown while a header field is focused.
pub fn hint(mode: HeaderMode) -> &'static str {
    match mode {
        HeaderMode::Insert => {
            " -- INSERT --  Esc normal  Tab fields  Alt-f from  Ctrl-J body  :send "
        }
        HeaderMode::Normal => {
            " -- NORMAL --  i insert  j/k fields  x/D/dd edit  Alt-f from  Ctrl-J body  :send "
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::compose::Draft;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn screen_with_to(to: &str) -> ComposeScreen {
        let draft = Draft {
            account: "acct".into(),
            from: "me@example.com".into(),
            to: vec![to.into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "hi".into(),
            body: String::new(),
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
        };
        let mut s = ComposeScreen::from_draft(draft).unwrap();
        s.set_focus(ComposeField::To);
        // from_draft seeds the To row from the joined draft addresses; for
        // these tests park the cursor on the first char (Normal default).
        if let Some(i) = s.focused_input_mut() {
            i.move_home();
        }
        s
    }

    #[test]
    fn opens_in_normal_mode() {
        let s = screen_with_to("alice@example.com");
        assert_eq!(s.header_mode, HeaderMode::Normal);
    }

    #[test]
    fn typing_in_normal_does_not_insert() {
        let mut s = screen_with_to("alice@example.com");
        handle_field_key(&mut s, key('z'));
        assert_eq!(s.to.as_str(), "alice@example.com");
        assert_eq!(s.header_mode, HeaderMode::Normal);
    }

    #[test]
    fn i_enters_insert_and_types() {
        let mut s = screen_with_to("bob");
        handle_field_key(&mut s, key('i'));
        assert_eq!(s.header_mode, HeaderMode::Insert);
        handle_field_key(&mut s, key('X'));
        assert_eq!(s.to.as_str(), "Xbob");
    }

    #[test]
    fn a_appends_after_cursor() {
        let mut s = screen_with_to("bob");
        handle_field_key(&mut s, key('a'));
        handle_field_key(&mut s, key('Z'));
        assert_eq!(s.to.as_str(), "bZob");
    }

    #[test]
    fn capital_a_appends_at_end() {
        let mut s = screen_with_to("bob");
        handle_field_key(&mut s, key('A'));
        handle_field_key(&mut s, key('!'));
        assert_eq!(s.to.as_str(), "bob!");
    }

    #[test]
    fn esc_leaves_insert_and_nudges_left() {
        let mut s = screen_with_to("bob");
        handle_field_key(&mut s, key('A')); // cursor at end, Insert
        handle_field_key(&mut s, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(s.header_mode, HeaderMode::Normal);
        // Back on the last char; `x` removes it.
        handle_field_key(&mut s, key('x'));
        assert_eq!(s.to.as_str(), "bo");
    }

    #[test]
    fn x_deletes_char_under_cursor() {
        let mut s = screen_with_to("bob");
        handle_field_key(&mut s, key('x'));
        assert_eq!(s.to.as_str(), "ob");
    }

    #[test]
    fn dollar_lands_on_last_char_not_past() {
        let mut s = screen_with_to("bob");
        handle_field_key(&mut s, key('$'));
        handle_field_key(&mut s, key('x'));
        assert_eq!(s.to.as_str(), "bo");
    }

    #[test]
    fn dd_clears_the_line() {
        let mut s = screen_with_to("alice@example.com");
        handle_field_key(&mut s, key('d'));
        handle_field_key(&mut s, key('d'));
        assert_eq!(s.to.as_str(), "");
        assert_eq!(s.header_mode, HeaderMode::Normal);
    }

    #[test]
    fn cc_clears_and_enters_insert() {
        let mut s = screen_with_to("alice@example.com");
        handle_field_key(&mut s, key('c'));
        handle_field_key(&mut s, key('c'));
        assert_eq!(s.to.as_str(), "");
        assert_eq!(s.header_mode, HeaderMode::Insert);
    }

    #[test]
    fn capital_d_deletes_to_end() {
        let mut s = screen_with_to("alice@example.com");
        handle_field_key(&mut s, key('l')); // cursor on 'l'
        handle_field_key(&mut s, key('D'));
        assert_eq!(s.to.as_str(), "a");
    }

    #[test]
    fn r_replaces_one_char() {
        let mut s = screen_with_to("bob");
        handle_field_key(&mut s, key('r'));
        handle_field_key(&mut s, key('R'));
        assert_eq!(s.to.as_str(), "Rob");
        // Cursor stays on the replaced char.
        handle_field_key(&mut s, key('x'));
        assert_eq!(s.to.as_str(), "ob");
    }

    #[test]
    fn tilde_toggles_case_and_advances() {
        let mut s = screen_with_to("ab");
        handle_field_key(&mut s, key('~'));
        assert_eq!(s.to.as_str(), "Ab");
        handle_field_key(&mut s, key('~'));
        assert_eq!(s.to.as_str(), "AB");
    }

    #[test]
    fn word_motion_then_delete() {
        let mut s = screen_with_to("foo bar");
        handle_field_key(&mut s, key('w')); // jump to 'bar'
        handle_field_key(&mut s, key('D')); // delete to end
        assert_eq!(s.to.as_str(), "foo ");
    }

    #[test]
    fn j_k_walk_between_fields() {
        let mut s = screen_with_to("x");
        assert_eq!(s.focused, ComposeField::To);
        handle_field_key(&mut s, key('j'));
        assert_eq!(s.focused, ComposeField::Cc);
        handle_field_key(&mut s, key('k'));
        assert_eq!(s.focused, ComposeField::To);
        handle_field_key(&mut s, key('k'));
        assert_eq!(s.focused, ComposeField::From);
        // k at the top row is a stop.
        handle_field_key(&mut s, key('k'));
        assert_eq!(s.focused, ComposeField::From);
    }

    #[test]
    fn dangling_operator_eats_the_next_key() {
        // `d` then a non-`d` key cancels the operator (we don't support
        // d+motion) and the key is swallowed, vim-style: focus is
        // unchanged and nothing is deleted.
        let mut s = screen_with_to("alice");
        handle_field_key(&mut s, key('d')); // pending 'd'
        handle_field_key(&mut s, key('j')); // cancels, no field move
        assert_eq!(s.focused, ComposeField::To);
        assert_eq!(s.to.as_str(), "alice");
        assert!(s.header_pending.is_none());
        // The next `j` moves normally.
        handle_field_key(&mut s, key('j'));
        assert_eq!(s.focused, ComposeField::Cc);
    }

    #[test]
    fn tab_cancels_pending_op_via_set_focus() {
        // Tab routes through compose::handle_key → focus_next → set_focus,
        // which clears the latch so it can't fire on the next field.
        let mut s = screen_with_to("alice");
        handle_field_key(&mut s, key('d')); // pending 'd'
        s.focus_next(); // simulate Tab cycling fields
        assert!(s.header_pending.is_none());
    }
}
