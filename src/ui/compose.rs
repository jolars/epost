//! Compose screen: aerc-style form for one in-flight draft. Lives in
//! its own tab so the user can `Ctrl-PgDn` away to other work and come
//! back. Header fields (From / To / Cc / Bcc / Subject) edit in-place
//! via `TextInput`; the body is owned by a vim-style native editor
//! ([`crate::ui::compose_body::BodyEditor`]). `:edit` materialises the
//! body to a tempfile and hands it to `$EDITOR` under a pty for users
//! who want their heavy editor config.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use tempfile::NamedTempFile;
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::config::Config;
use crate::mail::compose::Draft;
pub use crate::ui::compose_body::KeyOutcome;
use crate::ui::compose_body::{BodyEditor, BodyMode, VisualKind};
use crate::ui::embed::EditorSession;
use crate::ui::style::pane_block;
use crate::ui::text_input::TextInput;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeField {
    From,
    To,
    Cc,
    Bcc,
    Subject,
    Body,
}

pub struct ComposeScreen {
    pub title: String,
    pub account: String,
    pub from: TextInput,
    pub to: TextInput,
    pub cc: TextInput,
    pub bcc: TextInput,
    pub subject: TextInput,
    pub focused: ComposeField,
    /// Most recent non-`Body` field that held focus. Used by the
    /// Ctrl-K "jump back to header" shortcut so a Ctrl-J → edit body →
    /// Ctrl-K round-trip returns to the same row the user came from
    /// (instead of always snapping to the first header). Updated by
    /// `set_focus`, `focus_next`, `focus_prev`, and the From picker
    /// commit path; never set to `Body` itself.
    pub last_header_focused: ComposeField,
    /// Vim-style body editor. Always present; holds the canonical body
    /// text. When the user invokes `:edit`, the contents are flushed to
    /// `body_tempfile` and `$EDITOR` runs against that path; on exit we
    /// read the tempfile back into here and drop it.
    pub body: BodyEditor,
    /// Set by `:edit` (or by the `mode = "external"` config branch at
    /// tab open) to ask the main loop to spawn an `$EDITOR` session
    /// against the body. Cleared once the session is live.
    pub editor_pending: bool,
    /// Embedded `$EDITOR` pty + parsed screen. While `Some`, all
    /// keys forward to the pty and the body region renders the
    /// editor inline. Transient: lifetime of one `$EDITOR` invocation.
    pub editor: Option<EditorSession>,
    /// Transient tempfile owning the body text while `$EDITOR` is live.
    /// Created right before spawn, dropped after the post-exit reload.
    pub body_tempfile: Option<NamedTempFile>,
    /// Most recent body rect (inner of the body block) recorded by
    /// `compose::draw`. Used to size the pty when spawning and to
    /// resize it on terminal resize.
    pub last_body_inner: Option<(u16, u16)>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// Files queued for `multipart/mixed` attachment. Maintained by
    /// `:attach <path>` / `:detach <n>`; rendered as a read-only row in
    /// the compose header when non-empty.
    pub attachments: Vec<PathBuf>,
    /// Open account-picker overlay. `Some` while the From dropdown is
    /// active; `None` ambient. Triggered by Enter on the From field;
    /// j/k or arrows navigate, Enter commits, Esc cancels. Selecting an
    /// option rewrites both `account` (drives SMTP + Sent folder) and
    /// `from` (the visible header) so changing the displayed identity
    /// also changes the actual sending account.
    pub from_picker: Option<FromPicker>,
    /// File this composer was loaded from in `Drafts/cur/`. `Some` when
    /// the tab came from "Enter on a draft" — re-saving / `:send`
    /// success uses it to delete the stale draft file. `None` for fresh
    /// `:compose`, `:reply`, and `:forward` flows.
    pub origin_draft_path: Option<PathBuf>,
    /// Open close-confirm prompt. `Some` while the "Save / Discard /
    /// Cancel" overlay is up; `None` ambient. Set by `:close` when the
    /// composer is dirty; cleared by Cancel (or by Save on success).
    pub confirm_close: Option<CloseConfirm>,
}

