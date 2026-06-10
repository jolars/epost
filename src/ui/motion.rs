//! Shared vim-motion vocabulary for the reader and composer.
//!
//! Both panes accept the same `hjkl / w / b / e / 0 / $ / ^ / gg / G /
//! Ctrl-d / Ctrl-u` keys; before this module each side dispatched its
//! own per-key arms against its own cursor model, which let the keymaps
//! drift apart. Now both call [`key_to_motion`] to translate a key into
//! a [`Motion`], then [`apply`] to dispatch it through the
//! [`MotionTarget`] trait — each side implements the primitives
//! against its native buffer.
//!
//! The trait is "hook per motion" rather than "primitives + generic
//! algorithm" because the reader and composer have fundamentally
//! different buffers (HTML Block-IR vs `tui_textarea::TextArea`) and
//! reusing the native engines' word walkers / clamps is more honest
//! than hand-rolling a third one. Word motions default to no-op so the
//! reader, which has no notion of a "word" at keymap time, doesn't
//! pretend to support them.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::words::{self, WordMotion};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    CharLeft,
    CharRight,
    CharUp,
    CharDown,
    WordForward,
    WordBack,
    WordEnd,
    WordForwardBig,
    WordBackBig,
    WordEndBig,
    /// `ge` / `gE` — end of the previous word.
    WordEndBack,
    WordEndBackBig,
    LineStart,
    /// `^` — first non-blank char on the line.
    FirstNonBlank,
    LineEnd,
    FirstLine,
    LastLine,
    HalfPageDown,
    HalfPageUp,
}

/// How a resolved motion's `[start, end]` endpoints translate into an
/// operated region (vim's "inclusive / exclusive / linewise" motion
/// classes). Used by the operator-pending engine, not by plain cursor
/// movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionKind {
    /// `$ e f` — the cell under `end` is included.
    CharInclusive,
    /// `h l w b 0 ^` — the cell under `end` is the first *not* operated.
    CharExclusive,
    /// `j k G gg` — whole lines from `start.0` to `end.0`.
    Linewise,
}

/// A motion resolved against a buffer: the cursor it started from, the
/// target it reached, and the [`MotionKind`] that says how to turn the
/// pair into an operated region. `start`/`end` are `(row, col)` char
/// coords and may be in either order (the operator normalises them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotionSpan {
    pub start: (usize, usize),
    pub end: (usize, usize),
    pub kind: MotionKind,
}

/// A normalised region an operator acts on: `start <= end` in `(row,
/// col)` order. When `linewise`, `start.1`/`end.1` are 0 and the region
/// covers whole rows `start.0..=end.0`; otherwise it's the half-open
/// char range `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub start: (usize, usize),
    pub end: (usize, usize),
    pub linewise: bool,
}

/// Vim-flavoured motion sink. Implementers translate each motion into
/// their native cursor model. `apply` is just dispatch; the semantics
/// live in the impl. Word motions and `move_half_page` default to
/// no-ops — pane that don't track text or viewport height can skip
/// them without affecting the other motions.
pub trait MotionTarget {
    fn move_char_left(&mut self);
    fn move_char_right(&mut self);
    fn move_char_up(&mut self);
    fn move_char_down(&mut self);
    fn move_line_start(&mut self);
    fn move_line_end(&mut self);
    fn move_first_line(&mut self);
    fn move_last_line(&mut self);
    fn move_word_forward(&mut self) {}
    fn move_word_back(&mut self) {}
    fn move_word_end(&mut self) {}
    fn move_word_forward_big(&mut self) {}
    fn move_word_back_big(&mut self) {}
    fn move_word_end_big(&mut self) {}
    fn move_word_end_back(&mut self) {}
    fn move_word_end_back_big(&mut self) {}
    /// `^`. Defaults to line-start for panes that don't track leading
    /// whitespace (the reader); the composer overrides it.
    fn move_first_non_blank(&mut self) {
        self.move_line_start();
    }
    fn move_half_page(&mut self, _down: bool) {}
}

