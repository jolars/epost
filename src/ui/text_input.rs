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

    /// Take the buffer's contents, leaving the input empty.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.buf)
    }

    /// Dispatch a key event to the standard set of editing operations.
    /// Returns true if the key was consumed. Modal callers should
    /// intercept Esc / Enter / mode-exit chords before calling.
    pub fn handle(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Char(c)
                if !k
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
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
}