/// State for the close-confirm overlay. Currently a unit struct used
/// as a presence flag — the prompt has no internal state beyond
/// "open" — but kept as a named type so a future expansion (e.g.
/// remembering which key arm errored last) doesn't need a struct
/// rewrite.
#[derive(Debug, Default)]
pub struct CloseConfirm;

/// One entry in the From dropdown: an account key (`[accounts.<name>]`)
/// paired with its configured `from` display string. The picker is
/// built from a `Config` snapshot at open time so it survives without a
/// borrow back into the app's config.
#[derive(Debug, Clone)]
pub struct FromOption {
    pub account: String,
    pub from: String,
}

/// State for the active From dropdown. Options are sorted by account
/// name so the list order is stable across opens regardless of
/// `HashMap` iteration order.
#[derive(Debug)]
pub struct FromPicker {
    pub options: Vec<FromOption>,
    pub selected: usize,
}

impl ComposeScreen {
    /// Build a fresh compose screen from a Draft. The native body
    /// editor is initialised with `draft.body` (so quoted text for
    /// replies and forwards lands in the editor); `editor_pending`
    /// starts false. Callers that want `$EDITOR` to auto-spawn on tab
    /// open (the `mode = "external"` config branch) flip
    /// `editor_pending = true` after constructing.
    pub fn from_draft(draft: Draft) -> std::io::Result<Self> {
        let title = title_for(&draft);
        let body = BodyEditor::new(&draft.body);
        Ok(Self {
            title,
            account: draft.account,
            from: TextInput::from_string(draft.from),
            to: TextInput::from_string(draft.to.join(", ")),
            cc: TextInput::from_string(draft.cc.join(", ")),
            bcc: TextInput::from_string(draft.bcc.join(", ")),
            subject: TextInput::from_string(draft.subject),
            // Start on To. New compose: it's empty and that's where the
            // user types first. Reply/forward: it's pre-filled with the
            // original sender(s); user can Ctrl-J straight to the body.
            // Picked over From because From is a fixed identity normally
            // changed via the picker (Enter), not by typing.
            focused: ComposeField::To,
            last_header_focused: ComposeField::To,
            body,
            editor_pending: false,
            editor: None,
            body_tempfile: None,
            last_body_inner: None,
            in_reply_to: draft.in_reply_to,
            references: draft.references,
            attachments: draft.attachments,
            from_picker: None,
            origin_draft_path: None,
            confirm_close: None,
        })
    }

    /// Materialise the current body to a fresh tempfile, store the
    /// tempfile on the screen so it stays alive while `$EDITOR` runs,
    /// and return the path to hand to the pty. Used by both the
    /// explicit `:edit` flow and the `mode = "external"` auto-spawn.
    pub fn materialize_body_tempfile(&mut self) -> std::io::Result<PathBuf> {
        let f = NamedTempFile::with_prefix("epost-body-")?;
        std::fs::write(f.path(), self.body.text())?;
        let path = f.path().to_path_buf();
        self.body_tempfile = Some(f);
        Ok(path)
    }

    /// Read the body tempfile back into the native editor and drop the
    /// tempfile. Called by the main loop after the `$EDITOR` session
    /// finishes.
    pub fn reload_body_from_tempfile(&mut self) {
        if let Some(f) = self.body_tempfile.take() {
            let text = std::fs::read_to_string(f.path()).unwrap_or_default();
            self.body.set_text(&text);
        }
    }

    /// True when the body has user-typed content (or pre-filled
    /// quoted text from reply/forward). Drives the `*` marker on the
    /// tab label. Cheap — we just check whether the textarea is empty.
    pub fn body_is_dirty(&self) -> bool {
        let lines = self.body.textarea.lines();
        !(lines.len() == 1 && lines[0].is_empty())
    }

