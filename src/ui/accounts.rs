use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::ui::app::{App, Mode};

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let parts = Layout::horizontal([Constraint::Min(0), Constraint::Length(10)]).split(area);
    let left = Paragraph::new(Line::from(vec![
        Span::raw(" [dev] "),
        Span::styled("inbox", Style::default().fg(Color::Cyan)),
        Span::raw(" · "),
        Span::styled("idle", Style::default().fg(Color::DarkGray)),
    ]));
    let right = Paragraph::new(Line::from(Span::styled(
        mode_tag(app.mode),
        Style::default().fg(Color::Yellow),
    )));
    f.render_widget(left, parts[0]);
    f.render_widget(right, parts[1]);
}

fn mode_tag(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => " NORMAL  ",
        Mode::Command => " COMMAND ",
        Mode::LinkPick => " LINKPICK",
    }
}
