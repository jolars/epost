//! Single-line text input used by the cmdline and (in Step 8) the
//! compose tab's header fields. Tracks a byte-offset cursor that always
//! lands on a UTF-8 char boundary; arrow / home / end / backspace /
//! delete move and edit around it. Grapheme-cluster awareness is
//! deliberately out of scope for v1 — combining marks render slightly
//! oddly but don't crash.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Default, Clone)]
pub struct TextInput {
    buf: String,
    cursor: usize,
}

impl TextInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_string(s: impl Into<String>) -> Self {
        let buf: String = s.into();
        let cursor = buf.len();
        Self { buf, cursor }
    }

    pub fn as_str(&self) -> &str {
        &self.buf
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Byte offset of the cursor. Always sits on a UTF-8 char boundary
    /// so `buf.split_at(cursor)` is safe.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn insert_char(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn delete_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = prev_boundary(&self.buf, self.cursor);
        self.buf.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    pub fn delete_right(&mut self) {
        if self.cursor >= self.buf.len() {
            return;
        }
        let next = next_boundary(&self.buf, self.cursor);
        self.buf.replace_range(self.cursor..next, "");
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = prev_boundary(&self.buf, self.cursor);
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.buf.len() {
            self.cursor = next_boundary(&self.buf, self.cursor);
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buf.len();
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
    }

    /// Char index of the cursor (number of chars before it). The byte
    /// `cursor` is the readline-facing offset; this is what the vim word
    /// scanner ([`crate::ui::words`]) and motions want.
    pub fn cursor_char(&self) -> usize {
        self.buf[..self.cursor].chars().count()
    }

    /// Move the cursor to char index `ci`, clamped to end-of-buffer. The
    /// resulting byte offset always lands on a UTF-8 boundary.
    pub fn set_cursor_char(&mut self, ci: usize) {
        self.cursor = self
            .buf
            .char_indices()
            .nth(ci)
            .map(|(b, _)| b)
            .unwrap_or(self.buf.len());
    }

    /// The char the cursor currently sits on, or `None` at end-of-buffer.
    pub fn char_under_cursor(&self) -> Option<char> {
        self.buf[self.cursor..].chars().next()
    }

    /// vim Normal-mode clamp: the cursor sits *on* a char, never one past
    /// the last (an empty buffer keeps the cursor at 0). Insert mode skips
    /// this so appending at end-of-line still works.
    pub fn clamp_normal(&mut self) {
        if self.buf.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.buf.len() {
            self.cursor = prev_boundary(&self.buf, self.buf.len());
        }
    }

    /// Replace the char under the cursor with `c`, leaving the cursor on
    /// it (vim `r`). No-op at end-of-buffer.
    pub fn replace_char_under_cursor(&mut self, c: char) {
        if self.cursor >= self.buf.len() {
            return;
        }
        let next = next_boundary(&self.buf, self.cursor);
        let mut s = String::with_capacity(c.len_utf8());
        s.push(c);
        self.buf.replace_range(self.cursor..next, &s);
    }

    /// Readline `unix-word-rubout`: skip any trailing whitespace, then
    /// delete back through the run of non-whitespace under / before the
    /// cursor. Whitespace is the only delimiter — punctuation stays
    /// glued to the word, which is what you want on address rows
    /// (`Ctrl-W` over `"alice@example.com, bob@example.org"` kills the
    /// whole second address, not just `.org`).
    pub fn delete_word_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prefix = &self.buf[..self.cursor];
        let chars: Vec<(usize, char)> = prefix.char_indices().collect();
        let mut i = chars.len();
        while i > 0 && chars[i - 1].1.is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].1.is_whitespace() {
            i -= 1;
        }
        let new_cursor = if i == 0 { 0 } else { chars[i].0 };
        self.buf.replace_range(new_cursor..self.cursor, "");
        self.cursor = new_cursor;
    }

    /// Readline `unix-line-discard` (Ctrl-U): drop everything from the
    /// start of the line up to the cursor.
    pub fn delete_to_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.buf.replace_range(0..self.cursor, "");
        self.cursor = 0;
    }

    /// Readline `kill-line` (Ctrl-K): drop everything from the cursor
    /// to the end of the line.
    pub fn delete_to_end(&mut self) {
        if self.cursor >= self.buf.len() {
            return;
        }
        self.buf.truncate(self.cursor);
    }

    /// Take the buffer's contents, leaving the input empty.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.buf)
    }

    /// Replace `range` with `text` and move the cursor to `new_cursor`.
    /// Used by the address-completion popup to swap a partial token
    /// (`"ali"`) for the rendered contact (`"Alice <alice@…>, "`) in
    /// one shot, without dropping out of TextInput's cursor-boundary
    /// invariant. Caller must provide a `new_cursor` that lands on a
    /// UTF-8 boundary in the resulting buffer; address strings are
    /// ASCII so the popup never crosses that line in practice.
    pub fn replace_range(&mut self, range: std::ops::Range<usize>, text: &str, new_cursor: usize) {
        self.buf.replace_range(range, text);
        let n = self.buf.len();
        self.cursor = new_cursor.min(n);
        // Snap onto the nearest char boundary at-or-before, so callers
        // with off-by-one math don't trip the invariant.
        while self.cursor > 0 && !self.buf.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }

    /// Dispatch a key event to the standard set of editing operations.
    /// Returns true if the key was consumed. Modal callers should
    /// intercept Esc / Enter / mode-exit chords before calling.
    ///
    /// Recognised readline chords (no modes, no surprises):
    /// `Ctrl-A` head, `Ctrl-E` end, `Ctrl-B` left, `Ctrl-F` right,
    /// `Ctrl-W` delete-word-back, `Ctrl-U` delete-to-start,
    /// `Ctrl-K` delete-to-end, `Ctrl-H` backspace, `Ctrl-D` delete.
    /// Any other Ctrl/Alt chord is left for the caller to handle.
    pub fn handle(&mut self, k: KeyEvent) -> bool {
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            return match k.code {
                KeyCode::Char('a') => {
                    self.move_home();
                    true
                }
                KeyCode::Char('e') => {
                    self.move_end();
                    true
                }
                KeyCode::Char('b') => {
                    self.move_left();
                    true
                }
                KeyCode::Char('f') => {
                    self.move_right();
                    true
                }
                KeyCode::Char('w') => {
                    self.delete_word_left();
                    true
                }
                KeyCode::Char('u') => {
                    self.delete_to_start();
                    true
                }
                KeyCode::Char('k') => {
                    self.delete_to_end();
                    true
                }
                KeyCode::Char('h') => {
                    self.delete_left();
                    true
                }
                KeyCode::Char('d') => {
                    self.delete_right();
                    true
                }
                _ => false,
            };
        }
        match k.code {
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::ALT) => {
                self.insert_char(c);
                true
            }
            KeyCode::Backspace => {
                self.delete_left();
                true
            }
            KeyCode::Delete => {
                self.delete_right();
                true
            }
            KeyCode::Left => {
                self.move_left();
                true
            }
            KeyCode::Right => {
                self.move_right();
                true
            }
            KeyCode::Home => {
                self.move_home();
                true
            }
            KeyCode::End => {
                self.move_end();
                true
            }
            _ => false,
        }
    }
}

