//! Inline address-completion popup for the compose tab's To / Cc / Bcc
//! fields. The popup lives on `ComposeScreen` (per-tab state); the
//! native cache + debounce + external query receiver live on `App` (one
//! pipeline at a time across the whole session).
//!
//! Visual grammar mirrors `draw_from_picker` (yellow border, `Clear`'d
//! background) so the user reads them as the same class of overlay.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::mail::addressbook::Contact;
use crate::ui::compose::ComposeField;
use crate::ui::text_input::TextInput;

/// Maximum items rendered in the popup at once. Beyond this the user
/// should keep typing to narrow the list.
pub const MAX_ITEMS: usize = 8;

/// Maximum overlay width in cells. Wider than the From picker because
/// `Name <email>` rows are longer than `account — from`.
pub const MAX_WIDTH: u16 = 60;

/// State for an open address-completion popup. Pinned to one field
/// (`field`); the host closes the popup before re-opening on a
/// different field so the byte-offset bookkeeping stays simple.
#[derive(Debug)]
pub struct AddressCompleteState {
    pub field: ComposeField,
    /// Byte offset of the trimmed token within the field's TextInput
    /// buffer. The accept path replaces `[token_start..cursor]` with
    /// the chosen contact's `Name <email>`.
    pub token_start: usize,
    /// Lowercased prefix the popup is currently matching against.
    /// `merge_into` reads this so the merge filter and the popup
    /// display stay in sync even if the cursor moves between the
    /// query firing and the result landing.
    pub token: String,
    /// Merged candidate list: external first, then native, deduped by
    /// lowercase email. The accept path indexes into this with `selected`.
    pub items: Vec<Contact>,
    pub selected: usize,
}

/// Reason `handle_key` returns "not my key" — the host falls through
/// to the regular TextInput dispatch (and then re-queries the popup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyDispatch {
    /// Popup consumed the key (navigation, Esc, Enter).
    Consumed,
    /// Key wasn't ours; host should pass it through to the TextInput
    /// for normal editing, then refresh the popup.
    PassThrough,
    /// User accepted the selection. Host should call `accept_into` to
    /// rewrite the TextInput and then drop the popup state.
    Accept,
}

/// Inspect a header field's current TextInput state and return the
/// byte-range of the token under the cursor. A token starts after the
/// most recent `,` before the cursor (skipping whitespace) and runs to
/// the cursor itself. Returns `None` when the cursor is *inside*
/// already-typed angle-bracketed text (`Name <al|ice@…>` shouldn't
/// re-open the popup) or when the segment is empty.
pub fn extract_token(input: &TextInput) -> Option<(usize, String)> {
    let buf = input.as_str();
    let cursor = input.cursor();
    let before = &buf[..cursor];
    // Most-recent comma scanned forward gives the segment-start byte.
    let seg_start = before.rfind(',').map(|i| i + 1).unwrap_or(0);
    let segment = &before[seg_start..];
    // Skip leading whitespace within the segment.
    let lead_ws = segment.len() - segment.trim_start().len();
    let token_start = seg_start + lead_ws;
    let token = &before[token_start..];
    // Don't pop the picker while the user is mid-angle-bracket — that
    // means they've already accepted a contact and are tweaking the
    // local-part by hand, and a popup overlay there would obscure the
    // edit point.
    if token.contains('<') && !token.contains('>') {
        return None;
    }
    if token.is_empty() {
        return None;
    }
    Some((token_start, token.to_string()))
}

/// Replace the popup's token in `input` with the selected contact's
/// rendered address, position the cursor at the end of the insertion,
/// and append `", "` when the user was at end-of-buffer so the next
/// recipient flows naturally.
pub fn accept_into(input: &mut TextInput, state: &AddressCompleteState) -> bool {
    let Some(contact) = state.items.get(state.selected) else {
        return false;
    };
    let buf = input.as_str();
    let cursor = input.cursor();
    if state.token_start > buf.len() || cursor > buf.len() || state.token_start > cursor {
        return false;
    }
    let at_end = cursor == buf.len();
    let rendered = contact.render_address();
    let replacement = if at_end {
        format!("{rendered}, ")
    } else {
        rendered
    };
    let new_cursor = state.token_start + replacement.len();
    input.replace_range(state.token_start..cursor, &replacement, new_cursor);
    true
}

