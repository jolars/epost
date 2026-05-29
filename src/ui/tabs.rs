use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::ui::app::{App, Mode, Screen};

/// Maximum width of the left-anchored `account · folder` badge before
/// truncating with an ellipsis. Sized for `personal · INBOX` / `work ·
/// SomeLongFolder` etc. and matched against the layout constraint.
const BADGE_WIDTH: usize = 28;

/// Extra cells reserved on the right of the badge for the live `/needle (N)`
/// search chip when a search is active. Kept compact so the badge still
/// fits in the same top-bar slot.
const SEARCH_CHIP_WIDTH: usize = 24;

/// Render the top-row strip: `[account · folder]` badge on the left,
/// tab strip in the middle, sync indicator + mode tag right-aligned.
/// The active tab is inverted; dirty (compose) tabs get a trailing `*`.
pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let search_active = matches!(app.screens.first(), Some(Screen::Inbox(i)) if i.search.is_some());
    let chip_width = if search_active { SEARCH_CHIP_WIDTH } else { 0 };
    let parts = Layout::horizontal([
        Constraint::Length(BADGE_WIDTH as u16),
        Constraint::Length(chip_width as u16),
        Constraint::Min(0),
        Constraint::Length(12),
    ])
    .split(area);

    let badge = badge_line(app);
    let chip = search_chip(app);

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(app.screens.len() * 2 + 1);
    spans.push(Span::raw(" "));
    for (i, screen) in app.screens.iter().enumerate() {
        let label = tab_label(screen);
        let style = if i == app.active {
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(format!(" {label} "), style));
        spans.push(Span::raw(" "));
    }

    let right = Line::from(vec![
        Span::styled("idle ", Style::default().fg(Color::DarkGray)),
        Span::styled(mode_tag(app.mode), Style::default().fg(Color::Yellow)),
    ]);

    f.render_widget(Paragraph::new(badge), parts[0]);
    f.render_widget(Paragraph::new(chip), parts[1]);
    f.render_widget(Paragraph::new(Line::from(spans)), parts[2]);
    f.render_widget(Paragraph::new(right), parts[3]);
}

/// `/needle (12)` or `g/needle (12)` chip surfaced when a search is
/// active. Renders into the slot between the account/folder badge and
/// the tab strip; empty `Line` when no search.
fn search_chip(app: &App) -> Line<'static> {
    let Some(Screen::Inbox(inbox)) = app.screens.first() else {
        return Line::from(Span::raw(""));
    };
    let Some(s) = inbox.search.as_ref() else {
        return Line::from(Span::raw(""));
    };
    let prefix = if s.kind.is_global() { "g/" } else { "/" };
    // Budget: " {prefix}{query} ({n}) ", where the query is truncated to
    // whatever fits in SEARCH_CHIP_WIDTH after the fixed parts.
    let q = s.query.as_str();
    let count = format!(" ({})", s.results.len());
    let fixed = 1 + prefix.chars().count() + count.chars().count() + 1; // leading + trailing space
    let q_budget = SEARCH_CHIP_WIDTH.saturating_sub(fixed);
    let q_t = truncate_to(q, q_budget);
    Line::from(vec![
        Span::raw(" "),
        Span::styled(
            prefix.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(q_t, Style::default().fg(Color::Yellow)),
        Span::styled(count, Style::default().fg(Color::DarkGray)),
        Span::raw(" "),
    ])
}

/// `[account · folder]` badge. Account defaults to `all` when no scope
/// is selected. Overflow truncates with an ellipsis on the folder side
/// so the account label stays legible.
fn badge_line(app: &App) -> Line<'static> {
    let inbox = app.inbox();
    let account = inbox.current_account.as_deref().unwrap_or("all");
    let text = format_badge(account, &inbox.current_folder, BADGE_WIDTH);
    Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))
}

fn format_badge(account: &str, folder: &str, max_width: usize) -> String {
    // " {account} · {folder} " is the target; budget so the whole
    // thing fits in `max_width` cells.
    let prefix_chars = 1 + account.chars().count() + 3; // " {account} · "
    let suffix_pad = 1; // trailing " "
    if prefix_chars + suffix_pad >= max_width {
        // Account name alone already overflows; truncate that.
        let allowed = max_width.saturating_sub(2); // leading + trailing " "
        let acc = truncate_to(account, allowed);
        return format!(" {acc} ");
    }
    let folder_budget = max_width - prefix_chars - suffix_pad;
    let folder_t = truncate_to(folder, folder_budget);
    format!(" {account} · {folder_t} ")
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

fn tab_label(screen: &Screen) -> String {
    match screen {
        Screen::Inbox(_) => "INBOX".to_string(),
        Screen::Compose(c) => {
            let dirty = if c.body_dirty { "*" } else { "" };
            format!("{}{dirty}", c.title)
        }
    }
}

fn mode_tag(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "NORMAL  ",
        Mode::Command => "COMMAND ",
        Mode::LinkPick => "LINKPICK",
        Mode::Search => "SEARCH  ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_fits_within_width() {
        let text = format_badge("personal", "INBOX", BADGE_WIDTH);
        assert!(text.chars().count() <= BADGE_WIDTH);
        assert_eq!(text, " personal · INBOX ");
    }

    #[test]
    fn badge_defaults_to_all() {
        // The `app` branch can't be tested without an App; format_badge
        // does the heavy lifting and the "all" string is wired in
        // badge_line.
        let text = format_badge("all", "INBOX", BADGE_WIDTH);
        assert_eq!(text, " all · INBOX ");
    }

    #[test]
    fn badge_truncates_long_folder() {
        let text = format_badge("work", "ReallyLongFolderNameHere", BADGE_WIDTH);
        assert!(text.chars().count() <= BADGE_WIDTH);
        assert!(text.ends_with("… "));
    }

    #[test]
    fn badge_truncates_long_account_when_alone() {
        let text = format_badge("verylongaccountname-overflowing", "INBOX", 16);
        assert!(text.chars().count() <= 16);
        assert!(text.contains('…'));
    }
}
