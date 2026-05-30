use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui_image::sliced::{SignedPosition, SlicedImage};

use crate::mail::html::{Block, Inline, InlineStyle};
use crate::ui::app::{InboxScreen, Mode, Pane, ParsedBody, ScanState, VisualKind, VisualState};
use crate::ui::images::ImageKey;
use crate::ui::style::pane_block;

/// Marker text the pass-1 walker stamps into a single sentinel `Line`
/// for each `Block::Image`. The pass-2 expansion finds these lines, pulls
/// the `ImageKey` off the trailing span (encoded into its `content`),
/// and replaces the marker line with `height_cells` blank lines.
const IMAGE_MARKER_PREFIX: &str = "\u{E000}img:";

/// Reader-pane layout output. `lines` is what gets rendered into ratatui
/// cells; `links` is the link-picker table; `images` is the list of
/// rects that need a `SlicedImage` painted over them after the paragraph
/// has drawn. `line_block_idx` is a parallel vec the same length as
/// `lines`: each entry records which top-level input `Block` produced
/// that line, so reader yanks can resolve a cursor row back to the
/// source IR.
#[derive(Debug, Default)]
pub struct LaidOutBody {
    pub lines: Vec<Line<'static>>,
    pub links: Vec<LinkSlot>,
    pub images: Vec<ImageSlot>,
    pub line_block_idx: Vec<Option<usize>>,
    /// Plain text of each rendered line — derived from the layout's
    /// own `Span::content` strings (i.e. from the IR via layout, not
    /// from the cell buffer). Drives visual-mode selection extraction
    /// without scraping cells. One entry per `lines` row.
    pub line_text: Vec<String>,
}

impl LaidOutBody {
    /// Top-level input Block index responsible for the wrapped line
    /// `line`. Lines in trailing whitespace (e.g. the empty separator
    /// emitted after a paragraph) inherit the preceding block, so a
    /// cursor parked on a blank-line gap still resolves to a real
    /// paragraph rather than `None`.
    pub fn block_at(&self, line: u16) -> Option<usize> {
        if self.line_block_idx.is_empty() {
            return None;
        }
        let mut idx = (line as usize).min(self.line_block_idx.len() - 1);
        loop {
            if let Some(b) = self.line_block_idx[idx] {
                return Some(b);
            }
            if idx == 0 {
                return None;
            }
            idx -= 1;
        }
    }

    /// First link whose earliest segment lies on or after `line`.
    /// Falls back to the first link in the body when nothing matches —
    /// `yl` from above a single off-screen link still copies it.
    pub fn first_link_at_or_after(&self, line: u16) -> Option<&LinkSlot> {
        let line = line as usize;
        let mut best: Option<(&LinkSlot, usize)> = None;
        for slot in &self.links {
            let Some(min_line) = slot.segments.iter().map(|s| s.line).min() else {
                continue;
            };
            if min_line < line {
                continue;
            }
            best = Some(match best {
                Some((cur, cur_min)) if cur_min <= min_line => (cur, cur_min),
                _ => (slot, min_line),
            });
        }
        best.map(|(s, _)| s).or_else(|| self.links.first())
    }

    /// Extract the text spanned by a visual-mode selection. Endpoints
    /// are body-relative coords (`line` into `line_text`, `col` as a
    /// char index into the line's string). The endpoints normalize so
    /// `(anchor) <= (cursor)` doesn't matter — pass them in any order.
    ///
    /// * `VisualKind::Line` ignores columns and joins whole lines.
    /// * `VisualKind::Char` cuts the first line at `start_col`, the
    ///   last line at `end_col` (inclusive), keeps middles whole.
    ///
    /// Returns the empty string when `line_text` is empty (e.g. body
    /// has no rendered content). Column overshoots are clamped to the
    /// line's char count, matching the draw-time cursor clamp.
    pub fn extract_selection(
        &self,
        anchor_line: u16,
        anchor_col: u16,
        cursor_line: u16,
        cursor_col: u16,
        kind: VisualKind,
    ) -> String {
        if self.line_text.is_empty() {
            return String::new();
        }
        let n = self.line_text.len();
        let (start_line, start_col, end_line, end_col) =
            if (anchor_line, anchor_col) <= (cursor_line, cursor_col) {
                (anchor_line, anchor_col, cursor_line, cursor_col)
            } else {
                (cursor_line, cursor_col, anchor_line, anchor_col)
            };
        let start_line = (start_line as usize).min(n - 1);
        let end_line = (end_line as usize).min(n - 1);
        match kind {
            VisualKind::Line => {
                let slice: Vec<String> = self.line_text[start_line..=end_line].to_vec();
                slice.join("\n")
            }
            VisualKind::Char => {
                if start_line == end_line {
                    let line = &self.line_text[start_line];
                    let n_chars = line.chars().count();
                    if n_chars == 0 {
                        return String::new();
                    }
                    let s = (start_col as usize).min(n_chars.saturating_sub(1));
                    // `end_col` is inclusive in vim's char-wise visual.
                    let e = (end_col as usize).min(n_chars.saturating_sub(1));
                    line.chars().skip(s).take(e + 1 - s).collect()
                } else {
                    let mut out = String::new();
                    let first = &self.line_text[start_line];
                    let first_chars = first.chars().count();
                    let s = (start_col as usize).min(first_chars);
                    out.extend(first.chars().skip(s));
                    for middle in &self.line_text[start_line + 1..end_line] {
                        out.push('\n');
                        out.push_str(middle);
                    }
                    out.push('\n');
                    let last = &self.line_text[end_line];
                    let last_chars = last.chars().count();
                    let e = (end_col as usize).min(last_chars.saturating_sub(1));
                    out.extend(last.chars().take(e + 1));
                    out
                }
            }
        }
    }

    /// Cell-column ranges to highlight for a visual-mode selection on
    /// each visible line. Returns one `(line, col_start, col_end_excl)`
    /// per affected line — `col_start..col_end_excl` is the inclusive
    /// cell range to paint REVERSED. For line-wise, the entire line is
    /// covered (cell range = `0..line_width_cells`). For char-wise:
    /// first/last lines cut at the cursor cols; middles span the whole
    /// line. Columns are char-counts, consistent with `line_text` and
    /// the existing cell-counting convention in `LineBuilder`.
    pub fn selection_cell_ranges(
        &self,
        sel: &VisualState,
        cursor_line: u16,
        cursor_col: u16,
    ) -> Vec<(u16, u16, u16)> {
        if self.line_text.is_empty() {
            return Vec::new();
        }
        let n = self.line_text.len();
        let (sl, sc, el, ec) = if (sel.anchor_line, sel.anchor_col) <= (cursor_line, cursor_col) {
            (sel.anchor_line, sel.anchor_col, cursor_line, cursor_col)
        } else {
            (cursor_line, cursor_col, sel.anchor_line, sel.anchor_col)
        };
        let sl = (sl as usize).min(n - 1);
        let el = (el as usize).min(n - 1);
        let mut out: Vec<(u16, u16, u16)> = Vec::new();
        for li in sl..=el {
            let line_chars = self.line_text[li].chars().count() as u16;
            let (a, b) = match sel.kind {
                VisualKind::Line => (0u16, line_chars),
                VisualKind::Char => {
                    let start = if li == sl { sc } else { 0 };
                    // `end` is inclusive in char-wise; convert to exclusive.
                    let end_incl = if li == el {
                        ec
                    } else {
                        line_chars.saturating_sub(1)
                    };
                    let end_excl = end_incl.saturating_add(1).min(line_chars);
                    (start.min(line_chars), end_excl)
                }
            };
            if a < b {
                out.push((li as u16, a, b));
            } else if matches!(sel.kind, VisualKind::Char) && line_chars == 0 {
                // Empty line under a multi-line selection still gets a
                // 1-cell highlight so the user sees the line is part of
                // the range. Same as vim's behavior for empty lines.
                out.push((li as u16, 0, 1));
            }
        }
        out
    }

    /// Count of links with at least one segment inside the
    /// `[scroll, scroll + height)` viewport. Drives the "yanked link 1
    /// of N" status hint when more than one link is visible.
    pub fn visible_link_count(&self, scroll: u16, height: u16) -> usize {
        let top = scroll as usize;
        let bot = top + height as usize;
        self.links
            .iter()
            .filter(|s| {
                s.segments
                    .iter()
                    .any(|seg| seg.line >= top && seg.line < bot)
            })
            .count()
    }
}