    /// True when any addressable field has content — drives the
    /// "save draft?" prompt on `:close`. Conservative: a `:reply`
    /// will always count as dirty because the template populates
    /// To+Subject+quoted body. That's intended; the alternative
    /// (touched-since-open tracking) silently loses content when the
    /// dirty heuristic guesses wrong, and the user explicitly chose
    /// the conservative version.
    pub fn is_dirty(&self) -> bool {
        !self.to.as_str().is_empty()
            || !self.cc.as_str().is_empty()
            || !self.bcc.as_str().is_empty()
            || !self.subject.as_str().is_empty()
            || !self.attachments.is_empty()
            || self.body_is_dirty()
    }

    /// Build a Draft from the current field contents + native editor.
    pub fn collect_draft(&self) -> std::io::Result<Draft> {
        Ok(Draft {
            account: self.account.clone(),
            from: self.from.as_str().to_string(),
            to: split_addresses(self.to.as_str()),
            cc: split_addresses(self.cc.as_str()),
            bcc: split_addresses(self.bcc.as_str()),
            subject: self.subject.as_str().to_string(),
            body: self.body.text(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            attachments: self.attachments.clone(),
        })
    }

    /// Switch focus to `field`, also pinning `last_header_focused`
    /// whenever the destination is a header row. All focus changes
    /// should go through here (Tab/BackTab, Ctrl-J/Ctrl-K, the From
    /// picker commit) so the Ctrl-K "return to last header" jump always
    /// points at the most recent header the user actually visited.
    pub fn set_focus(&mut self, field: ComposeField) {
        if field != ComposeField::Body {
            self.last_header_focused = field;
        }
        self.focused = field;
    }

    pub fn focus_next(&mut self) {
        self.set_focus(match self.focused {
            ComposeField::From => ComposeField::To,
            ComposeField::To => ComposeField::Cc,
            ComposeField::Cc => ComposeField::Bcc,
            ComposeField::Bcc => ComposeField::Subject,
            ComposeField::Subject => ComposeField::Body,
            ComposeField::Body => ComposeField::From,
        });
    }

    pub fn focus_prev(&mut self) {
        self.set_focus(match self.focused {
            ComposeField::From => ComposeField::Body,
            ComposeField::To => ComposeField::From,
            ComposeField::Cc => ComposeField::To,
            ComposeField::Bcc => ComposeField::Cc,
            ComposeField::Subject => ComposeField::Bcc,
            ComposeField::Body => ComposeField::Subject,
        });
    }

    fn focused_input_mut(&mut self) -> Option<&mut TextInput> {
        Some(match self.focused {
            ComposeField::From => &mut self.from,
            ComposeField::To => &mut self.to,
            ComposeField::Cc => &mut self.cc,
            ComposeField::Bcc => &mut self.bcc,
            ComposeField::Subject => &mut self.subject,
            ComposeField::Body => return None,
        })
    }
}

/// Compose-mode key dispatch. Only called when no `$EDITOR` pty
/// session is active — `ui::keys::handle` forwards everything to the
/// pty when one is live, so this path handles form-level navigation
/// plus the native body editor.
///
/// Returns [`KeyOutcome::PassThrough`] when the key should fall through
/// to app-level dispatch (the only currently-passthrough keys are
/// `:` `/` `?` from the body editor's Normal / Visual modes; the host
/// uses them for `:`-cmdline / search). Everything else is consumed.
pub fn handle_key(screen: &mut ComposeScreen, k: KeyEvent, cfg: &Config) -> KeyOutcome {
    // Close-confirm prompt sits in front of everything else (including
    // the From picker) so a stray keystroke during the prompt can't
    // also drive form state. All keys are consumed; the Save and
    // Discard arms bubble back to the host loop via the new
    // `KeyOutcome` variants so it can call `close_active_tab` /
    // `postpone_active` with the borrows it has and we don't.
    if screen.confirm_close.is_some() {
        return handle_confirm_close_key(screen, k);
    }

    // When the From picker is open, capture navigation/commit keys here
    // and short-circuit the rest of the form dispatch. Anything we don't
    // recognise is swallowed so a stray keystroke can't both edit a
    // header field and dismiss the popup in one event.
    if screen.from_picker.is_some() {
        handle_from_picker_key(screen, k);
        return KeyOutcome::Consumed;
    }

    // Alt-e drops into `$EDITOR` from any field. Same end result as
    // `:edit`, just a chord. Kept for muscle memory from the old
    // external-only flow.
    if k.modifiers.contains(KeyModifiers::ALT) && k.code == KeyCode::Char('e') {
        screen.editor_pending = true;
        return KeyOutcome::Consumed;
    }

    // Alt-f opens the From / account picker from any field. Mirrors
    // Alt-e for the editor — the picker is otherwise only reachable
    // by tabbing to From and pressing Enter, which is hard to discover.
    if k.modifiers.contains(KeyModifiers::ALT) && k.code == KeyCode::Char('f') {
        open_from_picker(screen, cfg);
        return KeyOutcome::Consumed;
    }

    // Ctrl-J / Ctrl-K: explicit header↔body jump. Intercepted before the
    // body editor so they don't get swallowed by Insert mode or interpreted
    // as half-page-up in Normal. Ctrl-J always jumps to Body. Ctrl-K only
    // fires from Body — from a header it falls through so `TextInput`'s
    // readline `kill-to-end` (Ctrl-K) keeps working there.
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        match k.code {
            KeyCode::Char('j') => {
                screen.set_focus(ComposeField::Body);
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('k') if screen.focused == ComposeField::Body => {
                screen.set_focus(screen.last_header_focused);
                return KeyOutcome::Consumed;
            }
            _ => {}
        }
    }

    // Body: let the native vim editor have first crack. If it returns
    // PassThrough (only happens from Normal/Visual, for `:` `/` `?`),
    // the caller will route the key to the app dispatch. Tab/BackTab
    // also passes through the editor and we use it below to cycle out
    // of the body to other fields.
    if screen.focused == ComposeField::Body {
        if let KeyOutcome::Consumed = screen.body.handle_key(k) {
            return KeyOutcome::Consumed;
        }
        // Tab / BackTab: cycle fields. Other passthroughs (`:` etc.)
        // are routed up to the app.
        match k.code {
            KeyCode::Tab => {
                screen.focus_next();
                return KeyOutcome::Consumed;
            }
            KeyCode::BackTab => {
                screen.focus_prev();
                return KeyOutcome::Consumed;
            }
            _ => return KeyOutcome::PassThrough,
        }
    }

    // Non-Body fields: Tab/BackTab cycle the form, Enter on From opens
    // the picker, otherwise route to the TextInput.
    match k.code {
        KeyCode::Tab => {
            screen.focus_next();
            return KeyOutcome::Consumed;
        }
        KeyCode::BackTab => {
            screen.focus_prev();
            return KeyOutcome::Consumed;
        }
        _ => {}
    }
    // Enter on the From field opens the account picker. The TextInput
    // doesn't bind Enter, so claiming it here is non-breaking — manual
    // editing of From still works for one-off display-name tweaks; the
    // picker is for switching the *sending identity* (account, which
    // drives SMTP routing and the Sent-folder destination).
    if screen.focused == ComposeField::From && k.code == KeyCode::Enter {
        open_from_picker(screen, cfg);
        return KeyOutcome::Consumed;
    }
    if let Some(input) = screen.focused_input_mut() {
        input.handle(k);
    }
    KeyOutcome::Consumed
}

fn body_block_title(screen: &ComposeScreen, editing: bool, focused: bool) -> String {
    if editing {
        return "Body — $EDITOR (exit the editor to return to the form)".into();
    }
    if !focused {
        return "Body".into();
    }
    let mode = match screen.body.mode {
        BodyMode::Normal => "NORMAL",
        BodyMode::Insert => "INSERT",
        BodyMode::Visual(VisualKind::Char) => "VISUAL",
        BodyMode::Visual(VisualKind::Line) => "V-LINE",
    };
    format!("Body — {mode}")
}

fn native_body_hint(body: &BodyEditor) -> &'static str {
    match body.mode {
        BodyMode::Insert => " -- INSERT --  Esc to leave  :send  :close ",
        BodyMode::Visual(_) => " -- VISUAL --  y yank  d delete  c change  Esc cancel ",
        BodyMode::Normal => " i insert  v/V visual  yy/dd/p yank/del/paste  :edit  :send  :close ",
    }
}

