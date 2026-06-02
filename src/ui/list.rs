use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};

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

pub fn draw(f: &mut Frame, area: Rect, inbox: &mut InboxScreen) {
    let focused = inbox.focus == Pane::List;
    let block = pane_block("Messages", focused);

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
            let inner_width = area.width.saturating_sub(2) as usize;
            let items: Vec<ListItem> = rows
                .iter()
                .map(|t| ListItem::new(render_row(t, inner_width)))
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
            let widget = List::new(items)
                .block(block)
                .highlight_style(highlight)
                .highlight_symbol("▌ ")
                .scroll_padding(SCROLL_PADDING);
            let mut state = ListState::default();
            *state.offset_mut() = initial_offset;
            let selected = selected_in.min(rows.len().saturating_sub(1));
            state.select(Some(selected));
            f.render_stateful_widget(widget, area, &mut state);
            new_offset = state.offset();
            pane_scrollbar(f, area, selected, rows.len(), focused);
        }
    }

    inbox.list_offset = new_offset;
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
            let inner_width = area.width.saturating_sub(2) as usize;
            let items: Vec<ListItem> = s
                .results
                .iter()
                .filter_map(|(i, _)| s.haystack.get(*i))
                .map(|row| ListItem::new(render_search_row(row, show_account, inner_width)))
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
                .highlight_symbol("▌ ")
                .scroll_padding(SCROLL_PADDING);
            let mut state = ListState::default();
            *state.offset_mut() = initial_offset;
            let selected = selected_in.min(s.results.len().saturating_sub(1));
            state.select(Some(selected));
            f.render_stateful_widget(widget, area, &mut state);
            new_offset = state.offset();
            pane_scrollbar(f, area, selected, s.results.len(), focused);
        }
    }
    inbox.list_offset = new_offset;
}

/// Flat search-result row. Layout: `YYYY-MM-DD  acct/Folder  From            Subject`.
/// Account is omitted when the haystack is already scoped to one account.
fn render_search_row(row: &MessageRow, show_account: bool, width: usize) -> Line<'static> {
    let MessageRow {
        date,
        from_addr,
        subject,
        flags,
        account,
        folder,
        ..
    } = row;
    let date_label = format_date(*date);
    let from = from_addr.as_deref().unwrap_or("(unknown)");
    let subject_text = subject.as_deref().unwrap_or("(no subject)");
    let unread = !flags.contains('S');
    let flagged = flags.contains('F');
    let trashed = flags.contains('T');

    let flag_glyph = if flagged { "★ " } else { "  " };
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

    let head = format!("{date_label}  ");
    let folder_col_width: usize = 14;
    let folder_label = if show_account {
        format!("{account}/{folder}")
    } else {
        folder.clone()
    };
    let folder_truncated = truncate_pad(&folder_label, folder_col_width);
    let folder_span = format!("{folder_truncated}  ");
    let from_col_width: usize = 14;
    let from_truncated = truncate_pad(from, from_col_width);
    let from_span = format!("{from_truncated}  ");

    let remaining = width
        .saturating_sub(head.len())
        .saturating_sub(folder_span.len())
        .saturating_sub(from_span.len())
        .saturating_sub(flag_cells);
    let subject_truncated = truncate_to(subject_text, remaining);

    Line::from(vec![
        Span::styled(head, Style::default().fg(Color::DarkGray)),
        Span::styled(folder_span, Style::default().fg(Color::Magenta)),
        Span::styled(from_span, Style::default().fg(Color::Cyan)),
        Span::styled(flag_glyph.to_string(), Style::default().fg(Color::Yellow)),
        Span::styled(subject_truncated, subj_style),
    ])
}

fn render_row(t: &ThreadedRow, width: usize) -> Line<'static> {
    let MessageRow {
        date,
        from_addr,
        subject,
        flags,
        ..
    } = &t.row;

    let date_label = format_date(*date);
    let indent = "  ".repeat(t.depth as usize);
    let arrow = if t.depth > 0 { "↳ " } else { "" };
    let from = from_addr.as_deref().unwrap_or("(unknown)");
    let subject_text = subject.as_deref().unwrap_or("(no subject)");
    let unread = !flags.contains('S');
    let flagged = flags.contains('F');
    let trashed = flags.contains('T');

    // Fixed 2-cell flag column: ★ + space when Flagged, two spaces
    // otherwise. Subtracted as 2 cells below; ★ is multi-byte so a `.len()`
    // would mis-budget.
    let flag_glyph = if flagged { "★ " } else { "  " };
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

    let head = format!("{date_label}  ");
    let from_col_width: usize = 16;
    let from_truncated = truncate_pad(from, from_col_width);
    let from_span = format!("{from_truncated}  ");

    let remaining = width
        .saturating_sub(head.len())
        .saturating_sub(from_span.len())
        .saturating_sub(flag_cells)
        .saturating_sub(indent.len())
        .saturating_sub(arrow.len());
    let subject_truncated = truncate_to(subject_text, remaining);

    Line::from(vec![
        Span::styled(head, Style::default().fg(Color::DarkGray)),
        Span::styled(from_span, Style::default().fg(Color::Cyan)),
        Span::styled(flag_glyph.to_string(), Style::default().fg(Color::Yellow)),
        Span::raw(indent),
        Span::styled(arrow.to_string(), Style::default().fg(Color::DarkGray)),
        Span::styled(subject_truncated, subj_style),
    ])
}

fn format_date(unix: i64) -> String {
    // Lightweight date label — full chrono dep is overkill for a yyyy-mm-dd
    // header that the user reads at a glance. Sequencing matters more than
    // wall-clock formatting; if `date == 0`, show a placeholder.
    if unix <= 0 {
        return "----------".to_string();
    }
    let days = unix / 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

// Hinnant's date algorithm (proleptic Gregorian, days from 1970-01-01).
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i32 + (era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp.wrapping_sub(9) };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

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

    #[test]
    fn date_label_formats_known_unix() {
        assert_eq!(format_date(0), "----------");
        // 1970-01-01
        assert_eq!(format_date(1), "1970-01-01");
        // 2026-01-01 00:00 UTC = 1767225600
        assert_eq!(format_date(1_767_225_600), "2026-01-01");
        // 2026-05-26 00:00 UTC = 2026-01-01 + 145 days = 1779753600
        assert_eq!(format_date(1_779_753_600), "2026-05-26");
        // 2024-02-29 (leap-day) 00:00 UTC = 1709164800
        assert_eq!(format_date(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn truncate_to_short_unchanged() {
        assert_eq!(truncate_to("hello", 10), "hello");
        assert_eq!(truncate_to("hello", 5), "hello");
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
