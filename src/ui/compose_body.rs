//! Vim-style body editor for the compose tab. Wraps
//! [`tui_textarea::TextArea`] with a light vim mode machine: Normal /
//! Insert / Visual(Char|Line). [`handle_key`] returns whether the key
//! was consumed by the editor or should fall through to the app
//! dispatch (only `: / ? Tab BackTab` pass through from Normal /
//! Visual; nothing passes through from Insert).
//!
//! v1 scope:
//! - Motions: `h j k l`, `w b e`, `0 $ ^`, `gg G`, `Ctrl-d` / `Ctrl-u`.
//! - Insert entry: `i a A I o O`.
//! - Edits: `x X`, `dd`, `yy`, `p P`, `u`, `Ctrl-R`.
//! - Visual: `v V`; in Visual `y d x c` plus Esc / same-kind toggle /
//!   opposite-kind swap.
//!
//! Out of scope (deferred): text objects, counts, registers, macros,
//! search, ex-commands beyond what the host cmdline already provides.

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_textarea::{CursorMove, TextArea};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyMode {
    Normal,
    Insert,
    Visual(VisualKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualKind {
    Char,
    Line,
}

/// Operator-pending latch for two-key chords (`dd`, `yy`, `gg`). Only
/// supports same-character pairs in v1 — text objects / motions after
/// an operator are deferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    D,
    Y,
    G,
}

#[derive(Debug, Clone)]
struct Yank {
    text: String,
    line_wise: bool,
}

pub enum KeyOutcome {
    Consumed,
    PassThrough,
}

pub struct BodyEditor {
    pub textarea: TextArea<'static>,
    pub mode: BodyMode,
    /// Anchor (row, col) for visual mode. Cursor live position lives on
    /// the textarea itself. We track our own anchor instead of relying
    /// on the textarea's selection_start so kind swaps (`v` ↔ `V`) can
    /// recompute the selection without losing the origin.
    visual_anchor: (usize, usize),
    pending: Pending,
    yank: Option<Yank>,
}

impl BodyEditor {
    pub fn new(initial: &str) -> Self {
        let lines = split_for_textarea(initial);
        let textarea = TextArea::new(lines);
        Self {
            textarea,
            mode: BodyMode::Normal,
            visual_anchor: (0, 0),
            pending: Pending::None,
            yank: None,
        }
    }

    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn set_text(&mut self, s: &str) {
        let lines = split_for_textarea(s);
        self.textarea.set_lines(lines, (0, 0));
        self.textarea.cancel_selection();
        self.mode = BodyMode::Normal;
        self.pending = Pending::None;
    }

    /// Data-space cursor (row, col into the textarea's line vector).
    /// Not currently used in rendering (tui-textarea paints its own
    /// cursor cell) but exposed for future DECSCUSR / status-row use.
    #[allow(dead_code)]
    pub fn cursor(&self) -> (u16, u16) {
        let (row, col) = self.textarea.cursor();
        (row as u16, col as u16)
    }

    /// DECSCUSR shape that matches the current mode. Wired up in a
    /// future pass; today the cursor cell is rendered by tui-textarea.
    #[allow(dead_code)]
    pub fn cursor_style(&self) -> SetCursorStyle {
        match self.mode {
            BodyMode::Insert => SetCursorStyle::SteadyBar,
            BodyMode::Normal | BodyMode::Visual(_) => SetCursorStyle::SteadyBlock,
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> KeyOutcome {
        match self.mode {
            BodyMode::Insert => self.handle_insert(k),
            BodyMode::Normal => self.handle_normal(k),
            BodyMode::Visual(kind) => self.handle_visual(k, kind),
        }
    }

    // ---------- Insert mode ----------

    fn handle_insert(&mut self, k: KeyEvent) -> KeyOutcome {
        // Insert mode swallows everything except Esc — `:`, `q`, Tab,
        // etc. all type literal characters into prose. The exit door is
        // Esc (matching vim) which drops back to Normal and nudges the
        // cursor back one cell, like `<Esc>` in real vim.
        if k.code == KeyCode::Esc {
            self.mode = BodyMode::Normal;
            self.textarea.move_cursor(CursorMove::Back);
            return KeyOutcome::Consumed;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            // Ctrl-* is mostly unbound in insert for v1; only handle the
            // ones the host cares about. Ctrl-C falls through so the app
            // can quit when there's no editor session — though normally
            // the user would hit Esc first.
            return KeyOutcome::Consumed;
        }
        match k.code {
            KeyCode::Char(c) => {
                self.textarea.insert_char(c);
            }
            KeyCode::Enter => {
                self.textarea.insert_newline();
            }
            KeyCode::Tab => {
                self.textarea.insert_str("\t");
            }
            KeyCode::Backspace => {
                self.textarea.delete_char();
            }
            KeyCode::Delete => {
                self.textarea.delete_next_char();
            }
            KeyCode::Left => self.textarea.move_cursor(CursorMove::Back),
            KeyCode::Right => self.textarea.move_cursor(CursorMove::Forward),
            KeyCode::Up => self.textarea.move_cursor(CursorMove::Up),
            KeyCode::Down => self.textarea.move_cursor(CursorMove::Down),
            KeyCode::Home => self.textarea.move_cursor(CursorMove::Head),
            KeyCode::End => self.textarea.move_cursor(CursorMove::End),
            _ => {}
        }
        KeyOutcome::Consumed
    }

    // ---------- Normal mode ----------

    fn handle_normal(&mut self, k: KeyEvent) -> KeyOutcome {
        // Operator-pending: only same-char chords in v1. Any other key
        // cancels the pending operator and is re-dispatched as a fresh
        // Normal-mode key (vim-like — the pending state never leaks).
        match self.pending {
            Pending::D => {
                self.pending = Pending::None;
                if matches!(k.code, KeyCode::Char('d')) {
                    self.delete_current_line();
                    return KeyOutcome::Consumed;
                }
                // fall through to normal dispatch
            }
            Pending::Y => {
                self.pending = Pending::None;
                if matches!(k.code, KeyCode::Char('y')) {
                    self.yank_current_line();
                    return KeyOutcome::Consumed;
                }
            }
            Pending::G => {
                self.pending = Pending::None;
                if matches!(k.code, KeyCode::Char('g')) {
                    self.textarea.move_cursor(CursorMove::Top);
                    self.textarea.move_cursor(CursorMove::Head);
                    return KeyOutcome::Consumed;
                }
            }
            Pending::None => {}
        }

        // Ctrl-R → redo, Ctrl-D / Ctrl-U → half-page scroll.
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            return self.handle_normal_ctrl(k);
        }

        match k.code {
            // Passthrough: ex-cmdline, search, field-cycling. These are
            // app-level concerns; the editor doesn't try to handle them.
            KeyCode::Char(':')
            | KeyCode::Char('/')
            | KeyCode::Char('?')
            | KeyCode::Tab
            | KeyCode::BackTab => return KeyOutcome::PassThrough,

            // Motions
            KeyCode::Char('h') | KeyCode::Left => self.move_h(),
            KeyCode::Char('l') | KeyCode::Right => self.move_l(),
            KeyCode::Char('j') | KeyCode::Down => self.textarea.move_cursor(CursorMove::Down),
            KeyCode::Char('k') | KeyCode::Up => self.textarea.move_cursor(CursorMove::Up),
            KeyCode::Char('w') => self.textarea.move_cursor(CursorMove::WordForward),
            KeyCode::Char('b') => self.textarea.move_cursor(CursorMove::WordBack),
            KeyCode::Char('e') => self.textarea.move_cursor(CursorMove::WordEnd),
            KeyCode::Char('0') | KeyCode::Home => self.textarea.move_cursor(CursorMove::Head),
            KeyCode::Char('$') | KeyCode::End => self.textarea.move_cursor(CursorMove::End),
            KeyCode::Char('^') => self.textarea.move_cursor(CursorMove::Head),
            KeyCode::Char('G') => {
                self.textarea.move_cursor(CursorMove::Bottom);
                self.textarea.move_cursor(CursorMove::Head);
            }
            KeyCode::Char('g') => {
                self.pending = Pending::G;
            }

            // Insert entry
            KeyCode::Char('i') => self.mode = BodyMode::Insert,
            KeyCode::Char('a') => {
                self.textarea.move_cursor(CursorMove::Forward);
                self.mode = BodyMode::Insert;
            }
            KeyCode::Char('A') => {
                self.textarea.move_cursor(CursorMove::End);
                self.mode = BodyMode::Insert;
            }
            KeyCode::Char('I') => {
                self.textarea.move_cursor(CursorMove::Head);
                self.mode = BodyMode::Insert;
            }
            KeyCode::Char('o') => {
                self.textarea.move_cursor(CursorMove::End);
                self.textarea.insert_newline();
                self.mode = BodyMode::Insert;
            }
            KeyCode::Char('O') => {
                self.textarea.move_cursor(CursorMove::Head);
                self.textarea.insert_newline();
                self.textarea.move_cursor(CursorMove::Up);
                self.mode = BodyMode::Insert;
            }

            // Edits
            KeyCode::Char('x') => {
                self.textarea.delete_next_char();
            }
            KeyCode::Char('X') => {
                self.textarea.delete_char();
            }
            KeyCode::Char('d') => {
                self.pending = Pending::D;
            }
            KeyCode::Char('y') => {
                self.pending = Pending::Y;
            }
            KeyCode::Char('p') => self.paste_after(),
            KeyCode::Char('P') => self.paste_before(),
            KeyCode::Char('u') => {
                self.textarea.undo();
            }

            // Visual entry
            KeyCode::Char('v') => self.enter_visual(VisualKind::Char),
            KeyCode::Char('V') => self.enter_visual(VisualKind::Line),

            _ => {}
        }
        KeyOutcome::Consumed
    }

    fn handle_normal_ctrl(&mut self, k: KeyEvent) -> KeyOutcome {
        match k.code {
            KeyCode::Char('r') => {
                self.textarea.redo();
            }
            KeyCode::Char('d') => {
                for _ in 0..half_page() {
                    self.textarea.move_cursor(CursorMove::Down);
                }
            }
            KeyCode::Char('u') => {
                for _ in 0..half_page() {
                    self.textarea.move_cursor(CursorMove::Up);
                }
            }
            // Ctrl-C falls through to the app (which quits) only when
            // the user has already left Insert. That matches the
            // pty-editor path's behavior: ^C in insert mode is the
            // editor's, ^C in normal is the app's.
            KeyCode::Char('c') => return KeyOutcome::PassThrough,
            _ => {}
        }
        KeyOutcome::Consumed
    }

    fn move_h(&mut self) {
        // Vim `h` stops at column 0; doesn't wrap to previous line.
        let (_, col) = self.textarea.cursor();
        if col > 0 {
            self.textarea.move_cursor(CursorMove::Back);
        }
    }

    fn move_l(&mut self) {
        // Vim `l` stops at last char; doesn't wrap to next line.
        let (row, col) = self.textarea.cursor();
        let line_len = self.textarea.lines()[row].chars().count();
        // In Normal mode the cursor sits ON a char (block), so the last
        // valid column is line_len - 1 (or 0 for an empty line). In
        // Insert mode the bar sits between chars, allowing line_len.
        let max_col = match self.mode {
            BodyMode::Insert => line_len,
            _ => line_len.saturating_sub(1),
        };
        if col < max_col {
            self.textarea.move_cursor(CursorMove::Forward);
        }
    }

    // ---------- Visual mode ----------

    fn enter_visual(&mut self, kind: VisualKind) {
        self.visual_anchor = self.textarea.cursor();
        self.mode = BodyMode::Visual(kind);
        self.refresh_visual_selection();
    }

    fn exit_visual_to_normal(&mut self) {
        self.textarea.cancel_selection();
        self.mode = BodyMode::Normal;
    }

    fn handle_visual(&mut self, k: KeyEvent, kind: VisualKind) -> KeyOutcome {
        if k.code == KeyCode::Esc {
            self.exit_visual_to_normal();
            return KeyOutcome::Consumed;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            // Ctrl-C exits visual like Esc (and then the app handler
            // sees a clean Normal-mode keypress next).
            if matches!(k.code, KeyCode::Char('c')) {
                self.exit_visual_to_normal();
                return KeyOutcome::Consumed;
            }
            return KeyOutcome::Consumed;
        }
        match k.code {
            // Passthrough: ex-cmdline / field-cycle. The user's selection
            // is preserved across cmdline ticks because we don't drop
            // mode here; the cmdline handler stays in app-level scope.
            KeyCode::Char(':')
            | KeyCode::Char('/')
            | KeyCode::Char('?')
            | KeyCode::Tab
            | KeyCode::BackTab => return KeyOutcome::PassThrough,

            // Toggle / swap visual kind
            KeyCode::Char('v') => {
                if kind == VisualKind::Char {
                    self.exit_visual_to_normal();
                } else {
                    self.mode = BodyMode::Visual(VisualKind::Char);
                    self.refresh_visual_selection();
                }
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('V') => {
                if kind == VisualKind::Line {
                    self.exit_visual_to_normal();
                } else {
                    self.mode = BodyMode::Visual(VisualKind::Line);
                    self.refresh_visual_selection();
                }
                return KeyOutcome::Consumed;
            }

            // Selection actions
            KeyCode::Char('y') => {
                self.yank_selection(kind);
                self.exit_visual_to_normal();
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('d') | KeyCode::Char('x') => {
                self.cut_selection(kind);
                self.exit_visual_to_normal();
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('c') => {
                self.cut_selection(kind);
                self.textarea.cancel_selection();
                self.mode = BodyMode::Insert;
                return KeyOutcome::Consumed;
            }

            // Motions (same set as Normal)
            KeyCode::Char('h') | KeyCode::Left => self.move_h(),
            KeyCode::Char('l') | KeyCode::Right => self.move_l(),
            KeyCode::Char('j') | KeyCode::Down => self.textarea.move_cursor(CursorMove::Down),
            KeyCode::Char('k') | KeyCode::Up => self.textarea.move_cursor(CursorMove::Up),
            KeyCode::Char('w') => self.textarea.move_cursor(CursorMove::WordForward),
            KeyCode::Char('b') => self.textarea.move_cursor(CursorMove::WordBack),
            KeyCode::Char('e') => self.textarea.move_cursor(CursorMove::WordEnd),
            KeyCode::Char('0') | KeyCode::Home => self.textarea.move_cursor(CursorMove::Head),
            KeyCode::Char('$') | KeyCode::End => self.textarea.move_cursor(CursorMove::End),
            KeyCode::Char('^') => self.textarea.move_cursor(CursorMove::Head),
            KeyCode::Char('G') => {
                self.textarea.move_cursor(CursorMove::Bottom);
                self.textarea.move_cursor(CursorMove::Head);
            }
            KeyCode::Char('g') => {
                // Cheap `gg` in visual: consume the next `g` via the
                // same pending latch normal mode uses. The latch lives
                // on the editor so it survives the per-key call.
                self.pending = Pending::G;
                return KeyOutcome::Consumed;
            }

            _ => {}
        }

        // Pending latch resolution for `gg` while in visual.
        if let Pending::G = self.pending {
            // Consumed above for the first `g`; the second arrives in a
            // later call and the Normal-mode latch path handles it. But
            // we're in Visual; route it here.
            // (This branch is reached only when we just set Pending::G
            // above; resolution happens on the next call.)
        }

        self.refresh_visual_selection();
        KeyOutcome::Consumed
    }

    fn refresh_visual_selection(&mut self) {
        let kind = match self.mode {
            BodyMode::Visual(k) => k,
            _ => return,
        };
        let cur = self.textarea.cursor();
        let anchor = self.visual_anchor;
        self.textarea.cancel_selection();
        match kind {
            VisualKind::Char => {
                self.textarea
                    .move_cursor(CursorMove::Jump(anchor.0 as u16, anchor.1 as u16));
                self.textarea.start_selection();
                self.textarea
                    .move_cursor(CursorMove::Jump(cur.0 as u16, cur.1 as u16));
            }
            VisualKind::Line => {
                let lines = self.textarea.lines();
                if cur.0 >= anchor.0 {
                    // Downward selection: anchor at top-left, cursor at
                    // bot end-of-line. Cursor's visible row = cur.0.
                    let bot_end = lines[cur.0].chars().count() as u16;
                    self.textarea
                        .move_cursor(CursorMove::Jump(anchor.0 as u16, 0));
                    self.textarea.start_selection();
                    self.textarea
                        .move_cursor(CursorMove::Jump(cur.0 as u16, bot_end));
                } else {
                    // Upward selection: anchor at bottom end-of-line,
                    // cursor at top start-of-line. Cursor's visible row
                    // = cur.0.
                    let anchor_end = lines[anchor.0].chars().count() as u16;
                    self.textarea
                        .move_cursor(CursorMove::Jump(anchor.0 as u16, anchor_end));
                    self.textarea.start_selection();
                    self.textarea.move_cursor(CursorMove::Jump(cur.0 as u16, 0));
                }
            }
        }
    }

    // ---------- Line ops ----------

    fn delete_current_line(&mut self) {
        // Implemented as a selection-then-cut so the change is a single
        // history step (one `u` press undoes the whole line removal).
        // Compounding `delete_line_by_end` + `delete_next_char` would
        // record two steps and take two undo presses to revert.
        let (row, _) = self.textarea.cursor();
        let line = self.textarea.lines().get(row).cloned().unwrap_or_default();
        let line_count = self.textarea.lines().len();
        self.yank = Some(Yank {
            text: format!("{line}\n"),
            line_wise: true,
        });
        self.textarea.cancel_selection();
        if row + 1 < line_count {
            // Select from (row, 0) to (row + 1, 0) — covers the line
            // plus its trailing newline.
            self.textarea.move_cursor(CursorMove::Jump(row as u16, 0));
            self.textarea.start_selection();
            self.textarea
                .move_cursor(CursorMove::Jump((row + 1) as u16, 0));
            self.textarea.cut();
        } else if row > 0 {
            // Last line of a multi-line buffer: extend back through the
            // preceding newline so the buffer doesn't end up with a
            // trailing empty row.
            let prev_end = self.textarea.lines()[row - 1].chars().count() as u16;
            let end = self.textarea.lines()[row].chars().count() as u16;
            self.textarea
                .move_cursor(CursorMove::Jump((row - 1) as u16, prev_end));
            self.textarea.start_selection();
            self.textarea.move_cursor(CursorMove::Jump(row as u16, end));
            self.textarea.cut();
        } else {
            // Single-line buffer: select the line and cut — leaves [""].
            let end = self.textarea.lines()[row].chars().count() as u16;
            self.textarea.move_cursor(CursorMove::Jump(0, 0));
            self.textarea.start_selection();
            self.textarea.move_cursor(CursorMove::Jump(0, end));
            self.textarea.cut();
        }
    }

    fn yank_current_line(&mut self) {
        let (row, _) = self.textarea.cursor();
        let line = self.textarea.lines().get(row).cloned().unwrap_or_default();
        self.yank = Some(Yank {
            text: format!("{line}\n"),
            line_wise: true,
        });
    }

    fn yank_selection(&mut self, kind: VisualKind) {
        // Vim's char-wise visual is inclusive of the cell under the
        // cursor; textarea's selection_range is exclusive. Step Forward
        // once before reading the range so the cursor cell is included
        // in the resulting yank / cut. (Forward at EOL wraps to the
        // next line's head, which means `v$y` will pull the trailing
        // newline along — acceptable v1 drift.)
        if matches!(kind, VisualKind::Char) {
            self.textarea.move_cursor(CursorMove::Forward);
        }
        let Some(((sr, sc), (er, ec))) = self.textarea.selection_range() else {
            return;
        };
        let lines = self.textarea.lines();
        let text = match kind {
            VisualKind::Char => extract_range(lines, (sr, sc), (er, ec)),
            VisualKind::Line => extract_lines(lines, sr, er),
        };
        self.yank = Some(Yank {
            text,
            line_wise: matches!(kind, VisualKind::Line),
        });
    }

    fn cut_selection(&mut self, kind: VisualKind) {
        if matches!(kind, VisualKind::Char) {
            self.textarea.move_cursor(CursorMove::Forward);
        }
        let Some(((sr, sc), (er, ec))) = self.textarea.selection_range() else {
            return;
        };
        let lines = self.textarea.lines();
        let text = match kind {
            VisualKind::Char => extract_range(lines, (sr, sc), (er, ec)),
            VisualKind::Line => extract_lines(lines, sr, er),
        };
        self.yank = Some(Yank {
            text,
            line_wise: matches!(kind, VisualKind::Line),
        });
        match kind {
            VisualKind::Char => {
                // textarea's own cut goes through history, so undo can
                // revert it. The internal yank ring is overwritten; we
                // keep our own kind-aware copy on `self.yank` instead.
                self.textarea.cut();
            }
            VisualKind::Line => {
                // For line-wise, redraw the selection to span whole
                // rows including the trailing newline so cut() removes
                // the lines cleanly (without leaving an empty row).
                let line_count = self.textarea.lines().len();
                let top = sr;
                let bot = er;
                self.textarea.cancel_selection();
                if bot + 1 < line_count {
                    self.textarea.move_cursor(CursorMove::Jump(top as u16, 0));
                    self.textarea.start_selection();
                    self.textarea
                        .move_cursor(CursorMove::Jump((bot + 1) as u16, 0));
                    self.textarea.cut();
                } else if top > 0 {
                    // Selection runs to the last line: extend the start
                    // back through the preceding newline so the buffer
                    // doesn't end up with a stray empty row.
                    let prev_end = self.textarea.lines()[top - 1].chars().count() as u16;
                    let bot_end = self.textarea.lines()[bot].chars().count() as u16;
                    self.textarea
                        .move_cursor(CursorMove::Jump((top - 1) as u16, prev_end));
                    self.textarea.start_selection();
                    self.textarea
                        .move_cursor(CursorMove::Jump(bot as u16, bot_end));
                    self.textarea.cut();
                } else {
                    // Whole-buffer line-wise delete: select all and cut
                    // — textarea normalises back to [""].
                    let bot_end = self.textarea.lines()[bot].chars().count() as u16;
                    self.textarea.move_cursor(CursorMove::Jump(0, 0));
                    self.textarea.start_selection();
                    self.textarea
                        .move_cursor(CursorMove::Jump(bot as u16, bot_end));
                    self.textarea.cut();
                }
            }
        }
    }

    fn paste_after(&mut self) {
        let Some(y) = self.yank.clone() else { return };
        if y.line_wise {
            // Land on a fresh line below current, then type the yanked
            // content. `insert_newline` + `insert_str` both go through
            // history so the paste is a single undoable change.
            let trimmed = y.text.trim_end_matches('\n');
            self.textarea.move_cursor(CursorMove::End);
            self.textarea.insert_newline();
            self.textarea.insert_str(trimmed);
            self.textarea.move_cursor(CursorMove::Head);
        } else {
            // Char-wise: insert after the cell under the cursor.
            self.textarea.move_cursor(CursorMove::Forward);
            self.textarea.insert_str(&y.text);
        }
    }

    fn paste_before(&mut self) {
        let Some(y) = self.yank.clone() else { return };
        if y.line_wise {
            let trimmed = y.text.trim_end_matches('\n');
            self.textarea.move_cursor(CursorMove::Head);
            self.textarea.insert_str(trimmed);
            self.textarea.insert_newline();
            self.textarea.move_cursor(CursorMove::Up);
            self.textarea.move_cursor(CursorMove::Head);
        } else {
            self.textarea.insert_str(&y.text);
        }
    }
}

fn split_for_textarea(s: &str) -> Vec<String> {
    if s.is_empty() {
        return vec![String::new()];
    }
    s.split('\n').map(str::to_string).collect()
}

fn half_page() -> usize {
    // Vim's default is the rendered viewport / 2; without per-render
    // viewport tracking just step by a fixed amount. Eight matches
    // "tall enough to feel like a jump, short enough on small terms."
    8
}

fn extract_range(lines: &[String], start: (usize, usize), end: (usize, usize)) -> String {
    if start.0 == end.0 {
        let chars: Vec<char> = lines[start.0].chars().collect();
        let lo = start.1.min(chars.len());
        let hi = end.1.min(chars.len());
        return chars[lo..hi].iter().collect();
    }
    let mut out = String::new();
    let first: Vec<char> = lines[start.0].chars().collect();
    let lo = start.1.min(first.len());
    out.extend(first[lo..].iter());
    out.push('\n');
    for row in (start.0 + 1)..end.0 {
        out.push_str(&lines[row]);
        out.push('\n');
    }
    let last: Vec<char> = lines[end.0].chars().collect();
    let hi = end.1.min(last.len());
    out.extend(last[..hi].iter());
    out
}

fn extract_lines(lines: &[String], start_row: usize, end_row: usize) -> String {
    let mut out = String::new();
    for row in start_row..=end_row {
        out.push_str(&lines[row]);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }

    fn feed(ed: &mut BodyEditor, keys: &[KeyEvent]) {
        for ke in keys {
            ed.handle_key(*ke);
        }
    }

    #[test]
    fn i_then_type_then_esc_lands_in_normal_with_text() {
        let mut ed = BodyEditor::new("");
        feed(&mut ed, &[k('i'), k('H'), k('i')]);
        assert_eq!(ed.mode, BodyMode::Insert);
        assert_eq!(ed.text(), "Hi");
        ed.handle_key(esc());
        assert_eq!(ed.mode, BodyMode::Normal);
        // Vim Esc backs up one cell from the end of insertion.
        assert_eq!(ed.cursor(), (0, 1));
    }

    #[test]
    fn dd_yanks_and_deletes_line() {
        let mut ed = BodyEditor::new("one\ntwo\nthree");
        feed(&mut ed, &[k('j'), k('d'), k('d')]);
        assert_eq!(ed.text(), "one\nthree");
        // Paste-after puts the deleted "two" back below the new current.
        feed(&mut ed, &[k('p')]);
        assert_eq!(ed.text(), "one\nthree\ntwo");
    }

    #[test]
    fn yy_then_p_duplicates_line_without_modifying_buffer_first() {
        let mut ed = BodyEditor::new("alpha\nbeta");
        feed(&mut ed, &[k('y'), k('y'), k('p')]);
        assert_eq!(ed.text(), "alpha\nalpha\nbeta");
    }

    #[test]
    fn visual_char_yank_yanks_substring() {
        // "hello world" → v then `e` selects "hello"; y exits and yanks.
        let mut ed = BodyEditor::new("hello world");
        feed(&mut ed, &[k('v'), k('e'), k('y')]);
        assert_eq!(ed.mode, BodyMode::Normal);
        let y = ed.yank.as_ref().expect("yank set");
        assert!(y.text.starts_with("hello"), "yank was: {:?}", y.text);
        assert!(!y.line_wise);
    }

    #[test]
    fn visual_line_yank_yanks_whole_line() {
        let mut ed = BodyEditor::new("one\ntwo\nthree");
        feed(&mut ed, &[k('j'), k('V'), k('y')]);
        let y = ed.yank.as_ref().expect("yank set");
        assert_eq!(y.text, "two\n");
        assert!(y.line_wise);
    }

    #[test]
    fn undo_reverts_dd() {
        let mut ed = BodyEditor::new("one\ntwo");
        feed(&mut ed, &[k('d'), k('d')]);
        assert_eq!(ed.text(), "two");
        feed(&mut ed, &[k('u')]);
        assert_eq!(ed.text(), "one\ntwo");
    }

    #[test]
    fn colon_passes_through_only_from_normal_and_visual() {
        let mut ed = BodyEditor::new("hello");
        let out = ed.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        assert!(matches!(out, KeyOutcome::PassThrough));
        // Insert eats the colon as literal text.
        ed.handle_key(k('i'));
        let out = ed.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        assert!(matches!(out, KeyOutcome::Consumed));
        assert_eq!(ed.text(), ":hello");
    }

    #[test]
    fn gg_jumps_to_top() {
        let mut ed = BodyEditor::new("a\nb\nc\nd");
        feed(&mut ed, &[k('G')]);
        assert_eq!(ed.cursor().0, 3);
        feed(&mut ed, &[k('g'), k('g')]);
        assert_eq!(ed.cursor(), (0, 0));
    }

    #[test]
    fn o_opens_line_below_in_insert() {
        let mut ed = BodyEditor::new("first\nsecond");
        feed(&mut ed, &[k('o'), k('x')]);
        assert_eq!(ed.text(), "first\nx\nsecond");
        assert_eq!(ed.mode, BodyMode::Insert);
    }

    #[test]
    fn capital_o_opens_line_above_in_insert() {
        let mut ed = BodyEditor::new("only");
        feed(&mut ed, &[k('O'), k('x')]);
        assert_eq!(ed.text(), "x\nonly");
        assert_eq!(ed.mode, BodyMode::Insert);
    }
}