/// Move selection by `delta`, wrapping at both ends.
pub fn move_selection(state: &mut AddressCompleteState, delta: i32) {
    if state.items.is_empty() {
        state.selected = 0;
        return;
    }
    let len = state.items.len() as i32;
    let mut next = state.selected as i32 + delta;
    while next < 0 {
        next += len;
    }
    state.selected = (next % len) as usize;
}

/// Handle a key event when the popup is open. Returns `Consumed` for
/// navigation/dismiss keys, `Accept` when the user picked a contact, and
/// `PassThrough` for keys the popup doesn't bind (the host then routes
/// them to the underlying TextInput and re-queries).
pub fn handle_key(state: &mut AddressCompleteState, k: KeyEvent) -> KeyDispatch {
    // Esc is always "close popup, don't leave compose mode."
    if k.code == KeyCode::Esc {
        return KeyDispatch::Consumed;
    }
    // Down / Up / Ctrl-n / Ctrl-p navigate. With empty items the keys
    // are still consumed so a stray Up doesn't surprise the user by
    // moving the TextInput cursor.
    match (k.code, k.modifiers) {
        (KeyCode::Down, _) => {
            move_selection(state, 1);
            return KeyDispatch::Consumed;
        }
        (KeyCode::Up, _) => {
            move_selection(state, -1);
            return KeyDispatch::Consumed;
        }
        (KeyCode::Char('n'), m) if m.contains(KeyModifiers::CONTROL) => {
            move_selection(state, 1);
            return KeyDispatch::Consumed;
        }
        (KeyCode::Char('p'), m) if m.contains(KeyModifiers::CONTROL) => {
            move_selection(state, -1);
            return KeyDispatch::Consumed;
        }
        _ => {}
    }
    // Tab / Enter accept when there's something to accept; with an
    // empty list Tab falls through so the form's tab-cycle still works.
    if state.items.is_empty() {
        return KeyDispatch::PassThrough;
    }
    match k.code {
        KeyCode::Enter | KeyCode::Tab => KeyDispatch::Accept,
        _ => KeyDispatch::PassThrough,
    }
}

