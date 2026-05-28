use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::ui::app::{App, Mode, Screen};

/// Render the top-row tab strip: one tagged region per screen on the
/// left, sync indicator + mode tag right-aligned. The active tab is
/// inverted; dirty (compose) tabs eventually get a trailing `*`.
pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let parts = Layout::horizontal([Constraint::Min(0), Constraint::Length(12)]).split(area);

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

    f.render_widget(Paragraph::new(Line::from(spans)), parts[0]);
    f.render_widget(Paragraph::new(right), parts[1]);
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
    }
}
