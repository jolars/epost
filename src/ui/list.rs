use jiff::{Timestamp, Zoned};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};
use unicode_width::UnicodeWidthStr;

use crate::store::index::MessageRow;
use crate::store::thread::ThreadedRow;
use crate::ui::app::{InboxScreen, Pane, ScanState};
use crate::ui::search::SearchKind;
use crate::ui::style::{pane_block, pane_scrollbar};

/// Cells of margin to keep visible above / below the selected row.
/// Vim's `scrolloff` semantic: walking the cursor *inside* the viewport
/// doesn't move the viewport, but once the cursor lands within this band
/// of the edge the viewport scrolls to maintain the margin.
const SCROLL_PADDING: usize = 2;

/// Selected-row marker. The `List` widget reserves this glyph's display
/// width as a left gutter on *every* row (blank-padded on the unselected
/// ones), so row layout must budget against `inner_width - its width` or the
/// right-flushed date gets clipped.
const LIST_HIGHLIGHT_SYMBOL: &str = "▌ ";

pub fn draw(f: &mut Frame, area: Rect, inbox: &mut InboxScreen) {
    let focused = inbox.focus == Pane::List;
    let block = pane_block("Messages", focused);

    // Record the inner height so Ctrl-d/u/f/b can size their page step.
    inbox.last_list_inner_height = area.height.saturating_sub(2);

    // Search results take over the list pane when active. Flat, no
    // threading — fzf-style. Each row prefixed with the folder (and
    // account when the scope crosses accounts) so cross-folder mixes
    // stay readable.
    if inbox.search.is_some() {
        draw_search(f, area, inbox, block, focused);
        return;
    }

    let initial_offset = inbox.list_offset;
    let selected_in = inbox.selected;
    let mut new_offset = initial_offset;

    match &inbox.scan {
        ScanState::Scanning => {
            let widget = List::new(vec![ListItem::new(Line::from(Span::styled(
                "Scanning maildir…",
                Style::default().fg(Color::DarkGray),
            )))])
            .block(block);
            f.render_widget(widget, area);
        }
        ScanState::Failed(err) => {
            let widget = List::new(vec![ListItem::new(Line::from(Span::styled(
                format!("scan failed: {err}"),
                Style::default().fg(Color::Red),
            )))])
            .block(block);
            f.render_widget(widget, area);
        }
        ScanState::Ready(rows) if rows.is_empty() => {
            let widget = List::new(vec![ListItem::new(Line::from(Span::styled(
                "INBOX is empty.",
                Style::default().fg(Color::DarkGray),
            )))])
            .block(block);
            f.render_widget(widget, area);
        }
        ScanState::Ready(rows) => {
            // Only the visible window is turned into `ListItem`s. Building one
            // per folder row every frame (render_row does date formatting,
            // unicode-width math, truncation, span allocs) was O(folder) per
            // draw even though ratatui only paints the viewport. We own the
            // scroll math now (clamp_offset), so the slice is all we render.
            let height = (area.height.saturating_sub(2) as usize).max(1);
            let row_width = (area.width.saturating_sub(2) as usize)
                .saturating_sub(disp_w(LIST_HIGHLIGHT_SYMBOL));
            let now = Zoned::now();
            let selected = selected_in.min(rows.len() - 1);
            let offset = clamp_offset(initial_offset, selected, height, rows.len());
            let end = (offset + height).min(rows.len());
            let items: Vec<ListItem> = rows[offset..end]
                .iter()
                .map(|t| ListItem::new(render_row(t, row_width, &now)))
                .collect();
            // Both fg and bg are set so the highlight uniformly overrides the
            // per-Span colors used in `render_row` (date=DarkGray, from=Cyan,
            // bold-unread); otherwise those bleed through and the selected
            // row reads as a multicolor stripe rather than a single block.
            let highlight = if focused {
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(Color::DarkGray).fg(Color::Gray)
            };
            // No `.scroll_padding` here: items is already the windowed slice
            // rendered from the top, so the selection is window-relative and
            // ratatui must not re-scroll inside it.
            let widget = List::new(items)
                .block(block)
                .highlight_style(highlight)
                .highlight_symbol(LIST_HIGHLIGHT_SYMBOL);
            let mut state = ListState::default();
            state.select(Some(selected - offset));
            f.render_stateful_widget(widget, area, &mut state);
            new_offset = offset;
            pane_scrollbar(f, area, selected, rows.len(), focused);
            paint_list_visual(f, area, inbox, offset, selected, rows.len());
        }
    }

    inbox.list_offset = new_offset;
}

