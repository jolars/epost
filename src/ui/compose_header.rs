//! Modal (Normal / Insert / Visual) editing for the compose header fields
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
//! Visual selection stays within the focused field. Explicit yanks share
//! the body's register and system clipboard sink, so `p` / `P` work across
//! headers and the body. Yank motions and text objects reuse the body's
//! pure resolvers. Counts and find-char remain outside the header subset.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::compose::{ComposeField, ComposeScreen, KeyOutcome};
use crate::ui::motion::{self, Motion, Region};
use crate::ui::paste::{Placement, insert_single};
use crate::ui::text_input::TextInput;
use crate::ui::textobj::{self, TextObjKind};
use crate::ui::words::{WordMotion, word_motion};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeaderMode {
    /// vim Normal: motions + edits, no text insertion. The default a tab
    /// opens in, matching the body editor.
    #[default]
    Normal,
    /// vim Insert: keys flow to `TextInput`'s readline editing.
    Insert,
    /// Selection endpoints use char indices, matching the shared motions.
    Visual { anchor: usize, linewise: bool },
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
    let normal = screen.header_mode != HeaderMode::Insert;
    if screen.header_pending.is_none()
        && let Some(input) = screen.focused_input_mut()
        && let Some(shortcut) = input.paste_prefix.handle(k, normal)
    {
        return match shortcut {
            crate::ui::paste::Shortcut::Consumed => KeyOutcome::Consumed,
            crate::ui::paste::Shortcut::Read(placement) => KeyOutcome::ClipboardPaste(placement),
        };
    }
    match screen.header_mode {
        HeaderMode::Insert => handle_insert(screen, k),
        HeaderMode::Normal => handle_normal(screen, k),
        HeaderMode::Visual { .. } => handle_visual(screen, k),
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
    // `:` / `/` / `?` pass through to the host cmdline so `:send` and
    // friends work from a header field's Normal mode, exactly as they do
    // from the body editor's Normal mode (compose_body's passthrough arm).
    if matches!(
        k.code,
        KeyCode::Char(':') | KeyCode::Char('/') | KeyCode::Char('?')
    ) {
        return KeyOutcome::PassThrough;
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
        KeyCode::Char('v') => toggle_visual(screen, false),
        KeyCode::Char('V') => toggle_visual(screen, true),
        KeyCode::Char('y') => screen.header_pending = Some('y'),
        KeyCode::Char('Y') => yank_motion(screen, Motion::LineEnd),
        KeyCode::Char('p') => put(screen, Placement::After),
        KeyCode::Char('P') => put(screen, Placement::Before),
        _ => {}
    }
    KeyOutcome::Consumed
}

fn resolve_pending(screen: &mut ComposeScreen, pending: char, k: KeyEvent) -> KeyOutcome {
    if k.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return KeyOutcome::Consumed;
    }
    if pending == 'y' {
        match k.code {
            KeyCode::Char('y') => yank_range(screen, 0..usize::MAX, true),
            KeyCode::Char(c @ ('i' | 'a')) => screen.header_pending = Some(c),
            _ => {
                if let Some(m) = single_line_motion(k) {
                    yank_motion(screen, m);
                }
            }
        }
        return KeyOutcome::Consumed;
    }
    if matches!(pending, 'i' | 'a') {
        if let KeyCode::Char(c) = k.code
            && let Some(obj) = textobj::key_to_text_object(c)
            && let Some(input) = screen.focused_input_mut()
        {
            let lines = [input.as_str().to_string()];
            let kind = if pending == 'i' {
                TextObjKind::Inner
            } else {
                TextObjKind::Around
            };
            if let Some(span) =
                textobj::resolve_text_object(&lines, (0, input.cursor_char()), obj, kind)
            {
                let region = motion::span_to_region(&lines, span);
                if matches!(screen.header_mode, HeaderMode::Visual { .. }) {
                    with_input(screen, |i| {
                        i.set_cursor_char(region.end.1.saturating_sub(1))
                    });
                    screen.header_mode = HeaderMode::Visual {
                        anchor: region.start.1,
                        linewise: region.linewise,
                    };
                } else {
                    yank_region(screen, region);
                }
            }
        }
        return KeyOutcome::Consumed;
    }
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

fn single_line_motion(k: KeyEvent) -> Option<Motion> {
    motion::key_to_motion(k).filter(|m| {
        matches!(
            m,
            Motion::CharLeft
                | Motion::CharRight
                | Motion::LineStart
                | Motion::FirstNonBlank
                | Motion::LineEnd
                | Motion::WordForward
                | Motion::WordForwardBig
                | Motion::WordBack
                | Motion::WordBackBig
                | Motion::WordEnd
                | Motion::WordEndBig
        )
    })
}

fn toggle_visual(screen: &mut ComposeScreen, linewise: bool) {
    screen.header_mode = match screen.header_mode {
        HeaderMode::Visual { linewise: old, .. } if old == linewise => HeaderMode::Normal,
        HeaderMode::Visual { anchor, .. } => HeaderMode::Visual { anchor, linewise },
        _ => {
            let Some(input) = screen.focused_input_mut() else {
                return;
            };
            input.clamp_normal();
            HeaderMode::Visual {
                anchor: input.cursor_char(),
                linewise,
            }
        }
    };
}

pub fn selection_range(mode: HeaderMode, input: &TextInput) -> Option<std::ops::Range<usize>> {
    let HeaderMode::Visual { anchor, linewise } = mode else {
        return None;
    };
    let len = input.as_str().chars().count();
    let cursor = input.cursor_char();
    Some(if linewise {
        0..len
    } else {
        anchor.min(cursor).min(len)..anchor.max(cursor).saturating_add(1).min(len)
    })
}

fn handle_visual(screen: &mut ComposeScreen, k: KeyEvent) -> KeyOutcome {
    if let Some(pending) = screen.header_pending.take() {
        return resolve_pending(screen, pending, k);
    }
    if k.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return KeyOutcome::Consumed;
    }
    match k.code {
        KeyCode::Esc => screen.header_mode = HeaderMode::Normal,
        KeyCode::Char('v') => toggle_visual(screen, false),
        KeyCode::Char('V') => toggle_visual(screen, true),
        KeyCode::Char('i' | 'a') => {
            if let KeyCode::Char(c) = k.code {
                screen.header_pending = Some(c);
            }
        }
        KeyCode::Char('y') => {
            let mode = screen.header_mode;
            if let Some(input) = screen.focused_input_mut()
                && let Some(range) = selection_range(mode, input)
            {
                yank_range(
                    screen,
                    range,
                    matches!(mode, HeaderMode::Visual { linewise: true, .. }),
                );
            }
        }
        KeyCode::Char('p') => put(screen, Placement::After),
        KeyCode::Char('P') => put(screen, Placement::Before),
        _ => {
            if let Some(m) = single_line_motion(k)
                && let Some(input) = screen.focused_input_mut()
            {
                let lines = [input.as_str().to_string()];
                if let Some(span) =
                    motion::resolve_motion_span(&lines, (0, input.cursor_char()), m, 1)
                {
                    input.set_cursor_char(span.end.1);
                    input.clamp_normal();
                }
            }
        }
    }
    KeyOutcome::Consumed
}

