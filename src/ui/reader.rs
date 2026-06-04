use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui_image::sliced::{SignedPosition, SlicedImage};
use unicode_width::UnicodeWidthStr;

use crate::mail::html::{Block, Inline, InlineStyle};
use crate::ui::app::{InboxScreen, Mode, Pane, ParsedBody, ScanState, VisualKind, VisualState};
use crate::ui::images::ImageKey;
use crate::ui::style::{pane_block, pane_scrollbar};

/// Brief "highlight on yank" state. After `yip` / `yap` / `yl` / a visual-mode `y`
/// fires, the yanked region's body-relative cell ranges are stashed here
/// alongside an `Instant` deadline; the next `tick` re-runs layout, paints
/// a yellow-on-black flash over the covered cells, and clears the highlight
/// once `expires_at` is past. Mirrors vim's `vim-highlightedyank`. The flash
/// is deliberately a background color, *not* `Modifier::REVERSED` — a visual
/// selection already renders REVERSED, so reusing it would give no feedback
/// when yanking straight out of visual mode.
///
/// Ranges are body-relative (`line_text` row, char cols) so a scroll
/// after the yank still lands the flash in the right place — the painter
/// translates to absolute rows via `header_offset` + `scroll` each
/// frame.
#[derive(Debug)]
pub struct YankHighlight {
    pub ranges: Vec<(u16, u16, u16)>,
    pub expires_at: Instant,
}

/// Display width, in terminal cells, of `s`. This is the ruler the
/// terminal and ratatui's `Wrap` actually use — it counts a zero-width
/// space (U+200B), combining marks, and variation selectors as 0, and a
/// wide / emoji-presentation grapheme as 2. `chars().count()` is *not*
/// this width (it miscounts every such codepoint by ±1), and using it as
/// a column proxy is what drove misaligned wraps, cursors, and OSC 8
/// anchors on mail that carries those characters.
pub(crate) fn cells(s: &str) -> u16 {
    UnicodeWidthStr::width(s).min(u16::MAX as usize) as u16
}

/// Display column (0-based cell offset) of the char at index `char_idx`
/// within `line` — i.e. the cell width of the prefix preceding it.
/// Measured as a *string* prefix rather than a per-char width sum so that
/// emoji + variation-selector sequences (e.g. `⬆\u{FE0F}`) count once.
/// Maps a logical cursor / selection char position to the cell it paints.
pub(crate) fn cell_col(line: &str, char_idx: usize) -> u16 {
    let byte = line
        .char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(line.len());
    cells(&line[..byte])
}

