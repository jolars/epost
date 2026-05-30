//! Plain-text extraction from the Block-IR. Drives reader yanks (`Y`,
//! `yp`): the *whole* source text of the target, not what's rendered in
//! the visible viewport. Cell glyphs are for display; this is the
//! clipboard-bound source of truth.
//!
//! Whitespace inside `Inline::Text` runs is already normalized by
//! `html::normalize_inline`, so concatenation is enough — no further
//! collapsing here.

use crate::mail::html::{Block, Inline};

/// Whole-body extraction. Top-level blocks join with a blank line.
pub fn extract_body(blocks: &[Block]) -> String {
    let mut out = String::new();
    let mut first = true;
    for block in blocks {
        let s = extract_block(block);
        if s.is_empty() {
            continue;
        }
        if !first {
            out.push_str("\n\n");
        }
        out.push_str(&s);
        first = false;
    }
    out
}

/// Single-block extraction. Used by `yp`.
pub fn extract_block(block: &Block) -> String {
    match block {
        Block::Paragraph(runs) => extract_inlines(runs),
        Block::Heading { text, .. } => extract_inlines(text),
        Block::List { ordered, items } => extract_list(*ordered, items),
        Block::Quote(inner) => prefix_lines(&extract_body(inner), "> "),
        Block::Table { rows } => extract_table(rows),
        Block::Pre(text) => text.clone(),
        Block::HRule => "---".to_string(),
        Block::Image { alt, .. } => {
            let label = if alt.is_empty() { "—" } else { alt.as_str() };
            format!("[image: {label}]")
        }
    }
}

fn extract_inlines(runs: &[Inline]) -> String {
    let mut out = String::new();
    walk_inlines(runs, &mut out);
    out
}

fn walk_inlines(runs: &[Inline], out: &mut String) {
    for r in runs {
        match r {
            Inline::Text { content, .. } => out.push_str(content),
            // Visible text only; URL stays separate (yank-link surfaces it).
            Inline::Link { runs, .. } => walk_inlines(runs, out),
            Inline::LineBreak => out.push('\n'),
        }
    }
}

fn extract_list(ordered: bool, items: &[Vec<Block>]) -> String {
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        let marker = if ordered {
            format!("{}. ", i + 1)
        } else {
            "- ".to_string()
        };
        let body = extract_body(item);
        if body.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        // Marker on the first line, continuation lines padded to align.
        let pad = " ".repeat(marker.chars().count());
        let mut first_line = true;
        for ln in body.split('\n') {
            if first_line {
                out.push_str(&marker);
                first_line = false;
            } else {
                out.push('\n');
                out.push_str(&pad);
            }
            out.push_str(ln);
        }
    }
    out
}

fn extract_table(rows: &[Vec<Vec<Inline>>]) -> String {
    let mut out = String::new();
    let mut first = true;
    for row in rows {
        if !first {
            out.push('\n');
        }
        first = false;
        let cells: Vec<String> = row.iter().map(|c| extract_inlines(c)).collect();
        out.push_str(&cells.join(" | "));
    }
    out
}

fn prefix_lines(s: &str, prefix: &str) -> String {
    let mut out = String::new();
    let mut first = true;
    for ln in s.split('\n') {
        if !first {
            out.push('\n');
        }
        first = false;
        out.push_str(prefix);
        out.push_str(ln);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::html;

    fn parse_and_extract_body(html_src: &str) -> String {
        extract_body(&html::parse(html_src))
    }

    #[test]
    fn plain_paragraph() {
        assert_eq!(parse_and_extract_body("<p>hello world</p>"), "hello world");
    }

    #[test]
    fn two_paragraphs_separated_by_blank_line() {
        assert_eq!(parse_and_extract_body("<p>one</p><p>two</p>"), "one\n\ntwo");
    }

    #[test]
    fn link_text_only_not_href() {
        let s = parse_and_extract_body(r#"<p>see <a href="https://x">here</a> please</p>"#);
        assert_eq!(s, "see here please");
        assert!(!s.contains("https://"));
    }

    #[test]
    fn blockquote_prefixes_every_line() {
        let s = parse_and_extract_body("<blockquote><p>one</p><p>two</p></blockquote>");
        assert_eq!(s, "> one\n> \n> two");
    }

    #[test]
    fn nested_blockquote_double_prefix() {
        let s =
            parse_and_extract_body("<blockquote><blockquote><p>deep</p></blockquote></blockquote>");
        assert_eq!(s, "> > deep");
    }

    #[test]
    fn ordered_list_uses_numbers() {
        let s = parse_and_extract_body("<ol><li>alpha</li><li>beta</li></ol>");
        assert_eq!(s, "1. alpha\n2. beta");
    }

    #[test]
    fn unordered_list_uses_dashes() {
        let s = parse_and_extract_body("<ul><li>alpha</li><li>beta</li></ul>");
        assert_eq!(s, "- alpha\n- beta");
    }

    #[test]
    fn heading_yields_text() {
        let s = parse_and_extract_body("<h2>title</h2><p>body</p>");
        assert_eq!(s, "title\n\nbody");
    }

    #[test]
    fn image_yields_alt_label() {
        let s = parse_and_extract_body(r#"<img src="cid:x" alt="logo">"#);
        assert_eq!(s, "[image: logo]");
    }

    #[test]
    fn pre_kept_verbatim() {
        let s = parse_and_extract_body("<pre>fn main() {\n    todo!()\n}</pre>");
        assert_eq!(s, "fn main() {\n    todo!()\n}");
    }

    #[test]
    fn hrule_yields_dashes() {
        let s = parse_and_extract_body("<p>a</p><hr><p>b</p>");
        assert_eq!(s, "a\n\n---\n\nb");
    }

    #[test]
    fn empty_body_yields_empty_string() {
        assert_eq!(parse_and_extract_body(""), "");
    }

    #[test]
    fn extract_block_paragraph_isolated() {
        let blocks = html::parse("<p>first</p><p>second</p>");
        // yp on the second paragraph: just its text, no leading blank
        // line, no following content.
        assert_eq!(extract_block(&blocks[1]), "second");
    }

    #[test]
    fn extract_block_quote_returns_whole_quote() {
        let blocks = html::parse("<blockquote><p>a</p><p>b</p></blockquote><p>after</p>");
        // The quote is one top-level block. yp on it must yield the
        // entire quote, not just one inner paragraph.
        let s = extract_block(&blocks[0]);
        assert_eq!(s, "> a\n> \n> b");
        assert!(!s.contains("after"));
    }

    #[test]
    fn extract_block_handles_deeply_wrapped_paragraph_in_full() {
        // The load-bearing case: even when the rendered paragraph would
        // wrap past any plausible viewport, extraction returns the full
        // source text byte-for-byte.
        let long = "word ".repeat(200);
        let html_src = format!("<p>{long}</p>");
        let blocks = html::parse(&html_src);
        let s = extract_block(&blocks[0]);
        // 200 "word " runs collapse to a single trimmed run.
        let expected = "word ".repeat(200).trim_end().to_string();
        assert_eq!(s, expected);
    }
}