pub fn open_from_picker(screen: &mut ComposeScreen, cfg: &Config) {
    let options = collect_from_options(cfg);
    if options.is_empty() {
        return;
    }
    let selected = options
        .iter()
        .position(|o| o.account == screen.account)
        .unwrap_or(0);
    screen.from_picker = Some(FromPicker { options, selected });
}

/// Switch the sending identity to `account` (must exist in `cfg`),
/// rewriting both `screen.account` (drives SMTP routing + Sent-folder
/// destination) and the visible `From:` header to the account's
/// configured display string. Returns `Err` with a user-facing message
/// when the account isn't configured.
pub fn set_account(screen: &mut ComposeScreen, cfg: &Config, account: &str) -> Result<(), String> {
    let Some(acc) = cfg.accounts.get(account) else {
        return Err(format!("unknown account: {account}"));
    };
    screen.account = account.to_string();
    screen.from = TextInput::from_string(acc.from.clone());
    screen.from_picker = None;
    Ok(())
}

/// Close-confirm prompt key handler. The Save and Discard arms return
/// a `KeyOutcome` variant the host loop acts on with `&mut App` in
/// scope; Cancel just clears the prompt and stays in the composer.
/// Any unrecognised key is consumed so it can't also reach the form.
///
/// On Save we deliberately leave `confirm_close` set: the host may
/// fail to write the draft (no Drafts folder configured, disk full,
/// …) and needs to keep the prompt up so the user can pick Discard
/// or Cancel instead. The host clears `confirm_close` on success.
fn handle_confirm_close_key(screen: &mut ComposeScreen, k: KeyEvent) -> KeyOutcome {
    match k.code {
        KeyCode::Esc => {
            screen.confirm_close = None;
            KeyOutcome::Consumed
        }
        KeyCode::Char('c') | KeyCode::Char('C') => {
            screen.confirm_close = None;
            KeyOutcome::Consumed
        }
        KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Char('y') | KeyCode::Char('Y') => {
            KeyOutcome::SaveAndClose
        }
        KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Char('n') | KeyCode::Char('N') => {
            screen.confirm_close = None;
            KeyOutcome::CloseTab
        }
        _ => KeyOutcome::Consumed,
    }
}

