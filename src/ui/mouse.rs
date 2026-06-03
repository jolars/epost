//! Reader-pane mouse handling: press anchors, drag promotes the click
//! to a visual-char selection and extends, release yanks and exits.
//! Scroll-wheel events over the reader pane adjust `reader_scroll`.
//!
//! Hit-testing keys off `InboxScreen.last_reader_inner` (set by
//! `reader::draw`) so the same body-relative coordinate space used by
//! visual-mode keyboard movement (`LaidOutBody.lines`) drives mouse
//! movement too. That keeps the yanked text consistent between
//! keyboard and mouse-driven selections — see Design Choice 0 in
//! `TODO.md`: yank content always comes from the IR.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::config::Config;
use crate::ui::app::{App, InboxScreen, Mode, Pane, Screen, VisualKind};
use crate::ui::keys;
use crate::ui::reader;

/// Per-tick wheel scroll amount, matched to terminal-style "three lines
/// per notch" feel. Tweaking this is purely cosmetic.
const WHEEL_LINES: u16 = 3;

pub fn handle(app: &mut App, cfg: &Config, ev: MouseEvent) {
    // Only the inbox screen has a reader pane. Compose tabs are pure
    // keyboard for now — letting mouse events trickle through would risk
    // moving cursor focus while the user is typing into the editor pty.
    if !matches!(app.screens.get(app.active), Some(Screen::Inbox(_))) {
        return;
    }
    // Suppress mouse during text-capturing modes so a stray click
    // doesn't yank focus mid-search / mid-command.
    if matches!(
        app.mode,
        Mode::Command | Mode::LinkPick | Mode::AttachmentPick | Mode::Search
    ) {
        return;
    }
    match ev.kind {
        MouseEventKind::Down(MouseButton::Left) => press(app, ev.column, ev.row),
        MouseEventKind::Drag(MouseButton::Left) => drag(app, ev.column, ev.row),
        MouseEventKind::Up(MouseButton::Left) => release(app, cfg),
        MouseEventKind::ScrollDown => wheel(app, ev.column, ev.row, 1),
        MouseEventKind::ScrollUp => wheel(app, ev.column, ev.row, -1),
        _ => {}
    }
}

/// Map a hit-tested `(body_line, cell_col)` onto the char index the rest
/// of the reader uses for `reader_cursor_col`. The hit-test yields a
/// display cell; on a line carrying a zero-width or wide character that's
/// not the same as the char index, so translate through the stashed body
/// text. Falls back to the cell value (correct for ASCII) when the line
/// isn't available.
fn cell_to_char(inbox: &InboxScreen, line: u16, cell: u16) -> u16 {
    inbox
        .last_reader_body_line_text
        .get(line as usize)
        .map(|s| reader::char_at_cell(s, cell))
        .unwrap_or(cell)
}

fn press(app: &mut App, col: u16, row: u16) {
    let inbox = app.inbox();
    let Some(inner) = inbox.last_reader_inner else {
        return;
    };
    let scroll = inbox.reader_scroll;
    let header = inbox.last_reader_header_offset;
    let lines = inbox.last_reader_body_only_lines;
    let Some((line, cell)) = hit_test_body(col, row, inner, scroll, header, lines) else {
        return;
    };
    let col = cell_to_char(inbox, line, cell);
    // A press while still inside a previous visual selection is a
    // fresh gesture — drop the old selection and re-anchor here.
    if app.mode == Mode::Visual {
        app.exit_visual();
    }
    let inbox = app.inbox_mut();
    inbox.focus = Pane::Reader;
    inbox.reader_cursor_line = line;
    inbox.reader_cursor_col = col;
    inbox.mouse_drag_anchor = Some((line, col));
    // Mode stays Normal: a plain click should leave the user where
    // they pressed without flashing -- VISUAL --. The first Drag
    // promotes the gesture; if Up arrives first this was a click.
}