fn prev_boundary(s: &str, mut i: usize) -> usize {
    if i == 0 {
        return 0;
    }
    i -= 1;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_boundary(s: &str, mut i: usize) -> usize {
    let n = s.len();
    if i >= n {
        return n;
    }
    i += 1;
    while i < n && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_appends_and_moves_cursor() {
        let mut t = TextInput::new();
        t.insert_char('h');
        t.insert_char('i');
        assert_eq!(t.as_str(), "hi");
        assert_eq!(t.cursor(), 2);
    }

    #[test]
    fn delete_left_removes_prev_char() {
        let mut t = TextInput::new();
        t.insert_char('a');
        t.insert_char('b');
        t.delete_left();
        assert_eq!(t.as_str(), "a");
        assert_eq!(t.cursor(), 1);
    }

    #[test]
    fn replace_range_swaps_text_and_places_cursor() {
        let mut t = TextInput::from_string("ali");
        t.replace_range(0..3, "Alice <alice@example.com>", 25);
        assert_eq!(t.as_str(), "Alice <alice@example.com>");
        assert_eq!(t.cursor(), 25);
    }

    #[test]
    fn replace_range_in_middle_keeps_tail() {
        // "ali, bob@x" — replace the first three chars with the
        // canonical contact, cursor lands at the end of the insertion.
        let mut t = TextInput::from_string("ali, bob@x");
        t.replace_range(0..3, "Alice <alice@example.com>", 25);
        assert_eq!(t.as_str(), "Alice <alice@example.com>, bob@x");
        assert_eq!(t.cursor(), 25);
    }

    #[test]
    fn replace_range_snaps_cursor_to_char_boundary() {
        // Buffer ends in a multi-byte char; an out-of-range cursor
        // gets clamped, and onto a UTF-8 boundary.
        let mut t = TextInput::from_string("ali");
        t.replace_range(0..3, "héllo", 999);
        assert_eq!(t.as_str(), "héllo");
        let n = t.as_str().len();
        assert_eq!(t.cursor(), n);
        assert!(t.as_str().is_char_boundary(t.cursor()));
    }

    #[test]
    fn delete_left_at_start_is_noop() {
        let mut t = TextInput::new();
        t.delete_left();
        assert_eq!(t.as_str(), "");
        assert_eq!(t.cursor(), 0);
    }

    #[test]
    fn move_left_then_insert_inserts_at_cursor() {
        let mut t = TextInput::new();
        t.insert_char('a');
        t.insert_char('c');
        t.move_left();
        t.insert_char('b');
        assert_eq!(t.as_str(), "abc");
    }

    #[test]
    fn delete_right_at_end_is_noop() {
        let mut t = TextInput::new();
        t.insert_char('x');
        t.delete_right();
        assert_eq!(t.as_str(), "x");
    }

    #[test]
    fn home_and_end_jump_to_extremes() {
        let mut t = TextInput::new();
        for c in "hello".chars() {
            t.insert_char(c);
        }
        t.move_home();
        assert_eq!(t.cursor(), 0);
        t.move_end();
        assert_eq!(t.cursor(), 5);
    }

    #[test]
    fn cursor_respects_utf8_boundaries() {
        let mut t = TextInput::new();
        t.insert_char('é'); // 2 bytes
        assert_eq!(t.cursor(), 2);
        t.move_left();
        assert_eq!(t.cursor(), 0);
        t.move_right();
        assert_eq!(t.cursor(), 2);
        t.delete_left();
        assert_eq!(t.as_str(), "");
    }

    #[test]
    fn take_resets_buffer_and_cursor() {
        let mut t = TextInput::new();
        t.insert_char('a');
        let s = t.take();
        assert_eq!(s, "a");
        assert!(t.is_empty());
        assert_eq!(t.cursor(), 0);
    }

    fn populated(s: &str) -> TextInput {
        let mut t = TextInput::from_string(s);
        t.move_end();
        t
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_w_deletes_word_back_treating_address_as_one_word() {
        let mut t = populated("alice@example.com, bob@example.org");
        assert!(t.handle(ctrl('w')));
        assert_eq!(t.as_str(), "alice@example.com, ");
        // Second Ctrl-W eats the comma+space and then `alice@example.com`.
        assert!(t.handle(ctrl('w')));
        assert_eq!(t.as_str(), "");
    }

    #[test]
    fn ctrl_u_drops_to_start_only_before_cursor() {
        let mut t = TextInput::from_string("hello world");
        // Park cursor in the middle ("hello "|"world").
        t.move_home();
        for _ in 0..6 {
            t.move_right();
        }
        assert!(t.handle(ctrl('u')));
        assert_eq!(t.as_str(), "world");
        assert_eq!(t.cursor(), 0);
    }

    #[test]
    fn ctrl_k_kills_to_end_from_cursor() {
        let mut t = TextInput::from_string("hello world");
        t.move_home();
        for _ in 0..5 {
            t.move_right();
        }
        assert!(t.handle(ctrl('k')));
        assert_eq!(t.as_str(), "hello");
        assert_eq!(t.cursor(), 5);
    }

    #[test]
    fn ctrl_a_and_ctrl_e_jump_to_extremes() {
        let mut t = TextInput::from_string("xyz");
        t.move_home();
        assert!(t.handle(ctrl('e')));
        assert_eq!(t.cursor(), 3);
        assert!(t.handle(ctrl('a')));
        assert_eq!(t.cursor(), 0);
    }

    #[test]
    fn ctrl_b_and_ctrl_f_step_one_char() {
        let mut t = populated("ab");
        assert!(t.handle(ctrl('b')));
        assert_eq!(t.cursor(), 1);
        assert!(t.handle(ctrl('f')));
        assert_eq!(t.cursor(), 2);
    }

    #[test]
    fn ctrl_h_is_backspace_and_ctrl_d_is_delete() {
        let mut t = populated("abc");
        assert!(t.handle(ctrl('h')));
        assert_eq!(t.as_str(), "ab");
        t.move_home();
        assert!(t.handle(ctrl('d')));
        assert_eq!(t.as_str(), "b");
    }

    #[test]
    fn unhandled_ctrl_chord_returns_false_so_caller_can_route() {
        let mut t = populated("abc");
        assert!(!t.handle(ctrl('z')));
        assert_eq!(t.as_str(), "abc");
    }
}
