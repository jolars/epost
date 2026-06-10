//! Vim text objects (`iw aw i" a" i( a( ip ap` …) as pure resolvers.
//!
//! Mirrors [`words`](crate::ui::words): a text object is a pure function
//! of the line vector and the cursor, returning a
//! [`MotionSpan`](crate::ui::motion::MotionSpan) the operator engine
//! turns into an operated region — so the composer (operators) and the
//! reader (operator-yanks) share one definition and it unit-tests
//! without a textarea.
//!
//! Scope: word (`iw`/`aw`, small + WORD), quote (`i"` `i'` `` i` ``),
//! bracket pair (`()` `{}` `[]` `<>`, with `b`/`B` aliases), and
//! paragraph (`ip`/`ap`). Tag objects (`it`/`at`) are intentionally
//! omitted — prose email bodies aren't markup.

use crate::ui::motion::{MotionKind, MotionSpan};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextObjKind {
    Inner,
    Around,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextObj {
    /// `iw`/`aw` (`big = false`) or `iW`/`aW` (`big = true`).
    Word(bool),
    /// `i"` `a"` / `i'` / `` i` `` — the quote char.
    Quote(char),
    /// `i(` `a{` … — the open/close pair.
    Pair(char, char),
    /// `ip`/`ap`.
    Paragraph,
}

/// Translate the char after `i`/`a` into a [`TextObj`]. Bracket aliases
/// follow vim: `b` → `()`, `B` → `{}`; either bracket of a pair selects
/// the pair.
pub fn key_to_text_object(c: char) -> Option<TextObj> {
    Some(match c {
        'w' => TextObj::Word(false),
        'W' => TextObj::Word(true),
        '"' => TextObj::Quote('"'),
        '\'' => TextObj::Quote('\''),
        '`' => TextObj::Quote('`'),
        '(' | ')' | 'b' => TextObj::Pair('(', ')'),
        '{' | '}' | 'B' => TextObj::Pair('{', '}'),
        '[' | ']' => TextObj::Pair('[', ']'),
        '<' | '>' => TextObj::Pair('<', '>'),
        'p' => TextObj::Paragraph,
        _ => return None,
    })
}

/// Resolve a text object around `cursor` to a [`MotionSpan`], or `None`
/// when the cursor isn't inside one (e.g. `di(` with no surrounding
/// parens). Inclusive char spans for word/quote/pair, linewise for
/// paragraph.
pub fn resolve_text_object(
    lines: &[String],
    cursor: (usize, usize),
    obj: TextObj,
    kind: TextObjKind,
) -> Option<MotionSpan> {
    let around = matches!(kind, TextObjKind::Around);
    match obj {
        TextObj::Word(big) => word_object(lines, cursor, big, around),
        TextObj::Quote(q) => quote_object(lines, cursor, q, around),
        TextObj::Pair(o, c) => pair_object(lines, cursor, o, c, around),
        TextObj::Paragraph => paragraph_object(lines, cursor, around),
    }
}

fn incl(start: (usize, usize), end: (usize, usize)) -> MotionSpan {
    MotionSpan {
        start,
        end,
        kind: MotionKind::CharInclusive,
    }
}

/// An empty char object (e.g. `di"` on `""`) — a zero-width exclusive
/// span so the operator no-ops cleanly.
fn empty(at: (usize, usize)) -> MotionSpan {
    MotionSpan {
        start: at,
        end: at,
        kind: MotionKind::CharExclusive,
    }
}

fn word_object(
    lines: &[String],
    cursor: (usize, usize),
    big: bool,
    around: bool,
) -> Option<MotionSpan> {
    let (row, col) = cursor;
    let chars: Vec<char> = lines.get(row)?.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let col = col.min(chars.len() - 1);
    // 0 = blank, 1 = keyword/non-blank, 2 = punctuation (collapsed into 1
    // when `big`).
    let class = |c: char| -> u8 {
        if c.is_whitespace() {
            0
        } else if big || c.is_alphanumeric() || c == '_' {
            1
        } else {
            2
        }
    };
    let target = class(chars[col]);
    let mut lo = col;
    while lo > 0 && class(chars[lo - 1]) == target {
        lo -= 1;
    }
    let mut hi = col;
    while hi + 1 < chars.len() && class(chars[hi + 1]) == target {
        hi += 1;
    }
    if around {
        // `aw`: extend over trailing whitespace; if none, over leading.
        let mut end = hi;
        let mut grew = false;
        while end + 1 < chars.len() && chars[end + 1].is_whitespace() {
            end += 1;
            grew = true;
        }
        if grew {
            hi = end;
        } else {
            while lo > 0 && chars[lo - 1].is_whitespace() {
                lo -= 1;
            }
        }
    }
    Some(incl((row, lo), (row, hi)))
}

fn quote_object(
    lines: &[String],
    cursor: (usize, usize),
    q: char,
    around: bool,
) -> Option<MotionSpan> {
    let (row, col) = cursor;
    let chars: Vec<char> = lines.get(row)?.chars().collect();
    let quotes: Vec<usize> = chars
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c == q)
        .map(|(i, _)| i)
        .collect();
    // Pair quotes left-to-right; pick the first pair whose close is at or
    // after the cursor (vim pairs from the start of the line).
    let mut pair = None;
    let mut i = 0;
    while i + 1 < quotes.len() {
        let (o, c) = (quotes[i], quotes[i + 1]);
        if col <= c {
            pair = Some((o, c));
            break;
        }
        i += 2;
    }
    let (o, c) = pair?;
    if around {
        let mut end = c;
        let mut grew = false;
        while end + 1 < chars.len() && chars[end + 1].is_whitespace() {
            end += 1;
            grew = true;
        }
        let start = if grew {
            o
        } else {
            let mut s = o;
            while s > 0 && chars[s - 1].is_whitespace() {
                s -= 1;
            }
            s
        };
        Some(incl((row, start), (row, end)))
    } else if c == o + 1 {
        Some(empty((row, o + 1)))
    } else {
        Some(incl((row, o + 1), (row, c - 1)))
    }
}