fn drag(app: &mut App, col: u16, row: u16) {
    let inbox = app.inbox();
    let Some(inner) = inbox.last_reader_inner else {
        return;
    };
    let Some((anchor_line, anchor_col)) = inbox.mouse_drag_anchor else {
        return;
    };
    let scroll = inbox.reader_scroll;
    let header = inbox.last_reader_header_offset;
    let lines = inbox.last_reader_body_only_lines;
    let (line, cell) = hit_test_clamp(col, row, inner, scroll, header, lines);
    let col = cell_to_char(inbox, line, cell);
    // First drag → promote to Visual::Char at the press cell. The
    // anchor is preserved on InboxScreen.visual.anchor_{line,col}.
    if app.mode != Mode::Visual {
        let ib = app.inbox_mut();
        ib.reader_cursor_line = anchor_line;
        ib.reader_cursor_col = anchor_col;
        app.enter_visual(VisualKind::Char);
    }
    let ib = app.inbox_mut();
    ib.reader_cursor_line = line;
    ib.reader_cursor_col = col;
    ib.follow_cursor();
}

fn release(app: &mut App, cfg: &Config) {
    if app.inbox().mouse_drag_anchor.is_none() {
        // Up without a matching Down in the reader (e.g. user pressed
        // outside) — ignore.
        return;
    }
    if app.mode == Mode::Visual {
        // The gesture promoted to a selection — deliver it.
        keys::yank_visual(app, cfg);
    }
    app.inbox_mut().mouse_drag_anchor = None;
}

fn wheel(app: &mut App, col: u16, row: u16, dir: i32) {
    let inbox = app.inbox();
    let Some(inner) = inbox.last_reader_inner else {
        return;
    };
    if !point_in(col, row, inner) {
        return;
    }
    let ib = app.inbox_mut();
    if dir > 0 {
        ib.reader_scroll = ib.reader_scroll.saturating_add(WHEEL_LINES);
    } else {
        ib.reader_scroll = ib.reader_scroll.saturating_sub(WHEEL_LINES);
    }
}

fn point_in(col: u16, row: u16, inner: Rect) -> bool {
    let right = inner.x.saturating_add(inner.width);
    let bottom = inner.y.saturating_add(inner.height);
    col >= inner.x && col < right && row >= inner.y && row < bottom
}

/// Strict hit-test: returns `Some((body_line, viewport_col))` only when
/// `(col, row)` falls on a body line of the reader. Clicks in the header
/// rows, below the last body line, or outside the inner area entirely
/// return `None` — that's the press path, where a click outside body
/// content shouldn't anchor a drag.
fn hit_test_body(
    col: u16,
    row: u16,
    inner: Rect,
    scroll: u16,
    header_offset: u16,
    body_lines: u16,
) -> Option<(u16, u16)> {
    if !point_in(col, row, inner) {
        return None;
    }
    if body_lines == 0 {
        return None;
    }
    let viewport_row = row - inner.y;
    let viewport_col = col - inner.x;
    let abs_line = scroll as i32 + viewport_row as i32;
    let body_line = abs_line - header_offset as i32;
    if body_line < 0 {
        return None;
    }
    let body_line = body_line as u16;
    if body_line >= body_lines {
        return None;
    }
    Some((body_line, viewport_col))
}

