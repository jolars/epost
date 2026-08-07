//! Display-width primitives for the fixed-column chrome (list rows, the
//! folder sidebar, tab labels).
//!
//! Every one of those layouts flushes something to the right of a
//! fixed-width column, so the column's *cell* width has to be measured
//! with the same ruler the terminal paints with. `chars().count()` is not
//! that ruler: a CJK or emoji-presentation glyph paints two columns and a
//! combining mark paints none, so a 16-char `From` column holding Chinese
//! text renders 32 cells wide and shoves the subject (and the
//! right-flushed date) past the pane edge.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display width of `s` in terminal cells.
pub(crate) fn disp_w(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncate `s` to at most `max_cells` terminal columns, marking a
/// truncation with a trailing `…`.
///
/// The result is never *wider* than `max_cells`, but it can be one cell
/// narrower: when the budget's last column falls in the middle of a
/// double-width glyph the glyph is dropped whole rather than half-painted.
/// Callers that need an exact column re-measure with [`disp_w`] (or use
/// [`truncate_pad`], which pads the shortfall).
pub(crate) fn truncate_to(s: &str, max_cells: usize) -> String {
    if disp_w(s) <= max_cells {
        return s.to_string();
    }
    if max_cells == 0 {
        return String::new();
    }
    // One column is reserved for the ellipsis.
    let budget = max_cells - 1;
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// [`truncate_to`], then right-pad with spaces so the result occupies
/// exactly `width` cells. This is the fixed-column primitive: the padding
/// also absorbs the cell a dropped double-width glyph would have left
/// short.
pub(crate) fn truncate_pad(s: &str, width: usize) -> String {
    let mut out = truncate_to(s, width);
    let w = disp_w(&out);
    if w < width {
        out.push_str(&" ".repeat(width - w));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_unchanged_when_it_fits() {
        assert_eq!(truncate_to("hello", 10), "hello");
        assert_eq!(truncate_to("hello", 5), "hello");
    }

    #[test]
    fn ascii_overflow_gets_ellipsis() {
        assert_eq!(truncate_to("hello world", 7), "hello …");
        assert_eq!(disp_w(&truncate_to("hello world", 7)), 7);
    }

    #[test]
    fn wide_chars_measured_in_cells_not_chars() {
        // Four CJK glyphs = 8 cells, so an 8-cell budget fits them whole.
        let cjk = "你好世界";
        assert_eq!(disp_w(cjk), 8);
        assert_eq!(truncate_to(cjk, 8), cjk);
        // A 6-cell budget keeps two glyphs (4 cells) + the ellipsis: the
        // third would land on cell 5-6 of a 5-cell budget-minus-ellipsis.
        let t = truncate_to(cjk, 6);
        assert_eq!(t, "你好…");
        assert_eq!(disp_w(&t), 5);
        // Never wider than the budget, even when the boundary splits a glyph.
        for max in 0..=10 {
            assert!(disp_w(&truncate_to(cjk, max)) <= max, "max={max}");
        }
    }

    #[test]
    fn zero_budget_yields_empty() {
        assert_eq!(truncate_to("hello", 0), "");
        assert_eq!(truncate_to("你好", 0), "");
    }

    #[test]
    fn pad_reaches_exact_cell_width() {
        assert_eq!(truncate_pad("bob", 6), "bob   ");
        assert_eq!(disp_w(&truncate_pad("bob", 6)), 6);
        // A CJK name that overflows a 16-cell column: exactly 16 cells out,
        // not 16 chars (which would paint ~32).
        let name = "王小明王小明王小明王小明";
        assert_eq!(disp_w(&truncate_pad(name, 16)), 16);
        // And a short CJK name pads out to the column instead of running long.
        assert_eq!(disp_w(&truncate_pad("王小明", 16)), 16);
        // The odd-budget case: the split glyph is dropped and the padding
        // covers the leftover cell.
        assert_eq!(disp_w(&truncate_pad("你好世界", 7)), 7);
    }

    #[test]
    fn combining_marks_count_zero() {
        // "e" + combining acute is one cell, so it fits a 1-cell column.
        let e = "e\u{0301}";
        assert_eq!(disp_w(e), 1);
        assert_eq!(truncate_to(e, 1), e);
    }
}
