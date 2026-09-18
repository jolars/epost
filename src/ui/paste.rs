//! Paste routing shared by terminal input and explicit clipboard reads.

use std::sync::mpsc::{Receiver, TryRecvError};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::Config;
use crate::ui::app::{App, Mode, Screen};
use crate::ui::compose::ComposeField;
use crate::ui::compose_body::{BodyMode, VisualKind};
use crate::ui::compose_header::HeaderMode;
use crate::ui::text_input::TextInput;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Cursor,
    Before,
    After,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Prefix {
    #[default]
    None,
    Register,
    Put,
    Insert,
}

pub enum Shortcut {
    Consumed,
    Read(Placement),
}

impl Prefix {
    /// Call only after any pending find, replace, or text-object capture.
    pub fn handle(&mut self, k: KeyEvent, normal: bool) -> Option<Shortcut> {
        let plain = !k
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let old = std::mem::take(self);
        let action = match (old, k.code) {
            (Self::Register, KeyCode::Char('+')) if plain => {
                *self = Self::Put;
                Shortcut::Consumed
            }
            (Self::Put, KeyCode::Char('p')) if plain => Shortcut::Read(Placement::After),
            (Self::Put, KeyCode::Char('P')) if plain => Shortcut::Read(Placement::Before),
            (Self::Insert, KeyCode::Char('+')) if plain => Shortcut::Read(Placement::Cursor),
            (Self::None, KeyCode::Char('"')) if plain && normal => {
                *self = Self::Register;
                Shortcut::Consumed
            }
            (Self::None, KeyCode::Char('r')) if !normal && k.modifiers == KeyModifiers::CONTROL => {
                *self = Self::Insert;
                Shortcut::Consumed
            }
            (Self::None, _) => return None,
            _ => Shortcut::Consumed,
        };
        Some(action)
    }
}