#[derive(Debug, Clone)]
pub struct LinkSlot {
    pub id: u32,
    pub href: String,
    /// Cell ranges the link's text occupies in the laid-out buffer. A
    /// single link wraps onto multiple lines as separate segments; words
    /// on the same line that belong to the same link (and the single
    /// space between them) are merged into one segment. Driven by the
    /// OSC 8 hyperlink wrapper in `draw`.
    pub segments: Vec<LinkSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSegment {
    pub line: usize,
    /// Column of the first cell of the segment, relative to the laid-out
    /// line (i.e. after any indent / list lead / blockquote `> ` prefix).
    pub col_start: u16,
    /// One past the last cell of the segment.
    pub col_end: u16,
}

#[derive(Debug, Clone)]
pub struct ImageSlot {
    pub key: ImageKey,
    /// Line index into `LaidOutBody.lines` where the image draws.
    pub line: usize,
    pub col: u16,
    pub width: u16,
    pub height: u16,
}

pub fn draw(f: &mut Frame, area: Rect, inbox: &mut InboxScreen, mode: Mode, link_pick_buf: &str) {
    let focused = inbox.focus == Pane::Reader;
    let subject = inbox
        .parsed
        .as_ref()
        .and_then(|_| selected_subject(inbox))
        .unwrap_or_default();
    let title = if subject.is_empty() {
        format!("Reader [{}]", inbox.reader_scroll)
    } else {
        format!("Reader [{}] — {subject}", inbox.reader_scroll)
    };
    let block = pane_block(&title, focused);

    let inner_width = area.width.saturating_sub(2);

    // Body changed since last frame — clear any rects we drew images
    // into so kitty / iTerm2 placements from the previous message don't
    // ghost over the new content.
    if inbox.body_changed_this_tick {
        for rect in std::mem::take(&mut inbox.last_image_rects) {
            f.render_widget(Clear, rect);
        }
        inbox.body_changed_this_tick = false;
    }

    let mut laid: Option<LaidOutBody> = None;
    let header_lines: Vec<Line<'static>>;
    let mut header_offset_this_frame: u16 = 0;
    let mut body_only_lines_this_frame: u16 = 0;

    // Gate on the search-aware selected row, not raw `inbox.scan`: a
    // global-search result may live in a folder whose scan isn't the
    // current scope's, so the old `match &inbox.scan` rendered "nothing
    // to read" for perfectly valid selections.
    let lines: Vec<Line<'static>> = if inbox.selected_message_row().is_some() {
        match &inbox.parsed {
            Some(parsed) => {
                let mut out = render_headers(inbox);
                out.push(Line::raw(""));
                let pick = if mode == Mode::LinkPick {
                    Some(link_pick_buf)
                } else {
                    None
                };
                let mut body = layout_with_images(&parsed.blocks, inner_width, pick, |k| {
                    inbox.resolved_image(k)
                });
                // Translate per-body image-slot indices into absolute
                // line indices by offsetting by the header rows.
                let offset = out.len();
                for slot in &mut body.images {
                    slot.line += offset;
                }
                header_offset_this_frame = offset.min(u16::MAX as usize) as u16;
                body_only_lines_this_frame = body.lines.len().min(u16::MAX as usize) as u16;
                header_lines = out;
                let mut combined: Vec<Line<'static>> =
                    Vec::with_capacity(header_lines.len() + body.lines.len() + 4);
                combined.extend(header_lines.iter().cloned());
                combined.extend(body.lines.iter().cloned());
                if !combined.iter().any(|l| !l.spans.is_empty())
                    && let Some(plain) = parsed.plain_fallback.as_deref()
                {
                    combined.push(dim_line("(no HTML body, showing text/plain)"));
                    for ln in plain.lines() {
                        combined.push(Line::raw(ln.to_string()));
                    }
                }
                laid = Some(body);
                combined
            }
            None => render_headers(inbox),
        }
    } else if inbox.search.is_some() {
        vec![dim_line("(no matches)")]
    } else {
        match &inbox.scan {
            ScanState::Scanning => vec![dim_line("(scanning maildir…)")],
            ScanState::Failed(err) => vec![Line::from(Span::styled(
                format!("scan failed: {err}"),
                Style::default().fg(Color::Red),
            ))],
            ScanState::Ready(_) => vec![dim_line("(nothing to read)")],
        }
    };

    // Stash body height + inner area for the `G` keybinding so the
    // keymap can pick a bottom-scroll position without re-running
    // layout. Counts pre-wrap `Line`s — heavy CSS wrap undershoots,
    // but `j` from there is fine.
    inbox.last_reader_body_lines = lines.len().min(u16::MAX as usize) as u16;
    inbox.last_reader_inner_height = area.height.saturating_sub(2);
    inbox.last_reader_inner_width = inner_width;
    inbox.last_reader_header_offset = header_offset_this_frame;
    inbox.last_reader_body_only_lines = body_only_lines_this_frame;
    // Clamp the body-relative cursor into the visible viewport so it
    // tracks "topmost visible body line" until visual mode introduces
    // independent movement. Scroll above the body (cursor below
    // viewport top) → snap cursor up; scroll past the cursor (cursor
    // above viewport bottom) → snap cursor down. Also clamp into the
    // body's actual length so `yp` never indexes off the end.
    let inner_h = inbox.last_reader_inner_height;
    let header = inbox.last_reader_header_offset;
    let body_top = inbox.reader_scroll.saturating_sub(header);
    if inner_h > 0 && body_only_lines_this_frame > 0 {
        let body_bot_excl = body_top.saturating_add(inner_h);
        // In Visual mode the cursor is the *driver* — scroll follows it.
        // Don't clamp the cursor into the viewport; that would silently
        // walk the selection back when the user scrolled. Visual entry
        // and `move_reader_cursor` already maintain the scroll-follow
        // invariant. In Normal mode the cursor is a passive shadow of
        // viewport-top, so the original clamp still applies.
        if mode != Mode::Visual {
            if inbox.reader_cursor_line < body_top {
                inbox.reader_cursor_line = body_top;
            }
            if inbox.reader_cursor_line >= body_bot_excl {
                inbox.reader_cursor_line = body_bot_excl - 1;
            }
        }
        if inbox.reader_cursor_line >= body_only_lines_this_frame {
            inbox.reader_cursor_line = body_only_lines_this_frame - 1;
        }
    }
    // Clamp `reader_cursor_col` against the real line length. Movement
    // helpers (`$`, `move_reader_cursor`) intentionally overshoot since
    // they don't have the laid-out body in hand; we true them up here.
    if let Some(laid_ref) = laid.as_ref() {
        let li = inbox.reader_cursor_line as usize;
        if let Some(line) = laid_ref.line_text.get(li) {
            let max_col = line.chars().count().saturating_sub(1) as u16;
            if inbox.reader_cursor_col > max_col {
                inbox.reader_cursor_col = max_col;
            }
        }
    }

    let widget = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((inbox.reader_scroll, 0))
        .block(block);
    f.render_widget(widget, area);

    // Overlay any resolved images on top of the paragraph at their
    // reserved rects. SlicedProtocol clips top/bottom automatically when
    // the image straddles the pane boundary after scroll.
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    if let Some(laid) = laid {
        let scroll = inbox.reader_scroll as i32;
        emit_osc8_hyperlinks(f.buffer_mut(), inner, &laid.links, scroll);
        // Visual-mode selection paint goes on top of the rendered text
        // (REVERSED modifier on covered cells). Done before the OSC 8
        // emit would have stripped escapes, but after Paragraph laid
        // the cells down. Painting cells doesn't disturb the existing
        // OSC 8 byte injections — those modify symbol strings, this
        // flips the cell style.
        if let Some(sel) = inbox.visual.as_ref() {
            paint_selection(
                f.buffer_mut(),
                inner,
                &laid,
                sel,
                PaintView {
                    header_offset: inbox.last_reader_header_offset,
                    scroll: inbox.reader_scroll,
                    cursor_line: inbox.reader_cursor_line,
                    cursor_col: inbox.reader_cursor_col,
                },
            );
        }
        let mut drawn: Vec<Rect> = Vec::new();
        for slot in &laid.images {
            let Some(resolved) = inbox.resolved_image(&slot.key) else {
                continue;
            };
            let abs_y = slot.line as i32 - scroll;
            let top = abs_y.max(0) as u16;
            let bottom = abs_y + slot.height as i32;
            if bottom <= 0 || top >= inner.height {
                continue;
            }
            let visible_top = inner.y + top.min(inner.height);
            let visible_height = (bottom.min(inner.height as i32) - top as i32).max(0) as u16;
            if visible_height == 0 {
                continue;
            }
            let visible_width = slot.width.min(inner.width.saturating_sub(slot.col));
            if visible_width == 0 {
                continue;
            }
            let rect = Rect {
                x: inner.x + slot.col,
                y: visible_top,
                width: visible_width,
                height: visible_height,
            };
            // SlicedImage.position is measured from the rect we hand it;
            // a negative y means "image starts N rows above the rect"
            // (i.e. has been scrolled off the top).
            let pos = SignedPosition::from((0_i16, (abs_y as i16).min(0)));
            let widget = SlicedImage::new(&resolved.protocol, pos);
            f.render_widget(widget, rect);
            drawn.push(rect);
        }
        inbox.last_image_rects = drawn;
    }
}

