//! HTML → Block-IR. Parses email HTML with `html5ever` + `markup5ever_rcdom`
//! and walks the DOM into the structural representation the reader pane
//! lays out. The IR deliberately depends on no UI types so it can be
//! snapshot-tested in isolation.
//!
//! Security stance (DESIGN.md invariants 4 + 5): `<script>`, `<style>`,
//! `<head>`, `<meta>`, `<link>` are dropped wholesale here; nothing in the
//! emitted IR can later be made to fetch remote content or execute code.

use html5ever::tendril::TendrilSink;
use html5ever::{ParseOpts, parse_document};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Block {
    Paragraph(Vec<Inline>),
    Heading {
        level: u8,
        text: Vec<Inline>,
    },
    List {
        ordered: bool,
        items: Vec<Vec<Block>>,
    },
    Quote(Vec<Block>),
    Table {
        rows: Vec<Vec<Vec<Inline>>>,
    },
    Pre(String),
    HRule,
    Image {
        cid: Option<String>,
        src: Option<String>,
        alt: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Inline {
    Text { content: String, style: InlineStyle },
    Link { href: String, runs: Vec<Inline> },
    LineBreak,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub code: bool,
}

pub fn parse(html: &str) -> Vec<Block> {
    let dom = parse_document(RcDom::default(), ParseOpts::default()).one(html);
    let root = find_body(&dom.document).unwrap_or_else(|| dom.document.clone());
    let mut out = Vec::new();
    walk_blocks(&root, &mut out);
    out
}

fn find_body(node: &Handle) -> Option<Handle> {
    if let NodeData::Element { name, .. } = &node.data
        && name.local.as_ref() == "body"
    {
        return Some(node.clone());
    }
    for child in node.children.borrow().iter() {
        if let Some(b) = find_body(child) {
            return Some(b);
        }
    }
    None
}

fn walk_blocks(parent: &Handle, out: &mut Vec<Block>) {
    let mut inline_buf: Vec<Inline> = Vec::new();
    for child in parent.children.borrow().iter() {
        match &child.data {
            NodeData::Text { contents } => {
                push_text(
                    &mut inline_buf,
                    contents.borrow().as_ref(),
                    InlineStyle::default(),
                );
            }
            NodeData::Element { name, .. } => {
                let tag = name.local.as_ref();
                if is_dropped(tag) {
                    continue;
                }
                if is_block(tag) {
                    flush_inline(&mut inline_buf, out);
                    walk_block_element(tag, child, out);
                } else if tag == "img" && inline_buf_is_blank(&inline_buf) {
                    // Lone img at the block level becomes Block::Image; img
                    // mixed with surrounding text stays inline via the
                    // walk_inline path below.
                    flush_inline(&mut inline_buf, out);
                    let (src, alt) = read_img_attrs(child);
                    let (cid, src) = split_src(src.as_deref());
                    out.push(Block::Image { cid, src, alt });
                } else if is_inline(tag) || is_collapse(tag) {
                    walk_inline(child, &mut inline_buf, InlineStyle::default());
                } else {
                    walk_unknown(child, &mut inline_buf, out);
                }
            }
            _ => {}
        }
    }
    flush_inline(&mut inline_buf, out);
}

fn inline_buf_is_blank(buf: &[Inline]) -> bool {
    buf.iter().all(|r| match r {
        Inline::Text { content, .. } => content.chars().all(char::is_whitespace),
        _ => false,
    })
}

fn walk_unknown(node: &Handle, inline_buf: &mut Vec<Inline>, out: &mut Vec<Block>) {
    for child in node.children.borrow().iter() {
        match &child.data {
            NodeData::Text { contents } => {
                push_text(
                    inline_buf,
                    contents.borrow().as_ref(),
                    InlineStyle::default(),
                );
            }
            NodeData::Element { name, .. } => {
                let tag = name.local.as_ref();
                if is_dropped(tag) {
                    continue;
                }
                if is_block(tag) {
                    flush_inline(inline_buf, out);
                    walk_block_element(tag, child, out);
                } else if is_inline(tag) || is_collapse(tag) {
                    walk_inline(child, inline_buf, InlineStyle::default());
                } else {
                    walk_unknown(child, inline_buf, out);
                }
            }
            _ => {}
        }
    }
}

fn walk_block_element(tag: &str, node: &Handle, out: &mut Vec<Block>) {
    match tag {
        "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "aside" | "nav" => {
            // Common email pattern: <p><img …></p>. Lift the image out
            // so the renderer sees a Block::Image (the only thing the
            // image-decode walker looks at), instead of folding it into
            // an Inline::Text placeholder inside the paragraph.
            if let Some(img) = lone_img_child(node) {
                let (src, alt) = read_img_attrs(&img);
                let (cid, src) = split_src(src.as_deref());
                out.push(Block::Image { cid, src, alt });
                return;
            }
            // Generic containers (notably Outlook/Word's nested <div>
            // wrappers) frequently hold block-level children — other
            // divs, <p>, lists. `collect_inline` refuses to descend past
            // a block child, so collecting these as a single paragraph
            // silently drops the entire subtree. Recurse with walk_blocks
            // when there's a block child so the structure is preserved;
            // <p> never legally contains blocks (html5ever closes it
            // first), so it always takes the inline path.
            if tag != "p" && has_block_child(node) {
                walk_blocks(node, out);
                return;
            }
            let runs = collect_inline(node);
            if !runs.is_empty() {
                out.push(Block::Paragraph(runs));
            }
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = tag.as_bytes()[1] - b'0';
            let text = collect_inline(node);
            if !text.is_empty() {
                out.push(Block::Heading { level, text });
            }
        }
        "ul" | "ol" => {
            let ordered = tag == "ol";
            let mut items: Vec<Vec<Block>> = Vec::new();
            for child in node.children.borrow().iter() {
                if let NodeData::Element { name, .. } = &child.data
                    && name.local.as_ref() == "li"
                {
                    let mut item_blocks: Vec<Block> = Vec::new();
                    walk_blocks(child, &mut item_blocks);
                    items.push(item_blocks);
                }
            }
            out.push(Block::List { ordered, items });
        }
        "blockquote" => {
            let mut inner: Vec<Block> = Vec::new();
            walk_blocks(node, &mut inner);
            out.push(Block::Quote(inner));
        }
        "table" => {
            let mut rows: Vec<Vec<Vec<Inline>>> = Vec::new();
            collect_table_rows(node, &mut rows);
            out.push(Block::Table { rows });
        }
        "pre" => {
            let mut buf = String::new();
            collect_text_verbatim(node, &mut buf);
            out.push(Block::Pre(buf));
        }
        "hr" => out.push(Block::HRule),
        _ => {
            walk_blocks(node, out);
        }
    }
}

fn has_block_child(node: &Handle) -> bool {
    node.children
        .borrow()
        .iter()
        .any(|c| matches!(&c.data, NodeData::Element { name, .. } if is_block(name.local.as_ref())))
}

fn walk_inline(node: &Handle, buf: &mut Vec<Inline>, parent_style: InlineStyle) {
    if let NodeData::Element { name, .. } = &node.data {
        let tag = name.local.as_ref();
        match tag {
            "br" => {
                buf.push(Inline::LineBreak);
                return;
            }
            "a" => {
                let href = read_attr(node, "href").unwrap_or_default();
                let mut runs: Vec<Inline> = Vec::new();
                for child in node.children.borrow().iter() {
                    walk_inline_child(child, &mut runs, parent_style);
                }
                if !runs.is_empty() {
                    buf.push(Inline::Link { href, runs });
                }
                return;
            }
            "img" => {
                // Mid-paragraph images become inline placeholder text so
                // wrapping keeps working; block-level images (img as a
                // direct child of a block container) become Block::Image
                // via walk_block_element / walk_blocks.
                let (src, alt) = read_img_attrs(node);
                let label = match split_src(src.as_deref()) {
                    (Some(_), _) | (None, None) => {
                        format!(
                            "[image: {}]",
                            if alt.is_empty() { "—" } else { alt.as_str() }
                        )
                    }
                    (None, Some(s)) if is_remote(&s) => {
                        format!(
                            "[remote image: {}]",
                            if alt.is_empty() {
                                s.as_str()
                            } else {
                                alt.as_str()
                            }
                        )
                    }
                    _ => format!(
                        "[image: {}]",
                        if alt.is_empty() { "—" } else { alt.as_str() }
                    ),
                };
                push_text(buf, &label, parent_style);
                return;
            }
            _ => {}
        }
        let style = apply_style(tag, parent_style);
        for child in node.children.borrow().iter() {
            walk_inline_child(child, buf, style);
        }
    }
}

fn walk_inline_child(node: &Handle, buf: &mut Vec<Inline>, style: InlineStyle) {
    match &node.data {
        NodeData::Text { contents } => {
            push_text(buf, contents.borrow().as_ref(), style);
        }
        NodeData::Element { name, .. } => {
            let tag = name.local.as_ref();
            if is_dropped(tag) {
                return;
            }
            if is_block(tag) {
                // Block element reached in an inline-only context — e.g.
                // a <div> inside a table cell (Outlook wraps cell text in
                // divs). The cell IR holds only inline runs, so flatten
                // the block's content into the run rather than dropping
                // it. A leading break keeps adjacent blocks from fusing.
                if !buf.is_empty() {
                    buf.push(Inline::LineBreak);
                }
                for c in node.children.borrow().iter() {
                    walk_inline_child(c, buf, style);
                }
                return;
            }
            walk_inline(node, buf, style);
        }
        _ => {}
    }
}

fn collect_inline(node: &Handle) -> Vec<Inline> {
    let mut buf: Vec<Inline> = Vec::new();
    for child in node.children.borrow().iter() {
        walk_inline_child(child, &mut buf, InlineStyle::default());
    }
    normalize_inline(&mut buf);
    buf
}

fn collect_table_rows(node: &Handle, rows: &mut Vec<Vec<Vec<Inline>>>) {
    for child in node.children.borrow().iter() {
        if let NodeData::Element { name, .. } = &child.data {
            let tag = name.local.as_ref();
            if matches!(tag, "thead" | "tbody" | "tfoot") {
                collect_table_rows(child, rows);
            } else if tag == "tr" {
                let mut cells: Vec<Vec<Inline>> = Vec::new();
                for c in child.children.borrow().iter() {
                    if let NodeData::Element { name: cn, .. } = &c.data
                        && matches!(cn.local.as_ref(), "td" | "th")
                    {
                        cells.push(collect_inline(c));
                    }
                }
                if !cells.is_empty() {
                    rows.push(cells);
                }
            }
        }
    }
}

fn collect_text_verbatim(node: &Handle, out: &mut String) {
    for child in node.children.borrow().iter() {
        match &child.data {
            NodeData::Text { contents } => out.push_str(contents.borrow().as_ref()),
            NodeData::Element { .. } => collect_text_verbatim(child, out),
            _ => {}
        }
    }
}

/// Returns the single `<img>` descendant of `node` if its content is
/// effectively just that image (and surrounding whitespace). Used to
/// promote `<p><img></p>` → `Block::Image` so the image-decode walker
/// finds it; nested anchors are followed so `<p><a><img></a></p>` is
/// treated the same way (the link is lost but real-email image-only
/// paragraphs are rarely the actual link target — `f` link-picker still
/// finds the surrounding paragraph's links).
fn lone_img_child(node: &Handle) -> Option<Handle> {
    let mut found: Option<Handle> = None;
    if !scan_for_lone_img(node, &mut found) {
        return None;
    }
    found
}

fn scan_for_lone_img(node: &Handle, found: &mut Option<Handle>) -> bool {
    for child in node.children.borrow().iter() {
        match &child.data {
            NodeData::Text { contents } if !contents.borrow().chars().all(char::is_whitespace) => {
                return false;
            }
            NodeData::Element { name, .. } => {
                let tag = name.local.as_ref();
                if is_dropped(tag) {
                    continue;
                }
                if tag == "img" {
                    if found.is_some() {
                        return false;
                    }
                    *found = Some(child.clone());
                } else if matches!(tag, "a" | "span" | "font" | "em" | "strong" | "b" | "i") {
                    // Transparent inline wrappers — descend.
                    if !scan_for_lone_img(child, found) {
                        return false;
                    }
                } else {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

fn read_img_attrs(node: &Handle) -> (Option<String>, String) {
    let src = read_attr(node, "src");
    let alt = read_attr(node, "alt").unwrap_or_default();
    (src, alt)
}

fn read_attr(node: &Handle, key: &str) -> Option<String> {
    if let NodeData::Element { attrs, .. } = &node.data {
        for a in attrs.borrow().iter() {
            if a.name.local.as_ref() == key {
                return Some(a.value.to_string());
            }
        }
    }
    None
}

fn split_src(src: Option<&str>) -> (Option<String>, Option<String>) {
    match src {
        Some(s) if s.starts_with("cid:") => (Some(s[4..].to_string()), None),
        Some(s) => (None, Some(s.to_string())),
        None => (None, None),
    }
}

fn is_remote(src: &str) -> bool {
    src.starts_with("http://") || src.starts_with("https://")
}

fn apply_style(tag: &str, base: InlineStyle) -> InlineStyle {
    let mut s = base;
    match tag {
        "b" | "strong" => s.bold = true,
        "i" | "em" => s.italic = true,
        "u" | "ins" => s.underline = true,
        "code" | "tt" | "kbd" | "samp" => s.code = true,
        _ => {}
    }
    s
}

fn push_text(buf: &mut Vec<Inline>, text: &str, style: InlineStyle) {
    if text.is_empty() {
        return;
    }
    if let Some(Inline::Text {
        content,
        style: existing,
    }) = buf.last_mut()
        && *existing == style
    {
        content.push_str(text);
        return;
    }
    buf.push(Inline::Text {
        content: text.to_string(),
        style,
    });
}

fn flush_inline(buf: &mut Vec<Inline>, out: &mut Vec<Block>) {
    if buf.is_empty() {
        return;
    }
    let mut runs = std::mem::take(buf);
    normalize_inline(&mut runs);
    if !runs.is_empty() {
        out.push(Block::Paragraph(runs));
    }
}

/// Collapse runs of HTML whitespace to single spaces; trim leading and
/// trailing whitespace at the block boundary. Mirrors how a browser
/// normalizes whitespace outside `<pre>`.
fn normalize_inline(runs: &mut Vec<Inline>) {
    fn norm_text(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut prev_space = false;
        for ch in s.chars() {
            if ch.is_whitespace() {
                if !prev_space {
                    out.push(' ');
                }
                prev_space = true;
            } else {
                out.push(ch);
                prev_space = false;
            }
        }
        out
    }
    for run in runs.iter_mut() {
        match run {
            Inline::Text { content, .. } => *content = norm_text(content),
            Inline::Link { runs: inner, .. } => {
                for r in inner.iter_mut() {
                    if let Inline::Text { content, .. } = r {
                        *content = norm_text(content);
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(Inline::Text { content, .. }) = runs.first_mut() {
        *content = content.trim_start().to_string();
    }
    if let Some(Inline::Text { content, .. }) = runs.last_mut() {
        *content = content.trim_end().to_string();
    }
    runs.retain(|r| match r {
        Inline::Text { content, .. } => !content.is_empty(),
        _ => true,
    });
}

fn is_dropped(tag: &str) -> bool {
    matches!(
        tag,
        "script" | "style" | "head" | "meta" | "link" | "noscript" | "iframe" | "object" | "embed"
    )
}

fn is_block(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "div"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "main"
            | "aside"
            | "nav"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "blockquote"
            | "table"
            | "pre"
            | "hr"
    )
}

fn is_inline(tag: &str) -> bool {
    matches!(
        tag,
        "b" | "strong"
            | "i"
            | "em"
            | "u"
            | "ins"
            | "code"
            | "tt"
            | "kbd"
            | "samp"
            | "a"
            | "span"
            | "br"
            | "img"
            | "small"
            | "sup"
            | "sub"
            | "mark"
            | "del"
            | "s"
            | "abbr"
            | "cite"
            | "q"
    )
}

fn is_collapse(tag: &str) -> bool {
    // Presentational wrappers; their children flow as if the wrapper
    // weren't there.
    matches!(tag, "font" | "center" | "html" | "body")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Inline {
        Inline::Text {
            content: s.to_string(),
            style: InlineStyle::default(),
        }
    }

    #[test]
    fn parses_basic_paragraph() {
        let blocks = parse("<p>Hello world</p>");
        assert_eq!(blocks, vec![Block::Paragraph(vec![text("Hello world")])]);
    }

    #[test]
    fn nests_lists() {
        let blocks = parse("<ul><li>a</li><li>b</li></ul>");
        let Block::List { ordered, items } = &blocks[0] else {
            panic!("expected list, got {:?}", blocks[0]);
        };
        assert!(!*ordered);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], vec![Block::Paragraph(vec![text("a")])]);
    }

    #[test]
    fn drops_script_and_style() {
        let blocks = parse(r#"<style>p{color:red}</style><script>alert(1)</script><p>visible</p>"#);
        assert_eq!(blocks, vec![Block::Paragraph(vec![text("visible")])]);
    }

    #[test]
    fn recognizes_inline_styles() {
        let blocks = parse("<p>plain <b>bold</b> <i>italic</i> <code>code</code></p>");
        let Block::Paragraph(runs) = &blocks[0] else {
            panic!("expected paragraph");
        };
        let has_bold = runs
            .iter()
            .any(|r| matches!(r, Inline::Text { style, .. } if style.bold));
        let has_italic = runs
            .iter()
            .any(|r| matches!(r, Inline::Text { style, .. } if style.italic));
        let has_code = runs
            .iter()
            .any(|r| matches!(r, Inline::Text { style, .. } if style.code));
        assert!(has_bold && has_italic && has_code, "{runs:?}");
    }

    #[test]
    fn link_run_preserves_href() {
        let blocks = parse(r#"<p>see <a href="https://x.example/">here</a></p>"#);
        let Block::Paragraph(runs) = &blocks[0] else {
            panic!("expected paragraph");
        };
        let link = runs.iter().find_map(|r| match r {
            Inline::Link { href, runs } => Some((href.clone(), runs.clone())),
            _ => None,
        });
        let (href, inner) = link.expect("link");
        assert_eq!(href, "https://x.example/");
        assert_eq!(inner, vec![text("here")]);
    }

    #[test]
    fn image_cid_extracted() {
        let blocks = parse(r#"<img src="cid:abc@host" alt="hi">"#);
        let img = blocks.iter().find_map(|b| match b {
            Block::Image { cid, src, alt } => Some((cid.clone(), src.clone(), alt.clone())),
            _ => None,
        });
        let (cid, src, alt) = img.expect("image block");
        assert_eq!(cid.as_deref(), Some("abc@host"));
        assert!(src.is_none());
        assert_eq!(alt, "hi");
    }

    #[test]
    fn image_http_marked_remote() {
        let blocks = parse(r#"<img src="https://tracker.example/p.gif" alt="pixel">"#);
        let img = blocks.iter().find_map(|b| match b {
            Block::Image { cid, src, alt } => Some((cid.clone(), src.clone(), alt.clone())),
            _ => None,
        });
        let (cid, src, alt) = img.expect("image block");
        assert!(cid.is_none());
        assert_eq!(src.as_deref(), Some("https://tracker.example/p.gif"));
        assert_eq!(alt, "pixel");
    }

    #[test]
    fn malformed_html_does_not_panic() {
        let blocks = parse("<p><b>oops<i>still going<p>new para");
        assert!(blocks.iter().any(|b| matches!(b, Block::Paragraph(_))));
    }

    #[test]
    fn whitespace_normalized_in_paragraph() {
        let blocks = parse("<p>  foo   bar\nbaz  </p>");
        assert_eq!(blocks, vec![Block::Paragraph(vec![text("foo bar baz")])]);
    }

    #[test]
    fn blockquote_nests_blocks() {
        let blocks = parse("<blockquote><p>quoted</p></blockquote>");
        let Block::Quote(inner) = &blocks[0] else {
            panic!("expected quote");
        };
        assert_eq!(inner, &vec![Block::Paragraph(vec![text("quoted")])]);
    }

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("dev/fixtures")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("reading fixture {}", path.display()))
    }

    #[test]
    fn snapshot_welcome_fixture() {
        let blocks = parse(&fixture("welcome.html"));
        insta::assert_yaml_snapshot!("welcome", blocks);
    }

    #[test]
    fn snapshot_cid_image_fixture() {
        let blocks = parse(&fixture("cid-image.html"));
        insta::assert_yaml_snapshot!("cid_image", blocks);
    }

    #[test]
    fn snapshot_remote_image_fixture() {
        let blocks = parse(&fixture("remote-image.html"));
        insta::assert_yaml_snapshot!("remote_image", blocks);
    }

    #[test]
    fn snapshot_broken_fixture() {
        let blocks = parse(&fixture("broken.html"));
        insta::assert_yaml_snapshot!("broken", blocks);
    }

    #[test]
    fn p_wrapping_lone_img_lifts_to_block_image() {
        // The common email pattern. Before the lift, this collapsed into
        // a Block::Paragraph with `[image: …]` text, and the
        // image-decode walker never saw the cid → reader stuck on the
        // placeholder forever.
        let blocks = parse(r#"<p><img src="cid:logo@x" alt="L"></p>"#);
        assert_eq!(blocks.len(), 1);
        assert!(
            matches!(
                &blocks[0],
                Block::Image { cid: Some(c), src: None, alt } if c == "logo@x" && alt == "L"
            ),
            "expected Block::Image, got {:?}",
            blocks[0]
        );
    }

    #[test]
    fn p_wrapping_anchored_img_still_lifts() {
        // <a> wrapping a lone <img> is also extremely common (click to
        // enlarge). We lose the link target but the image renders.
        let blocks = parse(r#"<p><a href="https://x"><img src="cid:y" alt="A"></a></p>"#);
        assert_eq!(blocks.len(), 1);
        assert!(
            matches!(&blocks[0], Block::Image { cid: Some(c), .. } if c == "y"),
            "expected Block::Image, got {:?}",
            blocks[0]
        );
    }

    #[test]
    fn div_wrapping_block_children_is_not_dropped() {
        // Outlook/Word nests content in container <div>s whose only
        // children are more blocks. collect_inline bails on block
        // children, so before the fix the whole subtree vanished.
        let blocks = parse(r#"<div><div><p>Dear Johan</p><p>second</p></div></div>"#);
        assert_eq!(
            blocks,
            vec![
                Block::Paragraph(vec![text("Dear Johan")]),
                Block::Paragraph(vec![text("second")]),
            ],
            "expected both paragraphs, got {blocks:?}"
        );
    }

    #[test]
    fn div_with_mixed_inline_and_block_keeps_both() {
        let blocks = parse(r#"<div>intro text<p>para</p></div>"#);
        assert_eq!(
            blocks,
            vec![
                Block::Paragraph(vec![text("intro text")]),
                Block::Paragraph(vec![text("para")]),
            ],
            "got {blocks:?}"
        );
    }

    #[test]
    fn table_cell_with_div_content_renders_text() {
        // The Outlook "external sender" banner wraps cell text in a
        // <div>; the inline-only cell path dropped it, leaving | | .
        let blocks = parse(r#"<table><tr><td><div>cell text</div></td></tr></table>"#);
        let Block::Table { rows } = &blocks[0] else {
            panic!("expected table, got {blocks:?}");
        };
        assert_eq!(rows[0][0], vec![text("cell text")]);
    }

    #[test]
    fn p_with_text_and_img_stays_a_paragraph() {
        // Mixed content must NOT lift — the user-visible text would
        // disappear. Keep the inline placeholder path.
        let blocks = parse(r#"<p>see <img src="cid:y" alt="A"> here</p>"#);
        assert_eq!(blocks.len(), 1);
        assert!(
            matches!(&blocks[0], Block::Paragraph(_)),
            "expected Block::Paragraph (mixed content), got {:?}",
            blocks[0]
        );
    }
}
