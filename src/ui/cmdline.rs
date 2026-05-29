use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::config;
use crate::config::Config;
use crate::mail::compose::{self as mail_compose, Draft};
use crate::mail::parse;
use crate::ui::app::{App, Mode};
use crate::ui::browser;
use crate::ui::compose::{ComposeScreen, ComposeStatus};

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let line = match app.mode {
        Mode::Command => {
            let buf = app.cmdline.as_str();
            let (before, after) = buf.split_at(app.cmdline.cursor());
            Line::from(vec![
                Span::styled(":", Style::default().fg(Color::Yellow)),
                Span::raw(before.to_string()),
                Span::styled(
                    "_",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::SLOW_BLINK),
                ),
                Span::raw(after.to_string()),
            ])
        }
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
        "open" => match app.inbox_parsed() {
            Some(body) => {
                if let Err(e) = browser::open_message(body, &cfg.reader.browser) {
                    app.status_error = Some(format!("open: {e:#}"));
                }
            }
            None => {
                app.status_error = Some("open: no parsed body".into());
            }
        },
        "compose" => open_blank_compose(app, cfg),
        "close" => match app.close_active_tab() {
            Ok(()) => {}
            Err(msg) => app.status_error = Some(msg.into()),
        },
        "send" => send_active(app, cfg),
        "reply" => open_reply(app, cfg, ReplyKind::Reply),
        "reply-all" => open_reply(app, cfg, ReplyKind::ReplyAll),
        "forward" => open_reply(app, cfg, ReplyKind::Forward),
        "archive" => move_named(app, cfg, MoveKind::Archive),
        "spam" => move_named(app, cfg, MoveKind::Spam),
        "trash" => move_named(app, cfg, MoveKind::Trash),
        "mv" => {
            let Some(folder) = parts.next() else {
                app.status_error = Some("mv: missing folder".into());
                return;
            };
            app.move_selected_to(folder, cfg);
        }
        "account" => match parts.next() {
            None | Some("all") => app.switch_to_scope(None, "INBOX"),
            Some(name) if cfg.accounts.contains_key(name) => {
                app.switch_to_scope(Some(name.to_string()), "INBOX");
            }
            Some(other) => {
                app.status_error = Some(format!("account: unknown {other:?}"));
            }
        },
        other => {
            app.status_error = Some(format!("unknown command: {other:?}"));
        }
    }
}

/// The three account-config-driven move targets. Sits alongside `:mv
/// <folder>` so the cross-folder primitive has both a "well-known
/// destination" form (driven by `[accounts.*]` config) and a free-form
/// one.
#[derive(Copy, Clone)]
enum MoveKind {
    Archive,
    Spam,
    Trash,
}

impl MoveKind {
    fn label(self) -> &'static str {
        match self {
            MoveKind::Archive => "archive",
            MoveKind::Spam => "spam",
            MoveKind::Trash => "trash",
        }
    }

    fn folder(self, acc: &crate::config::Account) -> Option<&String> {
        match self {
            MoveKind::Archive => acc.archive_folder.as_ref(),
            MoveKind::Spam => acc.spam_folder.as_ref(),
            MoveKind::Trash => acc.trash_folder.as_ref(),
        }
    }
}

/// Resolve the selected row's account → look up the configured target
/// folder for `kind` → dispatch the move. Errors land in
/// `app.status_error`; success leaves `move_selected_to` to set the
/// "moved to X" message.
fn move_named(app: &mut App, cfg: &Config, kind: MoveKind) {
    let label = kind.label();
    let Some(row) = app.inbox().selected_row().map(|t| t.row.clone()) else {
        app.status_error = Some(format!("{label}: no message selected"));
        return;
    };
    let Some(account) = cfg.accounts.get(&row.account) else {
        app.status_error = Some(format!("{label}: unknown account {}", row.account));
        return;
    };
    let Some(folder) = kind.folder(account).cloned() else {
        app.status_error = Some(format!(
            "{label}: no {label}_folder configured for {}",
            row.account
        ));
        return;
    };
    app.move_selected_to(&folder, cfg);
}

/// Re-export for the `a` / `D` keybindings: dispatch the same `:archive`
/// / `:trash` path the cmdline would, so account/folder resolution lives
/// in one place.
pub fn archive_selected(app: &mut App, cfg: &Config) {
    move_named(app, cfg, MoveKind::Archive);
}

