use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::config;
use crate::config::Config;
use crate::mail::compose::{self as mail_compose, Draft};
use crate::mail::parse;
use crate::store::sync as store_sync;
use crate::ui::app::{App, Mode, PendingSend, Screen};
use crate::ui::browser;
use crate::ui::compose::{self, ComposeScreen};

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
        Mode::Search => search_line(app),
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
        Mode::Search => "-- SEARCH --",
        // Char-wise vs line-wise lives on `InboxScreen.visual.kind`; the
        // label is the same shape for both — the user reads the
        // selection rendering, not the modeline, to tell them apart.
        Mode::Visual => "-- VISUAL --",
    }
}

/// `:`-style strip for the active search: `/needle_  (12)` (local) or
/// `g/needle_  (12)` (global). Result count is the live match count
/// after the matcher ran on the last keystroke.
fn search_line(app: &App) -> Line<'static> {
    let Some(Screen::Inbox(inbox)) = app.screens.first() else {
        return Line::from(Span::raw(""));
    };
    let Some(s) = inbox.search.as_ref() else {
        return Line::from(Span::raw(""));
    };
    let prefix = if s.kind.is_global() { "g/" } else { "/" };
    let buf = s.query.as_str();
    let (before, after) = buf.split_at(s.query.cursor());
    let count = format!("  ({})", s.results.len());
    Line::from(vec![
        Span::styled(prefix, Style::default().fg(Color::Yellow)),
        Span::raw(before.to_string()),
        Span::styled(
            "_",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::SLOW_BLINK),
        ),
        Span::raw(after.to_string()),
        Span::styled(count, Style::default().fg(Color::DarkGray)),
    ])
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
        "sync" => dispatch_sync(app, cfg),
        "close" => {
            // Open the Save / Discard / Cancel prompt instead of
            // dropping a half-written draft on the floor. The prompt's
            // own key handler then re-issues close (or postpone) for
            // real once the user picks an arm.
            if let Some(Screen::Compose(c)) = app.screens.get_mut(app.active)
                && c.is_dirty()
                && c.confirm_close.is_none()
            {
                c.confirm_close = Some(compose::CloseConfirm);
                return;
            }
            match app.close_active_tab() {
                Ok(()) => {}
                Err(msg) => app.status_error = Some(msg.into()),
            }
        }
        "postpone" => match postpone_active(app, cfg) {
            Ok(()) => {}
            Err(e) => app.status_error = Some(format!("postpone: {e}")),
        },
        "send" => send_active(app, cfg),
        "edit" => escape_to_external_editor(app),
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
        "attach" => attach_path(app, cmd),
        "detach" => detach_index(app, &mut parts),
        "save" => save_attachment(app, cmd),
        "open-attachment" => open_attachment(app, &mut parts, cfg),
        "drag" => drag_attachment_cmd(app, &mut parts, cfg),
        "from" => switch_from(app, cfg, parts.next()),
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

    fn role(self) -> crate::config::FolderRole {
        match self {
            MoveKind::Archive => crate::config::FolderRole::Archive,
            MoveKind::Spam => crate::config::FolderRole::Spam,
            MoveKind::Trash => crate::config::FolderRole::Trash,
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
    if account.role_disk_name(kind.role()).is_none() {
        app.status_error = Some(format!(
            "{label}: no {label} configured for {}",
            row.account
        ));
        return;
    }
    // The role's display label (`"Archive"` / `"Spam"` / `"Trash"`)
    // is what `move_selected_to` expects as the target — it resolves
    // the on-disk path via the binding list and writes that label
    // into the index.
    let target = kind.role().label().to_string();
    app.move_selected_to(&target, cfg);
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

/// Spawn the `[sync].command` worker if one isn't already running.
/// Mirrors the browser-fallback shell-out from Step 3: the command
/// lives on a `std::thread`, the result lands on `app.sync_rx`, and
/// `App::poll_sync` surfaces the outcome on the cmdline row. The
/// maildir watcher reconciles whatever the sync wrote to disk — we
/// only report on the command itself.
/// `:attach <path>` — queue a file for `multipart/mixed` attachment on
/// the active compose tab. Takes rest-of-line so unquoted spaces are
/// fine; a leading `~/` or bare `~` expands to `$HOME`. Existence + is-file
/// is checked at attach time; the bytes are read at serialize time so
/// the attachment list is just a `Vec<PathBuf>` until `:send`.
fn attach_path(app: &mut App, full_cmd: &str) {
    let raw = full_cmd.trim().strip_prefix("attach").unwrap_or("").trim();
    let Some(c) = app.active_compose_mut() else {
        app.status_error = Some("attach: not on a compose tab".into());
        return;
    };
    match mail_compose::validate_attachment(raw) {
        Ok(path) => {
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            c.attachments.push(path);
            let n = c.attachments.len();
            app.status_error = Some(format!("attached: {name} ({n} total)"));
        }
        Err(e) => app.status_error = Some(format!("attach: {e}")),
    }
}

/// `:detach <n>` — remove the 1-based attachment at index `n` from the
/// active compose tab.
fn detach_index(app: &mut App, parts: &mut std::str::SplitWhitespace) {
    let Some(arg) = parts.next() else {
        app.status_error = Some("detach: missing index".into());
        return;
    };
    let Ok(n) = arg.parse::<usize>() else {
        app.status_error = Some(format!("detach: not a number: {arg}"));
        return;
    };
    let Some(c) = app.active_compose_mut() else {
        app.status_error = Some("detach: not on a compose tab".into());
        return;
    };
    if n == 0 || n > c.attachments.len() {
        app.status_error = Some(format!(
            "detach: index {n} out of range (have {})",
            c.attachments.len()
        ));
        return;
    }
    let removed = c.attachments.remove(n - 1);
    let name = removed
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| removed.display().to_string());
    let rem = c.attachments.len();
    app.status_error = Some(format!("detached: {name} ({rem} remaining)"));
}

/// `:save <n> [path]` — write the inbox-side attachment at index `n` to
/// disk. With no `path`, writes to the current working directory using the
/// attachment's filename. With a directory path, appends the filename;
/// with a full file path, uses it verbatim. Refuses to overwrite an
/// existing file — caller can pick an explicit non-colliding path.
fn save_attachment(app: &mut App, full_cmd: &str) {
    let raw = full_cmd.trim().strip_prefix("save").unwrap_or("").trim();
    let mut parts = raw.splitn(2, char::is_whitespace);
    let Some(idx_arg) = parts.next().filter(|s| !s.is_empty()) else {
        app.status_error = Some("save: missing index".into());
        return;
    };
    let Ok(n) = idx_arg.parse::<usize>() else {
        app.status_error = Some(format!("save: not a number: {idx_arg}"));
        return;
    };
    let dest_arg = parts.next().map(str::trim).filter(|s| !s.is_empty());

    let Some(parsed) = app.inbox_parsed() else {
        app.status_error = Some("save: no parsed body".into());
        return;
    };
    if n == 0 || n > parsed.attachments.len() {
        app.status_error = Some(format!(
            "save: index {n} out of range (have {})",
            parsed.attachments.len()
        ));
        return;
    }
    let att = parsed.attachments[n - 1].clone();
    let target = match resolve_save_path(dest_arg, &att.filename) {
        Ok(p) => p,
        Err(e) => {
            app.status_error = Some(format!("save: {e}"));
            return;
        }
    };
    if target.exists() {
        app.status_error = Some(format!("save: refusing to overwrite {}", target.display()));
        return;
    }
    if let Err(e) = std::fs::write(&target, &att.bytes) {
        app.status_error = Some(format!("save: write {}: {e}", target.display()));
        return;
    }
    app.status_error = Some(format!("saved: {}", target.display()));
}

/// `:drag <n>` — write the attachment to a tempfile and hand it to the
/// `[reader].drag` command (typically `dragon` / `dragon-drop`). The
/// command stays open until the user drops the file into another app;
/// the worker thread runs detached and the tempfile lives until the OS
/// cleans /tmp. Errors when `[reader].drag` is unset.
fn drag_attachment_cmd(app: &mut App, parts: &mut std::str::SplitWhitespace, cfg: &Config) {
    let Some(arg) = parts.next() else {
        app.status_error = Some("drag: missing index".into());
        return;
    };
    let Ok(n) = arg.parse::<usize>() else {
        app.status_error = Some(format!("drag: not a number: {arg}"));
        return;
    };
    let Some(parsed) = app.inbox_parsed() else {
        app.status_error = Some("drag: no parsed body".into());
        return;
    };
    if n == 0 || n > parsed.attachments.len() {
        app.status_error = Some(format!(
            "drag: index {n} out of range (have {})",
            parsed.attachments.len()
        ));
        return;
    }
    let att = parsed.attachments[n - 1].clone();
    if let Err(e) = browser::drag_attachment(&att, cfg.reader.drag.as_deref()) {
        app.status_error = Some(format!("drag: {e:#}"));
        return;
    }
    app.status_error = Some(format!("dragging: {}", att.filename));
}

/// `:open-attachment <n>` — write the attachment to a tempfile preserving
/// its extension and hand the path to `[reader].browser` (which is the
/// same xdg-open-style command used by `:open`). The tempfile is left in
/// place because the spawn returns before the viewer reads it.
fn open_attachment(app: &mut App, parts: &mut std::str::SplitWhitespace, cfg: &Config) {
    let Some(arg) = parts.next() else {
        app.status_error = Some("open-attachment: missing index".into());
        return;
    };
    let Ok(n) = arg.parse::<usize>() else {
        app.status_error = Some(format!("open-attachment: not a number: {arg}"));
        return;
    };
    let Some(parsed) = app.inbox_parsed() else {
        app.status_error = Some("open-attachment: no parsed body".into());
        return;
    };
    if n == 0 || n > parsed.attachments.len() {
        app.status_error = Some(format!(
            "open-attachment: index {n} out of range (have {})",
            parsed.attachments.len()
        ));
        return;
    }
    let att = parsed.attachments[n - 1].clone();
    if let Err(e) = browser::open_attachment(&att, &cfg.reader.browser) {
        app.status_error = Some(format!("open-attachment: {e:#}"));
        return;
    }
    app.status_error = Some(format!("opening: {}", att.filename));
}

/// Resolve the destination path for `:save`. Tilde-expands a leading `~/`.
/// If `dest` is `None`, returns `<cwd>/<filename>`. If `dest` is a directory,
/// returns `<dest>/<filename>`. Otherwise returns `dest` as-is.
fn resolve_save_path(dest: Option<&str>, filename: &str) -> Result<std::path::PathBuf, String> {
    use std::path::PathBuf;
    let base_name = std::path::Path::new(filename)
        .file_name()
        .map(|s| s.to_owned())
        .ok_or_else(|| format!("attachment has no usable filename: {filename:?}"))?;
    let Some(dest) = dest else {
        let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
        return Ok(cwd.join(base_name));
    };
    let expanded: PathBuf = if let Some(rest) = dest.strip_prefix("~/") {
        let home =
            std::env::var_os("HOME").ok_or_else(|| "HOME not set, cannot expand ~/".to_string())?;
        PathBuf::from(home).join(rest)
    } else if dest == "~" {
        PathBuf::from(std::env::var_os("HOME").ok_or_else(|| "HOME not set".to_string())?)
    } else {
        PathBuf::from(dest)
    };
    if expanded.is_dir() {
        Ok(expanded.join(base_name))
    } else {
        Ok(expanded)
    }
}

fn dispatch_sync(app: &mut App, cfg: &Config) {
    if app.sync_rx.is_some() {
        app.status_error = Some("sync: already running".into());
        return;
    }
    let cmd = match cfg.sync.command.as_ref() {
        Some(c) if !c.is_empty() => c.clone(),
        _ => {
            app.status_error = Some("sync: command not configured".into());
            return;
        }
    };
    app.sync_rx = Some(store_sync::start_worker(cmd, app.event_tx.clone()));
    app.status_error = Some("syncing…".into());
}

fn send_active(app: &mut App, cfg: &Config) {
    let Some(c) = app.active_compose_mut() else {
        app.status_error = Some("send: not on a compose tab".into());
        return;
    };
    let account_name = c.account.clone();
    let origin_draft_path = c.origin_draft_path.clone();
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
    // Look up the Sent role on the account-derived binding list so the
    // disk path follows whatever the user wrote in `sent = "..."`.
    let sent_cur_dir = cfg.accounts.get(&account_name).and_then(|a| {
        crate::store::AccountSpec::from_account(&account_name, a)
            .binding_by_role(crate::config::FolderRole::Sent)
            .map(|b| b.path.join("cur"))
    });
    let bytes = match mail_compose::serialize(&draft) {
        Ok(b) => b,
        Err(e) => {
            app.status_error = Some(format!("send: serialize: {e}"));
            return;
        }
    };
    let label = send_label(&draft);
    let rx = mail_compose::start_send_worker(bytes, smtp_cmd, sent_cur_dir, app.event_tx.clone());
    app.pending_sends.push(PendingSend {
        rx,
        label: label.clone(),
        origin_draft_path,
    });
    // `:send` always runs on a compose tab, so close_active_tab won't
    // touch the inbox. Surface the close error defensively just in case.
    if let Err(msg) = app.close_active_tab() {
        app.status_error = Some(format!("send: {msg}"));
        return;
    }
    app.status_error = Some(format!("sending: {label}"));
}

/// Save the active compose tab as a draft in the account's Drafts
/// folder and close the tab. Shared between `:postpone` and the close
/// prompt's "Save" arm; either entry point returns `Err` with a
/// user-facing message on failure so the host can surface it in the
/// status row (and, for the prompt path, keep the overlay up).
pub fn postpone_active(app: &mut App, cfg: &Config) -> Result<(), String> {
    // Snapshot what we need from the compose tab before releasing the
    // mutable borrow — `app.self_writes` and `close_active_tab` both
    // need it next.
    let (account_name, draft, origin) = {
        let Some(c) = app.active_compose_mut() else {
            return Err("not on a compose tab".into());
        };
        let draft = c.collect_draft().map_err(|e| format!("read body: {e}"))?;
        (c.account.clone(), draft, c.origin_draft_path.clone())
    };

    let Some(account) = cfg.accounts.get(&account_name) else {
        return Err(format!("unknown account: {account_name}"));
    };
    let spec = crate::store::AccountSpec::from_account(&account_name, account);
    let Some(drafts_binding) = spec.binding_by_role(crate::config::FolderRole::Drafts) else {
        return Err(format!("no Drafts folder configured for {account_name}"));
    };
    let drafts_cur = drafts_binding.path.join("cur");

    let saved = mail_compose::save_draft(&draft, &drafts_cur, &app.self_writes)
        .map_err(|e| format!("save to {}: {e}", drafts_binding.path.display()))?;

    // Delete the originating draft if this composer was opened from
    // one. Record the self-write first so the maildir watcher doesn't
    // echo the deletion back as an external change. A NotFound here
    // (mbsync raced us, or the user deleted manually) is benign;
    // anything else surfaces as a status hint but doesn't unsave the
    // new draft we just wrote.
    if let Some(old) = origin
        && old != saved
    {
        app.self_writes.record(&old);
        match std::fs::remove_file(&old) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                app.self_writes.consume(&old);
            }
            Err(e) => {
                app.self_writes.consume(&old);
                app.status_error = Some(format!("postpone: clean old draft: {e}"));
            }
        }
    }

    // Clear the prompt before tearing down the tab (the screen is
    // about to drop, but the close-prompt's contract is "host clears
    // on success" so be explicit).
    if let Some(c) = app.active_compose_mut() {
        c.confirm_close = None;
    }
    app.close_active_tab().map_err(|s| s.to_string())?;
    Ok(())
}

