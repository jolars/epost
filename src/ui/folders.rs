use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};

use crate::store::index::FolderStat;
use crate::store::scan::AccountFolderStats;
use crate::ui::app::{InboxScreen, Pane, ScanState};
use crate::ui::style::pane_block;

/// The default folder name across maildirs. Pinned to the top of each
/// account group (and the unified `[all]` group) so users always have
/// an obvious home position.
pub const DEFAULT_FOLDER: &str = "INBOX";

/// One row of the sidebar tree. `Header` is a non-selectable group
/// label (e.g. "[all]", "[personal]"); `Folder` is a selectable
/// `(scope, name)` row whose `scope = None` means "unified across
/// accounts" and `Some(name)` means that account's folder. `Spacer` is
/// a blank line inserted between groups so they read apart without any
/// indentation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SidebarEntry {
    Spacer,
    Header {
        label: String,
    },
    Folder {
        scope: Option<String>,
        name: String,
        total: u64,
        unread: u64,
    },
}

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
            let entries = build_entries(&inbox.folder_stats);
            // Inner width minus the 2 cells the `List` reserves on every
            // row for the highlight symbol ("▌ "), so right-anchored
            // counts line up against the true right edge.
            let inner_width = (area.width.saturating_sub(2) as usize).saturating_sub(2);

            // Find the row that matches the active `(scope, folder)` so
            // ListState highlights it. The cursor never lands on a
            // Header in normal navigation; if the lookup misses (e.g.
            // a transient scope before the first scan completes), no
            // selection is set and the highlight just doesn't appear.
            let current_idx = entries.iter().position(|e| match e {
                SidebarEntry::Folder { scope, name, .. } => {
                    scope.as_deref() == inbox.current_account.as_deref()
                        && name == &inbox.current_folder
                }
                SidebarEntry::Header { .. } | SidebarEntry::Spacer => false,
            });

            let items: Vec<ListItem> = entries
                .iter()
                .map(|e| ListItem::new(render_entry(e, inner_width)))
                .collect();

            // Mirrors list.rs: both fg+bg set so the highlight uniformly
            // overrides per-span colors (counts column).
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

/// Flat list of selectable `(scope, folder)` pairs in sidebar order:
/// `[all]` group first, then each account alphabetically. Within each
/// group, INBOX pinned first, rest alphabetical. `cycle_folder` uses
/// this to walk folders skipping the group headers.
pub(crate) fn selectable_entries(groups: &[AccountFolderStats]) -> Vec<(Option<String>, String)> {
    build_entries(groups)
        .into_iter()
        .filter_map(|e| match e {
            SidebarEntry::Folder { scope, name, .. } => Some((scope, name)),
            SidebarEntry::Header { .. } | SidebarEntry::Spacer => None,
        })
        .collect()
}

/// Build the flat sidebar entry list. `[all]` group always comes first
/// (even when there are no rows, so the cursor has a stable home);
/// per-account groups follow alphabetically. Empty groups still emit a
/// header so the user sees their accounts even before any mail lands.
fn build_entries(groups: &[AccountFolderStats]) -> Vec<SidebarEntry> {
    // Sort groups: None (all) first, then accounts alphabetically.
    let mut ordered: Vec<&AccountFolderStats> = groups.iter().collect();
    ordered.sort_by(|a, b| match (&a.scope, &b.scope) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, _) => std::cmp::Ordering::Less,
        (_, None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(y),
    });

    let mut out = Vec::new();
    for g in ordered {
        // Blank line between groups (not before the first), so groups
        // read apart without relying on folder indentation.
        if !out.is_empty() {
            out.push(SidebarEntry::Spacer);
        }
        let label = match &g.scope {
            None => "[all]".to_string(),
            Some(name) => format!("[{name}]"),
        };
        out.push(SidebarEntry::Header { label });
        let mut folders: Vec<&FolderStat> = g.folders.iter().collect();
        folders.sort_by_key(|s| folder_sort_key(&s.folder));
        for f in folders {
            out.push(SidebarEntry::Folder {
                scope: g.scope.clone(),
                name: f.folder.clone(),
                total: f.total,
                unread: f.unread,
            });
        }
    }
    out
}

/// INBOX always pinned to the top; everything else alphabetical. Tuple
/// sort key so the comparison is plain `Ord` without a custom impl.
pub(crate) fn folder_sort_key(name: &str) -> (u8, String) {
    if name == DEFAULT_FOLDER {
        (0, String::new())
    } else {
        (1, name.to_string())
    }
}

fn render_entry(entry: &SidebarEntry, width: usize) -> Line<'static> {
    match entry {
        SidebarEntry::Spacer => Line::from(""),
        SidebarEntry::Header { label } => {
            let truncated = truncate_to(label, width);
            Line::from(Span::styled(
                truncated,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ))
        }
        SidebarEntry::Folder {
            name,
            total,
            unread,
            ..
        } => render_folder_row(name, *total, *unread, width),
    }
}

