use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::ui::app::{App, Mode, Screen};

/// Maximum width of the `account · folder` portion baked into the INBOX
/// tab label before the folder side is truncated with an ellipsis. Sized
/// for `personal · INBOX` / `work · SomeLongFolder` etc.
const INBOX_LABEL_WIDTH: usize = 26;

/// Fixed width of the tab-strip slot. Bounded (rather than spanning to
/// the right cluster) so the search chip / mode tag stay put, and fixed
/// (rather than content-sized) so it doesn't flicker as folder switches
/// resize the INBOX label. Sized to fit the widest INBOX label plus its
/// padding and a short compose-tab title; extra tabs clip gracefully.
const TAB_STRIP_WIDTH: usize = INBOX_LABEL_WIDTH + 8;

/// Extra cells reserved on the right of the tab strip for the live
/// `/needle (N)` search chip when a search is active. Kept compact.
const SEARCH_CHIP_WIDTH: usize = 24;

/// Render the top-row strip: tab strip flush-left (the INBOX tab carries
/// the active `account · folder` scope), search chip + sync indicator +
/// mode tag right-aligned. The active tab is inverted; dirty (compose)
/// tabs get a trailing `*`.
pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    // Paint the whole row with the default style first so every cell is
    // explicitly written. Without this, the gap between the (now
    // bounded) tab strip and the right cluster is left untouched, and
    // terminals with background-color-erase can keep a stale highlight
    // there from a previously wider label — which reads as the active
    // tab "bleeding all the way" to the mode tag for some folders.
    f.render_widget(Paragraph::new(""), area);

    let search_active = matches!(app.screens.first(), Some(Screen::Inbox(i)) if i.search.is_some());
    let chip_width = if search_active { SEARCH_CHIP_WIDTH } else { 0 };
    let parts = Layout::horizontal([
        Constraint::Length(TAB_STRIP_WIDTH as u16),
        Constraint::Min(0),
        Constraint::Length(chip_width as u16),
        Constraint::Length(12),
    ])
    .split(area);

    let chip = search_chip(app);

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(app.screens.len() * 2);
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

    f.render_widget(Paragraph::new(Line::from(spans)), parts[0]);
    f.render_widget(Paragraph::new(chip), parts[2]);
    f.render_widget(Paragraph::new(right), parts[3]);
}

/// `/needle (12)` or `g/needle (12)` chip surfaced when a search is
/// active. Renders into the slot between the tab strip and the mode tag;
/// empty `Line` when no search.
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

/// The INBOX tab's `account · folder` label. Account defaults to `all`
/// when no scope is selected. Overflow truncates with an ellipsis on the
/// folder side so the account label stays legible.
fn inbox_label(account: &str, folder: &str, max_width: usize) -> String {
    // "{account} · {folder}" is the target; budget so it fits in
    // `max_width` cells (the tab's own padding is added by the caller).
    let prefix_chars = account.chars().count() + 3; // "{account} · "
    if prefix_chars >= max_width {
        // Account name alone already overflows; truncate that.
        return truncate_to(account, max_width);
    }
    let folder_budget = max_width - prefix_chars;
    let folder_t = truncate_to(folder, folder_budget);
    format!("{account} · {folder_t}")
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
        Screen::Inbox(i) => {
            let account = i.current_account.as_deref().unwrap_or("all");
            inbox_label(account, &i.current_folder, INBOX_LABEL_WIDTH)
        }
        Screen::Compose(c) => {
            let dirty = if c.body_is_dirty() { "*" } else { "" };
            format!("{}{dirty}", c.title)
        }
    }
}

fn mode_tag(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "NORMAL  ",
        Mode::Command => "COMMAND ",
        Mode::LinkPick => "LINKPICK",
        Mode::AttachmentPick => "ATTPICK ",
        Mode::Search => "SEARCH  ",
        Mode::Visual => "VISUAL  ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_label_fits_within_width() {
        let text = inbox_label("personal", "INBOX", INBOX_LABEL_WIDTH);
        assert!(text.chars().count() <= INBOX_LABEL_WIDTH);
        assert_eq!(text, "personal · INBOX");
    }

    #[test]
    fn inbox_label_defaults_to_all() {
        // The `app` branch can't be tested without an App; inbox_label
        // does the heavy lifting and the "all" string is wired in
        // tab_label.
        let text = inbox_label("all", "INBOX", INBOX_LABEL_WIDTH);
        assert_eq!(text, "all · INBOX");
    }

    #[test]
    fn inbox_label_truncates_long_folder() {
        let text = inbox_label("work", "ReallyLongFolderNameHere", INBOX_LABEL_WIDTH);
        assert!(text.chars().count() <= INBOX_LABEL_WIDTH);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn inbox_label_truncates_long_account_when_alone() {
        let text = inbox_label("verylongaccountname-overflowing", "INBOX", 16);
        assert!(text.chars().count() <= 16);
        assert!(text.contains('…'));
    }
}