/// Inverse of `cell_col`: the char index whose cell falls at or just
/// before display column `target`. Maps a mouse click's cell column back
/// onto a logical char position so `reader_cursor_col` stays a char index
/// regardless of where the click landed.
pub(crate) fn char_at_cell(line: &str, target: u16) -> u16 {
    let count = line.chars().count();
    for i in 0..count {
        if cell_col(line, i + 1) > target {
            return i as u16;
        }
    }
    count as u16
}

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
    /// One slot per attachment, in attachment order. Each slot's `line`
    /// is the body-relative row that renders it (attachments are emitted
    /// as the first body lines). Drives `gx`/`gs` cursor resolution and
    /// the `gf` picker. Empty when the message has no attachments.
    pub attachments: Vec<AttachmentSlot>,
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
        // Block normalizes rows and columns *independently* (the rectangle
        // between the two corners), unlike the lexicographic ordering the
        // char/line kinds use. Handle it before that normalize.
        if matches!(kind, VisualKind::Block) {
            let r0 = (anchor_line.min(cursor_line) as usize).min(n - 1);
            let r1 = (anchor_line.max(cursor_line) as usize).min(n - 1);
            let c0 = anchor_col.min(cursor_col) as usize;
            let c1 = anchor_col.max(cursor_col) as usize;
            let rows: Vec<String> = (r0..=r1)
                .map(|li| {
                    let line = &self.line_text[li];
                    let len = line.chars().count();
                    let lo = c0.min(len);
                    let hi = (c1 + 1).min(len); // c1 inclusive
                    line.chars().skip(lo).take(hi.saturating_sub(lo)).collect()
                })
                .collect();
            return rows.join("\n");
        }
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
            // Block is handled by the early return above.
            VisualKind::Block => unreachable!("block selection handled above"),
        }
    }

    /// Cell-column ranges to highlight for a visual-mode selection on
    /// each visible line. Returns one `(line, col_start, col_end_excl)`
    /// per affected line — `col_start..col_end_excl` is the inclusive
    /// cell range to paint REVERSED. For line-wise, the entire line is
    /// covered (cell range = `0..line_width_cells`). For char-wise:
    /// first/last lines cut at the cursor cols; middles span the whole
    /// line. Returned columns are display cells (not char indices), so
    /// the painter aligns with the rendered grid even on lines carrying
    /// zero-width or wide characters.
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
        // Block: independent row/col normalize (a rectangle). Each row is
        // sliced to the same `[c0, c1]` char range; rows shorter than
        // `c0` emit nothing (no empty-line fallback, unlike char-wise).
        if matches!(sel.kind, VisualKind::Block) {
            let r0 = (sel.anchor_line.min(cursor_line) as usize).min(n - 1);
            let r1 = (sel.anchor_line.max(cursor_line) as usize).min(n - 1);
            let c0 = sel.anchor_col.min(cursor_col) as usize;
            let c1 = sel.anchor_col.max(cursor_col) as usize;
            let mut out: Vec<(u16, u16, u16)> = Vec::new();
            for li in r0..=r1 {
                let line = &self.line_text[li];
                let len = line.chars().count();
                let lo = c0.min(len);
                let hi = (c1 + 1).min(len); // c1 inclusive
                if hi > lo {
                    let a = cell_col(line, lo);
                    let b = cell_col(line, hi);
                    out.push((li as u16, a, b));
                }
            }
            return out;
        }
        let (sl, sc, el, ec) = if (sel.anchor_line, sel.anchor_col) <= (cursor_line, cursor_col) {
            (sel.anchor_line, sel.anchor_col, cursor_line, cursor_col)
        } else {
            (cursor_line, cursor_col, sel.anchor_line, sel.anchor_col)
        };
        let sl = (sl as usize).min(n - 1);
        let el = (el as usize).min(n - 1);
        let mut out: Vec<(u16, u16, u16)> = Vec::new();
        for li in sl..=el {
            let line = &self.line_text[li];
            let line_chars = line.chars().count() as u16;
            // Endpoints below are char indices into `line`; convert to
            // display-cell columns before pushing so the painter lands on
            // the right cells even past a zero-width / wide character.
            let (a_char, b_char) = match sel.kind {
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
                // Block is handled by the early return above.
                VisualKind::Block => unreachable!("block selection handled above"),
            };
            if a_char < b_char {
                let a = cell_col(line, a_char as usize);
                let b = cell_col(line, b_char as usize);
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

    /// Cell ranges for every line whose `line_block_idx` matches
    /// `block_idx`. Each entry is `(line, 0, line_width_cells)`. Used by
    /// the yank-highlight painter for `yp` so the flash covers the full
    /// resolved paragraph regardless of the cursor's column.
    pub fn block_ranges(&self, block_idx: usize) -> Vec<(u16, u16, u16)> {
        let mut out = Vec::new();
        for (i, bi) in self.line_block_idx.iter().enumerate() {
            if *bi == Some(block_idx) {
                let width = self.line_text.get(i).map(|s| cells(s)).unwrap_or(0);
                if width > 0 {
                    out.push((i as u16, 0, width));
                }
            }
        }
        out
    }

    /// Cell ranges for each segment of a single `LinkSlot`. Mirrors the
    /// in-buffer ranges OSC 8 already uses so the yank-highlight flash
    /// lines up exactly with the link text.
    pub fn link_segment_ranges(slot: &LinkSlot) -> Vec<(u16, u16, u16)> {
        slot.segments
            .iter()
            .filter_map(|seg| {
                if seg.col_end > seg.col_start {
                    Some((seg.line as u16, seg.col_start, seg.col_end))
                } else {
                    None
                }
            })
            .collect()
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

#[derive(Debug, Clone)]
pub struct AttachmentSlot {
    /// Body-relative line that renders this attachment's chip row. The
    /// slot's position in `LaidOutBody.attachments` is the attachment's
    /// 0-based index.
    pub line: u16,
}

pub fn draw(
    f: &mut Frame,
    area: Rect,
    inbox: &mut InboxScreen,
    mode: Mode,
    link_pick_buf: &str,
    attach_pick_buf: &str,
) {
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
    let mut attachment_lines_this_frame: Vec<u16> = Vec::new();

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
                let attach_pick = if mode == Mode::AttachmentPick {
                    Some(attach_pick_buf)
                } else {
                    None
                };
                let mut body = layout_with_images(
                    &parsed.blocks,
                    inner_width,
                    &parsed.attachments,
                    pick,
                    attach_pick,
                    |k| inbox.resolved_image(k),
                );
                // Attachment chip rows (+ blank separator) occupy the
                // first `prefix_len` body lines; the plain-text fallback
                // fires only when the HTML walk produced nothing beyond
                // them.
                let prefix_len = if body.attachments.is_empty() {
                    0
                } else {
                    body.attachments.len() + 1
                };
                attachment_lines_this_frame = body.attachments.iter().map(|s| s.line).collect();
                if body.lines.len() == prefix_len
                    && let Some(plain) = parsed.plain_fallback.as_deref()
                {
                    body.lines
                        .push(dim_line("(no HTML body, showing text/plain)"));
                    body.line_text.push(String::new());
                    body.line_block_idx.push(None);
                    for ln in plain.lines() {
                        body.lines.push(Line::raw(ln.to_string()));
                        body.line_text.push(ln.to_string());
                        body.line_block_idx.push(None);
                    }
                }
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
                    Vec::with_capacity(header_lines.len() + body.lines.len());
                combined.extend(header_lines.iter().cloned());
                combined.extend(body.lines.iter().cloned());
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
    let pane_inner_height = area.height.saturating_sub(2);
    inbox.last_reader_inner_height = pane_inner_height;
    inbox.last_reader_inner_width = inner_width;
    inbox.last_reader_header_offset = header_offset_this_frame;
    inbox.last_reader_body_only_lines = body_only_lines_this_frame;
    inbox.last_attachment_lines = attachment_lines_this_frame;
    inbox.last_reader_body_line_text = laid
        .as_ref()
        .map(|l| l.line_text.clone())
        .unwrap_or_default();
    inbox.last_reader_inner = Some(Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: pane_inner_height,
    });
    // Keep the body-relative cursor inside the visible viewport. Cursor
    // *moves* (`j`/`k`, page keys, visual) already maintain this via
    // `follow_cursor`, so this is a no-op for them; it earns its keep
    // after pure-scroll input (`Ctrl-e`/`Ctrl-y`, mouse wheel) by
    // sliding the cursor to the viewport edge, matching vim. Also clamps
    // into the body's actual length so `yp` never indexes off the end.
    let inner_h = inbox.last_reader_inner_height;
    let header = inbox.last_reader_header_offset;
    let body_top = inbox.reader_scroll.saturating_sub(header);
    if inner_h > 0 && body_only_lines_this_frame > 0 {
        let body_bot_excl = body_top.saturating_add(inner_h);
        // In Visual mode the cursor is the *driver* — scroll follows it.
        // Don't clamp the cursor into the viewport; that would silently
        // walk the selection back when the user scrolled past an edge to
        // extend it. Visual entry and `move_reader_cursor` already keep
        // the scroll-follow invariant.
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
    // True up the *live* column (`reader_cursor_col`) against the real
    // line length. Movement helpers (`$`, `move_reader_cursor`)
    // intentionally overshoot — the `$`/curswant sentinel rides EOL —
    // since they don't have the laid-out body in hand. This only touches
    // the live column, never `reader_goal_col`: a vertical move
    // re-sources the live column from the goal, so clamping it here is
    // harmless and the goal column survives short/empty lines.
    if let Some(laid_ref) = laid.as_ref() {
        let li = inbox.reader_cursor_line as usize;
        if let Some(line) = laid_ref.line_text.get(li) {
            let max_col = line.chars().count().saturating_sub(1) as u16;
            if inbox.reader_cursor_col > max_col {
                inbox.reader_cursor_col = max_col;
            }
        }
    }

    let total_lines = lines.len();
    // Render the pane border first, then the body Paragraph (no block)
    // into the inner area. Attachments now render inline at the top of
    // the body, so there's no reserved bottom strip to carve around.
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: pane_inner_height,
    };
    f.render_widget(block, area);
    let widget = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((inbox.reader_scroll, 0));
    f.render_widget(widget, inner);
    pane_scrollbar(f, area, inbox.reader_scroll as usize, total_lines, focused);
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
        } else if focused && mode == Mode::Normal {
            // Show the reader cursor in Normal mode: it's a real,
            // independently-movable cursor (`j`/`k` etc.), and marks where
            // `v` / `V` will start, where `yp` / `yl` act, and which
            // attachment `gx` / `gs` target. Renders as a REVERSED cell.
            paint_cursor_cell(
                f.buffer_mut(),
                inner,
                &laid,
                inbox.reader_cursor_line,
                inbox.reader_cursor_col,
                inbox.last_reader_header_offset,
                inbox.reader_scroll,
            );
        }
        if let Some(hl) = inbox.yank_highlight.as_ref()
            && hl.expires_at > Instant::now()
        {
            paint_yank_highlight(
                f.buffer_mut(),
                inner,
                &hl.ranges,
                inbox.last_reader_header_offset,
                inbox.reader_scroll,
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
    // selection range above didn't cover it. Same paint as Normal
    // mode's cursor — extracted into `paint_cursor_cell` so both
    // modes stay visually consistent.
    paint_cursor_cell(
        buf,
        inner,
        laid,
        view.cursor_line,
        view.cursor_col,
        view.header_offset,
        view.scroll,
    );
}

/// Paint a single REVERSED cell at the body-relative cursor position.
/// Shared between Normal mode (where the cursor advertises the start
/// point for `v` / `V` / `yp` / `yl`) and Visual mode (where it marks
/// the active extend end of the selection).
///
/// The cursor col is capped against the line's real text length so an
/// unclamped sentinel (`move_reader_cursor_to_line_end` sets `u16::MAX`)
/// can't park the cursor cell at the right edge of an otherwise-short
/// line — that was the "selection extends to end of screen" report.
fn paint_cursor_cell(
    buf: &mut ratatui::buffer::Buffer,
    inner: Rect,
    laid: &LaidOutBody,
    cursor_line: u16,
    cursor_col: u16,
    header_offset: u16,
    scroll: u16,
) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let abs_line = header_offset as i32 + cursor_line as i32;
    let row_signed = abs_line - scroll as i32;
    if row_signed < 0 || row_signed >= inner.height as i32 {
        return;
    }
    let row = inner.y + row_signed as u16;
    let line = laid.line_text.get(cursor_line as usize);
    let line_chars = line.map(|s| s.chars().count() as u16).unwrap_or(0);
    // `cursor_col` is a char index; cap it to the line then translate to
    // the display cell it occupies, and reverse the grapheme's full cell
    // width (2 for a wide char) so the cursor doesn't half-cover it.
    let max_col = line_chars.saturating_sub(1);
    let capped_col = cursor_col.min(max_col);
    let (cell_start, cell_w) = match line {
        Some(s) => {
            let start = cell_col(s, capped_col as usize);
            let end = cell_col(s, capped_col as usize + 1);
            (start, end.saturating_sub(start).max(1))
        }
        None => (capped_col, 1),
    };
    let x0 = inner.x.saturating_add(cell_start);
    let x_max = inner.x + inner.width.saturating_sub(1);
    for dx in 0..cell_w {
        let x = x0.saturating_add(dx).min(x_max);
        if let Some(cell) = buf.cell_mut((x, row)) {
            let mut style = cell.style();
            style = style.add_modifier(Modifier::REVERSED);
            cell.set_style(style);
        }
    }
}

/// Paint a transient yank highlight as a yellow-on-black flash over
/// every cell covered by `ranges`. Same coordinate convention as
/// `paint_selection`: ranges are body-relative `(line, col_start,
/// col_end_excl)`, translated to absolute rows via `header_offset` +
/// `scroll`. No cursor cell — the highlight is purely the yanked
/// region, no extending end-point to advertise. A background color
/// (not `Modifier::REVERSED`) so the flash reads as feedback even when
/// the yank came straight out of a REVERSED visual selection.
fn paint_yank_highlight(
    buf: &mut ratatui::buffer::Buffer,
    inner: Rect,
    ranges: &[(u16, u16, u16)],
    header_offset: u16,
    scroll: u16,
) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let scroll = scroll as i32;
    for (body_line, c_start, c_end_excl) in ranges {
        let abs_line = header_offset as i32 + *body_line as i32;
        let row_signed = abs_line - scroll;
        if row_signed < 0 || row_signed >= inner.height as i32 {
            continue;
        }
        let row = inner.y + row_signed as u16;
        let x_start = inner.x.saturating_add(*c_start).min(inner.x + inner.width);
        let x_end = inner
            .x
            .saturating_add(*c_end_excl)
            .min(inner.x + inner.width);
        for x in x_start..x_end {
            if let Some(cell) = buf.cell_mut((x, row)) {
                let style = cell.style().bg(Color::Yellow).fg(Color::Black);
                cell.set_style(style);
            }
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
pub fn layout(
    blocks: &[Block],
    width: u16,
    attachments: &[crate::mail::parse::Attachment],
    link_pick: Option<&str>,
    attach_pick: Option<&str>,
) -> LaidOutBody {
    layout_with_images(blocks, width, attachments, link_pick, attach_pick, |_| None)
}

/// Like `layout`, but consults `resolve` for every `Block::Image` and,
/// when the image is decoded, reserves `height_cells` blank lines for
/// the overlay widget and records an `ImageSlot` against the reserved
/// row range. Unresolved images fall back to the same placeholder
/// `layout` emits.
pub fn layout_with_images<'r, R>(
    blocks: &[Block],
    width: u16,
    attachments: &[crate::mail::parse::Attachment],
    link_pick: Option<&str>,
    attach_pick: Option<&str>,
    mut resolve: R,
) -> LaidOutBody
where
    R: FnMut(&ImageKey) -> Option<&'r crate::ui::images::ResolvedImage>,
{
    let mut ctx = LayoutCtx::new(width, link_pick);
    // Attachments render as the first body lines (one chip row each)
    // followed by a blank separator, so the reader cursor can land on
    // them (`gx`/`gs`) and the `gf` picker can tag them. Emitted before
    // the block walk so their `line_block_idx` is `None` and the image
    // two-pass leaves their indices untouched (no sentinels up front).
    let mut attach_slots: Vec<AttachmentSlot> = Vec::with_capacity(attachments.len());
    if !attachments.is_empty() {
        for (i, a) in attachments.iter().enumerate() {
            attach_slots.push(AttachmentSlot {
                line: ctx.lines.len().min(u16::MAX as usize) as u16,
            });
            ctx.lines
                .push(attachment_chip_line(i, a, ctx.width, attach_pick));
            ctx.line_block_idx.push(None);
        }
        ctx.lines.push(Line::raw(""));
        ctx.line_block_idx.push(None);
    }
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
        attachments: attach_slots,
    }
}

/// Build one attachment chip row for the reader body: `Attach: [n]
/// filename (size)` on the first row, indented under `Attach:` for the
/// rest. When `pick` is `Some`, the `[n]` tag is highlighted (yellow on
/// black) for candidates matching the typed prefix and dimmed otherwise,
/// mirroring the link picker's tag grammar. `n` is 1-based.
fn attachment_chip_line(
    i: usize,
    att: &crate::mail::parse::Attachment,
    width: u16,
    pick: Option<&str>,
) -> Line<'static> {
    const LABEL: &str = "Attach: ";
    let n = i + 1;
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
    // Align continuation rows under the first row's chips.
    let lead = if i == 0 {
        LABEL.to_string()
    } else {
        " ".repeat(LABEL.len())
    };
    spans.push(Span::styled(
        lead,
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    let tag_style = match pick {
        Some(prefix) if prefix.is_empty() || n.to_string().starts_with(prefix) => Style::default()
            .bg(Color::Yellow)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
        Some(_) => Style::default().fg(Color::DarkGray),
        None => Style::default().fg(Color::DarkGray),
    };
    spans.push(Span::styled(format!("[{n}]"), tag_style));
    spans.push(Span::raw(" "));
    spans.push(Span::raw(att.filename.clone()));
    spans.push(Span::styled(
        format!(" ({})", human_size(att.bytes.len())),
        Style::default().fg(Color::DarkGray),
    ));
    let _ = width; // chips truncate at the pane edge via the no-wrap Paragraph.
    Line::from(spans)
}

/// Compact human-readable byte size (e.g. `12 KB`, `3.4 MB`). Uses
/// 1024-based units; one decimal for MB+ to keep big files legible.
fn human_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    const GB: usize = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{} KB", bytes.div_ceil(KB))
    } else {
        format!("{bytes} B")
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
                // Reserve 2 cells for the `> ` prefix prepended below.
                // Without this, content wraps to the full pane width and
                // the prefix pushes `line_text` past the pane edge —
                // ratatui then auto-wraps the row to a second visible row
                // while `paint_selection` paints the first row entirely,
                // which a user reads as "highlight to end of screen" on
                // `$` / mouse-EOL. Nests correctly: a quote inside a
                // quote sees a width already reduced by the outer level.
                let saved_width = self.width;
                self.width = self.width.saturating_sub(2).max(8);
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
                self.width = saved_width;
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
                // Hard-wrap lines wider than the pane into multiple
                // laid-out lines so `paint_selection` doesn't see a
                // single `line_text` longer than `inner_width` (which
                // would paint the row fully REVERSED to the right edge
                // while ratatui's Paragraph auto-wraps the rendered
                // text to the row below). Preformatted whitespace is
                // preserved within each chunk.
                let usable = self.width.saturating_sub(indent).max(1) as usize;
                let style = Style::default().fg(Color::Gray).bg(Color::Reset);
                for ln in text.lines() {
                    let chars: Vec<char> = ln.chars().collect();
                    if chars.is_empty() {
                        self.lines.push(Line::from(pad_indent_spans(indent)));
                        continue;
                    }
                    for chunk in chars.chunks(usable) {
                        let mut spans: Vec<Span<'static>> = pad_indent_spans(indent);
                        spans.push(Span::styled(chunk.iter().collect::<String>(), style));
                        self.lines.push(Line::from(spans));
                    }
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
            width += cells(lead) as usize;
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
        if self.line_width > self.indent as usize + cells(&self.lead) as usize {
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
        let word_w = cells(text) as usize;
        let head_w = self.indent as usize + cells(&self.lead) as usize;
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
            spans.push(Span::raw(" ".repeat(cells(&self.lead) as usize)));
        }
        self.line_width = self.indent as usize + cells(&self.lead) as usize;
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
        let laid = layout(&blocks, w, &[], None, None);
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
    fn cells_ignores_zero_width_and_counts_wide() {
        // Zero-width space contributes 0 cells; emoji + VS16 is one wide
        // grapheme (2 cells). These are exactly the chars that broke the
        // old chars().count() width model.
        assert_eq!(cells("ab"), 2);
        assert_eq!(cells("a\u{200b}b"), 2, "ZWSP must be width 0");
        assert_eq!(cells("\u{2b06}\u{fe0f}"), 2, "emoji+VS16 is width 2");
    }

    #[test]
    fn cell_col_and_char_at_cell_roundtrip_past_zwsp() {
        // "a<ZWSP>bc": char indices 0=a 1=ZWSP 2=b 3=c map to cells
        // 0,1,1,2 — the ZWSP sits at the same cell as the following 'b'.
        let line = "a\u{200b}bc";
        assert_eq!(cell_col(line, 0), 0);
        assert_eq!(cell_col(line, 1), 1); // before ZWSP
        assert_eq!(cell_col(line, 2), 1); // before 'b' — ZWSP added nothing
        assert_eq!(cell_col(line, 3), 2); // before 'c'
        // A click on cell 1 lands on the ZWSP/'b' boundary → char 'b' (2).
        assert_eq!(char_at_cell(line, 1), 2);
        assert_eq!(char_at_cell(line, 0), 0);
    }

    #[test]
    fn wrapping_uses_display_width_not_char_count() {
        // A paragraph whose words carry zero-width spaces must wrap by
        // visible width, so the rendered line never exceeds the pane.
        let z = "\u{200b}";
        let html = format!("<p>aa{z} bb{z} cc{z} dd{z} ee{z} ff{z}</p>");
        let laid = layout(&html::parse(&html), 8, &[], None, None);
        for line in &laid.lines {
            let t: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(cells(&t) <= 8, "line exceeds 8 display cells: {t:?}");
        }
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
    fn block_ranges_covers_all_lines_tagged_with_block_idx() {
        // Two paragraphs → two top-level blocks. block_ranges(1) should
        // return ranges only for lines whose line_block_idx is Some(1).
        let blocks = html::parse("<p>one</p><p>two three</p>");
        let laid = layout(&blocks, 80, &[], None, None);
        let r0 = laid.block_ranges(0);
        let r1 = laid.block_ranges(1);
        assert!(!r0.is_empty(), "block 0 should have ranges");
        assert!(!r1.is_empty(), "block 1 should have ranges");
        // Each entry is (line, 0, width) — col_start is always 0 for
        // whole-block highlighting.
        for (_, c_start, _) in r0.iter().chain(r1.iter()) {
            assert_eq!(*c_start, 0);
        }
        // Ranges for the two blocks must not overlap on the same line.
        let lines0: Vec<u16> = r0.iter().map(|(l, _, _)| *l).collect();
        let lines1: Vec<u16> = r1.iter().map(|(l, _, _)| *l).collect();
        for l in &lines1 {
            assert!(!lines0.contains(l), "line {l} in both blocks");
        }
    }

    #[test]
    fn block_ranges_widths_match_line_text_char_counts() {
        let blocks = html::parse("<p>hello</p>");
        let laid = layout(&blocks, 80, &[], None, None);
        let ranges = laid.block_ranges(0);
        for (line, _, width) in &ranges {
            let actual = laid.line_text[*line as usize].chars().count() as u16;
            assert_eq!(*width, actual, "line {line} width drift");
        }
    }

    #[test]
    fn link_segment_ranges_maps_to_segments() {
        // Link spans one row; ranges should mirror the LinkSegment.
        let blocks = html::parse(r#"<p>see <a href="https://x">this</a></p>"#);
        let laid = layout(&blocks, 80, &[], None, None);
        let slot = laid.links.first().expect("link slot");
        let ranges = LaidOutBody::link_segment_ranges(slot);
        assert_eq!(ranges.len(), 1);
        assert_eq!(
            ranges[0],
            (0u16, slot.segments[0].col_start, slot.segments[0].col_end)
        );
    }

    #[test]
    fn link_segment_ranges_skips_zero_width_segments() {
        // Build a LinkSlot by hand with one valid + one degenerate
        // segment; the helper should drop the empty one.
        let slot = LinkSlot {
            id: 1,
            href: "https://x".into(),
            segments: vec![
                LinkSegment {
                    line: 0,
                    col_start: 0,
                    col_end: 5,
                },
                LinkSegment {
                    line: 1,
                    col_start: 3,
                    col_end: 3,
                },
            ],
        };
        let ranges = LaidOutBody::link_segment_ranges(&slot);
        assert_eq!(ranges, vec![(0u16, 0u16, 5u16)]);
    }

    #[test]
    fn link_text_is_collected() {
        let blocks = html::parse(r#"<p>see <a href="https://x">this</a> please</p>"#);
        let laid = layout(&blocks, 80, &[], None, None);
        assert!(!laid.links.is_empty(), "expected at least one link");
        assert_eq!(laid.links[0].href, "https://x");
    }

    #[test]
    fn link_segment_tracks_columns_on_single_line() {
        // "see this please" — link "this" occupies cells 4..8.
        let blocks = html::parse(r#"<p>see <a href="https://x">this</a> please</p>"#);
        let laid = layout(&blocks, 80, &[], None, None);
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
        let laid = layout(&blocks, 80, &[], None, None);
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
        let laid = layout(&blocks, 4, &[], None, None);
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
        let laid = layout(&blocks, 40, &[], None, None);
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
        let laid = layout(&blocks, 80, &[], None, None);
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
        let laid = layout_with_images(&blocks, 40, &[], None, None, |k| {
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
        let laid = layout_with_images(&blocks, 40, &[], None, None, |k| {
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
        let laid = layout_with_images(&blocks, 40, &[], None, None, |k| {
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
        let laid = layout_with_images(&blocks, 40, &[], None, None, |_| None);
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
        let laid = layout_with_images(&blocks, 40, &[], None, None, |_| Some(&resolved));
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
        let laid = layout(&blocks, 40, &[], None, None);
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
        let laid = layout(&blocks, 40, &[], None, None);
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
        let laid = layout(&blocks, 40, &[], None, None);
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
        let laid = layout(&blocks, 40, &[], None, None);
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
        let laid = layout(&blocks, 40, &[], None, None);
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
        let laid = layout_with_images(&blocks, 40, &[], None, None, |k| {
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
        let laid = layout(&blocks, 80, &[], Some(""), None);
        let joined: String = laid
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("[1]"), "{joined:?}");
        assert!(joined.contains("[2]"), "{joined:?}");
        // Without pick mode, tags should NOT appear.
        let laid2 = layout(&blocks, 80, &[], None, None);
        let joined2: String = laid2
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(!joined2.contains("[1]"), "{joined2:?}");
    }

    #[test]
    fn attachments_render_as_leading_body_rows() {
        let att = vec![
            crate::mail::parse::Attachment {
                filename: "report.pdf".into(),
                bytes: vec![0u8; 2048],
            },
            crate::mail::parse::Attachment {
                filename: "logo.png".into(),
                bytes: vec![0u8; 10],
            },
        ];
        let blocks = html::parse("<p>hello</p>");
        let laid = layout(&blocks, 80, &att, None, None);
        // Two slots, in order, on the first two body lines.
        assert_eq!(laid.attachments.len(), 2);
        assert_eq!(laid.attachments[0].line, 0);
        assert_eq!(laid.attachments[1].line, 1);
        // First chip row carries the label, tag, filename and size.
        assert!(
            laid.line_text[0].contains("Attach:"),
            "{:?}",
            laid.line_text[0]
        );
        assert!(laid.line_text[0].contains("[1]"), "{:?}", laid.line_text[0]);
        assert!(
            laid.line_text[0].contains("report.pdf"),
            "{:?}",
            laid.line_text[0]
        );
        assert!(
            laid.line_text[0].contains("2 KB"),
            "{:?}",
            laid.line_text[0]
        );
        // Second row is a continuation (indented, no repeated label).
        assert!(laid.line_text[1].contains("[2]"), "{:?}", laid.line_text[1]);
        assert!(
            !laid.line_text[1].contains("Attach:"),
            "{:?}",
            laid.line_text[1]
        );
        // Chip rows belong to no source block, so yp resolves past them.
        assert_eq!(laid.line_block_idx[0], None);
        assert_eq!(laid.line_block_idx[1], None);
        // The HTML body still renders, after the chips + blank separator.
        assert!(
            laid.line_text.iter().any(|t| t.contains("hello")),
            "body should follow the chips"
        );
    }

    #[test]
    fn attachment_pick_tags_only_in_pick_mode() {
        let att = vec![crate::mail::parse::Attachment {
            filename: "a.txt".into(),
            bytes: vec![1, 2, 3],
        }];
        let blocks = html::parse("<p>x</p>");
        // Tag always shows the index; pick mode just restyles it. Confirm
        // the chip row exists with the `[1]` tag and a byte size.
        let laid = layout(&blocks, 80, &att, None, Some(""));
        assert!(laid.line_text[0].contains("[1]"));
        assert!(laid.line_text[0].contains("3 B"), "{:?}", laid.line_text[0]);
    }

    #[test]
    fn line_text_aligns_with_lines() {
        let blocks = html::parse("<p>one</p><p>two</p>");
        let laid = layout(&blocks, 40, &[], None, None);
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
        let laid = layout(&blocks, 40, &[], None, None);
        assert!(
            laid.line_text.iter().any(|s| s.contains("> hi")),
            "line_text didn't capture quote prefix: {:?}",
            laid.line_text
        );
    }

    #[test]
    fn extract_selection_line_wise_joins_with_newline() {
        let blocks = html::parse("<p>alpha</p><p>beta</p><p>gamma</p>");
        let laid = layout(&blocks, 80, &[], None, None);
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

    fn block_body() -> LaidOutBody {
        LaidOutBody {
            lines: Vec::new(),
            links: Vec::new(),
            images: Vec::new(),
            line_block_idx: Vec::new(),
            line_text: vec!["foobar".into(), "bazqux".into(), "hello".into()],
            attachments: Vec::new(),
        }
    }

    #[test]
    fn extract_selection_block_is_rectangular() {
        let laid = block_body();
        // Columns 1..=3 across all three rows → "oob" / "azq" / "ell".
        let s = laid.extract_selection(0, 1, 2, 3, VisualKind::Block);
        assert_eq!(s, "oob\nazq\nell");
        // Reversed corners give the same rectangle (independent normalize).
        let rev = laid.extract_selection(2, 3, 0, 1, VisualKind::Block);
        assert_eq!(rev, s);
    }

    #[test]
    fn extract_selection_block_clamps_short_rows() {
        let mut laid = block_body();
        laid.line_text = vec!["longline".into(), "ab".into()];
        // Cols 3..=6: row0 → "glin", row1 "ab" is too short → empty slice.
        let s = laid.extract_selection(0, 3, 1, 6, VisualKind::Block);
        assert_eq!(s, "glin\n");
    }

    #[test]
    fn selection_cell_ranges_block_per_row() {
        let laid = block_body();
        let sel = VisualState {
            kind: VisualKind::Block,
            anchor_line: 0,
            anchor_col: 1,
        };
        let ranges = laid.selection_cell_ranges(&sel, 2, 3);
        // Each row gets cols [1, 4) (c1=3 inclusive → exclusive 4).
        assert_eq!(ranges, vec![(0, 1, 4), (1, 1, 4), (2, 1, 4)]);
    }

    #[test]
    fn extract_selection_char_wise_single_line() {
        let blocks = html::parse("<p>hello world</p>");
        let laid = layout(&blocks, 40, &[], None, None);
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
        let laid = layout(&blocks, 80, &[], None, None);
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
        let laid = layout(&blocks, 40, &[], None, None);
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
        let laid = layout(&blocks, 40, &[], None, None);
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
        let laid = layout(&blocks, 40, &[], None, None);
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
        let laid = layout(&blocks, 40, &[], None, None);
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

    #[test]
    fn selection_with_overshooting_anchor_does_not_extend_past_line_end() {
        // Repro for the "selection goes to end of screen" report: a press
        // past the line's text leaves `mouse_drag_anchor` (and the new
        // `visual.anchor_col`) at a viewport column larger than the line's
        // actual char count. Dragging back into the line content (or `$`
        // afterwards) must still cap the highlight at the line's length —
        // the painter should never light up cells beyond the IR's view of
        // the line.
        let blocks = html::parse("<p>hi</p>");
        let laid = layout(&blocks, 80, &[], None, None);
        let li = laid
            .line_text
            .iter()
            .position(|s| s == "hi")
            .expect("hi line present");
        let line_chars = laid.line_text[li].chars().count() as u16;
        // Anchor far past the line's last char (as the mouse press at
        // viewport col 50 would do on a 2-char line). Cursor anywhere
        // inside the line — drag back to col 0.
        let sel = VisualState {
            kind: VisualKind::Char,
            anchor_line: li as u16,
            anchor_col: 50,
        };
        let ranges = laid.selection_cell_ranges(&sel, li as u16, 0);
        // One range, capped at line length.
        assert_eq!(ranges.len(), 1);
        let (_, _, end_excl) = ranges[0];
        assert!(
            end_excl <= line_chars,
            "end_excl ({end_excl}) leaked past line length ({line_chars})"
        );
    }

    #[test]
    fn paint_selection_for_dollar_does_not_reverse_cells_past_text() {
        // Full-stack repro: layout a short paragraph on a wide pane, run
        // `paint_selection` with the same args the live draw uses for `v`
        // + `$`, and inspect the Buffer to confirm cells past the line's
        // last character don't carry the REVERSED modifier.
        use ratatui::buffer::Buffer;
        let blocks = html::parse("<p>hello</p>");
        let inner_width: u16 = 40;
        let laid = layout(&blocks, inner_width, &[], None, None);
        let li = laid
            .line_text
            .iter()
            .position(|s| s.contains("hello"))
            .expect("hello line present") as u16;
        let line_chars = laid.line_text[li as usize].chars().count() as u16;

        let inner = Rect {
            x: 0,
            y: 0,
            width: inner_width,
            height: 6,
        };
        let mut buf = Buffer::empty(inner);
        let sel = VisualState {
            kind: VisualKind::Char,
            anchor_line: li,
            anchor_col: 0,
        };
        // Cursor at the sentinel `$` sets — the draw-time clamp normally
        // brings this back to `line_chars - 1` before this point, so
        // simulate post-clamp.
        let cursor_col_clamped = line_chars.saturating_sub(1);
        super::paint_selection(
            &mut buf,
            inner,
            &laid,
            &sel,
            super::PaintView {
                header_offset: 0,
                scroll: 0,
                cursor_line: li,
                cursor_col: cursor_col_clamped,
            },
        );
        // Cells [0, line_chars) on the line's row should be REVERSED.
        // Cells [line_chars, inner_width) should NOT.
        let row = inner.y + li;
        for col in 0..line_chars {
            let cell = buf.cell((inner.x + col, row)).expect("cell in range");
            assert!(
                cell.style().add_modifier.contains(Modifier::REVERSED),
                "col {col} expected REVERSED but wasn't (modifier = {:?})",
                cell.style().add_modifier
            );
        }
        for col in line_chars..inner_width {
            let cell = buf.cell((inner.x + col, row)).expect("cell in range");
            assert!(
                !cell.style().add_modifier.contains(Modifier::REVERSED),
                "col {col} REVERSED but should be clean past line end (line_chars={line_chars})"
            );
        }
    }

    #[test]
    fn paint_selection_with_unclamped_cursor_col_does_not_leak_past_text() {
        // The cursor-cell paint must not light up cells past the line's
        // actual text length even when the caller passes an unclamped
        // `cursor_col` (e.g. straight from `move_reader_cursor_to_line_end`
        // before the draw-time clamp has run, or any path where the clamp
        // is skipped). This is the "highlight extends to end of screen"
        // regression report.
        use ratatui::buffer::Buffer;
        let blocks = html::parse("<p>hello</p>");
        let inner_width: u16 = 40;
        let laid = layout(&blocks, inner_width, &[], None, None);
        let li = laid
            .line_text
            .iter()
            .position(|s| s.contains("hello"))
            .expect("hello line present") as u16;
        let line_chars = laid.line_text[li as usize].chars().count() as u16;
        let inner = Rect {
            x: 0,
            y: 0,
            width: inner_width,
            height: 6,
        };
        let mut buf = Buffer::empty(inner);
        let sel = VisualState {
            kind: VisualKind::Char,
            anchor_line: li,
            anchor_col: 0,
        };
        super::paint_selection(
            &mut buf,
            inner,
            &laid,
            &sel,
            super::PaintView {
                header_offset: 0,
                scroll: 0,
                cursor_line: li,
                cursor_col: u16::MAX,
            },
        );
        let row = inner.y + li;
        for col in line_chars..inner_width {
            let cell = buf.cell((inner.x + col, row)).expect("cell in range");
            assert!(
                !cell.style().add_modifier.contains(Modifier::REVERSED),
                "col {col} REVERSED but should be clean past line end with unclamped cursor"
            );
        }
        // The cursor cell itself should land on the last text character,
        // not the right edge of the pane.
        let last_char_cell = buf
            .cell((inner.x + line_chars - 1, row))
            .expect("last char cell");
        assert!(
            last_char_cell
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "cursor cell should be at last text char (col {})",
            line_chars - 1
        );
    }

    #[test]
    fn blockquote_lines_fit_within_inner_width() {
        // Blockquoted content used to wrap to the full pane width and
        // then prepend `> `, pushing `line_text` past the pane edge.
        // After the fix, the wrap target is reduced by 2 so the post-
        // prefix line fits exactly. Realistic multi-word content; the
        // degenerate "single word wider than the pane" case is handled
        // separately by `LineBuilder`'s overflow rule.
        let blocks = html::parse(
            "<blockquote><p>Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.</p></blockquote>",
        );
        let inner_width: u16 = 80;
        let laid = layout(&blocks, inner_width, &[], None, None);
        for (i, t) in laid.line_text.iter().enumerate() {
            let n = t.chars().count();
            assert!(
                n <= inner_width as usize,
                "blockquote line {i} has {n} chars, wider than pane ({inner_width}): {:?}",
                t,
            );
        }
    }

    #[test]
    fn paragraphs_and_pre_line_widths_at_dev_width() {
        // Diagnostic: enumerate every block type and assert that none
        // produce `line_text` wider than the inner pane. Lines wider than
        // `inner_width` cause `paint_selection` (and ratatui's Paragraph
        // wrap) to paint the body_line's row fully REVERSED to the right
        // edge — the "highlight extends to end of screen" report. The
        // failure output identifies the offending block and line.
        let cases = [
            (
                "plain paragraph",
                "<p>If you can read this in the reader pane, the Block-IR walker is rendering into ratatui cells correctly.</p>",
            ),
            (
                "blockquote that packs tight",
                "<blockquote><p>alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega plus more text here</p></blockquote>",
            ),
            (
                "pre with a long line",
                "<pre>fn really_long_function_name_that_exceeds_eighty_columns_for_sure_indeed_yes_it_does() { todo!() }</pre>",
            ),
            (
                "ordered list",
                "<ol><li>first</li><li>second</li><li>third</li><li>fourth</li><li>fifth</li><li>sixth</li><li>seventh</li><li>eighth</li><li>ninth</li><li>tenth</li></ol>",
            ),
        ];
        let inner_width: u16 = 80;
        let mut bad: Vec<(String, usize, usize, String)> = Vec::new();
        for (name, html) in cases {
            let blocks = html::parse(html);
            let laid = layout(&blocks, inner_width, &[], None, None);
            for (i, t) in laid.line_text.iter().enumerate() {
                let n = t.chars().count();
                if n > inner_width as usize {
                    bad.push((name.to_string(), i, n, t.clone()));
                }
            }
        }
        if !bad.is_empty() {
            for (name, i, n, t) in &bad {
                eprintln!("OVERFLOW in {name} line {i}: {n} chars: {t:?}");
            }
            panic!("{} overflowing lines", bad.len());
        }
    }

    #[test]
    fn full_draw_dollar_eol_highlight_matches_text_extent() {
        // End-to-end repro: render Paragraph the way `draw` does, then
        // call `paint_selection` with cursor_col = u16::MAX (the sentinel
        // `$` sets) to mirror the case where the in-draw clamp got
        // skipped or applied a stale `line_text` length. The test
        // assertions show exactly which cells get REVERSED so we can see
        // a regression where the highlight leaks past the line's text.
        use ratatui::buffer::Buffer;
        use ratatui::text::Line;
        use ratatui::widgets::{Paragraph, Wrap};
        let blocks = html::parse(
            "<p>If you can read this in the reader pane, the Block-IR walker is rendering into ratatui cells correctly.</p>",
        );
        let inner_width: u16 = 80;
        let laid = layout(&blocks, inner_width, &[], None, None);
        // Pick the first wrapped line and find its actual char count.
        let li_idx = laid
            .line_text
            .iter()
            .position(|s| !s.is_empty())
            .expect("non-empty line");
        let line_chars = laid.line_text[li_idx].chars().count() as u16;
        let inner = Rect {
            x: 0,
            y: 0,
            width: inner_width,
            height: 12,
        };
        let mut buf = Buffer::empty(inner);
        // Render the laid-out paragraph the same way the live draw does.
        let lines: Vec<Line<'static>> = laid.lines.clone();
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        ratatui::widgets::Widget::render(paragraph, inner, &mut buf);
        // Simulate `$`: enter visual at (li, 0), cursor at (li, u16::MAX).
        let sel = VisualState {
            kind: VisualKind::Char,
            anchor_line: li_idx as u16,
            anchor_col: 0,
        };
        super::paint_selection(
            &mut buf,
            inner,
            &laid,
            &sel,
            super::PaintView {
                header_offset: 0,
                scroll: 0,
                cursor_line: li_idx as u16,
                cursor_col: u16::MAX,
            },
        );
        let row = inner.y + li_idx as u16;
        // Walk the row: text cells must be REVERSED; cells past line end
        // must not be.
        let reversed: Vec<bool> = (0..inner_width)
            .map(|col| {
                buf.cell((inner.x + col, row))
                    .map(|c| c.style().add_modifier.contains(Modifier::REVERSED))
                    .unwrap_or(false)
            })
            .collect();
        for col in 0..line_chars {
            assert!(
                reversed[col as usize],
                "col {col} should be REVERSED (text cell); line_chars={line_chars}"
            );
        }
        for col in line_chars..inner_width {
            assert!(
                !reversed[col as usize],
                "col {col} REVERSED but should be clean past line end (line_chars={line_chars})"
            );
        }
    }

    #[test]
    fn paint_cursor_cell_reverses_single_cell_in_normal_mode() {
        // Normal-mode cursor: one REVERSED cell at (cursor_line, cursor_col)
        // and nothing else on the row. Mirrors the call site in `draw`
        // when the Reader pane is focused and mode is Normal.
        use ratatui::buffer::Buffer;
        let blocks = html::parse("<p>hello world</p>");
        let laid = layout(&blocks, 80, &[], None, None);
        let li = laid
            .line_text
            .iter()
            .position(|s| s.contains("hello"))
            .expect("hello line present") as u16;
        let inner = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 6,
        };
        let mut buf = Buffer::empty(inner);
        super::paint_cursor_cell(&mut buf, inner, &laid, li, 0, 0, 0);
        let row = inner.y + li;
        let line_chars = laid.line_text[li as usize].chars().count() as u16;
        for col in 0..inner.width {
            let cell = buf.cell((inner.x + col, row)).expect("cell");
            let rev = cell.style().add_modifier.contains(Modifier::REVERSED);
            if col == 0 {
                assert!(rev, "cursor cell at col 0 should be REVERSED");
            } else {
                assert!(
                    !rev,
                    "col {col} REVERSED but only the cursor cell should be (line_chars={line_chars})"
                );
            }
        }
    }

    #[test]
    fn paint_cursor_cell_clamps_unset_col_to_line_end() {
        // u16::MAX is the `move_reader_cursor_to_line_end` sentinel.
        // The helper must cap against the line's actual char count
        // rather than the pane's inner width — same guard the visual-
        // mode cursor cell already enforces.
        use ratatui::buffer::Buffer;
        let blocks = html::parse("<p>hi</p>");
        let laid = layout(&blocks, 80, &[], None, None);
        let li = laid
            .line_text
            .iter()
            .position(|s| s.contains("hi"))
            .expect("hi line present") as u16;
        let line_chars = laid.line_text[li as usize].chars().count() as u16;
        let inner = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 6,
        };
        let mut buf = Buffer::empty(inner);
        super::paint_cursor_cell(&mut buf, inner, &laid, li, u16::MAX, 0, 0);
        let row = inner.y + li;
        let last_text_col = line_chars.saturating_sub(1);
        for col in 0..inner.width {
            let cell = buf.cell((inner.x + col, row)).expect("cell");
            let rev = cell.style().add_modifier.contains(Modifier::REVERSED);
            if col == last_text_col {
                assert!(
                    rev,
                    "cursor cell should land on last text col, not the pane edge"
                );
            } else {
                assert!(!rev, "col {col} REVERSED but should be clean");
            }
        }
    }

    #[test]
    fn dollar_at_eol_does_not_extend_past_line_end() {
        // Direct repro of `$` semantics: cursor_col = u16::MAX (set by
        // `move_reader_cursor_to_line_end`); the painter must clamp the
        // highlight to the line's char count, not the pane's inner width.
        let blocks = html::parse("<p>hello world</p>");
        let laid = layout(&blocks, 80, &[], None, None);
        let li = laid
            .line_text
            .iter()
            .position(|s| s.contains("hello"))
            .expect("hello line present") as u16;
        let line_chars = laid.line_text[li as usize].chars().count() as u16;
        let sel = VisualState {
            kind: VisualKind::Char,
            anchor_line: li,
            anchor_col: 0,
        };
        let ranges = laid.selection_cell_ranges(&sel, li, u16::MAX);
        assert_eq!(ranges.len(), 1);
        let (_, start, end_excl) = ranges[0];
        assert_eq!(start, 0);
        assert_eq!(end_excl, line_chars);
    }
}