/// Short identifier for a send used in the status row. Prefers the
/// subject; falls back to the first recipient when the subject is
/// empty so the user can still tell concurrent sends apart.
fn send_label(draft: &Draft) -> String {
    let subject = draft.subject.trim();
    if !subject.is_empty() {
        return subject.to_string();
    }
    draft
        .to
        .iter()
        .chain(draft.cc.iter())
        .chain(draft.bcc.iter())
        .find(|r| !r.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| "(no subject)".to_string())
}

/// Pub re-export of the cmdline `:compose` handler so the `c`
/// keybinding can open a blank compose tab without going through the
/// cmdline buffer.
pub fn open_blank_compose_external(app: &mut App, cfg: &Config) {
    open_blank_compose(app, cfg);
}

/// `:from [<account>]` — switch the sending identity on the active
/// compose tab. With no argument, opens the same dropdown as Alt-f /
/// Enter-on-From so the user can pick interactively. With an argument
/// it validates against `[accounts.*]` and applies directly. Both
/// paths rewrite `screen.account` (drives SMTP + Sent folder) and the
/// visible `From:` header in lockstep.
fn switch_from(app: &mut App, cfg: &Config, name: Option<&str>) {
    let Some(c) = app.active_compose_mut() else {
        app.status_error = Some("from: not on a compose tab".into());
        return;
    };
    match name {
        None => compose::open_from_picker(c, cfg),
        Some(n) => match compose::set_account(c, cfg, n) {
            Ok(()) => {}
            Err(msg) => app.status_error = Some(format!("from: {msg}")),
        },
    }
}