/// Vim `scrolloff`-style offset clamp. Slides `offset` the minimum amount so
/// the `selected` row keeps at least `SCROLL_PADDING` rows of margin from each
/// visible edge, then pins it within `0 ..= len - height` so the window never
/// runs past the end of the list. Replaces ratatui's `List::scroll_padding`,
/// which we can no longer lean on now that we render only the visible slice
/// (its math assumes the item vec is the whole folder). Returns `0` for an
/// empty list or zero-height viewport.
fn clamp_offset(offset: usize, selected: usize, height: usize, len: usize) -> usize {
    if height == 0 || len == 0 {
        return 0;
    }
    // Bound the margin so the top and bottom bands can't overlap on a short
    // viewport (otherwise the two adjustments below fight each other).
    let pad = SCROLL_PADDING.min(height.saturating_sub(1) / 2);
    let max_offset = len.saturating_sub(height);
    let mut offset = offset.min(max_offset);
    if selected < offset + pad {
        offset = selected.saturating_sub(pad);
    } else if selected + pad + 1 > offset + height {
        offset = (selected + pad + 1).saturating_sub(height);
    }
    offset.min(max_offset)
}

/// Reverse-video band over the rows in the active list-visual selection.
/// The cursor row keeps the `List` widget's own highlight so the active
/// end stays distinct; every other row in the range gets
/// `Modifier::REVERSED`, mirroring the reader's visual-mode paint. No-op
/// when no multi-select is active. `offset` is the list's resolved top
/// row, `selected` the cursor row, `len` the row count.
fn paint_list_visual(
    f: &mut Frame,
    area: Rect,
    inbox: &InboxScreen,
    offset: usize,
    selected: usize,
    len: usize,
) {
    let Some(anchor) = inbox.list_visual else {
        return;
    };
    // Need room for the 1-cell border on every side.
    if len == 0 || area.width < 3 || area.height < 3 {
        return;
    }
    let sel = selected.min(len - 1);
    let a = anchor.min(len - 1);
    let (lo, hi) = (a.min(sel), a.max(sel));
    let x0 = area.x + 1;
    let x1 = area.x + area.width - 1; // exclusive: last col is the border
    let top = area.y + 1;
    let bottom = area.y + area.height - 1; // exclusive: bottom border row
    let buf = f.buffer_mut();
    for i in lo..=hi {
        if i == sel || i < offset {
            continue;
        }
        let y = top + (i - offset) as u16;
        if y >= bottom {
            break;
        }
        for x in x0..x1 {
            if let Some(cell) = buf.cell_mut((x, y)) {
                let style = cell.style().add_modifier(Modifier::REVERSED);
                cell.set_style(style);
            }
        }
    }
}

fn draw_search(
    f: &mut Frame,
    area: Rect,
    inbox: &mut InboxScreen,
    block: ratatui::widgets::Block<'static>,
    focused: bool,
) {
    let initial_offset = inbox.list_offset;
    let selected_in = inbox.selected;
    let mut new_offset = initial_offset;
    {
        let s = inbox
            .search
            .as_ref()
            .expect("draw_search called without an active search");
        if s.results.is_empty() {
            let msg = if s.query.is_empty() {
                "no messages in scope"
            } else {
                "no matches"
            };
            let widget = List::new(vec![ListItem::new(Line::from(Span::styled(
                msg,
                Style::default().fg(Color::DarkGray),
            )))])
            .block(block);
            f.render_widget(widget, area);
        } else {
            // Account prefix only when the haystack spans accounts (`g/` from
            // `[all]`, or local `/` from `[all]`). Otherwise account is
            // redundant — the badge already names it.
            let show_account = match &s.kind {
                SearchKind::Local { account, .. } | SearchKind::Global { account, .. } => {
                    account.is_none()
                }
            };
            let row_width = (area.width.saturating_sub(2) as usize)
                .saturating_sub(disp_w(LIST_HIGHLIGHT_SYMBOL));
            let now = Zoned::now();
            let items: Vec<ListItem> = s
                .results
                .iter()
                .filter_map(|(i, _)| s.haystack.get(*i))
                .map(|row| ListItem::new(render_search_row(row, show_account, row_width, &now)))
                .collect();
            let highlight = if focused {
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(Color::DarkGray).fg(Color::Gray)
            };
            let widget = List::new(items)
                .block(block)
                .highlight_style(highlight)
                .highlight_symbol(LIST_HIGHLIGHT_SYMBOL)
                .scroll_padding(SCROLL_PADDING);
            let mut state = ListState::default();
            *state.offset_mut() = initial_offset;
            let selected = selected_in.min(s.results.len().saturating_sub(1));
            state.select(Some(selected));
            f.render_stateful_widget(widget, area, &mut state);
            new_offset = state.offset();
            pane_scrollbar(f, area, selected, s.results.len(), focused);
            paint_list_visual(f, area, inbox, new_offset, selected, s.results.len());
        }
    }
    inbox.list_offset = new_offset;
}

