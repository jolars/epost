use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::config::Config;
use crate::ui::app::{App, Mode};
use crate::ui::browser;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let line = match app.mode {
        Mode::Command => Line::from(vec![
            Span::styled(":", Style::default().fg(Color::Yellow)),
            Span::raw(app.cmdline_buf.clone()),
            Span::styled(
                "_",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::SLOW_BLINK),
            ),
        ]),
        Mode::LinkPick => Line::from(vec![
            Span::styled("link: ", Style::default().fg(Color::Yellow)),
            Span::raw(app.link_pick_buf.clone()),
            Span::styled("_", Style::default().fg(Color::Yellow)),
        ]),
        _ => {
            if let Some(err) = &app.status_error {
                Line::from(Span::styled(
                    format!(" {err} "),
                    Style::default().fg(Color::Red),
                ))
            } else {
                Line::from(Span::styled(
                    format!(" {} ", mode_label(app.mode)),
                    Style::default().fg(Color::DarkGray),
                ))
            }
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "-- NORMAL --",
        Mode::Reader => "-- READER --",
        Mode::Command => "-- COMMAND --",
        Mode::LinkPick => "-- LINK PICK --",
    }
}

/// Parse a command-line input (without the leading `:`) and execute it
/// against the app and config. Sets `app.status_error` on failure so
/// the user sees the result in the cmdline row.
pub fn dispatch(cmd: &str, app: &mut App, cfg: &Config) {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return;
    }
    let mut parts = cmd.split_whitespace();
    let head = parts.next().unwrap_or("");
    match head {
        "q" | "quit" => app.quit = true,
        "open" => match app.parsed.as_ref() {
            Some(body) => {
                if let Err(e) = browser::open_message(body, &cfg.reader.browser) {
                    app.status_error = Some(format!("open: {e:#}"));
                }
            }
            None => {
                app.status_error = Some("open: no parsed body".into());
            }
        },
        other => {
            app.status_error = Some(format!("unknown command: {other:?}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ui::app::App;
    use std::path::PathBuf;

    fn test_app() -> (App, Config) {
        let cfg = Config::default();
        // Fresh App with no scan worker; selected_msgid will be None.
        let app = App::new(&cfg, PathBuf::from("/tmp/epost-test.sqlite"), None);
        (app, cfg)
    }

    #[test]
    fn dispatch_quit_sets_quit_flag() {
        let (mut app, cfg) = test_app();
        dispatch("q", &mut app, &cfg);
        assert!(app.quit);
    }

    #[test]
    fn dispatch_open_with_no_body_reports_error() {
        let (mut app, cfg) = test_app();
        dispatch("open", &mut app, &cfg);
        assert!(app.status_error.is_some());
    }

    #[test]
    fn dispatch_unknown_command_reports_error() {
        let (mut app, cfg) = test_app();
        dispatch("nonsense", &mut app, &cfg);
        assert!(app.status_error.unwrap().contains("unknown"));
    }

    #[test]
    fn dispatch_empty_is_noop() {
        let (mut app, cfg) = test_app();
        dispatch("", &mut app, &cfg);
        assert!(app.status_error.is_none());
        assert!(!app.quit);
    }
}