/// Flatten the buffer into `(row, col, char)` triples (empty lines
/// contribute nothing) so bracket matching can scan across lines.
fn flatten(lines: &[String]) -> Vec<(usize, usize, char)> {
    lines
        .iter()
        .enumerate()
        .flat_map(|(r, l)| l.chars().enumerate().map(move |(c, ch)| (r, c, ch)))
        .collect()
}

fn pair_object(
    lines: &[String],
    cursor: (usize, usize),
    open: char,
    close: char,
    around: bool,
) -> Option<MotionSpan> {
    let flat = flatten(lines);
    if flat.is_empty() {
        return None;
    }
    // First triple at or after the cursor.
    let idx = flat
        .iter()
        .position(|&(r, c, _)| (r, c) >= cursor)
        .unwrap_or(flat.len() - 1);
    // Walk left for the enclosing open, tracking close depth. A close
    // sitting exactly under the cursor is skipped so we pair with its own
    // open rather than treating it as a nested level.
    let mut depth = 0i32;
    let mut oi = None;
    let mut i = idx as isize;
    while i >= 0 {
        let ch = flat[i as usize].2;
        if ch == close && i as usize != idx {
            depth += 1;
        } else if ch == open {
            if depth == 0 {
                oi = Some(i as usize);
                break;
            }
            depth -= 1;
        }
        i -= 1;
    }
    let oi = oi?;
    // Walk right from the open for its matching close.
    let mut depth = 0i32;
    let mut ci = None;
    for (j, &(_, _, ch)) in flat.iter().enumerate().skip(oi + 1) {
        if ch == open {
            depth += 1;
        } else if ch == close {
            if depth == 0 {
                ci = Some(j);
                break;
            }
            depth -= 1;
        }
    }
    let ci = ci?;
    let (or, oc, _) = flat[oi];
    let (cr, cc, _) = flat[ci];
    if around {
        Some(incl((or, oc), (cr, cc)))
    } else if ci == oi + 1 {
        let (r, c, _) = flat[ci];
        Some(empty((r, c)))
    } else {
        let (sr, sc, _) = flat[oi + 1];
        let (er, ec, _) = flat[ci - 1];
        Some(incl((sr, sc), (er, ec)))
    }
}