/// Paint the visual-mode selection by flipping `Modifier::REVERSED` on
/// every cell covered by the selection's body-relative range. Cells
/// receive the modifier in addition to whatever style the renderer
/// already laid down (so colored text stays colored, just inverted),
/// matching vim's visual look. Coordinates are body-relative
/// (`line_text` indices + char cols), translated to absolute rows by
/// adding `view.header_offset` and subtracting `view.scroll`.
struct PaintView {
    header_offset: u16,
    scroll: u16,
    cursor_line: u16,
    cursor_col: u16,
}

fn paint_selection(
    buf: &mut ratatui::buffer::Buffer,
    inner: Rect,
    laid: &LaidOutBody,
    sel: &VisualState,
    view: PaintView,
) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let ranges = laid.selection_cell_ranges(sel, view.cursor_line, view.cursor_col);
    let scroll = view.scroll as i32;
    for (body_line, c_start, c_end_excl) in ranges {
        let abs_line = view.header_offset as i32 + body_line as i32;
        let row_signed = abs_line - scroll;
        if row_signed < 0 || row_signed >= inner.height as i32 {
            continue;
        }
        let row = inner.y + row_signed as u16;
        let x_start = inner.x.saturating_add(c_start).min(inner.x + inner.width);
        let x_end = inner
            .x
            .saturating_add(c_end_excl)
            .min(inner.x + inner.width);
        for x in x_start..x_end {
            if let Some(cell) = buf.cell_mut((x, row)) {
                let mut style = cell.style();
                style = style.add_modifier(Modifier::REVERSED);
                cell.set_style(style);
            }
        }
    }
    // Cursor cell: always REVERSED so the user can see where the
    // active extend point sits even when (e.g. an empty line) the
    // selection range above didn't cover it.
    let abs_line = view.header_offset as i32 + view.cursor_line as i32;
    let row_signed = abs_line - scroll;
    if row_signed >= 0 && row_signed < inner.height as i32 {
        let row = inner.y + row_signed as u16;
        let x = inner
            .x
            .saturating_add(view.cursor_col)
            .min(inner.x + inner.width.saturating_sub(1));
        if let Some(cell) = buf.cell_mut((x, row)) {
            let mut style = cell.style();
            style = style.add_modifier(Modifier::REVERSED);
            cell.set_style(style);
        }
    }
}

/// Wrap each visible link segment with the OSC 8 hyperlink anchor by
/// patching the rendered buffer cells. The start sequence
/// `ESC ] 8 ; ; URL ESC \` is prepended to the first cell of the
/// segment and the close `ESC ] 8 ; ; ESC \` is appended to the last
/// cell, so the terminal sees the bytes between as part of the anchor
/// without the buffer's normal control-char stripping (which would
/// otherwise drop the escapes if we tried to embed them in a `Span`).
///
/// Capable terminals (kitty, wezterm, foot, iTerm2, recent gnome-terminal)
/// render this as a clickable / copyable hyperlink; others ignore the
/// OSC 8 anchor harmlessly and the underlined link text remains.
fn emit_osc8_hyperlinks(
    buf: &mut ratatui::buffer::Buffer,
    inner: Rect,
    links: &[LinkSlot],
    scroll: i32,
) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let inner_right = inner.x.saturating_add(inner.width);
    let inner_bottom = inner.y.saturating_add(inner.height);
    for slot in links {
        if slot.href.is_empty() {
            continue;
        }
        let start_seq = format!("\x1b]8;;{}\x1b\\", slot.href);
        for seg in &slot.segments {
            let row_signed = inner.y as i32 + seg.line as i32 - scroll;
            if row_signed < inner.y as i32 || row_signed >= inner_bottom as i32 {
                continue;
            }
            let row = row_signed as u16;
            let abs_start = inner.x.saturating_add(seg.col_start).min(inner_right);
            let abs_end = inner.x.saturating_add(seg.col_end).min(inner_right);
            if abs_start >= abs_end {
                continue;
            }
            let last_col = abs_end - 1;
            if let Some(cell) = buf.cell_mut((abs_start, row)) {
                let symbol = cell.symbol().to_string();
                cell.set_symbol(&format!("{start_seq}{symbol}"));
            }
            if let Some(cell) = buf.cell_mut((last_col, row)) {
                let symbol = cell.symbol().to_string();
                cell.set_symbol(&format!("{symbol}\x1b]8;;\x1b\\"));
            }
        }
    }
}

fn selected_subject(inbox: &InboxScreen) -> Option<String> {
    inbox.selected_message_row().and_then(|r| r.subject.clone())
}

fn render_headers(inbox: &InboxScreen) -> Vec<Line<'static>> {
    let Some(row) = inbox.selected_message_row() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(6);
    out.push(header_line(
        "From",
        row.from_addr.as_deref().unwrap_or("(unknown)"),
    ));
    out.push(header_line(
        "Subject",
        row.subject.as_deref().unwrap_or("(no subject)"),
    ));
    out.push(header_line("Folder", &row.folder));
    if !row.flags.is_empty() {
        out.push(header_line("Flags", &row.flags));
    }
    out
}

/// Walk a Block-IR tree at the given width into ratatui lines plus a
/// table of link slots. The width is the *inner* pane width (i.e. after
/// the border has been subtracted). When `link_pick` is `Some`, an
/// inverse-video `[id]` tag is emitted before each link; if the prefix
/// is non-empty, links whose id doesn't start with that prefix dim.
///
/// Image blocks always render as `[image: alt]` placeholder text; the
/// pixel-overlay flow goes through `layout_with_images`.
pub fn layout(blocks: &[Block], width: u16, link_pick: Option<&str>) -> LaidOutBody {
    layout_with_images(blocks, width, link_pick, |_| None)
}

/// Like `layout`, but consults `resolve` for every `Block::Image` and,
/// when the image is decoded, reserves `height_cells` blank lines for
/// the overlay widget and records an `ImageSlot` against the reserved
/// row range. Unresolved images fall back to the same placeholder
/// `layout` emits.
pub fn layout_with_images<'r, R>(
    blocks: &[Block],
    width: u16,
    link_pick: Option<&str>,
    mut resolve: R,
) -> LaidOutBody
where
    R: FnMut(&ImageKey) -> Option<&'r crate::ui::images::ResolvedImage>,
{
    let mut ctx = LayoutCtx::new(width, link_pick);
    // Tag each newly-pushed line with its top-level block index. The
    // catch-up pattern (extend after the block emits) means we don't
    // have to thread block-index through every `self.lines.push` and
    // `LineBuilder::flush_line` site — every line landing in `ctx.lines`
    // between `before` and `after` gets the same tag, including nested
    // pushes from `Block::Quote` / `Block::List` walkers that don't know
    // which top-level block they belong to.
    for (idx, block) in blocks.iter().enumerate() {
        ctx.emit_block(block, 0);
        while ctx.line_block_idx.len() < ctx.lines.len() {
            ctx.line_block_idx.push(Some(idx));
        }
    }
    // Pass 2: expand sentinel marker lines into reserved blanks and
    // record absolute image slots. Doing the expansion here (rather than
    // inside `emit_block`) keeps the blockquote `> ` prefix and list
    // bullet prefix on the marker line only — the reserved blanks below
    // stay clean for the overlay to paint over.
    let LayoutCtx {
        width: ctx_width,
        lines,
        line_block_idx,
        links,
        ..
    } = ctx;
    let mut out_lines: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    let mut out_images: Vec<ImageSlot> = Vec::new();
    let mut out_block_idx: Vec<Option<usize>> = Vec::with_capacity(line_block_idx.len());
    for (line, block_idx) in lines.into_iter().zip(line_block_idx) {
        if let Some((key, indent_cols)) = sentinel_key(&line) {
            if let Some(resolved) = resolve(&key) {
                let usable_w = ctx_width.saturating_sub(indent_cols);
                let cells_w = resolved.width_cells.min(usable_w).max(1);
                let cells_h = resolved.height_cells.max(1);
                let slot_line = out_lines.len();
                for _ in 0..cells_h {
                    out_lines.push(reserved_blank_line(indent_cols, cells_w));
                    out_block_idx.push(block_idx);
                }
                out_images.push(ImageSlot {
                    key,
                    line: slot_line,
                    col: indent_cols,
                    width: cells_w,
                    height: cells_h,
                });
            } else {
                out_lines.push(placeholder_from_sentinel(line, indent_cols));
                out_block_idx.push(block_idx);
            }
        } else {
            out_lines.push(line);
            out_block_idx.push(block_idx);
        }
    }
    // Per-line plain text, derived from final spans. Done here (rather
    // than during emit) so post-emit fixups — the `> ` blockquote
    // prefix in particular — are captured automatically. Visual-mode
    // selection extracts from this; it's IR-derived (via the layout
    // walker) not cell-scraped.
    let line_text: Vec<String> = out_lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    LaidOutBody {
        lines: out_lines,
        links,
        images: out_images,
        line_block_idx: out_block_idx,
        line_text,
    }
}