/// Flat search-result row. Layout: `From          acct/Folder   ★ Subject … Date`,
/// with the date flushed to the right edge. Account is omitted when the
/// haystack is already scoped to one account.
fn render_search_row(
    row: &MessageRow,
    show_account: bool,
    width: usize,
    now: &Zoned,
) -> Line<'static> {
    let MessageRow {
        date,
        from_addr,
        subject,
        flags,
        account,
        folder,
        ..
    } = row;
    let date_label = format_date(*date, now);
    let date_w = disp_w(&date_label);
    let from = from_addr.as_deref().unwrap_or("(unknown)");
    let subject_text = subject.as_deref().unwrap_or("(no subject)");
    let unread = !flags.contains('S');
    let flagged = flags.contains('F');
    let trashed = flags.contains('T');
    let replied = flags.contains('R');

    let (flag_span, replied_span) = flag_spans(flagged, replied);
    let flag_cells: usize = 2;

    let mut subj_mods = Modifier::empty();
    if unread {
        subj_mods |= Modifier::BOLD;
    }
    if trashed {
        subj_mods |= Modifier::CROSSED_OUT;
    }
    let subj_color = if trashed {
        Color::DarkGray
    } else {
        Color::Reset
    };
    let subj_style = Style::default().fg(subj_color).add_modifier(subj_mods);

    let from_col_width: usize = 14;
    let from_truncated = truncate_pad(from, from_col_width);
    let from_span = format!("{from_truncated}  ");
    let folder_col_width: usize = 14;
    let folder_label = if show_account {
        format!("{account}/{folder}")
    } else {
        folder.clone()
    };
    let folder_truncated = truncate_pad(&folder_label, folder_col_width);
    let folder_span = format!("{folder_truncated}  ");

    // Budget the subject from what's left after the date (plus a 2-cell gap)
    // is reserved on the right, so the date is always visible.
    let gap = 2;
    let used = disp_w(&from_span) + disp_w(&folder_span) + flag_cells;
    let avail_subject = width.saturating_sub(used + date_w + gap);
    let subject_truncated = truncate_to(subject_text, avail_subject);

    let left_w = used + disp_w(&subject_truncated);
    let filler = " ".repeat(width.saturating_sub(left_w + date_w));

    Line::from(vec![
        Span::styled(from_span, Style::default().fg(Color::Cyan)),
        Span::styled(folder_span, Style::default().fg(Color::Magenta)),
        flag_span,
        replied_span,
        Span::styled(subject_truncated, subj_style),
        Span::raw(filler),
        Span::styled(date_label, Style::default().fg(Color::DarkGray)),
    ])
}