fn paragraph_object(lines: &[String], cursor: (usize, usize), around: bool) -> Option<MotionSpan> {
    if lines.is_empty() {
        return None;
    }
    let row = cursor.0.min(lines.len() - 1);
    let is_blank = |r: usize| lines.get(r).map(|l| l.trim().is_empty()).unwrap_or(true);
    let on_blank = is_blank(row);
    let mut top = row;
    while top > 0 && is_blank(top - 1) == on_blank {
        top -= 1;
    }
    let mut bot = row;
    while bot + 1 < lines.len() && is_blank(bot + 1) == on_blank {
        bot += 1;
    }
    if around && !on_blank {
        // `ap`: include trailing blank lines, else leading.
        let mut end = bot;
        let mut grew = false;
        while end + 1 < lines.len() && is_blank(end + 1) {
            end += 1;
            grew = true;
        }
        if grew {
            bot = end;
        } else {
            while top > 0 && is_blank(top - 1) {
                top -= 1;
            }
        }
    }
    Some(MotionSpan {
        start: (top, 0),
        end: (bot, 0),
        kind: MotionKind::Linewise,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(str::to_string).collect()
    }
    fn resolve(s: &str, cur: (usize, usize), obj: TextObj, around: bool) -> Option<MotionSpan> {
        let kind = if around {
            TextObjKind::Around
        } else {
            TextObjKind::Inner
        };
        resolve_text_object(&lines(s), cur, obj, kind)
    }

    #[test]
    fn inner_word_covers_the_run() {
        let sp = resolve("foo bar", (0, 5), TextObj::Word(false), false).unwrap();
        assert_eq!((sp.start, sp.end), ((0, 4), (0, 6)));
        assert_eq!(sp.kind, MotionKind::CharInclusive);
    }

    #[test]
    fn around_word_eats_trailing_space() {
        let sp = resolve("foo bar", (0, 0), TextObj::Word(false), true).unwrap();
        assert_eq!((sp.start, sp.end), ((0, 0), (0, 3))); // "foo "
    }

    #[test]
    fn small_word_splits_on_punct() {
        // iw on the '.' of "foo.bar" is just the dot.
        let sp = resolve("foo.bar", (0, 3), TextObj::Word(false), false).unwrap();
        assert_eq!((sp.start, sp.end), ((0, 3), (0, 3)));
        // WORD spans the whole "foo.bar".
        let sp = resolve("foo.bar", (0, 3), TextObj::Word(true), false).unwrap();
        assert_eq!((sp.start, sp.end), ((0, 0), (0, 6)));
    }

    #[test]
    fn inner_quote_excludes_delimiters() {
        let sp = resolve("say \"hi\" now", (0, 6), TextObj::Quote('"'), false).unwrap();
        assert_eq!((sp.start, sp.end), ((0, 5), (0, 6))); // "hi"
    }

    #[test]
    fn around_quote_includes_delimiters_and_trailing_space() {
        let sp = resolve("say \"hi\" now", (0, 6), TextObj::Quote('"'), true).unwrap();
        assert_eq!((sp.start, sp.end), ((0, 4), (0, 8))); // `"hi" `
    }

    #[test]
    fn inner_paren_nested_depth() {
        let sp = resolve("a (b (c) d) e", (0, 6), TextObj::Pair('(', ')'), false).unwrap();
        assert_eq!((sp.start, sp.end), ((0, 6), (0, 6))); // just "c"
    }

    #[test]
    fn around_paren_includes_brackets() {
        let sp = resolve("a (bc) d", (0, 3), TextObj::Pair('(', ')'), true).unwrap();
        assert_eq!((sp.start, sp.end), ((0, 2), (0, 5))); // "(bc)"
    }

    #[test]
    fn paren_spans_lines() {
        let sp = resolve("f(\n  x\n)", (1, 2), TextObj::Pair('(', ')'), false).unwrap();
        assert_eq!((sp.start, sp.end), ((1, 0), (1, 2))); // "  x"
    }

    #[test]
    fn cursor_not_in_pair_returns_none() {
        assert!(resolve("no parens here", (0, 3), TextObj::Pair('(', ')'), false).is_none());
    }

    #[test]
    fn inner_paragraph_is_linewise() {
        let sp = resolve("a\nb\n\nc", (0, 0), TextObj::Paragraph, false).unwrap();
        assert_eq!((sp.start, sp.end), ((0, 0), (1, 0)));
        assert_eq!(sp.kind, MotionKind::Linewise);
    }

    #[test]
    fn around_paragraph_eats_trailing_blanks() {
        let sp = resolve("a\nb\n\nc", (0, 0), TextObj::Paragraph, true).unwrap();
        assert_eq!((sp.start, sp.end), ((0, 0), (2, 0)));
    }
}