fn sentinel_key(line: &Line<'static>) -> Option<(ImageKey, u16)> {
    let last = line.spans.last()?;
    let raw = last.content.as_ref();
    let payload = raw.strip_prefix(IMAGE_MARKER_PREFIX)?;
    let (kind, body) = payload.split_once(':')?;
    let key = match kind {
        "cid" => ImageKey::Cid(body.to_string()),
        "data" => ImageKey::Data(body.parse::<u64>().ok()?),
        _ => return None,
    };
    // Walk leading spans that are pure whitespace (indent + quote / list
    // lead) to recover the column the image should reserve at.
    let mut indent = 0u16;
    for span in &line.spans[..line.spans.len() - 1] {
        indent = indent.saturating_add(span.content.chars().count() as u16);
    }
    Some((key, indent))
}

fn reserved_blank_line(indent: u16, width: u16) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(2);
    if indent > 0 {
        spans.push(Span::raw(" ".repeat(indent as usize)));
    }
    if width > 0 {
        spans.push(Span::raw(" ".repeat(width as usize)));
    }
    Line::from(spans)
}

fn format_sentinel(key: &ImageKey) -> String {
    match key {
        ImageKey::Cid(c) => format!("{IMAGE_MARKER_PREFIX}cid:{c}"),
        ImageKey::Data(h) => format!("{IMAGE_MARKER_PREFIX}data:{h}"),
    }
}

fn placeholder_from_sentinel(mut line: Line<'static>, indent: u16) -> Line<'static> {
    // Drop the trailing sentinel span and replace it with the visible
    // `[image: alt]` placeholder that's encoded in the second-to-last
    // span (see `emit_block`). The blockquote / list walkers will have
    // already prefixed the indent / `> ` / bullet spans.
    if let Some(sentinel) = line.spans.pop() {
        // The visible placeholder span was pushed just before the
        // sentinel; leave it as-is. Defensive: if it's missing,
        // synthesize a generic `[image]`.
        if line.spans.is_empty() {
            line.spans.push(Span::raw(" ".repeat(indent as usize)));
            line.spans.push(Span::styled(
                "[image]".to_string(),
                Style::default().fg(Color::Magenta),
            ));
        }
        let _ = sentinel;
    }
    line
}

struct LayoutCtx<'a> {
    width: u16,
    next_link_id: u32,
    lines: Vec<Line<'static>>,
    /// Parallel to `lines` during pass-1, extended after each top-level
    /// block emits via the catch-up loop in `layout_with_images`. Pass-2
    /// rebuilds it alongside the expanded `lines` vec.
    line_block_idx: Vec<Option<usize>>,
    links: Vec<LinkSlot>,
    link_pick: Option<&'a str>,
}

impl<'a> LayoutCtx<'a> {
    fn new(width: u16, link_pick: Option<&'a str>) -> Self {
        Self {
            width: width.max(8),
            next_link_id: 1,
            lines: Vec::new(),
            line_block_idx: Vec::new(),
            links: Vec::new(),
            link_pick,
        }
    }

    fn emit_block(&mut self, block: &Block, indent: u16) {
        match block {
            Block::Paragraph(runs) => {
                self.emit_inlines(runs, indent, "");
                self.lines.push(Line::raw(""));
            }
            Block::Heading { level, text } => {
                let style = Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD);
                let prefix = "#".repeat(*level as usize) + " ";
                self.emit_inlines_with_lead(text, indent, &prefix, style);
                self.lines.push(Line::raw(""));
            }
            Block::List { ordered, items } => {
                for (i, item) in items.iter().enumerate() {
                    let marker = if *ordered {
                        format!("{}. ", i + 1)
                    } else {
                        "• ".to_string()
                    };
                    for (j, inner) in item.iter().enumerate() {
                        let lead = if j == 0 { marker.as_str() } else { "  " };
                        self.emit_block_with_lead(inner, indent + 2, lead);
                    }
                }
            }
            Block::Quote(inner) => {
                for b in inner {
                    let before = self.lines.len();
                    self.emit_block(b, indent);
                    let after = self.lines.len();
                    // The `> ` insertion below adds two cells at column
                    // zero of each affected line. Link segments recorded
                    // by `push_word` use pre-prefix columns, so shift any
                    // segment that landed on a quoted line by 2 to keep
                    // OSC 8 anchors aligned with their rendered cells.
                    for slot in &mut self.links {
                        for seg in &mut slot.segments {
                            if seg.line >= before && seg.line < after {
                                seg.col_start = seg.col_start.saturating_add(2);
                                seg.col_end = seg.col_end.saturating_add(2);
                            }
                        }
                    }
                    for ln in self.lines.iter_mut().skip(before) {
                        ln.spans
                            .insert(0, Span::styled("> ", Style::default().fg(Color::DarkGray)));
                    }
                }
            }
            Block::Table { rows } => {
                let mut col_widths = vec![0usize; rows.iter().map(|r| r.len()).max().unwrap_or(0)];
                for row in rows {
                    for (i, cell) in row.iter().enumerate() {
                        let w = inline_text_len(cell);
                        if w > col_widths[i] {
                            col_widths[i] = w;
                        }
                    }
                }
                let inner_w = self.width.saturating_sub(indent) as usize;
                let total: usize = col_widths.iter().sum::<usize>() + 3 * col_widths.len();
                let scale = if total > inner_w && total > 0 {
                    inner_w as f32 / total as f32
                } else {
                    1.0
                };
                let col_widths: Vec<usize> = col_widths
                    .iter()
                    .map(|w| ((*w as f32 * scale) as usize).max(3))
                    .collect();
                for row in rows {
                    let mut spans: Vec<Span<'static>> = pad_indent_spans(indent);
                    for (i, cell) in row.iter().enumerate() {
                        let text = truncate_inline(cell, col_widths.get(i).copied().unwrap_or(8));
                        spans.push(Span::raw(text));
                        spans.push(Span::raw(" | "));
                    }
                    if !spans.is_empty() && matches!(spans.last(), Some(s) if s.content == " | ") {
                        spans.pop();
                    }
                    self.lines.push(Line::from(spans));
                }
                self.lines.push(Line::raw(""));
            }
            Block::Pre(text) => {
                for ln in text.lines() {
                    let mut spans: Vec<Span<'static>> = pad_indent_spans(indent);
                    spans.push(Span::styled(
                        ln.to_string(),
                        Style::default().fg(Color::Gray).bg(Color::Reset),
                    ));
                    self.lines.push(Line::from(spans));
                }
                self.lines.push(Line::raw(""));
            }
            Block::HRule => {
                let inner = self.width.saturating_sub(indent) as usize;
                let bar = "─".repeat(inner);
                let mut spans = pad_indent_spans(indent);
                spans.push(Span::styled(bar, Style::default().fg(Color::DarkGray)));
                self.lines.push(Line::from(spans));
            }
            Block::Image { cid, src, alt } => {
                let (label, key) = match (cid, src.as_deref()) {
                    (Some(c), _) => (
                        format!(
                            "[image: {}]",
                            if alt.is_empty() { "—" } else { alt.as_str() }
                        ),
                        Some(ImageKey::Cid(c.clone())),
                    ),
                    (None, Some(s)) if s.starts_with("http://") || s.starts_with("https://") => (
                        format!(
                            "[remote image: {}]",
                            if alt.is_empty() { s } else { alt.as_str() }
                        ),
                        None,
                    ),
                    (None, Some(s)) if s.starts_with("data:") => (
                        format!(
                            "[image: {}]",
                            if alt.is_empty() { "—" } else { alt.as_str() }
                        ),
                        Some(ImageKey::Data(crate::ui::images::data_uri_key(s))),
                    ),
                    (None, Some(_)) => (
                        format!(
                            "[image: {}]",
                            if alt.is_empty() { "—" } else { alt.as_str() }
                        ),
                        None,
                    ),
                    (None, None) => ("[image]".to_string(), None),
                };
                let mut spans = pad_indent_spans(indent);
                spans.push(Span::styled(label, Style::default().fg(Color::Magenta)));
                if let Some(k) = key {
                    spans.push(Span::raw(format_sentinel(&k)));
                }
                self.lines.push(Line::from(spans));
            }
        }
    }

    fn emit_block_with_lead(&mut self, block: &Block, indent: u16, lead: &str) {
        let before = self.lines.len();
        self.emit_block(block, indent);
        if let Some(first) = self.lines.get_mut(before) {
            // Replace the leading indent padding with the marker.
            let pad_len = indent.saturating_sub(lead.chars().count() as u16) as usize;
            let lead_span = Span::styled(
                format!("{}{}", " ".repeat(pad_len), lead),
                Style::default().fg(Color::DarkGray),
            );
            if matches!(
                first.spans.first(),
                Some(s) if s.content.chars().all(|c| c == ' ')
            ) {
                first.spans[0] = lead_span;
            } else {
                first.spans.insert(0, lead_span);
            }
        }
    }

    fn emit_inlines(&mut self, runs: &[Inline], indent: u16, lead: &str) {
        self.emit_inlines_with_lead(runs, indent, lead, Style::default());
    }

    fn emit_inlines_with_lead(
        &mut self,
        runs: &[Inline],
        indent: u16,
        lead: &str,
        base_style: Style,
    ) {
        let inner_w = self.width.saturating_sub(indent) as usize;
        let mut wrapper = LineBuilder::new(indent, lead);
        let mut tokens: Vec<Token> = Vec::new();
        flatten_inlines(
            runs,
            base_style,
            None,
            &mut tokens,
            &mut self.links,
            &mut self.next_link_id,
            self.link_pick,
        );
        for tok in tokens {
            match tok {
                Token::Word {
                    text,
                    style,
                    link_id,
                } => {
                    wrapper.push_word(
                        &text,
                        style,
                        link_id,
                        inner_w,
                        &mut self.lines,
                        &mut self.links,
                    );
                }
                Token::Space => {
                    wrapper.push_space();
                }
                Token::LineBreak => {
                    wrapper.flush_line(&mut self.lines);
                }
            }
        }
        wrapper.flush_line(&mut self.lines);
    }
}