/// Dispatch a motion against a target. Pure dispatch — no clamping or
/// follow-cursor logic lives here; that's per-impl.
pub fn apply<T: MotionTarget>(t: &mut T, m: Motion) {
    match m {
        Motion::CharLeft => t.move_char_left(),
        Motion::CharRight => t.move_char_right(),
        Motion::CharUp => t.move_char_up(),
        Motion::CharDown => t.move_char_down(),
        Motion::WordForward => t.move_word_forward(),
        Motion::WordBack => t.move_word_back(),
        Motion::WordEnd => t.move_word_end(),
        Motion::WordForwardBig => t.move_word_forward_big(),
        Motion::WordBackBig => t.move_word_back_big(),
        Motion::WordEndBig => t.move_word_end_big(),
        Motion::WordEndBack => t.move_word_end_back(),
        Motion::WordEndBackBig => t.move_word_end_back_big(),
        Motion::LineStart => t.move_line_start(),
        Motion::FirstNonBlank => t.move_first_non_blank(),
        Motion::LineEnd => t.move_line_end(),
        Motion::FirstLine => t.move_first_line(),
        Motion::LastLine => t.move_last_line(),
        Motion::HalfPageDown => t.move_half_page(true),
        Motion::HalfPageUp => t.move_half_page(false),
    }
}

/// Translate a key event to a [`Motion`] from the shared vocabulary.
///
/// `gg` is NOT handled here — the caller owns the latch (composer has
/// its own `Pending::G` for visual-mode chord handling, reader uses
/// `App.pending_g`). When the caller's latch is armed and sees another
/// `g`, it dispatches [`Motion::FirstLine`] directly. This keeps the
/// translator stateless.
///
/// Returns `None` for non-motion keys (insert entry, edits, mode
/// switches, etc.) — the caller is responsible for those.
pub fn key_to_motion(k: KeyEvent) -> Option<Motion> {
    // Ctrl-d / Ctrl-u are the only Ctrl-modified motions. Other Ctrl
    // chords are non-motion (Ctrl-r redo, Ctrl-c quit, etc.) — the
    // caller handles them before falling through to here.
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        return match k.code {
            KeyCode::Char('d') => Some(Motion::HalfPageDown),
            KeyCode::Char('u') => Some(Motion::HalfPageUp),
            _ => None,
        };
    }
    // Alt / other non-shift modifiers aren't ours. SHIFT alone is fine
    // — KeyCode::Char('G') / Char('$') arrive with SHIFT in some
    // terminals and as raw chars in others.
    if k.modifiers.intersects(KeyModifiers::ALT) {
        return None;
    }
    match k.code {
        KeyCode::Char('h') | KeyCode::Left => Some(Motion::CharLeft),
        KeyCode::Char('l') | KeyCode::Right => Some(Motion::CharRight),
        KeyCode::Char('j') | KeyCode::Down => Some(Motion::CharDown),
        KeyCode::Char('k') | KeyCode::Up => Some(Motion::CharUp),
        KeyCode::Char('w') => Some(Motion::WordForward),
        KeyCode::Char('b') => Some(Motion::WordBack),
        KeyCode::Char('e') => Some(Motion::WordEnd),
        KeyCode::Char('W') => Some(Motion::WordForwardBig),
        KeyCode::Char('B') => Some(Motion::WordBackBig),
        KeyCode::Char('E') => Some(Motion::WordEndBig),
        KeyCode::Char('0') | KeyCode::Home => Some(Motion::LineStart),
        KeyCode::Char('$') | KeyCode::End => Some(Motion::LineEnd),
        KeyCode::Char('^') => Some(Motion::FirstNonBlank),
        KeyCode::Char('G') => Some(Motion::LastLine),
        _ => None,
    }
}

/// Half-page step for `Ctrl-d`/`Ctrl-u` resolved as operator targets.
/// Matches the composer's plain-movement `half_page()` (fixed 8 — no
/// per-render viewport tracking in the pure resolver).
const HALF_PAGE: usize = 8;

fn line_len(lines: &[String], row: usize) -> usize {
    lines.get(row).map(|l| l.chars().count()).unwrap_or(0)
}

fn first_non_blank(lines: &[String], row: usize) -> usize {
    let Some(line) = lines.get(row) else { return 0 };
    line.chars()
        .position(|c| !c.is_whitespace())
        .unwrap_or_else(|| line.chars().count().saturating_sub(1))
}

