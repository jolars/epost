//! Compose screen: aerc-style form for one in-flight draft. Lives in
//! its own tab so the user can `Ctrl-PgDn` away to other work and come
//! back. Header fields (From / To / Cc / Bcc / Subject) edit in-place
//! via `TextInput`; the body is delegated to `$EDITOR` against a
//! tempfile that persists for the tab's lifetime.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tempfile::NamedTempFile;
use tui_term::widget::PseudoTerminal;

use crate::mail::compose::{Draft, SendResult};
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

#[derive(Debug)]
pub enum ComposeStatus {
    Editing,
    Sending,
    Sent,
    Failed(String),
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
    pub status: ComposeStatus,
    pub send_rx: Option<Receiver<SendResult>>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
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
            status: ComposeStatus::Editing,
            send_rx: None,
            in_reply_to: draft.in_reply_to,
            references: draft.references,
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
pub fn handle_key(screen: &mut ComposeScreen, k: KeyEvent) {
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
    if let Some(input) = screen.focused_input_mut() {
        input.handle(k);
    }
}

/// Render the compose screen into `area`. Layout: a 6-line header
/// block (From / To / Cc / Bcc / Subject), a body-preview pane below,
/// hint line at the bottom.
pub fn draw(f: &mut Frame, area: Rect, screen: &mut ComposeScreen) {
    let outer = Layout::vertical([
        Constraint::Length(7),
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

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(header_inner);
    render_field(
        f,
        rows[0],
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
            ed.with_screen(|s| {
                let widget = PseudoTerminal::new(s);
                f.render_widget(widget, body_inner);
            });
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

    let hint = match &screen.status {
        ComposeStatus::Editing if editing => Line::from(Span::styled(
            " editing in $EDITOR — exit the editor (e.g. :wq) to return to the form ",
            Style::default().fg(Color::DarkGray),
        )),
        ComposeStatus::Editing => Line::from(Span::styled(
            " Tab/Shift-Tab fields  Alt-e edit body  :send  :close ",
            Style::default().fg(Color::DarkGray),
        )),
        ComposeStatus::Sending => Line::from(Span::styled(
            " sending… ",
            Style::default().fg(Color::Yellow),
        )),
        ComposeStatus::Sent => Line::from(Span::styled(
            " sent ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )),
        ComposeStatus::Failed(e) => Line::from(Span::styled(
            format!(" send failed: {e} "),
            Style::default().fg(Color::Red),
        )),
    };
    f.render_widget(Paragraph::new(hint), hint_area);
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