fn render_row(t: &ThreadedRow, width: usize, now: &Zoned) -> Line<'static> {
    let MessageRow {
        date,
        from_addr,
        subject,
        flags,
        ..
    } = &t.row;

    let date_label = format_date(*date, now);
    let date_w = disp_w(&date_label);
    let indent = "  ".repeat(t.depth as usize);
    let arrow = if t.depth > 0 { "↳ " } else { "" };
    let from = from_addr.as_deref().unwrap_or("(unknown)");
    let subject_text = subject.as_deref().unwrap_or("(no subject)");
    let unread = !flags.contains('S');
    let flagged = flags.contains('F');
    let trashed = flags.contains('T');
    let replied = flags.contains('R');

    // Fixed 2-cell flag column: a ★ cell (yellow) when Flagged and a ↩ cell
    // (green) when Replied, each a space otherwise. Budgeted as 2 cells below;
    // the glyphs are multi-byte so a `.len()` would mis-budget.
    let (flag_span, replied_span) = flag_spans(flagged, replied);
    let flag_cells: usize = 2;

    let mut subj_mods = Modifier::empty();
    if unread {
        subj_mods |= Modifier::BOLD;
    }
    if trashed {
        subj_mods |= Modifier::CROSSED_OUT;
    }
    let subj_color = if trashed {
        Color::DarkGray
    } else {
        Color::Reset
    };
    let subj_style = Style::default().fg(subj_color).add_modifier(subj_mods);

    let from_col_width: usize = 16;
    let from_truncated = truncate_pad(from, from_col_width);
    let from_span = format!("{from_truncated}  ");

    // Budget the subject from what's left after the date (plus a 2-cell gap)
    // is reserved on the right, so the date is always visible.
    let gap = 2;
    let used = disp_w(&from_span) + flag_cells + disp_w(&indent) + disp_w(arrow);
    let avail_subject = width.saturating_sub(used + date_w + gap);
    let subject_truncated = truncate_to(subject_text, avail_subject);

    let left_w = used + disp_w(&subject_truncated);
    let filler = " ".repeat(width.saturating_sub(left_w + date_w));

    Line::from(vec![
        Span::styled(from_span, Style::default().fg(Color::Cyan)),
        flag_span,
        replied_span,
        Span::raw(indent),
        Span::styled(arrow.to_string(), Style::default().fg(Color::DarkGray)),
        Span::styled(subject_truncated, subj_style),
        Span::raw(filler),
        Span::styled(date_label, Style::default().fg(Color::DarkGray)),
    ])
}