fn yank_motion(screen: &mut ComposeScreen, m: Motion) {
    if let Some(input) = screen.focused_input_mut() {
        input.clamp_normal();
        // A following empty line lets the shared operator resolver detect
        // end-of-field instead of treating the last char as a word start.
        let lines = [input.as_str().to_string(), String::new()];
        if let Some(span) = motion::resolve_motion_span(&lines, (0, input.cursor_char()), m, 1) {
            yank_region(screen, motion::span_to_region(&lines, span));
        }
    }
}

fn yank_region(screen: &mut ComposeScreen, region: Region) {
    let range = if region.linewise {
        0..usize::MAX
    } else {
        region.start.1..region.end.1
    };
    yank_range(screen, range, region.linewise);
}

fn yank_range(screen: &mut ComposeScreen, range: std::ops::Range<usize>, linewise: bool) {
    if let Some(input) = screen.focused_input_mut() {
        let mut text: String = input
            .as_str()
            .chars()
            .skip(range.start)
            .take(range.end.saturating_sub(range.start))
            .collect();
        input.set_cursor_char(range.start);
        input.clamp_normal();
        if linewise {
            text.push('\n');
        }
        if !text.is_empty() {
            screen.body.record_yank(text, linewise);
        }
    }
    screen.header_mode = HeaderMode::Normal;
}

fn put(screen: &mut ComposeScreen, placement: Placement) {
    if let Some((text, linewise)) = screen.body.yank_register() {
        // A linewise register's final newline separates body lines, but
        // should not add a space to a single-line address field.
        let text = if linewise {
            text.strip_suffix('\n').unwrap_or(text)
        } else {
            text
        }
        .to_string();
        paste(screen, &text, placement);
    }
}

