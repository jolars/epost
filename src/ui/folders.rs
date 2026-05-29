use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};

use crate::store::index::FolderStat;
use crate::ui::app::{InboxScreen, Pane, ScanState};
use crate::ui::style::pane_block;

/// The default unified folder. Pinned to the top of the sidebar so
/// users always have an obvious home position regardless of which
/// folder they last visited.
pub const DEFAULT_FOLDER: &str = "INBOX";

pub fn draw(f: &mut Frame, area: Rect, inbox: &InboxScreen) {
    let focused = inbox.focus == Pane::Folders;
    let block = pane_block("Folders", focused);

    match &inbox.scan {
        ScanState::Scanning if inbox.folder_stats.is_empty() => {
            let items = vec![ListItem::new(Line::from(Span::styled(
                "Scanning…",
                Style::default().fg(Color::DarkGray),
            )))];
            f.render_widget(List::new(items).block(block), area);
        }
        ScanState::Failed(_) if inbox.folder_stats.is_empty() => {
            let items = vec![ListItem::new(Line::from(Span::styled(
                "scan failed",
                Style::default().fg(Color::Red),
            )))];
            f.render_widget(List::new(items).block(block), area);
        }
        _ => {
            let mut entries: Vec<&FolderStat> = inbox.folder_stats.iter().collect();
            entries.sort_by_key(|a| folder_sort_key(&a.folder));
            // ListState selection drives the highlight bar; the bar
            // marks the *active* folder (the one the list pane is
            // rendering), not a hovered cursor. The two coincide today
            // because navigating folders also switches the active one.
            let current_idx = entries
                .iter()
                .position(|s| s.folder == inbox.current_folder);
            let inner_width = area.width.saturating_sub(2) as usize;
            let items: Vec<ListItem> = entries
                .into_iter()
                .map(|s| ListItem::new(render_row(s, inner_width)))
                .collect();
            // Mirrors list.rs: both fg+bg set so the highlight
            // uniformly overrides per-span colors (counts column).
            // Focused = vivid blue, unfocused = muted gray, so the
            // active folder still reads as "current" when the user
            // is reading mail in the list/reader.
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
                .highlight_symbol("▌ ");
            let mut state = ListState::default();
            state.select(current_idx);
            f.render_stateful_widget(widget, area, &mut state);
        }
    }
}

/// INBOX always pinned to the top; everything else alphabetical. Tuple
/// sort key so the comparison is plain `Ord` without a custom impl.
/// `pub(crate)` so `App::cycle_folder` walks folders in the same order
/// the sidebar shows them.
pub(crate) fn folder_sort_key(name: &str) -> (u8, String) {
    if name == DEFAULT_FOLDER {
        (0, String::new())
    } else {
        (1, name.to_string())
    }
}

fn render_row(s: &FolderStat, width: usize) -> Line<'static> {
    let has_unread = s.unread > 0;
    let name_mods = if has_unread {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };

    // Right-anchored counts: " 12 (3)" when unread, " 12" otherwise.
    // Hidden when total is zero so empty folders read as plain labels.
    let counts = if s.total == 0 {
        String::new()
    } else if has_unread {
        format!(" {} ({})", s.total, s.unread)
    } else {
        format!(" {}", s.total)
    };

    let label_max = width.saturating_sub(counts.chars().count());
    let label = truncate_to(&s.folder, label_max);

    Line::from(vec![
        Span::styled(label, Style::default().add_modifier(name_mods)),
        Span::styled(counts, Style::default().fg(Color::DarkGray)),
    ])
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_sorts_first() {
        let mut names = vec!["Sent", "INBOX", "Archive"];
        names.sort_by_key(|a| folder_sort_key(a));
        assert_eq!(names, vec!["INBOX", "Archive", "Sent"]);
    }

    #[test]
    fn render_row_hides_counts_when_empty() {
        let s = FolderStat {
            folder: "Spam".into(),
            total: 0,
            unread: 0,
        };
        let line = render_row(&s, 20);
        let text = line
            .spans
            .iter()
            .map(|sp| sp.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "Spam");
    }

    #[test]
    fn render_row_shows_total_only_when_all_read() {
        let s = FolderStat {
            folder: "Sent".into(),
            total: 4,
            unread: 0,
        };
        let line = render_row(&s, 20);
        let text = line
            .spans
            .iter()
            .map(|sp| sp.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "Sent 4");
    }

    #[test]
    fn render_row_shows_total_and_unread() {
        let s = FolderStat {
            folder: "INBOX".into(),
            total: 12,
            unread: 3,
        };
        let line = render_row(&s, 20);
        let text = line
            .spans
            .iter()
            .map(|sp| sp.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "INBOX 12 (3)");
    }

    #[test]
    fn render_row_bolds_when_unread() {
        let s = FolderStat {
            folder: "INBOX".into(),
            total: 5,
            unread: 2,
        };
        let line = render_row(&s, 20);
        assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }
}