#[derive(Debug)]
enum Token {
    Word {
        text: String,
        style: Style,
        link_id: Option<u32>,
    },
    Space,
    LineBreak,
}

fn flatten_inlines(
    runs: &[Inline],
    base: Style,
    in_link: Option<u32>,
    out: &mut Vec<Token>,
    links: &mut Vec<LinkSlot>,
    next_id: &mut u32,
    pick: Option<&str>,
) {
    for run in runs {
        match run {
            Inline::Text { content, style } => {
                let mut s = combine_style(base, *style);
                if let Some(id) = in_link
                    && let Some(prefix) = pick
                    && !prefix.is_empty()
                    && !id.to_string().starts_with(prefix)
                {
                    // Typed-prefix doesn't match → dim non-matching links so
                    // the eye lands on the candidate set.
                    s = s.fg(Color::DarkGray).remove_modifier(Modifier::UNDERLINED);
                }
                let mut buf = String::new();
                for ch in content.chars() {
                    if ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r' {
                        if !buf.is_empty() {
                            out.push(Token::Word {
                                text: std::mem::take(&mut buf),
                                style: s,
                                link_id: in_link,
                            });
                        }
                        out.push(Token::Space);
                    } else {
                        buf.push(ch);
                    }
                }
                if !buf.is_empty() {
                    out.push(Token::Word {
                        text: buf,
                        style: s,
                        link_id: in_link,
                    });
                }
            }
            Inline::Link { href, runs } => {
                let id = *next_id;
                *next_id += 1;
                links.push(LinkSlot {
                    id,
                    href: href.clone(),
                    segments: Vec::new(),
                });
                if let Some(prefix) = pick {
                    let dim = !prefix.is_empty() && !id.to_string().starts_with(prefix);
                    let tag_style = if dim {
                        Style::default().fg(Color::DarkGray)
                    } else {
                        Style::default()
                            .bg(Color::Yellow)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD)
                    };
                    out.push(Token::Word {
                        text: format!("[{id}]"),
                        style: tag_style,
                        link_id: None,
                    });
                }
                let inner_style = base.fg(Color::Blue).add_modifier(Modifier::UNDERLINED);
                flatten_inlines(runs, inner_style, Some(id), out, links, next_id, pick);
            }
            Inline::LineBreak => out.push(Token::LineBreak),
        }
    }
}

fn combine_style(base: Style, extra: InlineStyle) -> Style {
    let mut s = base;
    if extra.bold {
        s = s.add_modifier(Modifier::BOLD);
    }
    if extra.italic {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if extra.underline {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    if extra.code {
        s = s.fg(Color::Cyan);
    }
    s
}

struct LineBuilder {
    indent: u16,
    lead: String,
    line_spans: Vec<Span<'static>>,
    line_width: usize,
    pending_space: bool,
}

impl LineBuilder {
    fn new(indent: u16, lead: &str) -> Self {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut width = 0usize;
        if indent > 0 {
            spans.push(Span::raw(" ".repeat(indent as usize)));
            width += indent as usize;
        }
        if !lead.is_empty() {
            spans.push(Span::styled(
                lead.to_string(),
                Style::default().fg(Color::DarkGray),
            ));
            width += lead.chars().count();
        }
        Self {
            indent,
            lead: lead.to_string(),
            line_spans: spans,
            line_width: width,
            pending_space: false,
        }
    }

    fn push_space(&mut self) {
        if self.line_width > self.indent as usize + self.lead.chars().count() {
            self.pending_space = true;
        }
    }

    fn push_word(
        &mut self,
        text: &str,
        style: Style,
        link_id: Option<u32>,
        max_w: usize,
        out_lines: &mut Vec<Line<'static>>,
        links: &mut [LinkSlot],
    ) {
        let word_w = text.chars().count();
        let head_w = self.indent as usize + self.lead.chars().count();
        let space_w = if self.pending_space { 1 } else { 0 };
        if self.line_width + space_w + word_w > max_w && self.line_width > head_w {
            self.flush_line(out_lines);
        }
        if self.pending_space {
            self.line_spans.push(Span::raw(" "));
            self.line_width += 1;
            self.pending_space = false;
        }
        let col_start = self.line_width as u16;
        self.line_spans.push(Span::styled(text.to_string(), style));
        self.line_width += word_w;
        let col_end = self.line_width as u16;
        if let Some(id) = link_id {
            let current_line = out_lines.len();
            if let Some(slot) = links.iter_mut().find(|s| s.id == id) {
                // Merge with the prior segment on the same line if it's
                // adjacent (the only gap LineBuilder ever leaves between
                // two link words is a single space cell).
                let merged = slot.segments.last_mut().is_some_and(|seg| {
                    seg.line == current_line && seg.col_end.saturating_add(1) >= col_start
                });
                if merged {
                    let seg = slot.segments.last_mut().unwrap();
                    seg.col_end = col_end;
                } else {
                    slot.segments.push(LinkSegment {
                        line: current_line,
                        col_start,
                        col_end,
                    });
                }
            }
        }
    }

    fn flush_line(&mut self, out: &mut Vec<Line<'static>>) {
        out.push(Line::from(std::mem::take(&mut self.line_spans)));
        let mut spans: Vec<Span<'static>> = Vec::new();
        if self.indent > 0 {
            spans.push(Span::raw(" ".repeat(self.indent as usize)));
        }
        if !self.lead.is_empty() {
            // Continuation lines pad rather than re-print the marker.
            spans.push(Span::raw(" ".repeat(self.lead.chars().count())));
        }
        self.line_width = self.indent as usize + self.lead.chars().count();
        self.line_spans = spans;
        self.pending_space = false;
    }
}

fn pad_indent_spans(indent: u16) -> Vec<Span<'static>> {
    if indent == 0 {
        Vec::new()
    } else {
        vec![Span::raw(" ".repeat(indent as usize))]
    }
}

fn inline_text_len(runs: &[Inline]) -> usize {
    let mut n = 0;
    for r in runs {
        match r {
            Inline::Text { content, .. } => n += content.chars().count(),
            Inline::Link { runs, .. } => n += inline_text_len(runs),
            Inline::LineBreak => {}
        }
    }
    n
}

fn truncate_inline(runs: &[Inline], max: usize) -> String {
    let mut out = String::new();
    let mut left = max;
    fn push(out: &mut String, left: &mut usize, s: &str) {
        for ch in s.chars() {
            if *left == 0 {
                break;
            }
            out.push(ch);
            *left -= 1;
        }
    }
    fn walk(runs: &[Inline], out: &mut String, left: &mut usize) {
        for r in runs {
            if *left == 0 {
                break;
            }
            match r {
                Inline::Text { content, .. } => push(out, left, content),
                Inline::Link { runs, .. } => walk(runs, out, left),
                Inline::LineBreak => {}
            }
        }
    }
    walk(runs, &mut out, &mut left);
    out
}

fn header_line(name: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{name}: "),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(value.to_string()),
    ])
}

