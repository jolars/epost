//! Shared vim word-boundary scanner for the reader and the composer.
//!
//! Both surfaces have a `(row, col)` char-index cursor over a vector of
//! lines — the reader over `LaidOutBody.line_text` (wrapped display
//! lines), the composer over `tui_textarea`'s logical lines. The vim
//! word motions (`w W b B e E`) are pure functions of that line vector
//! and the cursor, so they live here once instead of being reinvented
//! per pane. [`word_motion`] returns the new `(row, col)`; the caller
//! applies it to its own cursor model.
//!
//! Boundary rules follow vim:
//! - A *small word* (`w`/`b`/`e`) is a run of keyword chars (alnum +
//!   `_`) OR a run of punctuation; whitespace and line breaks separate.
//!   The seam between a keyword run and a punctuation run (no space) is
//!   a boundary.
//! - A *WORD* (`W`/`B`/`E`, `big = true`) is a run of non-blank chars;
//!   only whitespace and line breaks separate.
//! - An empty line is a stop for `w`/`b` (you land on it) but is skipped
//!   by `e`, matching vim.
//!
//! A line break always separates words, so a word continued across two
//! (wrapped) display lines is treated as two words. That's the right
//! behaviour for the reader's display-line cursor and harmless for the
//! composer's logical lines.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordMotion {
    /// `w` / `W` — start of the next word.
    Forward,
    /// `b` / `B` — start of the current or previous word.
    Back,
    /// `e` / `E` — end of the next word.
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Blank,
    Word,
    Punct,
    /// A line with no characters. Its own class so it reads as a stop for
    /// `w`/`b` regardless of the neighbours.
    Empty,
}

fn classify(c: char) -> Class {
    if c.is_whitespace() {
        Class::Blank
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// Whether a seam between two *non-blank* classes `a → b` is a word
/// boundary. Under `big`, Word and Punct collapse to one non-blank class
/// so only whitespace separates.
fn nonblank_seam(a: Class, b: Class, big: bool) -> bool {
    !big && a != b
}

struct Pos {
    row: usize,
    col: usize,
    class: Class,
}

/// Flatten the line vector into an ordered position list, one entry per
/// char plus a single `Empty` entry per empty line.
fn flatten(lines: &[String]) -> Vec<Pos> {
    let mut out = Vec::new();
    for (row, line) in lines.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            out.push(Pos {
                row,
                col: 0,
                class: Class::Empty,
            });
        } else {
            for (col, &c) in chars.iter().enumerate() {
                out.push(Pos {
                    row,
                    col,
                    class: classify(c),
                });
            }
        }
    }
    out
}

fn is_word_start(p: &[Pos], j: usize, big: bool) -> bool {
    let cur = &p[j];
    match cur.class {
        Class::Blank => false,
        Class::Empty => true,
        _ => {
            if j == 0 {
                return true;
            }
            let prev = &p[j - 1];
            match prev.class {
                Class::Blank | Class::Empty => true,
                _ if prev.row != cur.row => true,
                _ => nonblank_seam(prev.class, cur.class, big),
            }
        }
    }
}

fn is_word_end(p: &[Pos], j: usize, big: bool) -> bool {
    let cur = &p[j];
    match cur.class {
        Class::Blank | Class::Empty => false,
        _ => {
            if j + 1 == p.len() {
                return true;
            }
            let next = &p[j + 1];
            match next.class {
                Class::Blank | Class::Empty => true,
                _ if next.row != cur.row => true,
                _ => nonblank_seam(cur.class, next.class, big),
            }
        }
    }
}

/// Index into the flattened list for the cursor `(row, col)`, clamping a
/// row past the end and a column past the line (the reader's `$` sentinel
/// arrives as `u16::MAX`).
fn resolve_index(p: &[Pos], lines: &[String], row: usize, col: usize) -> usize {
    let row = row.min(lines.len().saturating_sub(1));
    let line_len = lines[row].chars().count();
    let col = if line_len == 0 {
        0
    } else {
        col.min(line_len - 1)
    };
    p.iter()
        .position(|q| q.row == row && q.col == col)
        .unwrap_or(0)
}