/// `:edit` — escape from the native body editor into `$EDITOR` under a
/// pty. The main loop's `spawn_pending_editors` picks up the flag,
/// flushes the body to a tempfile, and starts the pty session.
fn escape_to_external_editor(app: &mut App) {
    let Some(c) = app.active_compose_mut() else {
        app.status_error = Some("edit: not on a compose tab".into());
        return;
    };
    if c.editor.is_some() {
        app.status_error = Some("edit: editor already active".into());
        return;
    }
    c.editor_pending = true;
}

fn open_blank_compose(app: &mut App, cfg: &Config) {
    // Pre-select the From identity. From an account-scoped inbox the
    // pre-selection is "obvious" — the scope itself implies the sender.
    // From the unified `[all]` scope there is no implied account, so
    // fall back to `[accounts.*].primary = true` (resolved by
    // `config::primary_account_name`, which also handles the
    // alphabetic-tiebreaker for "no primary set" / "multiple primaries").
    // Either way the picker still opens via Enter on the From field if
    // the user wants to switch identity before sending.
    let name = match app.inbox().current_account.clone() {
        Some(scoped) => scoped,
        None => match config::primary_account_name(cfg) {
            Some(n) => n,
            None => {
                app.status_error = Some("compose: no accounts configured".into());
                return;
            }
        },
    };
    let Some(account) = cfg.accounts.get(&name) else {
        // Defensive: scope name should always exist in cfg, but if the
        // user removed the account from config and `:reload` lands, we
        // shouldn't panic — surface a clean error instead.
        app.status_error = Some(format!("compose: unknown account: {name}"));
        return;
    };
    let draft = Draft::new_blank(&name, &account.from);
    match ComposeScreen::from_draft(draft) {
        Ok(mut screen) => {
            // `[compose].mode = "external"`: behave like the old
            // pty-only path and spawn `$EDITOR` as soon as the tab
            // renders. Otherwise the native editor is active and the
            // user invokes `:edit` on demand.
            if matches!(cfg.compose.mode, config::ComposeMode::External) {
                screen.editor_pending = true;
            }
            app.open_compose(screen);
        }
        Err(e) => {
            app.status_error = Some(format!("compose: {e}"));
        }
    }
}