fn render_folder_row(name: &str, total: u64, unread: u64, width: usize) -> Line<'static> {
    let has_unread = unread > 0;
    let name_mods = if has_unread {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };

    // Right-anchored counts: "unread/total" when there's anything
    // unread, bare "total" when all read, "0" for an empty folder.
    let counts = if total == 0 {
        "0".to_string()
    } else if has_unread {
        format!("{unread}/{total}")
    } else {
        format!("{total}")
    };

    // Name flush left, counts flush right, at least one space between.
    let counts_w = counts.chars().count();
    let label_max = width.saturating_sub(counts_w).saturating_sub(1);
    let label = truncate_to(name, label_max);
    let gap = width
        .saturating_sub(label.chars().count())
        .saturating_sub(counts_w)
        .max(1);

    Line::from(vec![
        Span::styled(label, Style::default().add_modifier(name_mods)),
        Span::raw(" ".repeat(gap)),
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

    fn stat(name: &str, total: u64, unread: u64) -> FolderStat {
        FolderStat {
            folder: name.to_string(),
            total,
            unread,
        }
    }

    fn group(scope: Option<&str>, folders: Vec<FolderStat>) -> AccountFolderStats {
        AccountFolderStats {
            scope: scope.map(str::to_string),
            folders,
        }
    }

    #[test]
    fn inbox_sorts_first() {
        let mut names = vec!["Sent", "INBOX", "Archive"];
        names.sort_by_key(|a| folder_sort_key(a));
        assert_eq!(names, vec!["INBOX", "Archive", "Sent"]);
    }

    #[test]
    fn entries_have_all_first_then_accounts_alphabetic() {
        let groups = vec![
            group(Some("work"), vec![stat("INBOX", 1, 0)]),
            group(None, vec![stat("INBOX", 3, 1)]),
            group(Some("personal"), vec![stat("INBOX", 2, 1)]),
        ];
        let entries = build_entries(&groups);
        let headers: Vec<&str> = entries
            .iter()
            .filter_map(|e| match e {
                SidebarEntry::Header { label } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, vec!["[all]", "[personal]", "[work]"]);
    }

    #[test]
    fn entries_pin_inbox_first_within_each_group() {
        let groups = vec![group(
            None,
            vec![
                stat("Sent", 1, 0),
                stat("INBOX", 2, 0),
                stat("Archive", 4, 0),
            ],
        )];
        let entries = build_entries(&groups);
        let names: Vec<&str> = entries
            .iter()
            .filter_map(|e| match e {
                SidebarEntry::Folder { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["INBOX", "Archive", "Sent"]);
    }

    #[test]
    fn selectable_entries_skips_headers() {
        let groups = vec![
            group(None, vec![stat("INBOX", 1, 0), stat("Sent", 1, 0)]),
            group(Some("personal"), vec![stat("INBOX", 2, 0)]),
        ];
        let sel = selectable_entries(&groups);
        assert_eq!(
            sel,
            vec![
                (None, "INBOX".to_string()),
                (None, "Sent".to_string()),
                (Some("personal".to_string()), "INBOX".to_string()),
            ]
        );
    }

    /// Folder rows are three spans: label, gap, counts. The label is
    /// flush left; the counts flush right; the gap pads between.
    fn row_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|sp| sp.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn render_folder_row_shows_zero_when_empty() {
        let line = render_folder_row("Spam", 0, 0, 20);
        assert_eq!(line.spans.first().unwrap().content.as_ref(), "Spam");
        assert_eq!(line.spans.last().unwrap().content.as_ref(), "0");
        // No leading indent; name is flush left.
        assert!(!row_text(&line).starts_with(' '));
        // Counts are flush right: total rendered width fills the column.
        assert_eq!(row_text(&line).chars().count(), 20);
    }

    #[test]
    fn render_folder_row_shows_total_only_when_all_read() {
        let line = render_folder_row("Sent", 4, 0, 20);
        assert_eq!(line.spans.first().unwrap().content.as_ref(), "Sent");
        assert_eq!(line.spans.last().unwrap().content.as_ref(), "4");
    }

    #[test]
    fn render_folder_row_shows_unread_over_total() {
        let line = render_folder_row("INBOX", 12, 3, 20);
        assert_eq!(line.spans.first().unwrap().content.as_ref(), "INBOX");
        assert_eq!(line.spans.last().unwrap().content.as_ref(), "3/12");
    }

    #[test]
    fn render_folder_row_bolds_when_unread() {
        let line = render_folder_row("INBOX", 5, 2, 20);
        // span 0 is the label.
        assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }
}