pub fn trash_selected(app: &mut App, cfg: &Config) {
    move_named(app, cfg, MoveKind::Trash);
}

fn send_active(app: &mut App, cfg: &Config) {
    let Some(c) = app.active_compose_mut() else {
        app.status_error = Some("send: not on a compose tab".into());
        return;
    };
    let account_name = c.account.clone();
    let draft = match c.collect_draft() {
        Ok(d) => d,
        Err(e) => {
            app.status_error = Some(format!("send: read body: {e}"));
            return;
        }
    };
    if draft.to.is_empty() && draft.cc.is_empty() && draft.bcc.is_empty() {
        app.status_error = Some("send: no recipients".into());
        return;
    }
    let smtp_cmd = match config::smtp_command_for(cfg, &account_name) {
        Ok(c) => c.to_vec(),
        Err(e) => {
            app.status_error = Some(format!("send: {e}"));
            return;
        }
    };
    let sent_cur_dir = cfg.accounts.get(&account_name).and_then(|a| {
        a.sent_folder
            .as_ref()
            .map(|sf| a.maildir.join(format!(".{sf}")).join("cur"))
    });
    let bytes = match mail_compose::serialize(&draft) {
        Ok(b) => b,
        Err(e) => {
            app.status_error = Some(format!("send: serialize: {e}"));
            return;
        }
    };
    let rx = mail_compose::start_send_worker(bytes, smtp_cmd, sent_cur_dir);
    if let Some(c) = app.active_compose_mut() {
        c.send_rx = Some(rx);
        c.status = ComposeStatus::Sending;
    }
    app.status_error = Some("sending…".into());
}

/// Pub re-export of the cmdline `:compose` handler so the `c`
/// keybinding can open a blank compose tab without going through the
/// cmdline buffer.
pub fn open_blank_compose_external(app: &mut App, cfg: &Config) {
    open_blank_compose(app, cfg);
}

fn open_blank_compose(app: &mut App, cfg: &Config) {
    let Some((name, account)) = cfg.accounts.iter().next() else {
        app.status_error = Some("compose: no accounts configured".into());
        return;
    };
    let draft = Draft::new_blank(name, &account.from);
    match ComposeScreen::from_draft(draft) {
        Ok(screen) => {
            app.open_compose(screen);
        }
        Err(e) => {
            app.status_error = Some(format!("compose: {e}"));
        }
    }
}

pub enum ReplyKind {
    Reply,
    ReplyAll,
    Forward,
}

pub fn open_reply(app: &mut App, cfg: &Config, kind: ReplyKind) {
    let label = match kind {
        ReplyKind::Reply => "reply",
        ReplyKind::ReplyAll => "reply-all",
        ReplyKind::Forward => "forward",
    };
    let Some(row) = app.inbox().selected_row().map(|t| t.row.clone()) else {
        app.status_error = Some(format!("{label}: no message selected"));
        return;
    };
    let Some(account) = cfg.accounts.get(&row.account) else {
        app.status_error = Some(format!("{label}: unknown account {}", row.account));
        return;
    };
    let from = account.from.clone();
    let headers = match parse::read_headers(&row.path) {
        Ok(Some(h)) => h,
        Ok(None) => {
            app.status_error = Some(format!("{label}: failed to parse headers"));
            return;
        }
        Err(e) => {
            app.status_error = Some(format!("{label}: {e:#}"));
            return;
        }
    };
    let body = match parse::read_body(&row.path) {
        Ok(b) => b,
        Err(e) => {
            app.status_error = Some(format!("{label}: {e:#}"));
            return;
        }
    };
    let draft = match kind {
        ReplyKind::Reply => Draft::reply_to(&headers, &body, &row.account, &from, false),
        ReplyKind::ReplyAll => Draft::reply_to(&headers, &body, &row.account, &from, true),
        ReplyKind::Forward => Draft::forward(&headers, &body, &row.account, &from),
    };
    match ComposeScreen::from_draft(draft) {
        Ok(screen) => {
            app.open_compose(screen);
        }
        Err(e) => {
            app.status_error = Some(format!("{label}: {e}"));
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
        let app = App::new(&cfg, PathBuf::from("/tmp/epost-test.sqlite"), None, None);
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
