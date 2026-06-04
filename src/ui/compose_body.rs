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

use std::time::{Duration, Instant};

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;
use tui_textarea::{CursorMove, TextArea};

use crate::ui::motion::{self, MotionTarget};

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

/// Transient "highlight on yank" flash for the body editor. Mirrors the
/// reader's `YankHighlight`: `yy` / visual-mode `y` stash the yanked
/// region's cell ranges plus the arm time, the compose draw paints a
/// yellow-on-black flash over them, and the host loop clears it once the
/// configured `[reader].yank_highlight_ms` window elapses. Ranges are
/// `(row, col_start, col_end_excl)` in textarea char coords — the painter
/// maps them straight onto the body pane the same way the cursor does
/// (no scroll subtraction; bodies that fit the pane are the common case).
#[derive(Debug, Clone)]
pub struct BodyYankHighlight {
    pub ranges: Vec<(u16, u16, u16)>,
    pub armed_at: Instant,
}

pub enum KeyOutcome {
    Consumed,
    PassThrough,
    /// Compose-level signal: close the active tab without saving.
    /// Used by the "discard" arm of the close-confirm prompt.
    CloseTab,
    /// Compose-level signal: save the draft, then close the active tab.
    /// Used by the "save" arm of the close-confirm prompt and by the
    /// equivalent `:postpone` keystroke when one is added. The body
    /// editor never produces this — it bubbles up from the outer
    /// compose key handler.
    SaveAndClose,
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
    /// Active yank-highlight flash, if any. Painted by `compose::draw`
    /// and expired by the host loop via [`Self::expire_yank_highlight`].
    pub yank_highlight: Option<BodyYankHighlight>,
}