fn disp_w(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// The two single-cell spans of the fixed-width flag column: a yellow ★ when
/// the message is Flagged (`F`) and a green ↩ when it has been Replied to
/// (`R`), each a blank cell otherwise. Kept to a 2-cell total so the subject
/// budget math is unchanged. Shared by the threaded and search row renderers.
fn flag_spans(flagged: bool, replied: bool) -> (Span<'static>, Span<'static>) {
    let flag = if flagged { "★" } else { " " };
    let reply = if replied { "↩" } else { " " };
    (
        Span::styled(flag, Style::default().fg(Color::Yellow)),
        Span::styled(reply, Style::default().fg(Color::Green)),
    )
}

/// Relative date label. `now` is the local-zoned wall clock for this frame;
/// the message timestamp is converted into the same zone before comparison.
///
/// - today → 24h clock (`14:15`)
/// - earlier this year → abbreviated month + day (`May 2`)
/// - a previous year → year + full month + day (`2025 April 9`)
fn format_date(unix: i64, now: &Zoned) -> String {
    if unix <= 0 {
        return "·".to_string(); // placeholder; right-flushed, so width-agnostic
    }
    let Ok(ts) = Timestamp::from_second(unix) else {
        return "·".to_string();
    };
    let z = ts.to_zoned(now.time_zone().clone());
    if z.date() == now.date() {
        format!("{:02}:{:02}", z.hour(), z.minute())
    } else if z.year() == now.year() {
        format!("{} {}", MONTH_ABBR[(z.month() - 1) as usize], z.day())
    } else {
        format!(
            "{} {} {}",
            z.year(),
            MONTH_FULL[(z.month() - 1) as usize],
            z.day()
        )
    }
}

const MONTH_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTH_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn truncate_to(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (count, ch) in s.chars().enumerate() {
        if count + 1 > max_chars {
            if max_chars >= 1 {
                out.pop();
                out.push('…');
            }
            return out;
        }
        out.push(ch);
    }
    out
}

fn truncate_pad(s: &str, width: usize) -> String {
    let mut out: String = s.chars().take(width).collect();
    let len = out.chars().count();
    if len < width {
        out.push_str(&" ".repeat(width - len));
    } else if s.chars().count() > width && width >= 1 {
        out.pop();
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::tz::TimeZone;

    fn utc_now(unix: i64) -> Zoned {
        Timestamp::from_second(unix)
            .unwrap()
            .to_zoned(TimeZone::UTC)
    }

    #[test]
    fn date_label_relative_to_now() {
        // "now" = 2026-05-26 12:00 UTC.
        let now = utc_now(1_779_796_800);

        // Missing/zero date → placeholder.
        assert_eq!(format_date(0, &now), "·");

        // Same calendar day → 24h clock (zero-padded). 2026-05-26 01:15 UTC.
        assert_eq!(format_date(1_779_758_100, &now), "01:15");
        // 2026-05-26 14:15 UTC.
        assert_eq!(format_date(1_779_804_900, &now), "14:15");

        // Earlier this year → abbrev month + day, no leading zero.
        // 2026-05-02 00:00 UTC = 1777680000.
        assert_eq!(format_date(1_777_680_000, &now), "May 2");
        // 2026-04-08 00:00 UTC = 1775606400.
        assert_eq!(format_date(1_775_606_400, &now), "Apr 8");

        // A previous year → year + full month + day.
        // 2025-04-09 00:00 UTC = 1744156800.
        assert_eq!(format_date(1_744_156_800, &now), "2025 April 9");
    }

    #[test]
    fn flag_spans_reflect_flagged_and_replied() {
        let cells = |flagged, replied| {
            let (f, r) = flag_spans(flagged, replied);
            (f.content.to_string(), r.content.to_string())
        };
        // Neither flag: both cells blank, column stays 2 wide.
        assert_eq!(cells(false, false), (" ".into(), " ".into()));
        // Flagged only: ★ in the first cell, replied cell blank.
        assert_eq!(cells(true, false), ("★".into(), " ".into()));
        // Replied only: ↩ in the second cell, flag cell blank.
        assert_eq!(cells(false, true), (" ".into(), "↩".into()));
        // Both: ★ and ↩ side by side.
        assert_eq!(cells(true, true), ("★".into(), "↩".into()));
    }

    #[test]
    fn truncate_to_short_unchanged() {
        assert_eq!(truncate_to("hello", 10), "hello");
        assert_eq!(truncate_to("hello", 5), "hello");
    }

    #[test]
    fn clamp_offset_keeps_window_inside_list() {
        // 100 rows, 10-row viewport.
        let len = 100;
        let h = 10;
        // Top of list: offset pinned to 0, no negative underflow.
        assert_eq!(clamp_offset(0, 0, h, len), 0);
        assert_eq!(clamp_offset(5, 1, h, len), 0);
        // Bottom of list: window never runs past the end (max = len - h).
        assert_eq!(clamp_offset(0, 99, h, len), len - h);
        // A stale offset far below the cursor scrolls up to bring the
        // cursor into view with the top padding margin (selected - pad).
        assert_eq!(clamp_offset(500, 50, h, len), 48);
    }

    #[test]
    fn clamp_offset_respects_scroll_padding() {
        let len = 100;
        let h = 20;
        let pad = SCROLL_PADDING; // 2
        // Cursor sitting inside the viewport (with margin) doesn't move it.
        let off = clamp_offset(40, 50, h, len);
        assert!(off <= 50 - pad, "top margin: {off}");
        assert!(50 < off + h - pad, "bottom margin honored");
        // Walking the cursor up to the top padding band scrolls up by 1.
        assert_eq!(clamp_offset(40, 41, h, len), 39);
        // Walking down into the bottom band scrolls down by 1.
        assert_eq!(clamp_offset(40, 58, h, len), 41);
    }

    #[test]
    fn clamp_offset_degenerate_inputs() {
        // Empty list / zero viewport → offset 0, no panic.
        assert_eq!(clamp_offset(7, 3, 0, 10), 0);
        assert_eq!(clamp_offset(7, 3, 10, 0), 0);
        // List shorter than the viewport → everything fits at offset 0.
        assert_eq!(clamp_offset(3, 4, 20, 5), 0);
    }

    #[test]
    fn truncate_to_long_gets_ellipsis() {
        assert_eq!(truncate_to("hello world", 7), "hello …");
    }

    #[test]
    fn truncate_pad_short_padded() {
        assert_eq!(truncate_pad("bob", 6), "bob   ");
    }
}
