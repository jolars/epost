use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{List, ListItem};

use crate::ui::app::{App, Pane};
use crate::ui::style::pane_block;

const FOLDERS: &[&str] = &["INBOX", "Sent", "Archive"];

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = FOLDERS.iter().map(|s| ListItem::new(*s)).collect();
    let widget = List::new(items).block(pane_block("Folders", app.focus == Pane::Folders));
    f.render_widget(widget, area);
}