fn dim_line(s: &str) -> Line<'static> {
    Line::from(Span::styled(
        s.to_string(),
        Style::default().fg(Color::DarkGray),
    ))
}

#[allow(dead_code)] // surfaced via ParsedBody when step 4 lands
fn _force_use(_inbox: &InboxScreen, _mode: Mode, _parsed: &ParsedBody) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::html;

    fn layout_first_para(html_src: &str, w: u16) -> Vec<String> {
        let blocks = html::parse(html_src);
        let laid = layout(&blocks, w, None);
        laid.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn wraps_paragraph_at_width() {
        let lines = layout_first_para("<p>one two three four five six</p>", 12);
        // First non-empty line shouldn't exceed width.
        let first = lines.iter().find(|l| !l.trim().is_empty()).unwrap();
        assert!(first.chars().count() <= 12, "{first:?}");
    }

    #[test]
    fn list_emits_markers() {
        let lines = layout_first_para("<ul><li>alpha</li><li>beta</li></ul>", 40);
        let joined = lines.join("\n");
        assert!(joined.contains("• alpha"), "{joined}");
        assert!(joined.contains("• beta"), "{joined}");
    }

    #[test]
    fn quote_prefixes_with_gt() {
        let lines = layout_first_para("<blockquote><p>hello</p></blockquote>", 40);
        let joined = lines.join("\n");
        assert!(joined.contains("> hello"), "{joined}");
    }

    #[test]
    fn hrule_emits_separator() {
        let lines = layout_first_para("<hr>", 10);
        assert!(lines.iter().any(|l| l.contains("─")));
    }

    #[test]
    fn cid_image_renders_placeholder() {
        let lines = layout_first_para(r#"<img src="cid:x" alt="logo">"#, 40);
        assert!(lines.iter().any(|l| l.contains("[image: logo]")));
    }

    #[test]
    fn remote_image_renders_placeholder() {
        let lines = layout_first_para(r#"<img src="https://x.example/p" alt="pixel">"#, 40);
        assert!(lines.iter().any(|l| l.contains("[remote image: pixel]")));
    }

    #[test]
    fn link_text_is_collected() {
        let blocks = html::parse(r#"<p>see <a href="https://x">this</a> please</p>"#);
        let laid = layout(&blocks, 80, None);
        assert!(!laid.links.is_empty(), "expected at least one link");
        assert_eq!(laid.links[0].href, "https://x");
    }

    #[test]
    fn link_segment_tracks_columns_on_single_line() {
        // "see this please" — link "this" occupies cells 4..8.
        let blocks = html::parse(r#"<p>see <a href="https://x">this</a> please</p>"#);
        let laid = layout(&blocks, 80, None);
        let slot = laid.links.first().expect("link slot");
        assert_eq!(slot.segments.len(), 1, "{:?}", slot.segments);
        let seg = &slot.segments[0];
        assert_eq!(seg.line, 0);
        assert_eq!(seg.col_start, 4);
        assert_eq!(seg.col_end, 8);
    }

    #[test]
    fn link_segment_merges_multi_word_link_with_space() {
        // Link spans two words on the same line; segments should merge
        // across the connecting space cell.
        let blocks = html::parse(r#"<p><a href="https://x">foo bar</a></p>"#);
        let laid = layout(&blocks, 80, None);
        let slot = laid.links.first().expect("link slot");
        assert_eq!(slot.segments.len(), 1, "{:?}", slot.segments);
        let seg = &slot.segments[0];
        assert_eq!(seg.col_start, 0);
        // "foo" (3) + " " (1) + "bar" (3) = 7 cells.
        assert_eq!(seg.col_end, 7);
    }

    #[test]
    fn link_segment_splits_across_wrapped_lines() {
        // Force the link to wrap by giving it a narrow inner width.
        let blocks = html::parse(r#"<p><a href="https://x">foo bar baz</a></p>"#);
        let laid = layout(&blocks, 4, None);
        let slot = laid.links.first().expect("link slot");
        assert!(
            slot.segments.len() >= 2,
            "expected wrap into multiple segments: {:?}",
            slot.segments
        );
        // All segments must point at the same link id and never share a line.
        let mut lines: Vec<usize> = slot.segments.iter().map(|s| s.line).collect();
        lines.sort();
        lines.dedup();
        assert_eq!(lines.len(), slot.segments.len(), "{:?}", slot.segments);
    }

    #[test]
    fn osc8_wraps_link_cells_in_buffer() {
        use ratatui::buffer::Buffer;
        // Lay out a paragraph with one link, render it into a Buffer the
        // same way `draw` would, then run the OSC 8 patch and check that
        // the first / last link cells carry the expected escape bytes.
        let html_src = r#"<p>see <a href="https://x.example/p">click</a> done</p>"#;
        let blocks = html::parse(html_src);
        let laid = layout(&blocks, 40, None);
        let inner = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 4,
        };
        let mut buf = Buffer::empty(inner);
        // Paint the layout into the buffer cell by cell so the test
        // doesn't depend on Paragraph's wrap behavior — segment columns
        // are inner-relative, which is the contract we care about.
        for (row, line) in laid.lines.iter().enumerate() {
            if row >= inner.height as usize {
                break;
            }
            let mut col: u16 = 0;
            for span in &line.spans {
                for ch in span.content.chars() {
                    if col >= inner.width {
                        break;
                    }
                    if let Some(cell) = buf.cell_mut((col, row as u16)) {
                        cell.set_symbol(&ch.to_string());
                    }
                    col += 1;
                }
            }
        }
        super::emit_osc8_hyperlinks(&mut buf, inner, &laid.links, 0);
        // Link "click" starts at column 4 (after "see ").
        let first_cell = buf.cell((4, 0)).expect("first link cell");
        let first_sym = first_cell.symbol();
        assert!(
            first_sym.starts_with("\x1b]8;;https://x.example/p\x1b\\"),
            "first link cell symbol was {first_sym:?}"
        );
        assert!(first_sym.ends_with('c'), "{first_sym:?}");
        // Last cell of the link is column 8 ("click" cells 4..9, last = 8).
        let last_cell = buf.cell((8, 0)).expect("last link cell");
        let last_sym = last_cell.symbol();
        assert!(
            last_sym.ends_with("\x1b]8;;\x1b\\"),
            "last link cell symbol was {last_sym:?}"
        );
        assert!(last_sym.starts_with('k'), "{last_sym:?}");
        // Non-link cells must not carry any OSC 8 bytes.
        let outside = buf.cell((0, 0)).expect("first cell");
        assert!(!outside.symbol().contains('\x1b'), "{:?}", outside.symbol());
    }

    #[test]
    fn osc8_skips_scrolled_off_segments() {
        use ratatui::buffer::Buffer;
        let inner = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 2,
        };
        let mut buf = Buffer::empty(inner);
        let links = vec![LinkSlot {
            id: 1,
            href: "https://x".to_string(),
            segments: vec![LinkSegment {
                line: 0,
                col_start: 0,
                col_end: 3,
            }],
        }];
        // scroll past the segment's line — patch must be a no-op.
        super::emit_osc8_hyperlinks(&mut buf, inner, &links, 5);
        for x in 0..inner.width {
            for y in 0..inner.height {
                let sym = buf.cell((x, y)).unwrap().symbol();
                assert!(!sym.contains('\x1b'), "leaked OSC 8 at ({x},{y}): {sym:?}");
            }
        }
    }

    #[test]
    fn link_segment_inside_blockquote_shifts_by_two() {
        // The `> ` prefix is inserted after layout; segment columns must
        // be bumped so they line up with the rendered cells.
        let blocks =
            html::parse(r#"<blockquote><p><a href="https://x">click</a></p></blockquote>"#);
        let laid = layout(&blocks, 80, None);
        let slot = laid.links.first().expect("link slot");
        let seg = slot.segments.first().expect("segment");
        // Without quote shift this would be 0; with the `> ` prefix the
        // link starts at column 2 of the rendered line.
        assert_eq!(seg.col_start, 2);
        assert_eq!(seg.col_end, 7); // "click" is 5 cells, so 2..7.
        // The rendered line genuinely begins with `> `.
        let first_line = laid
            .lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("click")))
            .expect("rendered line with link");
        let rendered: String = first_line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(rendered.starts_with("> "), "{rendered:?}");
    }

    fn fake_resolved(width: u16, height: u16) -> crate::ui::images::ResolvedImage {
        // Build a halfblocks-only Picker + a tiny in-memory PNG so tests
        // exercise the real `decode` path without probing stdio.
        use image::{ImageBuffer, Rgba};
        use ratatui_image::picker::Picker;
        let picker = Picker::halfblocks();
        let img = ImageBuffer::from_fn(8, 8, |_, _| Rgba([255u8, 0, 0, 255]));
        let dyn_img = image::DynamicImage::ImageRgba8(img);
        let mut bytes: Vec<u8> = Vec::new();
        dyn_img
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        let mut resolved = crate::ui::images::decode(&picker, &bytes, 24).unwrap();
        resolved.width_cells = width;
        resolved.height_cells = height;
        resolved
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn resolved_cid_records_slot_and_reserves_lines() {
        let blocks = html::parse(r#"<img src="cid:logo" alt="epost">"#);
        let resolved = fake_resolved(8, 5);
        let key = ImageKey::Cid("logo".to_string());
        let laid = layout_with_images(&blocks, 40, None, |k| {
            if *k == key { Some(&resolved) } else { None }
        });
        assert_eq!(laid.images.len(), 1, "expected one image slot");
        let slot = &laid.images[0];
        assert_eq!(slot.key, key);
        assert_eq!(slot.width, 8);
        assert_eq!(slot.height, 5);
        // The 5 reserved lines should all be whitespace-only (no `[image: …]`).
        for i in 0..slot.height as usize {
            let l = &laid.lines[slot.line + i];
            let text = line_text(l);
            assert!(
                !text.contains("[image"),
                "reserved line {i} leaked placeholder text: {text:?}"
            );
            assert!(
                text.chars().all(|c| c == ' '),
                "reserved line {i} not blank: {text:?}"
            );
        }
    }

    #[test]
    fn quote_image_only_prefixes_marker_line() {
        let blocks = html::parse(r#"<blockquote><img src="cid:x" alt="L"></blockquote>"#);
        let resolved = fake_resolved(6, 3);
        let key = ImageKey::Cid("x".to_string());
        let laid = layout_with_images(&blocks, 40, None, |k| {
            if *k == key { Some(&resolved) } else { None }
        });
        let slot = laid.images.first().expect("image slot recorded");
        // Reserved blank lines must not carry the blockquote `> ` prefix.
        for i in 0..slot.height as usize {
            let text = line_text(&laid.lines[slot.line + i]);
            assert!(
                !text.contains(">"),
                "reserved line {i} got quote prefix: {text:?}"
            );
        }
    }

    #[test]
    fn list_image_only_prefixes_marker_line() {
        let blocks = html::parse(r#"<ul><li><img src="cid:x" alt="L"></li></ul>"#);
        let resolved = fake_resolved(6, 3);
        let key = ImageKey::Cid("x".to_string());
        let laid = layout_with_images(&blocks, 40, None, |k| {
            if *k == key { Some(&resolved) } else { None }
        });
        let slot = laid.images.first().expect("image slot recorded");
        for i in 0..slot.height as usize {
            let text = line_text(&laid.lines[slot.line + i]);
            assert!(
                !text.contains("•"),
                "reserved line {i} got list bullet: {text:?}"
            );
        }
    }

    #[test]
    fn missing_cache_entry_falls_back_to_placeholder() {
        let blocks = html::parse(r#"<img src="cid:x" alt="L">"#);
        let laid = layout_with_images(&blocks, 40, None, |_| None);
        assert!(laid.images.is_empty());
        let joined = laid
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("[image: L]"), "{joined}");
        // Sentinel suffix must not leak into the visible text.
        assert!(
            !joined.contains(IMAGE_MARKER_PREFIX),
            "sentinel leaked: {joined:?}"
        );
    }

    #[test]
    fn remote_image_records_no_slot_even_with_stale_resolver() {
        let blocks = html::parse(r#"<img src="https://x.example/p" alt="pixel">"#);
        let resolved = fake_resolved(4, 2);
        let laid = layout_with_images(&blocks, 40, None, |_| Some(&resolved));
        assert!(
            laid.images.is_empty(),
            "remote images must never get a slot"
        );
        let joined = laid
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("[remote image: pixel]"), "{joined}");
    }

    #[test]
    fn line_block_idx_aligns_with_lines() {
        let blocks = html::parse("<p>one</p><p>two</p><h2>three</h2>");
        let laid = layout(&blocks, 40, None);
        assert_eq!(
            laid.line_block_idx.len(),
            laid.lines.len(),
            "parallel vec must mirror lines length"
        );
        // Each non-empty rendered line should resolve to a real block.
        for (i, line) in laid.lines.iter().enumerate() {
            let text = line_text(line);
            if text.trim().is_empty() {
                continue;
            }
            assert!(
                laid.line_block_idx[i].is_some(),
                "line {i} ({text:?}) had no block tag"
            );
        }
        // The line containing "one" must be tagged with block 0;
        // "three" with block 2.
        let one = laid
            .lines
            .iter()
            .position(|l| line_text(l).contains("one"))
            .unwrap();
        assert_eq!(laid.line_block_idx[one], Some(0));
        let three = laid
            .lines
            .iter()
            .position(|l| line_text(l).contains("three"))
            .unwrap();
        assert_eq!(laid.line_block_idx[three], Some(2));
    }

    #[test]
    fn block_at_resolves_cursor_to_top_level_block() {
        let blocks = html::parse("<p>one</p><blockquote><p>quoted</p></blockquote><p>tail</p>");
        let laid = layout(&blocks, 40, None);
        let quoted_line = laid
            .lines
            .iter()
            .position(|l| line_text(l).contains("quoted"))
            .expect("quoted line");
        // The quote is the second top-level block (index 1) and a
        // cursor on the quoted line must resolve to it.
        assert_eq!(laid.block_at(quoted_line as u16), Some(1));
    }

    #[test]
    fn first_link_at_or_after_skips_earlier_links() {
        // Three links on three separate paragraphs so they're on
        // distinct lines without wrapping.
        let html_src = r#"<p><a href="https://a">A</a></p>
            <p><a href="https://b">B</a></p>
            <p><a href="https://c">C</a></p>"#;
        let blocks = html::parse(html_src);
        let laid = layout(&blocks, 40, None);
        // Cursor above the first link: first match is link A.
        let first = laid.first_link_at_or_after(0).expect("any link");
        assert_eq!(first.href, "https://a");
        // Cursor on the line of link B: jumps to B, not A.
        let b_line = laid.links[1].segments.first().expect("b segment").line as u16;
        let second = laid.first_link_at_or_after(b_line).expect("any link");
        assert_eq!(second.href, "https://b");
    }

    #[test]
    fn first_link_at_or_after_falls_back_when_cursor_past_last() {
        let blocks = html::parse(r#"<p><a href="https://only">only</a></p>"#);
        let laid = layout(&blocks, 40, None);
        // Cursor below the only link: still yanks it (the fallback that
        // makes `yl` "just work" when the user is scrolled past the
        // single link in the body).
        let s = laid.first_link_at_or_after(9999).expect("fallback link");
        assert_eq!(s.href, "https://only");
    }

    #[test]
    fn visible_link_count_filters_by_viewport() {
        let html_src = r#"<p><a href="https://a">A</a></p>
            <p><a href="https://b">B</a></p>"#;
        let blocks = html::parse(html_src);
        let laid = layout(&blocks, 40, None);
        let a_line = laid.links[0].segments[0].line as u16;
        let b_line = laid.links[1].segments[0].line as u16;
        // Viewport tight enough to include only A.
        assert_eq!(laid.visible_link_count(a_line, 1), 1);
        // Wide enough to cover both.
        assert_eq!(laid.visible_link_count(a_line, b_line - a_line + 1), 2);
        // Past both links.
        assert_eq!(laid.visible_link_count(b_line + 5, 5), 0);
    }

    #[test]
    fn line_block_idx_survives_image_expansion() {
        let blocks = html::parse(r#"<p>before</p><img src="cid:x" alt="L"><p>after</p>"#);
        let resolved = fake_resolved(4, 3);
        let key = ImageKey::Cid("x".to_string());
        let laid = layout_with_images(&blocks, 40, None, |k| {
            if *k == key { Some(&resolved) } else { None }
        });
        assert_eq!(laid.line_block_idx.len(), laid.lines.len());
        // Image is block 1 (zero-indexed). All reserved blank lines
        // from its expansion must carry block_idx = Some(1) so a yp
        // there resolves back to the image's alt text.
        let slot = laid.images.first().expect("image slot");
        for i in 0..slot.height as usize {
            assert_eq!(
                laid.line_block_idx[slot.line + i],
                Some(1),
                "reserved blank line {i} lost its block tag"
            );
        }
    }

    #[test]
    fn link_pick_renders_overlay_tags() {
        let html_src = r#"<p>see <a href="https://x/a">A</a> and <a href="https://x/b">B</a></p>"#;
        let blocks = html::parse(html_src);
        let laid = layout(&blocks, 80, Some(""));
        let joined: String = laid
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("[1]"), "{joined:?}");
        assert!(joined.contains("[2]"), "{joined:?}");
        // Without pick mode, tags should NOT appear.
        let laid2 = layout(&blocks, 80, None);
        let joined2: String = laid2
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(!joined2.contains("[1]"), "{joined2:?}");
    }

    #[test]
    fn line_text_aligns_with_lines() {
        let blocks = html::parse("<p>one</p><p>two</p>");
        let laid = layout(&blocks, 40, None);
        assert_eq!(
            laid.line_text.len(),
            laid.lines.len(),
            "line_text must mirror lines length"
        );
        // Plain text for the "one" line must match the line's joined spans.
        for (i, line) in laid.lines.iter().enumerate() {
            let expected = line_text(line);
            assert_eq!(laid.line_text[i], expected, "mismatch at line {i}");
        }
    }

    #[test]
    fn line_text_captures_blockquote_prefix() {
        // Blockquote `> ` is inserted post-emit; line_text must include
        // it so visual selection yields the rendered form.
        let blocks = html::parse("<blockquote><p>hi</p></blockquote>");
        let laid = layout(&blocks, 40, None);
        assert!(
            laid.line_text.iter().any(|s| s.contains("> hi")),
            "line_text didn't capture quote prefix: {:?}",
            laid.line_text
        );
    }

    #[test]
    fn extract_selection_line_wise_joins_with_newline() {
        let blocks = html::parse("<p>alpha</p><p>beta</p><p>gamma</p>");
        let laid = layout(&blocks, 80, None);
        // Pick the lines containing each word, line-wise from alpha → gamma.
        let a = laid
            .line_text
            .iter()
            .position(|s| s.contains("alpha"))
            .unwrap() as u16;
        let g = laid
            .line_text
            .iter()
            .position(|s| s.contains("gamma"))
            .unwrap() as u16;
        let s = laid.extract_selection(a, 0, g, 0, VisualKind::Line);
        // Includes every line between (including the empty separator
        // lines our paragraph emit leaves behind).
        assert!(s.contains("alpha"), "{s:?}");
        assert!(s.contains("beta"), "{s:?}");
        assert!(s.contains("gamma"), "{s:?}");
        assert!(s.contains('\n'));
    }

    #[test]
    fn extract_selection_char_wise_single_line() {
        let blocks = html::parse("<p>hello world</p>");
        let laid = layout(&blocks, 40, None);
        // Find the line, grab "world" by char index.
        let li = laid
            .line_text
            .iter()
            .position(|s| s.contains("hello"))
            .unwrap() as u16;
        let line = &laid.line_text[li as usize];
        let start = line.find("world").unwrap() as u16;
        let end = start + "world".chars().count() as u16 - 1;
        let s = laid.extract_selection(li, start, li, end, VisualKind::Char);
        assert_eq!(s, "world");
    }

    #[test]
    fn extract_selection_char_wise_multi_line() {
        // Span two paragraphs: from middle of "hello" to middle of "world".
        let blocks = html::parse("<p>hello</p><p>world</p>");
        let laid = layout(&blocks, 80, None);
        let l1 = laid
            .line_text
            .iter()
            .position(|s| s.contains("hello"))
            .unwrap() as u16;
        let l2 = laid
            .line_text
            .iter()
            .position(|s| s.contains("world"))
            .unwrap() as u16;
        // From "llo" through "wor": col 2 of l1 → col 2 of l2.
        let s = laid.extract_selection(l1, 2, l2, 2, VisualKind::Char);
        assert!(s.starts_with("llo"), "{s:?}");
        assert!(s.ends_with("wor"), "{s:?}");
        assert!(s.contains('\n'));
    }

    #[test]
    fn extract_selection_normalizes_reversed_endpoints() {
        let blocks = html::parse("<p>hello</p>");
        let laid = layout(&blocks, 40, None);
        let li = laid
            .line_text
            .iter()
            .position(|s| s.contains("hello"))
            .unwrap() as u16;
        // Anchor > cursor: should produce same output as cursor > anchor.
        let fwd = laid.extract_selection(li, 0, li, 4, VisualKind::Char);
        let rev = laid.extract_selection(li, 4, li, 0, VisualKind::Char);
        assert_eq!(fwd, rev);
        assert_eq!(fwd, "hello");
    }

    #[test]
    fn extract_selection_clamps_col_overshoot() {
        // `$` sets cursor_col = u16::MAX; extraction must clamp.
        let blocks = html::parse("<p>hi</p>");
        let laid = layout(&blocks, 40, None);
        let li = laid
            .line_text
            .iter()
            .position(|s| s.contains("hi"))
            .unwrap() as u16;
        let s = laid.extract_selection(li, 0, li, u16::MAX, VisualKind::Char);
        assert_eq!(s, "hi");
    }

    #[test]
    fn selection_cell_ranges_line_wise_covers_full_line() {
        let blocks = html::parse("<p>hello</p>");
        let laid = layout(&blocks, 40, None);
        let li = laid
            .line_text
            .iter()
            .position(|s| s.contains("hello"))
            .unwrap() as u16;
        let sel = VisualState {
            kind: VisualKind::Line,
            anchor_line: li,
            anchor_col: 0,
        };
        let ranges = laid.selection_cell_ranges(&sel, li, 0);
        assert_eq!(ranges.len(), 1);
        let (l, a, b) = ranges[0];
        assert_eq!(l, li);
        assert_eq!(a, 0);
        // "hello" has 5 chars.
        assert_eq!(b, 5);
    }

    #[test]
    fn selection_cell_ranges_char_wise_clamps_to_cursor() {
        let blocks = html::parse("<p>abcdef</p>");
        let laid = layout(&blocks, 40, None);
        let li = laid
            .line_text
            .iter()
            .position(|s| s.contains("abcdef"))
            .unwrap() as u16;
        // Anchor at col 1, cursor at col 3 → covers cols 1, 2, 3 (3 cells).
        let sel = VisualState {
            kind: VisualKind::Char,
            anchor_line: li,
            anchor_col: 1,
        };
        let ranges = laid.selection_cell_ranges(&sel, li, 3);
        assert_eq!(ranges.len(), 1);
        let (_, a, b) = ranges[0];
        assert_eq!(a, 1);
        assert_eq!(b, 4); // exclusive end
    }
}