fn handle_from_picker_key(screen: &mut ComposeScreen, k: KeyEvent) {
    let Some(picker) = screen.from_picker.as_mut() else {
        return;
    };
    match k.code {
        KeyCode::Esc => {
            screen.from_picker = None;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let last = picker.options.len().saturating_sub(1);
            picker.selected = picker.selected.saturating_add(1).min(last);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            picker.selected = picker.selected.saturating_sub(1);
        }
        KeyCode::Enter => {
            if let Some(opt) = picker.options.get(picker.selected).cloned() {
                screen.account = opt.account;
                screen.from = TextInput::from_string(opt.from);
            }
            screen.from_picker = None;
        }
        _ => {}
    }
}

/// Build the From-dropdown choices from configured accounts. Sorted by
/// account name so the list order is stable across opens (the underlying
/// `HashMap` iteration order isn't).
fn collect_from_options(cfg: &Config) -> Vec<FromOption> {
    let mut opts: Vec<FromOption> = cfg
        .accounts
        .iter()
        .map(|(name, acc)| FromOption {
            account: name.clone(),
            from: acc.from.clone(),
        })
        .collect();
    opts.sort_by(|a, b| a.account.cmp(&b.account));
    opts
}

/// Render the compose screen into `area`. Layout: a header block
/// (From / To / Cc / Bcc / Subject, plus an Attachments row when files
/// are queued), a body-preview pane below, hint line at the bottom.
pub fn draw(f: &mut Frame, area: Rect, screen: &mut ComposeScreen) {
    let show_attachments = !screen.attachments.is_empty();
    // 5 fixed rows + 2 border lines = 7; +1 for the Attachments row when shown.
    let header_height: u16 = if show_attachments { 8 } else { 7 };
    let outer = Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(area);
    let header_area = outer[0];
    let body_area = outer[1];
    let hint_area = outer[2];

    let header_block = pane_block("Compose", true);
    let header_inner = header_block.inner(header_area);
    f.render_widget(header_block, header_area);

    let mut constraints: Vec<Constraint> = vec![Constraint::Length(1); 5];
    if show_attachments {
        constraints.push(Constraint::Length(1));
    }
    let rows = Layout::vertical(constraints).split(header_inner);
    let from_row = rows[0];
    render_field(
        f,
        from_row,
        "From:    ",
        &screen.from,
        screen.focused == ComposeField::From,
    );
    render_field(
        f,
        rows[1],
        "To:      ",
        &screen.to,
        screen.focused == ComposeField::To,
    );
    render_field(
        f,
        rows[2],
        "Cc:      ",
        &screen.cc,
        screen.focused == ComposeField::Cc,
    );
    render_field(
        f,
        rows[3],
        "Bcc:     ",
        &screen.bcc,
        screen.focused == ComposeField::Bcc,
    );
    render_field(
        f,
        rows[4],
        "Subject: ",
        &screen.subject,
        screen.focused == ComposeField::Subject,
    );
    if show_attachments {
        render_attachments_row(f, rows[5], &screen.attachments);
    }

    let editing = screen.editor.is_some();
    let body_focused = screen.focused == ComposeField::Body;
    let body_title = body_block_title(screen, editing, body_focused);
    let body_block = pane_block(&body_title, body_focused || editing);
    let body_inner = body_block.inner(body_area);
    screen.last_body_inner = Some((body_inner.height, body_inner.width));
    f.render_widget(body_block, body_area);

    if let Some(ed) = screen.editor.as_mut() {
        if ed.is_primed() {
            ed.resize(body_inner.height, body_inner.width);
            let hide = ed.hide_cursor();
            let (c_row, c_col) = ed.cursor_position();
            ed.with_screen(|s| {
                // Suppress tui-term's cell-painted cursor: we drive
                // the real host cursor below so the user gets a
                // genuine bar / block (DECSCUSR-controlled) instead
                // of an always-block character pasted into the grid.
                let widget = PseudoTerminal::new(s).cursor(Cursor::default().visibility(false));
                f.render_widget(widget, body_inner);
            });
            if !hide && c_row < body_inner.height && c_col < body_inner.width {
                f.set_cursor_position((body_inner.x + c_col, body_inner.y + c_row));
            }
        } else {
            let placeholder = Paragraph::new(Line::from(Span::styled(
                "starting $EDITOR…",
                Style::default().fg(Color::DarkGray),
            )));
            f.render_widget(placeholder, body_inner);
        }
    } else {
        // Native editor: tui-textarea's own cursor cell is disabled in
        // `BodyEditor::new` so we drive the real host cursor here. Mode
        // is signalled by DECSCUSR (steady block in Normal/Visual, steady
        // bar in Insert) — emitted by the main loop's
        // `collect_cursor_style_escapes` based on `body.mode`. The data
        // cursor maps to (body_inner.x + col, body_inner.y + row); for
        // bodies that fit in the pane (the typical case) there's no
        // scroll to subtract. Bodies large enough to scroll the textarea
        // will see a brief cursor-position drift until the next motion;
        // acceptable v1 trade-off vs poking at tui-textarea internals.
        f.render_widget(&screen.body.textarea, body_inner);
        if body_focused {
            let (row, col) = screen.body.textarea.cursor();
            let x = body_inner.x.saturating_add(col as u16);
            let y = body_inner.y.saturating_add(row as u16);
            if x < body_inner.x.saturating_add(body_inner.width)
                && y < body_inner.y.saturating_add(body_inner.height)
            {
                f.set_cursor_position((x, y));
            }
        }
    }

    let hint = if editing {
        Line::from(Span::styled(
            " editing in $EDITOR — exit the editor (e.g. :wq) to return to the form ",
            Style::default().fg(Color::DarkGray),
        ))
    } else if screen.from_picker.is_some() {
        Line::from(Span::styled(
            " ↑/↓ or j/k pick account  Enter select  Esc cancel ",
            Style::default().fg(Color::DarkGray),
        ))
    } else if body_focused {
        Line::from(Span::styled(
            native_body_hint(&screen.body),
            Style::default().fg(Color::DarkGray),
        ))
    } else if screen.focused == ComposeField::From {
        Line::from(Span::styled(
            " Enter/Alt-f pick account  Tab/Shift-Tab fields  Alt-e edit body  :send  :close ",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(Span::styled(
            " Alt-f pick account  Tab/Shift-Tab fields  Alt-e edit body  :send  :close ",
            Style::default().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(hint), hint_area);

    // The From dropdown overlays the form when open. Drawn last so it
    // paints over whatever sits below the From row (To/Cc, the body
    // pane, etc.). The Clear widget blanks the underlying cells so a
    // rendered $EDITOR or body preview doesn't bleed through the popup.
    if let Some(picker) = screen.from_picker.as_ref() {
        draw_from_picker(f, from_row, picker, area);
    }

    // The Save / Discard / Cancel prompt sits in front of everything
    // (including the From picker) — `handle_key` already routes input
    // to it first, so rendering it last keeps the visual and logical
    // layering in sync.
    if screen.confirm_close.is_some() {
        draw_confirm_close(f, area);
    }
}

/// Centered confirmation popup: "Save draft? [S]ave / [D]iscard /
/// [C]ancel". Visual grammar mirrors `draw_from_picker` (yellow
/// border, `Clear`'d background) so the user reads them as the same
/// class of modal.
fn draw_confirm_close(f: &mut Frame, bounds: Rect) {
    const WIDTH: u16 = 44;
    const HEIGHT: u16 = 5;
    let width = WIDTH.min(bounds.width);
    let height = HEIGHT.min(bounds.height);
    let x = bounds
        .x
        .saturating_add(bounds.width.saturating_sub(width) / 2);
    let y = bounds
        .y
        .saturating_add(bounds.height.saturating_sub(height) / 2);
    let area = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Save draft? ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let key_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let body = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("[S]", key_style),
            Span::raw("ave   "),
            Span::styled("[D]", key_style),
            Span::raw("iscard   "),
            Span::styled("[C]", key_style),
            Span::raw("ancel"),
        ]),
    ];
    f.render_widget(Paragraph::new(body), inner);
}

/// Draw the account-picker dropdown anchored below the From row. Width
/// is sized to the longest option (clamped to the available width), and
/// the popup is positioned to stay inside `bounds` even when the From
/// row is near the right edge.
fn draw_from_picker(f: &mut Frame, from_row: Rect, picker: &FromPicker, bounds: Rect) {
    // Indent the dropdown to align with the field contents, not the
    // label — visually it then hangs off the editable area.
    const LABEL_WIDTH: u16 = 9; // "From:    "

    let max_option_width = picker
        .options
        .iter()
        .map(|o| {
            (o.account.chars().count() + " — ".chars().count() + o.from.chars().count()) as u16
        })
        .max()
        .unwrap_or(20);
    // 2 cells for the L/R border, 2 cells of inner padding.
    let want_width = max_option_width.saturating_add(4).max(20);
    let height = (picker.options.len() as u16).saturating_add(2);

    let anchor_x = from_row.x.saturating_add(LABEL_WIDTH);
    let available_width = bounds.right().saturating_sub(anchor_x).min(bounds.width);
    let width = want_width.min(available_width).max(10);
    let x = anchor_x.min(bounds.right().saturating_sub(width));
    let y = from_row.y.saturating_add(1);
    let max_height = bounds.bottom().saturating_sub(y);
    let height = height.min(max_height).max(3);

    let area = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" From — pick account ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines: Vec<Line<'static>> = picker
        .options
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            let selected = i == picker.selected;
            let marker = if selected { "▶ " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(marker.to_string(), style),
                Span::styled(format!("{} — {}", opt.account, opt.from), style),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_field(f: &mut Frame, area: Rect, label: &str, input: &TextInput, focused: bool) {
    let label_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let buf = input.as_str();
    let cursor = input.cursor();
    let (before, after) = buf.split_at(cursor);
    let mut spans = vec![Span::styled(label.to_string(), label_style)];
    if focused {
        spans.push(Span::raw(before.to_string()));
        spans.push(Span::styled(
            "_",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::SLOW_BLINK),
        ));
        spans.push(Span::raw(after.to_string()));
    } else {
        spans.push(Span::raw(buf.to_string()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Read-only "Attachments:" row. Lists each queued file as
/// `<1-based-index>: <basename>` so the user can target `:detach <n>`.
/// Truncation is implicit — ratatui's `Paragraph` clips at the row
/// edge, which is fine here since the list is informational.
fn render_attachments_row(f: &mut Frame, area: Rect, attachments: &[PathBuf]) {
    let label_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let body: String = attachments
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string());
            format!("{}: {}", i + 1, name)
        })
        .collect::<Vec<_>>()
        .join(", ");
    let spans = vec![Span::styled("Attach:  ", label_style), Span::raw(body)];
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn split_addresses(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn title_for(draft: &Draft) -> String {
    let subj = draft.subject.trim();
    if subj.is_empty() {
        "compose: (new)".to_string()
    } else {
        format!("compose: {subj}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::compose::Draft;

    fn blank() -> ComposeScreen {
        ComposeScreen::from_draft(Draft::new_blank("acct", "me <me@example.com>"))
            .expect("compose screen")
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn fresh_compose_starts_on_to() {
        let s = blank();
        assert_eq!(s.focused, ComposeField::To);
        assert_eq!(s.last_header_focused, ComposeField::To);
    }

    #[test]
    fn ctrl_j_jumps_to_body_from_header() {
        let mut s = blank();
        s.set_focus(ComposeField::Cc);
        let cfg = Config::default();
        let out = handle_key(&mut s, key(KeyCode::Char('j'), KeyModifiers::CONTROL), &cfg);
        assert!(matches!(out, KeyOutcome::Consumed));
        assert_eq!(s.focused, ComposeField::Body);
        // Last header should be the one we came from, so Ctrl-K returns there.
        assert_eq!(s.last_header_focused, ComposeField::Cc);
    }

    #[test]
    fn ctrl_k_from_body_returns_to_last_header() {
        let mut s = blank();
        s.set_focus(ComposeField::Subject);
        let cfg = Config::default();
        // Jump to body, then jump back.
        handle_key(&mut s, key(KeyCode::Char('j'), KeyModifiers::CONTROL), &cfg);
        assert_eq!(s.focused, ComposeField::Body);
        handle_key(&mut s, key(KeyCode::Char('k'), KeyModifiers::CONTROL), &cfg);
        assert_eq!(s.focused, ComposeField::Subject);
    }

    #[test]
    fn tab_through_headers_updates_last_header_focused() {
        let mut s = blank();
        // To → Cc → Bcc via Tab.
        s.focus_next();
        s.focus_next();
        assert_eq!(s.focused, ComposeField::Bcc);
        assert_eq!(s.last_header_focused, ComposeField::Bcc);
        // Tab to Subject then Body — Body shouldn't overwrite the
        // last_header_focused marker.
        s.focus_next();
        s.focus_next();
        assert_eq!(s.focused, ComposeField::Body);
        assert_eq!(s.last_header_focused, ComposeField::Subject);
    }
}