pub fn normalize(text: &str, single_line: bool) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter_map(|c| match c {
            '\n' | '\t' => Some(if single_line { ' ' } else { c }),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

fn insert_single(input: &mut TextInput, text: &str, placement: Placement, normal: bool) {
    let text = normalize(text, true);
    input.paste_prefix = Prefix::None;
    if text.is_empty() {
        return;
    }
    if placement == Placement::After {
        input.move_right();
    }
    input.insert_str(&text);
    if normal {
        input.move_left();
        input.clamp_normal();
    }
}

/// Modal dialogs capture input before the form, just as in key dispatch.
fn target(app: &App) -> Option<Target> {
    if app
        .active_compose()
        .is_some_and(|c| c.editor.is_some() || c.editor_pending)
    {
        return None;
    }
    let kind = match app.mode {
        Mode::Command => TargetKind::Command,
        Mode::Search if app.inbox().search.is_some() => TargetKind::Search,
        Mode::Normal => {
            let c = app.active_compose()?;
            if c.confirm_close.is_some() || c.from_picker.is_some() {
                return None;
            }
            match c.focused {
                ComposeField::Body => TargetKind::Body(c.body.mode),
                ComposeField::Attach if c.attach.adding.is_some() => TargetKind::Attachment,
                ComposeField::Attach => return None,
                field => TargetKind::Header(field, c.header_mode),
            }
        }
        _ => return None,
    };
    Some(Target {
        tab: app.active,
        kind,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Target {
    tab: usize,
    kind: TargetKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Command,
    Search,
    Body(BodyMode),
    Header(ComposeField, HeaderMode),
    Attachment,
}

pub fn insert(app: &mut App, cfg: &Config, text: &str, placement: Placement) {
    if text.is_empty() {
        return;
    }
    if let Some(c) = app.active_compose_mut()
        && let Some(editor) = c.editor.as_mut()
    {
        editor.forward_paste(text);
        return;
    }
    let Some(target) = target(app) else { return };
    match target.kind {
        TargetKind::Command => insert_single(&mut app.cmdline, text, placement, false),
        TargetKind::Search => {
            let inbox = app.inbox_mut();
            let s = inbox.search.as_mut().expect("checked target");
            insert_single(&mut s.query, text, placement, false);
            s.refresh();
            inbox.selected = 0;
        }
        TargetKind::Body(_) => {
            let c = app.active_compose_mut().expect("checked target");
            if let Err(e) = c.body.paste(text, placement) {
                app.status_error = Some(e.into());
            }
        }
        TargetKind::Header(_, mode) => {
            let c = app.active_compose_mut().expect("checked target");
            c.header_pending = None;
            insert_single(
                c.focused_input_mut().expect("header"),
                text,
                placement,
                mode == HeaderMode::Normal,
            );
            app.refresh_address_complete(cfg);
        }
        TargetKind::Attachment => {
            let c = app.active_compose_mut().expect("checked target");
            insert_single(
                c.attach.adding.as_mut().expect("checked target"),
                text,
                placement,
                false,
            );
        }
    }
}

pub struct PendingRead {
    pub(crate) rx: Receiver<Result<String, String>>,
    target: Target,
    placement: Placement,
    canceled: bool,
}

pub fn request(app: &mut App, cfg: &Config, placement: Placement) {
    let Some(target) = target(app) else { return };
    if matches!(
        target.kind,
        TargetKind::Body(BodyMode::Visual(VisualKind::Block))
    ) {
        app.status_error = Some("paste: visual-block paste is not supported".into());
        return;
    }
    if app.clipboard_read.is_some() {
        app.status_error = Some("paste: clipboard read already in progress".into());
        return;
    }
    let Some(cmd) = cfg
        .clipboard
        .paste_command
        .as_ref()
        .filter(|cmd| !cmd.is_empty())
    else {
        app.status_error = Some(
            "paste: set [clipboard].paste_command or use your terminal's paste shortcut".into(),
        );
        return;
    };
    let rx = crate::ui::clipboard::read(cmd.clone(), app.event_tx.clone());
    app.clipboard_read = Some(PendingRead {
        rx,
        target,
        placement,
        canceled: false,
    });
}

pub fn cancel_read(app: &mut App) {
    if let Some(pending) = app.clipboard_read.as_mut() {
        pending.canceled = true;
    }
}

pub fn poll(app: &mut App, cfg: &Config) {
    let Some(pending) = app.clipboard_read.as_ref() else {
        return;
    };
    let result = match pending.rx.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => return,
        Err(TryRecvError::Disconnected) => Err("clipboard worker disconnected".into()),
    };
    let pending = app.clipboard_read.take().expect("checked above");
    if pending.canceled || target(app) != Some(pending.target) {
        return;
    }
    match result {
        Ok(text) => insert(app, cfg, &text, pending.placement),
        Err(error) => app.status_error = Some(format!("paste: {error}")),
    }
}

pub fn clear_prefixes(app: &mut App) {
    app.cmdline.paste_prefix = Prefix::None;
    if let Some(s) = app.inbox_mut().search.as_mut() {
        s.query.paste_prefix = Prefix::None;
    }
    for screen in &mut app.screens {
        if let Screen::Compose(c) = screen {
            c.clear_paste_prefixes();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::compose::Draft;
    use crate::ui::compose::ComposeScreen;
    use crate::ui::search::{SearchKind, SearchState};
    use std::sync::mpsc;

    fn app() -> (App, Config, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::default();
        let mut app = App::new(&cfg, dir.path().join("index.sqlite"), None, None);
        app.open_compose(
            ComposeScreen::from_draft(Draft::new_blank("test", "me@example.com"), cfg.compose.wrap)
                .unwrap(),
        );
        (app, cfg, dir)
    }

    fn pending(app: &mut App) -> mpsc::Sender<Result<String, String>> {
        let (tx, rx) = mpsc::channel();
        app.clipboard_read = Some(PendingRead {
            rx,
            target: target(app).unwrap(),
            placement: Placement::Cursor,
            canceled: false,
        });
        tx
    }

    #[test]
    fn clipboard_result_inserts_once() {
        let (mut app, cfg, _dir) = app();
        let tx = pending(&mut app);
        tx.send(Ok("alice@example.com".into())).unwrap();
        poll(&mut app, &cfg);
        poll(&mut app, &cfg);
        assert_eq!(
            app.active_compose().unwrap().to.as_str(),
            "alice@example.com"
        );
        assert!(app.clipboard_read.is_none());
    }

    #[test]
    fn canceled_clipboard_read_is_drained_without_insertion() {
        let (mut app, cfg, _dir) = app();
        let tx = pending(&mut app);
        cancel_read(&mut app);
        tx.send(Ok("stale".into())).unwrap();
        poll(&mut app, &cfg);
        assert!(app.active_compose().unwrap().to.is_empty());
        assert!(app.clipboard_read.is_none());
    }

    #[test]
    fn later_input_cancels_clipboard_read_without_blocking_input() {
        use crate::ui::events::AppEvent;
        use crossterm::event::{Event, MouseButton, MouseEvent, MouseEventKind};
        let events = [
            Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::CONTROL)),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
            Event::Paste("new".into()),
        ];
        for event in events {
            let (mut app, cfg, _dir) = app();
            let tx = pending(&mut app);
            crate::process_event(&mut app, &cfg, AppEvent::Input(event.clone()));
            tx.send(Ok("stale".into())).unwrap();
            poll(&mut app, &cfg);
            let Screen::Compose(c) = &app.screens[1] else {
                panic!("draft missing")
            };
            assert_eq!(
                c.to.as_str(),
                if matches!(event, Event::Paste(_)) {
                    "new"
                } else {
                    ""
                }
            );
            assert!(app.clipboard_read.is_none());
        }
    }

    #[test]
    fn clipboard_result_does_not_follow_focus_or_reused_tab_index() {
        let (mut app, cfg, _dir) = app();
        let tx = pending(&mut app);
        app.active_compose_mut()
            .unwrap()
            .set_focus(ComposeField::Subject);
        tx.send(Ok("wrong field".into())).unwrap();
        poll(&mut app, &cfg);
        assert!(app.active_compose().unwrap().subject.is_empty());
        let tx = pending(&mut app);
        app.close_active_tab().unwrap();
        app.open_compose(
            ComposeScreen::from_draft(Draft::new_blank("test", "me@example.com"), cfg.compose.wrap)
                .unwrap(),
        );
        tx.send(Ok("wrong draft".into())).unwrap();
        poll(&mut app, &cfg);
        assert!(app.active_compose().unwrap().to.is_empty());
    }

    #[test]
    fn clipboard_failures_and_missing_configuration_surface_in_status() {
        let (mut app, cfg, _dir) = app();
        request(&mut app, &cfg, Placement::Cursor);
        assert!(
            app.status_error
                .as_deref()
                .unwrap()
                .contains("[clipboard].paste_command")
        );
        let tx = pending(&mut app);
        tx.send(Err("test failure".into())).unwrap();
        poll(&mut app, &cfg);
        assert_eq!(app.status_error.as_deref(), Some("paste: test failure"));
        let tx = pending(&mut app);
        request(&mut app, &cfg, Placement::Cursor);
        assert!(
            app.status_error
                .as_deref()
                .unwrap()
                .contains("already in progress")
        );
        drop(tx);
        poll(&mut app, &cfg);
        assert!(
            app.status_error
                .as_deref()
                .unwrap()
                .contains("disconnected")
        );
    }

    #[test]
    fn paste_respects_dialogs_and_all_header_fields() {
        let (mut app, cfg, _dir) = app();
        for field in [
            ComposeField::From,
            ComposeField::To,
            ComposeField::Cc,
            ComposeField::Bcc,
            ComposeField::Subject,
        ] {
            let c = app.active_compose_mut().unwrap();
            c.set_focus(field);
            c.focused_input_mut().unwrap().clear();
            insert(&mut app, &cfg, "å\nb", Placement::Cursor);
            assert_eq!(
                app.active_compose_mut()
                    .unwrap()
                    .focused_input_mut()
                    .unwrap()
                    .as_str(),
                "å b"
            );
        }
        app.active_compose_mut().unwrap().confirm_close = Some(crate::ui::compose::CloseConfirm);
        insert(&mut app, &cfg, "ignored", Placement::Cursor);
        assert_eq!(app.active_compose().unwrap().subject.as_str(), "å b");
        app.active_compose_mut().unwrap().confirm_close = None;
        app.active_compose_mut()
            .unwrap()
            .set_focus(ComposeField::Attach);
        insert(&mut app, &cfg, "ignored", Placement::Cursor);
        assert!(app.active_compose().unwrap().attachments.is_empty());
    }

    #[test]
    fn paste_refreshes_search_without_committing_and_empty_paste_is_noop() {
        let (mut app, cfg, _dir) = app();
        app.active = 0;
        app.mode = Mode::Search;
        app.inbox_mut().search = Some(Box::new(SearchState::new(
            SearchKind::Local {
                account: None,
                folder: "INBOX".into(),
            },
            vec![],
            None,
        )));
        app.inbox_mut().selected = 5;
        insert(&mut app, &cfg, "", Placement::Cursor);
        assert_eq!(app.inbox().selected, 5);
        insert(&mut app, &cfg, "hello\r\nworld", Placement::Cursor);
        assert_eq!(
            app.inbox().search.as_ref().unwrap().query.as_str(),
            "hello world"
        );
        assert_eq!(app.mode, Mode::Search);
        assert_eq!(app.inbox().selected, 0);
    }
}
