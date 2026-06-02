use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Scrollbar, ScrollbarOrientation, ScrollbarState};

pub fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let border_style = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title)
}

/// Draw a vertical progress scrollbar over the right border of a bordered
/// pane. `area` is the block's outer rect; the scrollbar paints the right
/// border column between the top and bottom corners (begin/end arrows
/// suppressed so the block's corner glyphs survive). No-op when the
/// content fits the viewport — keeps short messages / empty lists clean.
pub fn pane_scrollbar(
    f: &mut Frame,
    area: Rect,
    position: usize,
    content_length: usize,
    focused: bool,
) {
    let viewport = area.height.saturating_sub(2) as usize;
    if viewport == 0 || content_length <= viewport {
        return;
    }
    let track = area.inner(Margin {
        vertical: 1,
        horizontal: 0,
    });
    let thumb_color = if focused { Color::Yellow } else { Color::Gray };
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_style(Style::default().fg(Color::DarkGray))
        .thumb_style(Style::default().fg(thumb_color));
    let mut state = ScrollbarState::new(content_length)
        .position(position)
        .viewport_content_length(viewport);
    f.render_stateful_widget(scrollbar, track, &mut state);
}