/// If the currently-selected list row sits in the active account's
/// Drafts folder, open it in a new compose tab and return `true`.
/// Returns `false` for non-Drafts rows so the caller can fall back to
/// its default Enter behaviour (focusing the reader). Failures while
/// opening a known-Drafts row surface in the status row and still
/// return `true`; the caller treats them as "handled."
pub fn resume_selected_draft_if_drafts(app: &mut App, cfg: &Config) -> bool {
    let Some(row) = app.inbox().selected_row().map(|t| t.row.clone()) else {
        return false;
    };
    let Some(account) = cfg.accounts.get(&row.account) else {
        return false;
    };
    if account.drafts.as_deref() != Some(row.folder.as_str()) {
        return false;
    }

    let path = row.path.clone();
    let headers = match parse::read_headers(&path) {
        Ok(Some(h)) => h,
        Ok(None) => {
            app.status_error = Some("resume: failed to parse headers".into());
            return true;
        }
        Err(e) => {
            app.status_error = Some(format!("resume: {e:#}"));
            return true;
        }
    };
    let body = match parse::read_body(&path) {
        Ok(b) => b,
        Err(e) => {
            app.status_error = Some(format!("resume: {e:#}"));
            return true;
        }
    };
    let attachment_count = parse::count_attachments(&path);

    let body_text = body
        .plain
        .clone()
        .or_else(|| body.html.clone())
        .unwrap_or_default();

    let draft = Draft {
        account: row.account.clone(),
        from: account.from.clone(),
        to: headers.to.clone(),
        cc: headers.cc.clone(),
        bcc: headers.bcc.clone(),
        subject: headers.subject.clone().unwrap_or_default(),
        body: body_text,
        in_reply_to: headers.in_reply.clone(),
        references: headers.refs.clone(),
        attachments: Vec::new(),
    };

    match ComposeScreen::from_draft(draft) {
        Ok(mut screen) => {
            screen.origin_draft_path = Some(path);
            app.open_compose(screen);
            if attachment_count > 0 {
                app.status_error = Some(format!(
                    "draft re-opened — {attachment_count} attachment(s) dropped, re-:attach as needed"
                ));
            }
        }
        Err(e) => {
            app.status_error = Some(format!("resume: {e}"));
        }
    }
    true
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
        Ok(mut screen) => {
            if matches!(cfg.compose.mode, config::ComposeMode::External) {
                screen.editor_pending = true;
            }
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

    #[test]
    fn dispatch_sync_without_command_reports_error() {
        let (mut app, cfg) = test_app();
        dispatch("sync", &mut app, &cfg);
        assert!(app.sync_rx.is_none());
        let err = app.status_error.as_deref().expect("status");
        assert!(err.contains("not configured"), "got {err:?}");
    }

    #[test]
    fn dispatch_sync_with_empty_command_reports_error() {
        let (mut app, mut cfg) = test_app();
        cfg.sync.command = Some(Vec::new());
        dispatch("sync", &mut app, &cfg);
        assert!(app.sync_rx.is_none());
        let err = app.status_error.as_deref().expect("status");
        assert!(err.contains("not configured"), "got {err:?}");
    }

    #[test]
    fn dispatch_sync_spawns_worker_and_completes() {
        let (mut app, mut cfg) = test_app();
        cfg.sync.command = Some(vec!["/bin/sh".into(), "-c".into(), "exit 0".into()]);
        dispatch("sync", &mut app, &cfg);
        assert!(app.sync_rx.is_some(), "worker should be in flight");
        let pending = app.status_error.as_deref().expect("status");
        assert!(pending.contains("syncing"), "got {pending:?}");

        // Spin the poll loop briefly so the worker has time to finish.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.sync_rx.is_some() {
            app.poll_sync();
            if std::time::Instant::now() >= deadline {
                panic!("sync worker did not complete");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(app.status_error.as_deref(), Some("synced"));
    }

    #[test]
    fn dispatch_sync_already_running_reports_error() {
        let (mut app, mut cfg) = test_app();
        // `sleep 5` keeps the first worker alive long enough that the
        // second `:sync` lands on a busy slot.
        cfg.sync.command = Some(vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()]);
        dispatch("sync", &mut app, &cfg);
        assert!(app.sync_rx.is_some());
        dispatch("sync", &mut app, &cfg);
        let err = app.status_error.as_deref().expect("status");
        assert!(err.contains("already running"), "got {err:?}");
    }

    #[test]
    fn dispatch_sync_nonzero_exit_reports_failure() {
        let (mut app, mut cfg) = test_app();
        cfg.sync.command = Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'mbsync: broken pipe' 1>&2; exit 3".into(),
        ]);
        dispatch("sync", &mut app, &cfg);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.sync_rx.is_some() {
            app.poll_sync();
            if std::time::Instant::now() >= deadline {
                panic!("sync worker did not complete");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let err = app.status_error.as_deref().expect("status");
        assert!(err.starts_with("sync failed:"), "got {err:?}");
        assert!(err.contains("exit 3"), "got {err:?}");
    }

    #[test]
    fn resolve_save_path_defaults_to_cwd() {
        let p = resolve_save_path(None, "report.pdf").unwrap();
        assert!(p.ends_with("report.pdf"), "{p:?}");
        assert!(p.is_absolute(), "{p:?}");
    }

    #[test]
    fn resolve_save_path_appends_filename_to_directory() {
        let tmp = std::env::temp_dir().join("epost-test-save-dir");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let p = resolve_save_path(Some(tmp.to_str().unwrap()), "report.pdf").unwrap();
        assert_eq!(p, tmp.join("report.pdf"));
    }

    #[test]
    fn resolve_save_path_uses_explicit_path_verbatim() {
        let tmp = std::env::temp_dir().join("epost-test-explicit.pdf");
        let _ = std::fs::remove_file(&tmp);
        let p = resolve_save_path(Some(tmp.to_str().unwrap()), "report.pdf").unwrap();
        assert_eq!(p, tmp);
    }

    #[test]
    fn dispatch_save_with_no_body_reports_error() {
        let (mut app, cfg) = test_app();
        dispatch("save 1", &mut app, &cfg);
        let err = app.status_error.as_deref().expect("status");
        assert!(err.contains("no parsed body"), "got {err:?}");
    }

    #[test]
    fn dispatch_save_missing_index_reports_error() {
        let (mut app, cfg) = test_app();
        dispatch("save", &mut app, &cfg);
        let err = app.status_error.as_deref().expect("status");
        assert!(err.contains("missing index"), "got {err:?}");
    }

    #[test]
    fn dispatch_open_attachment_missing_index_reports_error() {
        let (mut app, cfg) = test_app();
        dispatch("open-attachment", &mut app, &cfg);
        let err = app.status_error.as_deref().expect("status");
        assert!(err.contains("missing index"), "got {err:?}");
    }

    #[test]
    fn dispatch_drag_missing_index_reports_error() {
        let (mut app, cfg) = test_app();
        dispatch("drag", &mut app, &cfg);
        let err = app.status_error.as_deref().expect("status");
        assert!(err.contains("missing index"), "got {err:?}");
    }

    #[test]
    fn dispatch_drag_with_no_body_reports_error() {
        let (mut app, cfg) = test_app();
        dispatch("drag 1", &mut app, &cfg);
        let err = app.status_error.as_deref().expect("status");
        assert!(err.contains("no parsed body"), "got {err:?}");
    }
}