/// Render the popup anchored beneath the field row. `anchor` is the
/// row the field is rendered on (a single-line `Rect`); the popup hangs
/// off `anchor.y + 1` and aligns with the field-content column (we
/// indent past the label, same trick as `draw_from_picker`).
pub fn draw(f: &mut Frame, anchor: Rect, state: &AddressCompleteState, bounds: Rect) {
    if state.items.is_empty() {
        return;
    }
    // Field label widths from compose.rs: "From:    ", "To:      ",
    // "Cc:      ", "Bcc:     ", "Subject: " — all 9 cells.
    const LABEL_WIDTH: u16 = 9;

    let visible = state.items.len().min(MAX_ITEMS);
    let max_render_width = state
        .items
        .iter()
        .take(visible)
        .map(|c| display_width(c) as u16)
        .max()
        .unwrap_or(20);
    // 2 cells L/R border + 2 cells inner padding.
    let want_width = max_render_width.saturating_add(4).max(24);

    let anchor_x = anchor.x.saturating_add(LABEL_WIDTH);
    let available_width = bounds.right().saturating_sub(anchor_x).min(bounds.width);
    let width = want_width.min(available_width).clamp(12, MAX_WIDTH);
    let x = anchor_x.min(bounds.right().saturating_sub(width));
    let y = anchor.y.saturating_add(1);
    let max_height = bounds.bottom().saturating_sub(y);
    let height = (visible as u16).saturating_add(2).min(max_height).max(3);
    let area = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, area);
    let title = format!(" Complete — {} ", field_label(state.field));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Name column width: at most half the inner width, but never less
    // than 12 cells so short names don't crowd the email column.
    let name_width = (inner.width.saturating_sub(3) / 2).max(12).min(inner.width);

    let lines: Vec<Line<'static>> = state
        .items
        .iter()
        .take(visible)
        .enumerate()
        .map(|(i, c)| {
            let selected = i == state.selected;
            let marker = if selected { "▶ " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let name = c.name.clone().unwrap_or_default();
            let name = pad_truncate(&name, name_width as usize);
            let email_style = if selected {
                style
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(vec![
                Span::styled(marker.to_string(), style),
                Span::styled(name, style),
                Span::raw("  "),
                Span::styled(c.email.clone(), email_style),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn field_label(field: ComposeField) -> &'static str {
    match field {
        ComposeField::To => "To",
        ComposeField::Cc => "Cc",
        ComposeField::Bcc => "Bcc",
        // The popup only opens on recipient fields, but match
        // exhaustively so adding fields doesn't silently regress.
        ComposeField::From | ComposeField::Subject | ComposeField::Attach | ComposeField::Body => {
            "?"
        }
    }
}

fn display_width(c: &Contact) -> usize {
    // Approximate cell width as char count — same approximation the
    // rest of the TUI uses. Combining marks etc. drift a little but
    // the popup is cosmetic.
    let name = c.name.as_deref().unwrap_or("");
    name.chars().count() + 2 + c.email.chars().count()
}

fn pad_truncate(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    for ch in s.chars() {
        if count + 1 > width {
            break;
        }
        out.push(ch);
        count += 1;
    }
    while count < width {
        out.push(' ');
        count += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::addressbook::{Contact, Source};

    fn input(s: &str, cursor: usize) -> TextInput {
        let mut t = TextInput::from_string(s);
        // TextInput::from_string puts the cursor at end; nudge it.
        while t.cursor() > cursor {
            t.move_left();
        }
        t
    }

    #[test]
    fn extracts_token_from_simple_prefix() {
        let t = input("ali", 3);
        let (start, tok) = extract_token(&t).unwrap();
        assert_eq!(start, 0);
        assert_eq!(tok, "ali");
    }

    #[test]
    fn extracts_token_after_comma_and_whitespace() {
        let s = "alice@x, bo";
        let t = input(s, s.len());
        let (start, tok) = extract_token(&t).unwrap();
        // Token "bo" begins after ", ".
        assert_eq!(&s[start..s.len()], "bo");
        assert_eq!(tok, "bo");
    }

    #[test]
    fn no_token_when_cursor_in_angle_brackets() {
        // User is editing already-accepted Name <foo@x> by hand. The
        // popup would obscure the edit, so suppress.
        let s = "Alice <al";
        let t = input(s, s.len());
        assert!(extract_token(&t).is_none());
    }

    #[test]
    fn no_token_when_segment_empty() {
        let s = "alice@x, ";
        let t = input(s, s.len());
        assert!(extract_token(&t).is_none());
    }

    #[test]
    fn accept_replaces_token_and_appends_separator_at_end() {
        let mut t = input("ali", 3);
        let state = AddressCompleteState {
            field: ComposeField::To,
            token_start: 0,
            token: "ali".into(),
            items: vec![Contact {
                name: Some("Alice".into()),
                email: "alice@example.com".into(),
                email_lc: "alice@example.com".into(),
                source: Source::Native,
            }],
            selected: 0,
        };
        assert!(accept_into(&mut t, &state));
        assert_eq!(t.as_str(), "Alice <alice@example.com>, ");
        assert_eq!(t.cursor(), t.as_str().len());
    }

    #[test]
    fn accept_in_middle_does_not_append_separator() {
        let s = "ali, bob@x";
        let mut t = input(s, 3);
        let state = AddressCompleteState {
            field: ComposeField::To,
            token_start: 0,
            token: "ali".into(),
            items: vec![Contact {
                name: Some("Alice".into()),
                email: "alice@example.com".into(),
                email_lc: "alice@example.com".into(),
                source: Source::Native,
            }],
            selected: 0,
        };
        assert!(accept_into(&mut t, &state));
        assert_eq!(t.as_str(), "Alice <alice@example.com>, bob@x");
    }

    #[test]
    fn move_selection_wraps_both_ways() {
        let mut state = AddressCompleteState {
            field: ComposeField::To,
            token_start: 0,
            token: String::new(),
            items: vec![dummy_contact("a"), dummy_contact("b"), dummy_contact("c")],
            selected: 0,
        };
        move_selection(&mut state, -1);
        assert_eq!(state.selected, 2);
        move_selection(&mut state, 1);
        assert_eq!(state.selected, 0);
        move_selection(&mut state, 5);
        assert_eq!(state.selected, 2);
    }

    fn dummy_contact(name: &str) -> Contact {
        Contact {
            name: Some(name.into()),
            email: format!("{name}@x"),
            email_lc: format!("{name}@x"),
            source: Source::Native,
        }
    }
}