/// Forgiving hit-test for the drag path: clamps coordinates to the
/// nearest valid body cell instead of returning `None`. Drag-above-the-
/// body extends to `(0, 0)`; drag-below-body extends to `(last,
/// u16::MAX)` (column overshoot is fixed up at draw time against
/// `LaidOutBody.line_text`, same as the `$` keybinding).
fn hit_test_clamp(
    col: u16,
    row: u16,
    inner: Rect,
    scroll: u16,
    header_offset: u16,
    body_lines: u16,
) -> (u16, u16) {
    let bodies = body_lines.max(1);
    let last = bodies - 1;
    // Clamp the cursor's y first.
    let row = row
        .max(inner.y)
        .min(inner.y.saturating_add(inner.height).saturating_sub(1));
    let col = col
        .max(inner.x)
        .min(inner.x.saturating_add(inner.width).saturating_sub(1));
    let viewport_row = row - inner.y;
    let viewport_col = col - inner.x;
    let abs_line = scroll as i32 + viewport_row as i32;
    let body_line = abs_line - header_offset as i32;
    if body_line < 0 {
        return (0, 0);
    }
    let body_line = body_line as u16;
    if body_line > last {
        return (last, u16::MAX);
    }
    (body_line, viewport_col)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn press_inside_body_returns_body_coords() {
        // Reader inner pane at (10, 5), 40×20. Scroll = 0, header = 3.
        // Click at terminal (15, 8) → inner (5, 3) → body line 0, col 5.
        let inner = rect(10, 5, 40, 20);
        let hit = hit_test_body(15, 8, inner, 0, 3, 100);
        assert_eq!(hit, Some((0, 5)));
    }

    #[test]
    fn press_with_scroll_offsets_line() {
        // Scrolled past the header — viewport row 0 is body line `scroll - header`.
        let inner = rect(10, 5, 40, 20);
        // scroll = 5, header = 3. Click at row 5 (viewport row 0) →
        // abs_line = 5, body_line = 2.
        let hit = hit_test_body(10, 5, inner, 5, 3, 100);
        assert_eq!(hit, Some((2, 0)));
    }

    #[test]
    fn press_in_header_returns_none() {
        let inner = rect(10, 5, 40, 20);
        // scroll = 0, header = 3 → first 3 viewport rows are header.
        // Click at row 5 (viewport row 0): abs_line=0, body_line=-3 → header.
        let hit = hit_test_body(10, 5, inner, 0, 3, 100);
        assert_eq!(hit, None);
    }

    #[test]
    fn press_below_body_returns_none() {
        let inner = rect(10, 5, 40, 20);
        // body_lines = 5, header = 0, scroll = 0. Click on viewport row 10 →
        // body_line = 10, past the end.
        let hit = hit_test_body(10, 15, inner, 0, 0, 5);
        assert_eq!(hit, None);
    }

    #[test]
    fn press_outside_inner_returns_none() {
        let inner = rect(10, 5, 40, 20);
        // Left of inner.x.
        assert_eq!(hit_test_body(9, 10, inner, 0, 0, 100), None);
        // Above inner.y.
        assert_eq!(hit_test_body(15, 4, inner, 0, 0, 100), None);
        // Right of inner.x + width.
        assert_eq!(hit_test_body(50, 10, inner, 0, 0, 100), None);
        // Below inner.y + height.
        assert_eq!(hit_test_body(15, 25, inner, 0, 0, 100), None);
    }

    #[test]
    fn press_with_empty_body_returns_none() {
        let inner = rect(10, 5, 40, 20);
        let hit = hit_test_body(15, 8, inner, 0, 0, 0);
        assert_eq!(hit, None);
    }

    #[test]
    fn drag_clamps_above_body_to_origin() {
        let inner = rect(10, 5, 40, 20);
        // header = 3 → viewport rows 0..3 are header.
        // Click at viewport row 1 (in header) clamps to (0, 0).
        let hit = hit_test_clamp(15, 6, inner, 0, 3, 100);
        assert_eq!(hit, (0, 0));
    }

    #[test]
    fn drag_clamps_below_body_to_last_line() {
        let inner = rect(10, 5, 40, 20);
        // body_lines = 5, header = 0. Drag at viewport row 18 →
        // body_line = 18, clamped to (4, u16::MAX).
        let hit = hit_test_clamp(20, 23, inner, 0, 0, 5);
        assert_eq!(hit, (4, u16::MAX));
    }

    #[test]
    fn drag_outside_inner_clamps_to_edge() {
        let inner = rect(10, 5, 40, 20);
        // Click far left of inner — should still produce a valid cell
        // (col clamps to inner.x → viewport col 0).
        let hit = hit_test_clamp(0, 10, inner, 0, 0, 100);
        assert_eq!(hit, (5, 0));
    }

    #[test]
    fn point_in_inclusive_left_and_top_exclusive_right_and_bottom() {
        let inner = rect(10, 5, 40, 20);
        assert!(point_in(10, 5, inner));
        assert!(point_in(49, 24, inner));
        assert!(!point_in(50, 24, inner));
        assert!(!point_in(49, 25, inner));
        assert!(!point_in(9, 5, inner));
        assert!(!point_in(10, 4, inner));
    }
}