pub fn paste(screen: &mut ComposeScreen, text: &str, placement: Placement) {
    let mode = screen.header_mode;
    screen.header_pending = None;
    if let Some(input) = screen.focused_input_mut() {
        if let Some(range) = selection_range(mode, input) {
            let byte = |col| {
                input
                    .as_str()
                    .char_indices()
                    .nth(col)
                    .map(|(b, _)| b)
                    .unwrap_or(input.as_str().len())
            };
            let start = byte(range.start);
            let end = byte(range.end);
            let text = crate::ui::paste::normalize(text, true);
            input.replace_range(start..end, &text, start + text.len());
            input.move_left();
            input.clamp_normal();
        } else {
            insert_single(input, text, placement, mode == HeaderMode::Normal);
        }
    }
    if matches!(mode, HeaderMode::Visual { .. }) {
        screen.header_mode = HeaderMode::Normal;
    }
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
        HeaderMode::Visual {
            linewise: false, ..
        } => "VISUAL",
        HeaderMode::Visual { linewise: true, .. } => "VISUAL LINE",
    }
}

/// Bottom hint line shown while a header field is focused.
pub fn hint(mode: HeaderMode) -> &'static str {
    match mode {
        HeaderMode::Insert => {
            " -- INSERT --  Esc normal  Tab fields  Alt-f from  Ctrl-J body  :send "
        }
        HeaderMode::Visual { .. } => {
            " -- VISUAL --  h/l/w/b/e select  y copy  p paste  Esc cancel "
        }
        HeaderMode::Normal => {
            " -- NORMAL --  v/V select  yy copy  p paste  i insert  j/k fields  x/D/dd edit  Alt-f from  Ctrl-J body  :send  q close "
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
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
        let mut s = ComposeScreen::from_draft(
            draft,
            crate::config::ComposeWrap::default(),
            Config::default().compose.wrap_width,
        )
        .unwrap();
        s.set_focus(ComposeField::To);
        // from_draft seeds the To row from the joined draft addresses; for
        // these tests park the cursor on the first char (Normal default).
        if let Some(i) = s.focused_input_mut() {
            i.move_home();
        }
        s
    }

    fn type_keys(screen: &mut ComposeScreen, keys: &str) {
        for c in keys.chars() {
            handle_field_key(screen, key(c));
        }
    }

    #[test]
    fn visual_yank_is_inclusive_in_both_directions_and_handles_unicode() {
        for keys in ["vlly", "$vhhy"] {
            let mut s = screen_with_to("é中x");
            type_keys(&mut s, keys);
            assert_eq!(s.body.take_clipboard_yank().as_deref(), Some("é中x"));
            assert_eq!(s.to.as_str(), "é中x");
            assert_eq!(s.to.cursor_char(), 0);
            assert_eq!(s.header_mode, HeaderMode::Normal);
        }
    }

    #[test]
    fn yank_motions_and_text_objects_share_the_body_register() {
        for (keys, expected) in [
            ("yy", "Alice <alice@example.com>\n"),
            ("wY", "<alice@example.com>"),
            ("yw", "Alice "),
            ("yiw", "Alice"),
            ("wyi<", "alice@example.com"),
            ("wya<", "<alice@example.com>"),
            ("wvi<y", "alice@example.com"),
        ] {
            let mut s = screen_with_to("Alice <alice@example.com>");
            type_keys(&mut s, keys);
            assert_eq!(
                s.body.take_clipboard_yank().as_deref(),
                Some(expected),
                "{keys}"
            );
            s.body.handle_key(key('p'), 72);
            let body = if expected.ends_with('\n') {
                format!("\n{}", expected.trim_end_matches('\n'))
            } else {
                expected.to_string()
            };
            assert_eq!(s.body.text(), body, "{keys}");
        }
    }

    #[test]
    fn word_yank_at_end_of_field_includes_the_final_character() {
        for (text, keys, expected) in [
            ("alice@example.com", "yW", "alice@example.com"),
            ("hello world", "wyw", "world"),
            ("é", "yw", "é"),
        ] {
            let mut s = screen_with_to(text);
            type_keys(&mut s, keys);
            assert_eq!(s.body.take_clipboard_yank().as_deref(), Some(expected));
        }
    }

    #[test]
    fn quoted_text_object_bypasses_clipboard_prefix() {
        let mut s = screen_with_to("\"Alice Doe\" <alice@example.com>");
        type_keys(&mut s, "yi\"");
        assert_eq!(s.body.take_clipboard_yank().as_deref(), Some("Alice Doe"));
        assert_eq!(s.to.paste_prefix, crate::ui::paste::Prefix::None);
    }

    #[test]
    fn every_header_can_yank_and_put_into_another_field() {
        for field in [
            ComposeField::From,
            ComposeField::To,
            ComposeField::Cc,
            ComposeField::Bcc,
            ComposeField::Subject,
        ] {
            let mut s = screen_with_to("");
            s.set_focus(field);
            *s.focused_input_mut().unwrap() = TextInput::from_string("é@example.com");
            type_keys(&mut s, "Vy");
            assert_eq!(
                s.body.take_clipboard_yank().as_deref(),
                Some("é@example.com\n")
            );
            s.set_focus(ComposeField::To);
            s.to.clear();
            type_keys(&mut s, "p");
            assert_eq!(s.to.as_str(), "é@example.com");
            assert!(s.body.take_clipboard_yank().is_none());
        }
    }

    #[test]
    fn header_put_uses_body_yank_and_replaces_visual_selection() {
        let mut s = screen_with_to("abc");
        s.body.set_text("é");
        s.body.handle_key(key('y'), 72);
        s.body.handle_key(key('y'), 72);
        type_keys(&mut s, "vlp");
        assert_eq!(s.to.as_str(), "éc");
        assert_eq!(s.to.cursor_char(), 0);
        assert_eq!(s.header_mode, HeaderMode::Normal);
    }

    #[test]
    fn put_before_and_after_have_vim_placement() {
        for (keys, expected) in [("p", "aébc"), ("P", "éabc")] {
            let mut s = screen_with_to("abc");
            s.body.record_yank("é".into(), false);
            type_keys(&mut s, keys);
            assert_eq!(s.to.as_str(), expected);
            assert_eq!(s.to.char_under_cursor(), Some('é'));
        }
    }

    #[test]
    fn visual_paste_normalizes_multiline_text_and_preserves_register() {
        let mut s = screen_with_to("é中x");
        s.body.record_yank("keep".into(), false);
        type_keys(&mut s, "vl");
        paste(&mut s, "å\r\nb", Placement::Cursor);
        assert_eq!(s.to.as_str(), "å bx");
        assert_eq!(s.to.char_under_cursor(), Some('b'));
        assert_eq!(s.body.yank_register(), Some(("keep", false)));
        assert_eq!(s.header_mode, HeaderMode::Normal);
        type_keys(&mut s, "V");
        paste(&mut s, "new", Placement::After);
        assert_eq!(s.to.as_str(), "new");
    }

    #[test]
    fn leaving_visual_or_switching_fields_cancels_selection() {
        let mut s = screen_with_to("abc");
        type_keys(&mut s, "vVv");
        assert!(matches!(
            s.header_mode,
            HeaderMode::Visual {
                linewise: false,
                ..
            }
        ));
        type_keys(&mut s, "v");
        assert_eq!(s.header_mode, HeaderMode::Normal);
        type_keys(&mut s, "v");
        handle_field_key(&mut s, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(s.header_mode, HeaderMode::Normal);
        type_keys(&mut s, "vi");
        s.focus_next();
        assert_eq!(s.header_mode, HeaderMode::Normal);
        assert!(s.header_pending.is_none());
        assert!(s.body.take_clipboard_yank().is_none());
    }

    #[test]
    fn empty_yank_is_safe_and_empty_line_yank_remains_linewise() {
        let mut s = screen_with_to("");
        type_keys(&mut s, "vy");
        assert!(s.body.take_clipboard_yank().is_none());
        type_keys(&mut s, "yy");
        assert_eq!(s.body.take_clipboard_yank().as_deref(), Some("\n"));
        type_keys(&mut s, "p");
        assert_eq!(s.to.as_str(), "");
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
    fn colon_passes_through_to_host_cmdline() {
        // `:` in a header field's Normal mode must reach the app-level
        // cmdline (so `:send` works from To/From/etc.), not be swallowed.
        let mut s = screen_with_to("alice");
        let out = handle_field_key(&mut s, key(':'));
        assert!(matches!(out, KeyOutcome::PassThrough));
        // The field text is untouched.
        assert_eq!(s.to.as_str(), "alice");
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