/// Resolve a motion to a [`MotionSpan`] *without* moving any cursor —
/// the operator-pending path needs the endpoints, not a side effect.
/// Pure over `(lines, cursor)` so it unit-tests without a textarea, and
/// shared by the composer (operators) and the reader (operator-yanks).
///
/// Operator-specific quirks are baked in here because this function is
/// only ever called for an operator: `w`/`W` clamp to end-of-line when
/// they would cross into the next line (vim's "last word on the line"
/// rule), and `G`/`gg` ignore the count (the dispatch passes the doubled
/// count, which would otherwise misread `dG` as a line number).
pub fn resolve_motion_span(
    lines: &[String],
    cursor: (usize, usize),
    motion: Motion,
    count: usize,
) -> Option<MotionSpan> {
    use Motion::*;
    use MotionKind::*;
    let count = count.max(1);
    let (row, col) = cursor;
    let last_row = lines.len().saturating_sub(1);
    let span = |end, kind| {
        Some(MotionSpan {
            start: cursor,
            end,
            kind,
        })
    };
    let word = |m: WordMotion, big: bool| {
        let mut pos = cursor;
        for _ in 0..count {
            pos = words::word_motion(lines, pos.0, pos.1, m, big);
        }
        pos
    };
    match motion {
        CharLeft => span((row, col.saturating_sub(count)), CharExclusive),
        CharRight => span(
            (row, (col + count).min(line_len(lines, row))),
            CharExclusive,
        ),
        CharDown => span(((row + count).min(last_row), col), Linewise),
        CharUp => span((row.saturating_sub(count), col), Linewise),
        HalfPageDown => span(((row + HALF_PAGE * count).min(last_row), col), Linewise),
        HalfPageUp => span((row.saturating_sub(HALF_PAGE * count), col), Linewise),
        LineStart => span((row, 0), CharExclusive),
        FirstNonBlank => span((row, first_non_blank(lines, row)), CharExclusive),
        LineEnd => {
            let end_row = (row + count - 1).min(last_row);
            span(
                (end_row, line_len(lines, end_row).saturating_sub(1)),
                CharInclusive,
            )
        }
        FirstLine => span((0, 0), Linewise),
        LastLine => span((last_row, 0), Linewise),
        WordForward | WordForwardBig => {
            let big = matches!(motion, WordForwardBig);
            let pos = word(WordMotion::Forward, big);
            // Vim: an operator + `w` that would step onto a new line
            // stops at the end of the current line instead.
            if pos.0 > row {
                span((row, line_len(lines, row)), CharExclusive)
            } else {
                span(pos, CharExclusive)
            }
        }
        WordBack | WordBackBig => span(
            word(WordMotion::Back, matches!(motion, WordBackBig)),
            CharExclusive,
        ),
        WordEnd | WordEndBig => span(
            word(WordMotion::End, matches!(motion, WordEndBig)),
            CharInclusive,
        ),
        WordEndBack | WordEndBackBig => span(
            word(WordMotion::EndBack, matches!(motion, WordEndBackBig)),
            CharInclusive,
        ),
    }
}