/// Resolve a vim word motion to the destination `(row, col)`. Returns the
/// input unchanged when the buffer has no positions (truly empty input).
pub fn word_motion(
    lines: &[String],
    row: usize,
    col: usize,
    motion: WordMotion,
    big: bool,
) -> (usize, usize) {
    let positions = flatten(lines);
    if positions.is_empty() {
        return (row, col);
    }
    let start = resolve_index(&positions, lines, row, col);
    let last = positions.len() - 1;
    let target = match motion {
        WordMotion::Forward => (start + 1..positions.len())
            .find(|&j| is_word_start(&positions, j, big))
            // No further word start: clamp to the last position (the end
            // of the last word), matching vim `w` at buffer end.
            .unwrap_or(last),
        WordMotion::End => (start + 1..positions.len())
            .find(|&j| is_word_end(&positions, j, big))
            .unwrap_or(last),
        WordMotion::Back => (0..start)
            .rev()
            .find(|&j| is_word_start(&positions, j, big))
            // No previous word start: clamp to buffer start, matching
            // vim `b` at the first word.
            .unwrap_or(0),
    };
    (positions[target].row, positions[target].col)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(str::to_string).collect()
    }
    fn fwd(s: &str, r: usize, c: usize, big: bool) -> (usize, usize) {
        word_motion(&lines(s), r, c, WordMotion::Forward, big)
    }
    fn back(s: &str, r: usize, c: usize, big: bool) -> (usize, usize) {
        word_motion(&lines(s), r, c, WordMotion::Back, big)
    }
    fn end(s: &str, r: usize, c: usize, big: bool) -> (usize, usize) {
        word_motion(&lines(s), r, c, WordMotion::End, big)
    }

    #[test]
    fn small_word_forward_within_line() {
        // "hello world": w from h → w(orld); from w → clamp at last char.
        assert_eq!(fwd("hello world", 0, 0, false), (0, 6));
        assert_eq!(fwd("hello world", 0, 6, false), (0, 10));
    }

    #[test]
    fn small_word_splits_on_punctuation() {
        // "foo.bar baz": f→. , .→b(ar), bar→baz
        assert_eq!(fwd("foo.bar baz", 0, 0, false), (0, 3));
        assert_eq!(fwd("foo.bar baz", 0, 3, false), (0, 4));
        assert_eq!(fwd("foo.bar baz", 0, 4, false), (0, 8));
    }

    #[test]
    fn big_word_ignores_punctuation() {
        // WORD skips "foo.bar" as one unit → "baz".
        assert_eq!(fwd("foo.bar baz", 0, 0, true), (0, 8));
        assert_eq!(back("foo.bar baz", 0, 10, true), (0, 8));
        assert_eq!(back("foo.bar baz", 0, 8, true), (0, 0));
    }

    #[test]
    fn word_end_small() {
        // e from start of "foo.bar": foo→2, .→3, bar→6
        assert_eq!(end("foo.bar baz", 0, 0, false), (0, 2));
        assert_eq!(end("foo.bar baz", 0, 2, false), (0, 3));
        assert_eq!(end("foo.bar baz", 0, 3, false), (0, 6));
    }

    #[test]
    fn back_from_mid_word_goes_to_word_start() {
        assert_eq!(back("hello world", 0, 9, false), (0, 6));
        assert_eq!(back("hello world", 0, 6, false), (0, 0));
    }

    #[test]
    fn crosses_lines() {
        let s = "foo\nbar";
        assert_eq!(fwd(s, 0, 0, false), (1, 0));
        assert_eq!(back(s, 1, 0, false), (0, 0));
        // e from the 'o' at (0,2) lands on the end of "bar".
        assert_eq!(end(s, 0, 2, false), (1, 2));
    }

    #[test]
    fn empty_line_is_a_stop_for_w_and_b_but_not_e() {
        let s = "a\n\nb"; // ["a", "", "b"]
        // w lands on the empty line, then on 'b'.
        assert_eq!(fwd(s, 0, 0, false), (1, 0));
        assert_eq!(fwd(s, 1, 0, false), (2, 0));
        // b from 'b' lands on the empty line, then on 'a'.
        assert_eq!(back(s, 2, 0, false), (1, 0));
        assert_eq!(back(s, 1, 0, false), (0, 0));
        // e skips the empty line: from 'a' end → 'b' end.
        assert_eq!(end(s, 0, 0, false), (2, 0));
    }

    #[test]
    fn column_sentinel_clamps() {
        // Reader's `$` parks col at u16::MAX; back should still resolve.
        let huge = u16::MAX as usize;
        assert_eq!(back("hello", 0, huge, false), (0, 0));
        // Forward past the last word clamps to the last char.
        assert_eq!(fwd("hello", 0, huge, false), (0, 4));
    }

    #[test]
    fn empty_buffer_does_not_move() {
        assert_eq!(fwd("", 0, 0, false), (0, 0));
        assert_eq!(back("", 0, 0, false), (0, 0));
        assert_eq!(end("", 0, 0, false), (0, 0));
    }

    #[test]
    fn multibyte_counts_chars_not_bytes() {
        // "héllo wörld" — accented chars are single positions.
        assert_eq!(fwd("héllo wörld", 0, 0, false), (0, 6));
        assert_eq!(end("héllo wörld", 0, 0, false), (0, 4));
    }
}
