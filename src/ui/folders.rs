use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{List, ListItem};

use crate::ui::app::{InboxScreen, Pane};
use crate::ui::style::pane_block;

const FOLDERS: &[&str] = &["INBOX", "Sent", "Archive"];

pub fn draw(f: &mut Frame, area: Rect, inbox: &InboxScreen) {
    let items: Vec<ListItem> = FOLDERS.iter().map(|s| ListItem::new(*s)).collect();
    let widget = List::new(items).block(pane_block("Folders", inbox.focus == Pane::Folders));
    f.render_widget(widget, area);
}