/// Normalise a [`MotionSpan`] into an ordered [`Region`]. Inclusive char
/// motions get `+1` on the high column (clamped to the line length so
/// `d$` / `de` stop before the trailing newline). Linewise spans collapse
/// to whole-row coverage.
pub fn span_to_region(lines: &[String], span: MotionSpan) -> Region {
    let order = |a: (usize, usize), b: (usize, usize)| if a <= b { (a, b) } else { (b, a) };
    match span.kind {
        MotionKind::Linewise => {
            let (top, bot) = (span.start.0.min(span.end.0), span.start.0.max(span.end.0));
            Region {
                start: (top, 0),
                end: (bot, 0),
                linewise: true,
            }
        }
        MotionKind::CharExclusive => {
            let (lo, hi) = order(span.start, span.end);
            Region {
                start: lo,
                end: hi,
                linewise: false,
            }
        }
        MotionKind::CharInclusive => {
            let (lo, mut hi) = order(span.start, span.end);
            hi.1 = (hi.1 + 1).min(line_len(lines, hi.0));
            Region {
                start: lo,
                end: hi,
                linewise: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    #[test]
    fn motions_map_from_keys() {
        assert_eq!(key_to_motion(key('h')), Some(Motion::CharLeft));
        assert_eq!(key_to_motion(key('j')), Some(Motion::CharDown));
        assert_eq!(key_to_motion(key('k')), Some(Motion::CharUp));
        assert_eq!(key_to_motion(key('l')), Some(Motion::CharRight));
        assert_eq!(key_to_motion(key('w')), Some(Motion::WordForward));
        assert_eq!(key_to_motion(key('b')), Some(Motion::WordBack));
        assert_eq!(key_to_motion(key('e')), Some(Motion::WordEnd));
        assert_eq!(key_to_motion(key('W')), Some(Motion::WordForwardBig));
        assert_eq!(key_to_motion(key('B')), Some(Motion::WordBackBig));
        assert_eq!(key_to_motion(key('E')), Some(Motion::WordEndBig));
        assert_eq!(key_to_motion(key('0')), Some(Motion::LineStart));
        assert_eq!(key_to_motion(key('$')), Some(Motion::LineEnd));
        assert_eq!(key_to_motion(key('^')), Some(Motion::FirstNonBlank));
        assert_eq!(key_to_motion(key('G')), Some(Motion::LastLine));
        assert_eq!(key_to_motion(ctrl('d')), Some(Motion::HalfPageDown));
        assert_eq!(key_to_motion(ctrl('u')), Some(Motion::HalfPageUp));
    }

    #[test]
    fn non_motion_keys_return_none() {
        assert_eq!(key_to_motion(key('i')), None);
        assert_eq!(key_to_motion(key('a')), None);
        assert_eq!(key_to_motion(key('y')), None);
        assert_eq!(key_to_motion(key('g')), None); // caller's latch handles gg
        assert_eq!(key_to_motion(ctrl('r')), None);
        assert_eq!(key_to_motion(ctrl('c')), None);
        assert_eq!(key_to_motion(alt('j')), None);
    }

    // A tiny in-memory MotionTarget for unit-testing `apply`. Tracks
    // primitive calls so we can assert each Motion routes correctly.
    #[derive(Default)]
    struct Spy {
        calls: Vec<&'static str>,
    }
    impl MotionTarget for Spy {
        fn move_char_left(&mut self) {
            self.calls.push("char_left");
        }
        fn move_char_right(&mut self) {
            self.calls.push("char_right");
        }
        fn move_char_up(&mut self) {
            self.calls.push("char_up");
        }
        fn move_char_down(&mut self) {
            self.calls.push("char_down");
        }
        fn move_line_start(&mut self) {
            self.calls.push("line_start");
        }
        fn move_line_end(&mut self) {
            self.calls.push("line_end");
        }
        fn move_first_line(&mut self) {
            self.calls.push("first_line");
        }
        fn move_last_line(&mut self) {
            self.calls.push("last_line");
        }
        fn move_word_forward(&mut self) {
            self.calls.push("word_forward");
        }
        fn move_word_back(&mut self) {
            self.calls.push("word_back");
        }
        fn move_word_end(&mut self) {
            self.calls.push("word_end");
        }
        fn move_word_forward_big(&mut self) {
            self.calls.push("word_forward_big");
        }
        fn move_word_back_big(&mut self) {
            self.calls.push("word_back_big");
        }
        fn move_word_end_big(&mut self) {
            self.calls.push("word_end_big");
        }
        fn move_half_page(&mut self, down: bool) {
            self.calls.push(if down { "half_down" } else { "half_up" });
        }
    }

    #[test]
    fn apply_dispatches_every_motion() {
        let cases = [
            (Motion::CharLeft, "char_left"),
            (Motion::CharRight, "char_right"),
            (Motion::CharUp, "char_up"),
            (Motion::CharDown, "char_down"),
            (Motion::WordForward, "word_forward"),
            (Motion::WordBack, "word_back"),
            (Motion::WordEnd, "word_end"),
            (Motion::WordForwardBig, "word_forward_big"),
            (Motion::WordBackBig, "word_back_big"),
            (Motion::WordEndBig, "word_end_big"),
            (Motion::LineStart, "line_start"),
            (Motion::LineEnd, "line_end"),
            (Motion::FirstLine, "first_line"),
            (Motion::LastLine, "last_line"),
            (Motion::HalfPageDown, "half_down"),
            (Motion::HalfPageUp, "half_up"),
        ];
        for (motion, expected) in cases {
            let mut s = Spy::default();
            apply(&mut s, motion);
            assert_eq!(s.calls, vec![expected], "motion {motion:?}");
        }
    }
}