impl BodyEditor {
    pub fn new(initial: &str) -> Self {
        let lines = split_for_textarea(initial);
        let mut textarea = TextArea::new(lines);
        // We draw the cursor ourselves via `f.set_cursor_position()` +
        // DECSCUSR (block in Normal/Visual, bar in Insert), so kill
        // tui-textarea's own cell-painted cursor — otherwise it shows a
        // REVERSED block in Insert that looks wrong against the host
        // bar and lingers as a stray white cell at EOL.
        textarea.set_cursor_style(Style::default());
        // Same reason: the default UNDERLINED highlight on the cursor
        // line is jarring for prose composition.
        textarea.set_cursor_line_style(Style::default());
        Self {
            textarea,
            mode: BodyMode::Normal,
            visual_anchor: (0, 0),
            pending: Pending::None,
            yank: None,
            yank_highlight: None,
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
        self.yank_highlight = None;
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
            // Readline-flavoured editing chords that vim's insert mode
            // also accepts. Everything else under Ctrl is swallowed (no
            // accidental command-line / pane-cycling escape from insert).
            match k.code {
                KeyCode::Char('w') => {
                    self.textarea.delete_word();
                }
                KeyCode::Char('u') => {
                    self.textarea.delete_line_by_head();
                }
                KeyCode::Char('h') => {
                    // Some terminals send Ctrl-H for Ctrl-Backspace; treat
                    // as backspace so it isn't silently dropped.
                    self.textarea.delete_char();
                }
                _ => {}
            }
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

        // Ctrl-R → redo, Ctrl-C → passthrough. Ctrl-d / Ctrl-u are
        // motions and route through `motion::key_to_motion` below.
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(m) = motion::key_to_motion(k) {
                motion::apply(self, m);
                return KeyOutcome::Consumed;
            }
            return self.handle_normal_ctrl(k);
        }

        // Passthrough first so cmdline keys reach the app.
        match k.code {
            KeyCode::Char(':')
            | KeyCode::Char('/')
            | KeyCode::Char('?')
            | KeyCode::Tab
            | KeyCode::BackTab => return KeyOutcome::PassThrough,
            _ => {}
        }

        // Motions: hjkl / w / b / e / 0 / $ / ^ / G — shared with the
        // reader via the MotionTarget impl above.
        if let Some(m) = motion::key_to_motion(k) {
            motion::apply(self, m);
            return KeyOutcome::Consumed;
        }

        match k.code {
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
        // Motion-flavoured Ctrl chords (Ctrl-d / Ctrl-u) are dispatched
        // by the caller before this lands; left here are the non-motion
        // chords: Ctrl-R redo and Ctrl-C app passthrough.
        match k.code {
            KeyCode::Char('r') => {
                self.textarea.redo();
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
        // `gg` chord resolution: a prior `g` armed `Pending::G`; this
        // call's `g` triggers FirstLine, anything else clears the latch
        // and re-dispatches as a fresh key.
        if matches!(self.pending, Pending::G) {
            self.pending = Pending::None;
            if matches!(k.code, KeyCode::Char('g')) {
                motion::apply(self, motion::Motion::FirstLine);
                self.refresh_visual_selection();
                return KeyOutcome::Consumed;
            }
            // fall through
        }

        // Passthrough: ex-cmdline / field-cycle. The user's selection
        // is preserved across cmdline ticks because we don't drop
        // mode here; the cmdline handler stays in app-level scope.
        match k.code {
            KeyCode::Char(':')
            | KeyCode::Char('/')
            | KeyCode::Char('?')
            | KeyCode::Tab
            | KeyCode::BackTab => return KeyOutcome::PassThrough,
            _ => {}
        }

        // Motions: shared keymap with Normal + the reader.
        if let Some(m) = motion::key_to_motion(k) {
            motion::apply(self, m);
            self.refresh_visual_selection();
            return KeyOutcome::Consumed;
        }

        match k.code {
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

            // Arm the `gg` chord; resolution handled at the top of the
            // next `handle_visual` call.
            KeyCode::Char('g') => {
                self.pending = Pending::G;
                return KeyOutcome::Consumed;
            }

            _ => {}
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
        let ranges = self.line_ranges(row, row);
        self.arm_yank_highlight(ranges);
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
        let ranges = match kind {
            VisualKind::Char => self.char_ranges(sr, sc, er, ec),
            VisualKind::Line => self.line_ranges(sr, er),
        };
        self.arm_yank_highlight(ranges);
    }

    /// Cell ranges (`(row, col_start, col_end_excl)`, char coords) for a
    /// char-wise selection from `(sr, sc)` to `(er, ec)` exclusive. First
    /// line runs from `sc`, last line to `ec`, interior lines span their
    /// full width.
    fn char_ranges(&self, sr: usize, sc: usize, er: usize, ec: usize) -> Vec<(u16, u16, u16)> {
        let lines = self.textarea.lines();
        let mut out = Vec::new();
        for row in sr..=er {
            let Some(line) = lines.get(row) else { continue };
            let w = line.chars().count() as u16;
            let start = if row == sr { sc as u16 } else { 0 };
            let end = if row == er { ec as u16 } else { w };
            let end = end.min(w);
            if end > start {
                out.push((row as u16, start, end));
            }
        }
        out
    }

    /// Cell ranges for a line-wise selection over rows `sr..=er`. Each row
    /// spans its full width, floored at one cell so an empty line still
    /// shows a flash.
    fn line_ranges(&self, sr: usize, er: usize) -> Vec<(u16, u16, u16)> {
        let lines = self.textarea.lines();
        (sr..=er)
            .filter_map(|row| {
                lines
                    .get(row)
                    .map(|l| (row as u16, 0u16, (l.chars().count() as u16).max(1)))
            })
            .collect()
    }

    /// Arm a transient yank-highlight flash over `ranges`. Empty ranges
    /// clear any existing flash instead of arming an invisible one.
    fn arm_yank_highlight(&mut self, ranges: Vec<(u16, u16, u16)>) {
        if ranges.is_empty() {
            self.yank_highlight = None;
            return;
        }
        self.yank_highlight = Some(BodyYankHighlight {
            ranges,
            armed_at: Instant::now(),
        });
    }

    /// Clear the yank highlight once `ms` has elapsed since it was armed
    /// (or immediately when `ms == 0`, the disable switch). Called from
    /// the host loop's tick.
    pub fn expire_yank_highlight(&mut self, ms: u16) {
        if let Some(hl) = self.yank_highlight.as_ref()
            && (ms == 0 || hl.armed_at.elapsed() >= Duration::from_millis(ms as u64))
        {
            self.yank_highlight = None;
        }
    }

    /// Time left before the yank-highlight flash should clear, or `None`
    /// when no flash is armed (or highlighting is disabled). Floored at
    /// 1 ms so a just-expired flash still wakes the loop to clear it.
    pub fn yank_highlight_deadline(&self, ms: u16) -> Option<Duration> {
        if ms == 0 {
            return None;
        }
        let hl = self.yank_highlight.as_ref()?;
        let remaining = Duration::from_millis(ms as u64).saturating_sub(hl.armed_at.elapsed());
        Some(remaining.max(Duration::from_millis(1)))
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

/// Composer motion impl. Delegates to `tui_textarea::CursorMove` so
/// word boundaries / line clamps match the engine the rest of the
/// editor already uses — no risk of drifting from textarea's own word
/// walker. `h`/`l` use the existing `move_h` / `move_l` so the
/// "Normal-mode cursor sits ON a char (max col = line_len - 1)" rule
/// stays in one place.
impl MotionTarget for BodyEditor {
    fn move_char_left(&mut self) {
        self.move_h();
    }
    fn move_char_right(&mut self) {
        self.move_l();
    }
    fn move_char_up(&mut self) {
        self.textarea.move_cursor(CursorMove::Up);
    }
    fn move_char_down(&mut self) {
        self.textarea.move_cursor(CursorMove::Down);
    }
    fn move_word_forward(&mut self) {
        self.textarea.move_cursor(CursorMove::WordForward);
    }
    fn move_word_back(&mut self) {
        self.textarea.move_cursor(CursorMove::WordBack);
    }
    fn move_word_end(&mut self) {
        self.textarea.move_cursor(CursorMove::WordEnd);
    }
    fn move_line_start(&mut self) {
        self.textarea.move_cursor(CursorMove::Head);
    }
    fn move_line_end(&mut self) {
        self.textarea.move_cursor(CursorMove::End);
    }
    fn move_first_line(&mut self) {
        self.textarea.move_cursor(CursorMove::Top);
        self.textarea.move_cursor(CursorMove::Head);
    }
    fn move_last_line(&mut self) {
        self.textarea.move_cursor(CursorMove::Bottom);
        self.textarea.move_cursor(CursorMove::Head);
    }
    fn move_half_page(&mut self, down: bool) {
        let m = if down {
            CursorMove::Down
        } else {
            CursorMove::Up
        };
        for _ in 0..half_page() {
            self.textarea.move_cursor(m);
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
    for line in lines.iter().take(end.0).skip(start.0 + 1) {
        out.push_str(line);
        out.push('\n');
    }
    let last: Vec<char> = lines[end.0].chars().collect();
    let hi = end.1.min(last.len());
    out.extend(last[..hi].iter());
    out
}

fn extract_lines(lines: &[String], start_row: usize, end_row: usize) -> String {
    let mut out = String::new();
    for line in lines.iter().take(end_row + 1).skip(start_row) {
        out.push_str(line);
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
    fn yy_arms_yank_highlight_over_the_line() {
        let mut ed = BodyEditor::new("alpha\nbeta");
        feed(&mut ed, &[k('y'), k('y')]);
        let hl = ed.yank_highlight.as_ref().expect("highlight armed");
        // Row 0, full width of "alpha" (5 cells), starting at col 0.
        assert_eq!(hl.ranges, vec![(0, 0, 5)]);
    }

    #[test]
    fn visual_char_yank_arms_highlight_matching_selection() {
        let mut ed = BodyEditor::new("hello world");
        feed(&mut ed, &[k('v'), k('e'), k('y')]);
        let hl = ed.yank_highlight.as_ref().expect("highlight armed");
        // "hello" is cols 0..5 on row 0 (cursor cell included via the
        // Forward step in yank_selection).
        assert_eq!(hl.ranges, vec![(0, 0, 5)]);
    }

    #[test]
    fn expire_yank_highlight_clears_immediately_when_disabled() {
        let mut ed = BodyEditor::new("alpha");
        feed(&mut ed, &[k('y'), k('y')]);
        assert!(ed.yank_highlight.is_some());
        ed.expire_yank_highlight(0);
        assert!(ed.yank_highlight.is_none());
    }

    #[test]
    fn yank_highlight_deadline_none_when_disabled_or_idle() {
        let mut ed = BodyEditor::new("alpha");
        assert!(ed.yank_highlight_deadline(150).is_none());
        feed(&mut ed, &[k('y'), k('y')]);
        assert!(ed.yank_highlight_deadline(150).is_some());
        assert!(ed.yank_highlight_deadline(0).is_none());
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

    #[test]
    fn ctrl_w_in_insert_deletes_previous_word() {
        let mut ed = BodyEditor::new("");
        feed(&mut ed, &[k('i')]);
        for c in "hello world".chars() {
            ed.handle_key(k(c));
        }
        ed.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        // The exact split tui-textarea picks is its own concern; what we
        // care about is that some leading prefix survives and "world"
        // doesn't.
        assert!(!ed.text().contains("world"), "got: {:?}", ed.text());
        assert!(ed.text().starts_with("hello"), "got: {:?}", ed.text());
    }

    #[test]
    fn ctrl_u_in_insert_deletes_to_line_start() {
        let mut ed = BodyEditor::new("");
        feed(&mut ed, &[k('i')]);
        for c in "hello world".chars() {
            ed.handle_key(k(c));
        }
        ed.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(ed.text(), "");
    }
}
