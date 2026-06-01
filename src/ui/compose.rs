//! Compose screen: aerc-style form for one in-flight draft. Lives in
//! its own tab so the user can `Ctrl-PgDn` away to other work and come
//! back. Header fields (From / To / Cc / Bcc / Subject) edit in-place
//! via `TextInput`; the body is delegated to `$EDITOR` against a
//! tempfile that persists for the tab's lifetime.

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
    /// Tempfile holding the body text. Kept alive for the tab's
    /// lifetime so reopening $EDITOR shows the prior draft.
    body_file: NamedTempFile,
    pub body_preview: String,
    pub body_dirty: bool,
    /// Set by the keymap when the user requests `$EDITOR`; the main loop
    /// turns this into a live `EditorSession` once the next draw has
    /// measured the body area.
    pub editor_pending: bool,
    /// Embedded `$EDITOR` pty + parsed screen. While `Some`, all
    /// keys forward to the pty and the body region renders the
    /// editor inline (aerc-style).
    pub editor: Option<EditorSession>,
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
}

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
    pub fn from_draft(draft: Draft) -> std::io::Result<Self> {
        let body_file = NamedTempFile::with_prefix("epost-body-")?;
        if !draft.body.is_empty() {
            std::fs::write(body_file.path(), &draft.body)?;
        }
        let body_preview = preview_of(&draft.body);
        let body_dirty = !draft.body.is_empty();
        let title = title_for(&draft);
        Ok(Self {
            title,
            account: draft.account,
            from: TextInput::from_string(draft.from),
            to: TextInput::from_string(draft.to.join(", ")),
            cc: TextInput::from_string(draft.cc.join(", ")),
            bcc: TextInput::from_string(draft.bcc.join(", ")),
            subject: TextInput::from_string(draft.subject),
            // Aerc-style: focus starts on the body and the editor
            // spawns immediately, so switching to this tab lands the
            // user inside `$EDITOR` with no transition.
            focused: ComposeField::Body,
            body_file,
            body_preview,
            body_dirty,
            editor_pending: true,
            editor: None,
            last_body_inner: None,
            in_reply_to: draft.in_reply_to,
            references: draft.references,
            attachments: draft.attachments,
            from_picker: None,
        })
    }

    pub fn body_path(&self) -> PathBuf {
        self.body_file.path().to_path_buf()
    }

    /// Build a Draft from the current field contents + body tempfile.
    pub fn collect_draft(&self) -> std::io::Result<Draft> {
        let body = std::fs::read_to_string(self.body_file.path()).unwrap_or_default();
        Ok(Draft {
            account: self.account.clone(),
            from: self.from.as_str().to_string(),
            to: split_addresses(self.to.as_str()),
            cc: split_addresses(self.cc.as_str()),
            bcc: split_addresses(self.bcc.as_str()),
            subject: self.subject.as_str().to_string(),
            body,
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            attachments: self.attachments.clone(),
        })
    }

    /// Refresh `body_preview` from the tempfile contents (called by
    /// the main loop after $EDITOR returns).
    pub fn reload_body_preview(&mut self) {
        let text = std::fs::read_to_string(self.body_file.path()).unwrap_or_default();
        self.body_preview = preview_of(&text);
        self.body_dirty = !text.is_empty();
    }

    pub fn focus_next(&mut self) {
        self.focused = match self.focused {
            ComposeField::From => ComposeField::To,
            ComposeField::To => ComposeField::Cc,
            ComposeField::Cc => ComposeField::Bcc,
            ComposeField::Bcc => ComposeField::Subject,
            ComposeField::Subject => ComposeField::Body,
            ComposeField::Body => ComposeField::From,
        };
    }

    pub fn focus_prev(&mut self) {
        self.focused = match self.focused {
            ComposeField::From => ComposeField::Body,
            ComposeField::To => ComposeField::From,
            ComposeField::Cc => ComposeField::To,
            ComposeField::Bcc => ComposeField::Cc,
            ComposeField::Subject => ComposeField::Bcc,
            ComposeField::Body => ComposeField::Subject,
        };
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

/// Compose-mode key dispatch. Only called when no editor session is
/// active — `ui::keys::handle` forwards everything to the pty when
/// one is live, so this path only handles form editing (Tab cycles
/// fields, `Alt-e` / `Enter` on Body requests a re-open of `$EDITOR`).
/// Takes `cfg` so the From-field Enter handler can build the account
/// picker from `[accounts.*]` without reaching back to App.
pub fn handle_key(screen: &mut ComposeScreen, k: KeyEvent, cfg: &Config) {
    // When the From picker is open, capture navigation/commit keys here
    // and short-circuit the rest of the form dispatch. Anything we don't
    // recognise is swallowed so a stray keystroke can't both edit a
    // header field and dismiss the popup in one event.
    if screen.from_picker.is_some() {
        handle_from_picker_key(screen, k);
        return;
    }

    // Alt-e always requests editor; convenient from any header field.
    if k.modifiers.contains(KeyModifiers::ALT) && k.code == KeyCode::Char('e') {
        screen.editor_pending = true;
        return;
    }
    match k.code {
        KeyCode::Tab => screen.focus_next(),
        KeyCode::BackTab => screen.focus_prev(),
        _ => {}
    }
    if screen.focused == ComposeField::Body {
        if matches!(k.code, KeyCode::Enter | KeyCode::Char('e')) {
            screen.editor_pending = true;
        }
        return;
    }
    // Enter on the From field opens the account picker. The TextInput
    // doesn't bind Enter, so claiming it here is non-breaking — manual
    // editing of From still works for one-off display-name tweaks; the
    // picker is for switching the *sending identity* (account, which
    // drives SMTP routing and the Sent-folder destination).
    if screen.focused == ComposeField::From && k.code == KeyCode::Enter {
        open_from_picker(screen, cfg);
        return;
    }
    if let Some(input) = screen.focused_input_mut() {
        input.handle(k);
    }
}

fn open_from_picker(screen: &mut ComposeScreen, cfg: &Config) {
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
    let body_block = pane_block(
        if editing {
            "Body — $EDITOR (exit the editor to return to the form)"
        } else if screen.focused == ComposeField::Body {
            "Body (Enter/e to edit)"
        } else {
            "Body"
        },
        screen.focused == ComposeField::Body || editing,
    );
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
        let body_lines: Vec<Line<'static>> = if screen.body_preview.is_empty() {
            vec![Line::from(Span::styled(
                "(editor closed — press e to re-open, :send to send)",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            screen
                .body_preview
                .lines()
                .map(|l| Line::raw(l.to_string()))
                .collect()
        };
        f.render_widget(Paragraph::new(body_lines), body_inner);
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
    } else if screen.focused == ComposeField::From {
        Line::from(Span::styled(
            " Enter pick account  Tab/Shift-Tab fields  Alt-e edit body  :send  :close ",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(Span::styled(
            " Tab/Shift-Tab fields  Alt-e edit body  :send  :close ",
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

fn preview_of(body: &str) -> String {
    body.lines().take(8).collect::<Vec<_>>().join("\n")
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
