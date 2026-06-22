//! Vim-style body editor for the compose tab. Wraps
//! [`tui_textarea::TextArea`] with a light vim mode machine: Normal /
//! Insert / Visual(Char|Line|Block). [`handle_key`] returns whether the
//! key was consumed by the editor or should fall through to the app
//! dispatch (only `: / ? Tab BackTab` pass through from Normal /
//! Visual; nothing passes through from Insert).
//!
//! v1 scope:
//! - Motions: `h j k l`, `w b e` + `W B E` (WORD, via the shared
//!   [`words`](crate::ui::words) scanner), `0 $ ^`, `gg G`, `ge gE`,
//!   `f F t T`, `Ctrl-d` / `Ctrl-u`. All accept a count.
//! - Operators composing with any motion / text object / count:
//!   `d c y`, `> <` (indent), `gu gU g~` (case), `gw gq` (reflow). The
//!   doubled form (`dd cc yy >> guu gqq` …) is linewise over `count`
//!   lines. Text objects: `iw aw i" a" i' i` i( a( ib ab i{ aB i[ i<
//!   ip ap`.
//! - Insert entry: `i a A I o O`. Replace: `r{char}`, `R` (overtype mode).
//! - Single-key edits: `x X ~ J gJ s S C D`, `yy`/`Y`, `p P`, `u`, `Ctrl-R`.
//! - Visual: `v V`, block-wise `Ctrl-V`; in Visual `y d x c` plus the
//!   operators above, Esc / same-kind toggle / opposite-kind swap.
//!   Block-visual also takes `I` / `A` / `c` block-insert (type on the
//!   top row, replayed across the rest on Esc).
//!
//! Out of scope (deferred): block-paste (`p` of a rectangle), dot-repeat
//! (`.`), named registers, macros, `;`/`,` find-repeat, ex-commands
//! beyond the host cmdline. Case/reflow/indent operators are 2 undo
//! steps (delete + insert); deletes and yanks are 1.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use tui_textarea::{CursorMove, TextArea, WrapMode};
use unicode_width::UnicodeWidthStr;

use crate::config::ComposeWrap;
use crate::ui::motion::{self, Motion, MotionKind, MotionSpan, MotionTarget, Region};
use crate::ui::textobj::{self, TextObjKind};
use crate::ui::words::{self, WordMotion};

/// Reflow width fallback when the caller passes 0. Also the indent step
/// for `>` / `<`.
const SHIFTWIDTH: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyMode {
    Normal,
    Insert,
    /// `R` — overtype mode: typed chars replace the char under the cursor
    /// instead of inserting before it.
    Replace,
    Visual(VisualKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualKind {
    Char,
    Line,
    Block,
}

/// Which block-insert flavour is in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockInsertKind {
    /// `I` — insert at the block's left edge on every row.
    Insert,
    /// `A` — append after the block's right edge on every row.
    Append,
    /// `c` — delete the rectangle then insert at the left edge.
    Change,
}

/// Per-row insert column for a block insert.
#[derive(Debug, Clone, Copy)]
enum BlockColSpec {
    /// Insert at this absolute column (`I` / `c`).
    Left(usize),
    /// Append after this (inclusive) right edge column (`A`).
    Append(usize),
}

/// In-flight block-insert (`Ctrl-V` then `I` / `A` / `c`). The user types
/// on the top row in Insert mode; on `Esc` the inserted text is replayed
/// on every other selected row. See [`BodyEditor::finish_block_insert`].
#[derive(Debug, Clone)]
struct BlockInsert {
    /// Rows to replay onto (excludes the top row, where the live typing
    /// already landed).
    rows: Vec<usize>,
    /// How to resolve each row's insert column.
    spec: BlockColSpec,
    /// The top (anchor) row, where the live insert happens.
    top_row: usize,
    /// Column on the top row where the live insert began.
    top_col: usize,
    /// `chars(lines[top_row])` captured at entry, so the Esc handler can
    /// recover the typed span by diffing against the post-insert length.
    pre_len: usize,
    /// `lines.len()` captured at entry — if it changed (Enter pressed),
    /// replay is aborted.
    pre_rows: usize,
}

/// A vim operator awaiting a motion / text object / doubled form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Delete,
    Change,
    Yank,
    Indent,
    Dedent,
    Lower,
    Upper,
    ToggleCase,
    /// `gw` — reflow, keep the cursor.
    ReflowKeep,
    /// `gq` — reflow, move the cursor to the end of the formatted text.
    ReflowMove,
}

/// The key that, when typed immediately after the operator, runs its
/// linewise doubled form (`dd`, `cc`, `>>`, `guu`, `gqq`, …).
fn op_double_key(op: Operator) -> char {
    match op {
        Operator::Delete => 'd',
        Operator::Change => 'c',
        Operator::Yank => 'y',
        Operator::Indent => '>',
        Operator::Dedent => '<',
        Operator::Lower => 'u',
        Operator::Upper => 'U',
        Operator::ToggleCase => '~',
        Operator::ReflowKeep => 'w',
        Operator::ReflowMove => 'q',
    }
}

/// `f F t T` — find-char on the current line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FindKind {
    Forward,
    Back,
    Till,
    TillBack,
}

/// Operator-pending / multi-key parse state. Counts multiply: `2d3w`
/// deletes 6 words. A leading `g` and the various "awaiting the next
/// key" latches are mutually consumed one keystroke later.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct OpState {
    /// Count typed before the operator (`2` in `2dw`).
    count1: Option<usize>,
    op: Option<Operator>,
    /// Count typed after the operator (`3` in `d3w`).
    count2: Option<usize>,
    /// Saw a `g`; the next key completes a g-chord (`gg`, `gu`, `ge`, …).
    await_g: bool,
    /// Awaiting the text-object char after `i` / `a`; bool = "around".
    await_obj: Option<bool>,
    /// Awaiting the target char of an `f`/`F`/`t`/`T` find.
    await_find: Option<FindKind>,
    /// Awaiting the replacement char of `r`; the count is the run length.
    await_replace: Option<usize>,
}

impl OpState {
    fn eff_count(&self) -> usize {
        self.count1.unwrap_or(1) * self.count2.unwrap_or(1)
    }

    /// True when a multi-key chord (operator, count, `g`-prefix, text
    /// object, find, or `r` replace) is mid-parse and awaiting its next
    /// key. The compose-level `q` quick-close consults this so a pending
    /// `gq` reflow (or `rq`, `fq`, …) completes instead of closing the tab.
    fn pending(&self) -> bool {
        self.op.is_some()
            || self.await_g
            || self.await_obj.is_some()
            || self.await_find.is_some()
            || self.await_replace.is_some()
            || self.count1.is_some()
            || self.count2.is_some()
    }
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
    /// Anchor `(row, col)` for block-wise visual (`Ctrl-V`). Separate from
    /// `visual_anchor` because tui-textarea's single contiguous selection
    /// can't represent a rectangle — block selection is painted and
    /// extracted by us, not the textarea.
    block_anchor: Option<(usize, usize)>,
    /// In-flight block insert, set by `I`/`A`/`c` in block-visual and
    /// consumed by the `Esc` that ends the insert.
    block_insert: Option<BlockInsert>,
    /// Operator-pending / count / chord parse state.
    ops: OpState,
    /// Reflow width for `gw` / `gq`, refreshed from `[compose].text_width`
    /// on every keystroke (the editor doesn't store the whole config).
    text_width: u16,
    yank: Option<Yank>,
    /// Active yank-highlight flash, if any. Painted by `compose::draw`
    /// and expired by the host loop via [`Self::expire_yank_highlight`].
    pub yank_highlight: Option<BodyYankHighlight>,
    /// Vim "curswant": the column vertical motion (`j`/`k`, page moves)
    /// tries to keep across lines of differing length. tui-textarea's
    /// own `Up`/`Down` clamps the column to each line and forgets it, so
    /// we track the goal ourselves and drive vertical moves via `Jump`.
    goal_col: GoalCol,
    /// The cursor column right after the last move we tracked. If the
    /// live column differs at the start of a vertical move, an untracked
    /// path (edit, paste, insert entry) moved the cursor, so we reseed
    /// `goal_col` from the live column before applying the move.
    goal_anchor: usize,
}

/// Target column for vim-style vertical motion. `Col(n)` rides char
/// column `n` (clamped per line); `Eol` rides the end of each line —
/// vim's `curswant = MAXCOL`, set by `$`.
#[derive(Clone, Copy)]
enum GoalCol {
    Col(usize),
    Eol,
}

impl BodyEditor {
    pub fn new(initial: &str) -> Self {
        let lines = split_for_textarea(initial);
        let mut textarea = TextArea::new(lines);
        // Soft-wrap is what keeps long quoted-reply lines inside the pane.
        // Default matches `[compose].wrap` default; `set_wrap` overrides it
        // from config at the construction site.
        textarea.set_wrap_mode(WrapMode::WordOrGlyph);
        // The default UNDERLINED highlight on the whole cursor *line* is
        // jarring for prose composition, so kill it. The cursor *cell*
        // itself is painted by tui-textarea (see `sync_cursor_style`):
        // under soft-wrap the crate is the only thing that knows the
        // cursor's post-wrap/post-scroll screen position, so we can't
        // host-drive it the way we used to.
        textarea.set_cursor_line_style(Style::default());
        let mut ed = Self {
            textarea,
            mode: BodyMode::Normal,
            visual_anchor: (0, 0),
            block_anchor: None,
            block_insert: None,
            ops: OpState::default(),
            text_width: 72,
            yank: None,
            yank_highlight: None,
            goal_col: GoalCol::Col(0),
            goal_anchor: 0,
        };
        ed.sync_cursor_style();
        ed
    }

    /// Apply the configured soft-wrap mode. Visual-only — never inserts
    /// newlines, so the logical line model (and every vim motion built on
    /// it) is untouched. Called once from the construction site with
    /// `[compose].wrap`.
    pub fn set_wrap(&mut self, wrap: ComposeWrap) {
        let mode = match wrap {
            ComposeWrap::Off => WrapMode::None,
            ComposeWrap::Word => WrapMode::Word,
            ComposeWrap::Glyph => WrapMode::Glyph,
            ComposeWrap::WordOrGlyph => WrapMode::WordOrGlyph,
        };
        self.textarea.set_wrap_mode(mode);
    }

    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// True when the editor is mid-chord in Normal mode (a count,
    /// operator, `g`-prefix, text object, find, or `r` replace is
    /// awaiting its next key). The compose `q` quick-close defers to
    /// this so `gq` and friends complete instead of closing the tab.
    pub fn pending_chord(&self) -> bool {
        self.ops.pending()
    }

    pub fn set_text(&mut self, s: &str) {
        let lines = split_for_textarea(s);
        self.textarea.set_lines(lines, (0, 0));
        self.textarea.cancel_selection();
        self.mode = BodyMode::Normal;
        self.ops = OpState::default();
        self.block_anchor = None;
        self.block_insert = None;
        self.yank_highlight = None;
        self.goal_col = GoalCol::Col(0);
        self.goal_anchor = 0;
        self.sync_cursor_style();
    }

    /// Data-space cursor (row, col into the textarea's line vector).
    /// Not currently used in rendering (tui-textarea paints its own
    /// cursor cell) but exposed for future DECSCUSR / status-row use.
    #[allow(dead_code)]
    pub fn cursor(&self) -> (u16, u16) {
        let (row, col) = self.textarea.cursor();
        (row as u16, col as u16)
    }

    /// Repaint the tui-textarea cursor cell to signal the current mode.
    /// We used to host-drive the real terminal cursor and vary its
    /// DECSCUSR shape (block / bar / underline); under soft-wrap the crate
    /// owns cursor placement, so mode is approximated with a cell style:
    /// a REVERSED block in Normal/Visual, an UNDERLINED cell in
    /// Insert/Replace. Called after every keystroke so a mode change is
    /// reflected on the next frame.
    fn sync_cursor_style(&mut self) {
        let style = match self.mode {
            BodyMode::Insert | BodyMode::Replace => {
                Style::default().add_modifier(Modifier::UNDERLINED)
            }
            BodyMode::Normal | BodyMode::Visual(_) => {
                Style::default().add_modifier(Modifier::REVERSED)
            }
        };
        self.textarea.set_cursor_style(style);
    }

    /// Register the block-wise visual selection and the yank-flash as
    /// tui-textarea `custom_highlight`s so they render at the correct
    /// post-wrap/post-scroll cells. Char/line visual selection is *not*
    /// here — it rides the crate's own selection. Called once per frame
    /// from `compose::draw`, just before the textarea is rendered.
    ///
    /// `custom_highlight` ranges are **byte** offsets within the logical
    /// line (end exclusive), whereas our selection/flash ranges are char
    /// columns, so each endpoint is converted via `char_col_to_byte`.
    pub fn apply_overlays(&mut self) {
        self.textarea.clear_custom_highlight();
        let lines = self.textarea.lines();
        let block_style = Style::default().add_modifier(Modifier::REVERSED);
        // (start (row, byte), end (row, byte), style, priority) — collected
        // first because `lines` holds an immutable borrow of the textarea
        // while `custom_highlight` needs `&mut`.
        let mut emit: Vec<HighlightSpec> = Vec::new();
        for (row, c0, c1) in self.block_selection_ranges() {
            let (row, c0, c1) = (row as usize, c0 as usize, c1 as usize);
            if let Some(line) = lines.get(row) {
                let b0 = char_col_to_byte(line, c0);
                let b1 = char_col_to_byte(line, c1);
                emit.push(((row, b0), (row, b1), block_style, 1));
            }
        }
        if let Some(hl) = self.yank_highlight.as_ref() {
            let yank_style = Style::default().bg(Color::Yellow).fg(Color::Black);
            for (row, c0, c1) in &hl.ranges {
                let (row, c0, c1) = (*row as usize, *c0 as usize, *c1 as usize);
                if let Some(line) = lines.get(row) {
                    let b0 = char_col_to_byte(line, c0);
                    let b1 = char_col_to_byte(line, c1);
                    emit.push(((row, b0), (row, b1), yank_style, 2));
                }
            }
        }
        for (start, end, style, priority) in emit {
            self.textarea
                .custom_highlight((start, end), style, priority);
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent, text_width: u16) -> KeyOutcome {
        // Refresh the reflow width each keystroke — the editor doesn't
        // hold the config, the host threads `[compose].text_width` in.
        self.text_width = if text_width == 0 { 72 } else { text_width };
        let outcome = match self.mode {
            BodyMode::Insert => self.handle_insert(k),
            BodyMode::Replace => self.handle_replace(k),
            BodyMode::Normal => self.handle_normal(k),
            BodyMode::Visual(kind) => self.handle_visual(k, kind),
        };
        // A keystroke may have changed the mode; keep the painted cursor
        // cell in sync so its style always matches the live mode.
        self.sync_cursor_style();
        outcome
    }

    // ---------- Insert mode ----------

    fn handle_insert(&mut self, k: KeyEvent) -> KeyOutcome {
        // Insert mode swallows everything except Esc — `:`, `q`, Tab,
        // etc. all type literal characters into prose. The exit door is
        // Esc (matching vim) which drops back to Normal and nudges the
        // cursor back one cell, like `<Esc>` in real vim.
        if k.code == KeyCode::Esc {
            // A block insert (`Ctrl-V` then `I`/`A`/`c`) replays the
            // top-row text across the other selected rows on Esc.
            if let Some(bi) = self.block_insert.take() {
                self.finish_block_insert(bi);
            }
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

    // ---------- Replace mode ----------

    fn handle_replace(&mut self, k: KeyEvent) -> KeyOutcome {
        // `R` overtype: printable chars replace the char under the cursor
        // (appending past EOL), Backspace steps left, Esc returns to
        // Normal nudging back one cell like Insert. Other keys are
        // swallowed so nothing escapes mid-overtype.
        match k.code {
            KeyCode::Esc => {
                self.mode = BodyMode::Normal;
                self.textarea.move_cursor(CursorMove::Back);
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                let (row, col) = self.textarea.cursor();
                if col < self.textarea.lines()[row].chars().count() {
                    self.textarea.delete_next_char();
                }
                self.textarea.insert_char(c);
            }
            KeyCode::Enter => self.textarea.insert_newline(),
            KeyCode::Backspace | KeyCode::Left => self.textarea.move_cursor(CursorMove::Back),
            KeyCode::Right => self.textarea.move_cursor(CursorMove::Forward),
            _ => {}
        }
        KeyOutcome::Consumed
    }

    // ---------- Normal mode (operator-pending engine) ----------

    fn handle_normal(&mut self, k: KeyEvent) -> KeyOutcome {
        // --- multi-key continuations: each consumes its own latch ---
        if let Some(n) = self.ops.await_replace.take() {
            self.do_replace_char(n, k);
            self.ops = OpState::default();
            return KeyOutcome::Consumed;
        }
        if let Some(fk) = self.ops.await_find.take() {
            let (op, count) = (self.ops.op, self.ops.eff_count());
            self.do_find(fk, op, count, k);
            self.ops = OpState::default();
            return KeyOutcome::Consumed;
        }
        if let Some(around) = self.ops.await_obj.take() {
            let op = self.ops.op;
            self.do_text_object(op, around, k);
            self.ops = OpState::default();
            return KeyOutcome::Consumed;
        }
        if self.ops.await_g {
            self.ops.await_g = false;
            return self.handle_g(k);
        }

        // --- count digits ---
        if let KeyCode::Char(c) = k.code {
            let slot_has = if self.ops.op.is_some() {
                self.ops.count2
            } else {
                self.ops.count1
            };
            // `0` is the LineStart motion unless a count is in progress.
            if !k.modifiers.contains(KeyModifiers::CONTROL)
                && c.is_ascii_digit()
                && (c != '0' || slot_has.is_some())
            {
                let d = c as usize - '0' as usize;
                let slot = if self.ops.op.is_some() {
                    &mut self.ops.count2
                } else {
                    &mut self.ops.count1
                };
                *slot = Some(slot.unwrap_or(0) * 10 + d);
                return KeyOutcome::Consumed;
            }
        }

        // --- Ctrl chords (Ctrl-d/u are operator-capable motions) ---
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(m) = motion::key_to_motion(k) {
                return self.run_motion(m);
            }
            return self.handle_normal_ctrl(k);
        }

        // --- passthrough to the host cmdline / field cycle ---
        match k.code {
            KeyCode::Char(':')
            | KeyCode::Char('/')
            | KeyCode::Char('?')
            | KeyCode::Tab
            | KeyCode::BackTab => {
                self.ops = OpState::default();
                return KeyOutcome::PassThrough;
            }
            _ => {}
        }

        // --- operator-pending intro keys (before motion translation) ---
        if let Some(op) = self.ops.op {
            match k.code {
                KeyCode::Char(c) if c == op_double_key(op) => {
                    let n = self.ops.eff_count();
                    self.apply_linewise_lines(op, n);
                    self.ops = OpState::default();
                    return KeyOutcome::Consumed;
                }
                KeyCode::Char('i') => {
                    self.ops.await_obj = Some(false);
                    return KeyOutcome::Consumed;
                }
                KeyCode::Char('a') => {
                    self.ops.await_obj = Some(true);
                    return KeyOutcome::Consumed;
                }
                KeyCode::Char('f') => {
                    self.ops.await_find = Some(FindKind::Forward);
                    return KeyOutcome::Consumed;
                }
                KeyCode::Char('F') => {
                    self.ops.await_find = Some(FindKind::Back);
                    return KeyOutcome::Consumed;
                }
                KeyCode::Char('t') => {
                    self.ops.await_find = Some(FindKind::Till);
                    return KeyOutcome::Consumed;
                }
                KeyCode::Char('T') => {
                    self.ops.await_find = Some(FindKind::TillBack);
                    return KeyOutcome::Consumed;
                }
                KeyCode::Char('g') => {
                    self.ops.await_g = true;
                    return KeyOutcome::Consumed;
                }
                _ => {}
            }
        }

        // --- motions: operator target (run_motion applies the op) or
        //     plain count-repeated movement ---
        if let Some(m) = motion::key_to_motion(k) {
            return self.run_motion(m);
        }

        // An operator was pending but the key isn't a valid continuation
        // (e.g. `dz`): abandon it (and its counts) and handle the key
        // fresh. Fresh keys (no op) keep any count they were building.
        if self.ops.op.take().is_some() {
            self.ops = OpState::default();
        }

        match k.code {
            // ---- pending-state setters: keep counts, await next key ----
            KeyCode::Char('d') => {
                self.ops.op = Some(Operator::Delete);
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('c') => {
                self.ops.op = Some(Operator::Change);
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('y') => {
                self.ops.op = Some(Operator::Yank);
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('>') => {
                self.ops.op = Some(Operator::Indent);
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('<') => {
                self.ops.op = Some(Operator::Dedent);
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('g') => {
                self.ops.await_g = true;
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('f') => {
                self.ops.await_find = Some(FindKind::Forward);
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('F') => {
                self.ops.await_find = Some(FindKind::Back);
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('t') => {
                self.ops.await_find = Some(FindKind::Till);
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('T') => {
                self.ops.await_find = Some(FindKind::TillBack);
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('r') => {
                self.ops.await_replace = Some(self.ops.eff_count());
                return KeyOutcome::Consumed;
            }

            // ---- actions: do work, then fall through to the reset ----
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
                self.move_first_non_blank();
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
            KeyCode::Char('R') => self.mode = BodyMode::Replace,

            KeyCode::Char('x') => {
                let n = self.ops.eff_count();
                self.delete_chars_at_cursor(n);
            }
            KeyCode::Char('X') => {
                let n = self.ops.eff_count();
                self.delete_chars_before_cursor(n);
            }
            KeyCode::Char('~') => {
                let n = self.ops.eff_count();
                self.toggle_case_at_cursor(n);
            }
            KeyCode::Char('J') => {
                let n = self.ops.eff_count();
                self.join_lines(n, true);
            }
            KeyCode::Char('s') => {
                let n = self.ops.eff_count();
                self.substitute_chars(n);
            }
            KeyCode::Char('S') => {
                let n = self.ops.eff_count();
                self.substitute_lines(n);
            }
            KeyCode::Char('C') => self.change_to_eol(),
            KeyCode::Char('D') => self.delete_to_eol(),
            // `Y` is vim's line-yank, same as `yy`.
            KeyCode::Char('Y') => self.yank_current_line(),
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
        self.ops = OpState::default();
        KeyOutcome::Consumed
    }

    /// Dispatch a motion key: when an operator is pending, resolve the
    /// motion to a region and apply the operator; otherwise move the
    /// cursor `count` times. Always clears the parse state.
    fn run_motion(&mut self, m: Motion) -> KeyOutcome {
        let count = self.ops.eff_count();
        if let Some(op) = self.ops.op {
            let cursor = self.textarea.cursor();
            let m = self.remap_change_word(op, m, cursor);
            if let Some(span) = motion::resolve_motion_span(self.textarea.lines(), cursor, m, count)
            {
                let region = motion::span_to_region(self.textarea.lines(), span);
                self.apply_operator(op, region);
            }
        } else {
            for _ in 0..count {
                motion::apply(self, m);
            }
        }
        self.ops = OpState::default();
        KeyOutcome::Consumed
    }

    /// Vim's `cw` special case: Change + `w`/`W` on a non-blank char acts
    /// like `ce`/`cE` (no trailing whitespace swallowed).
    fn remap_change_word(&self, op: Operator, m: Motion, cursor: (usize, usize)) -> Motion {
        if op != Operator::Change {
            return m;
        }
        let on_blank = self
            .textarea
            .lines()
            .get(cursor.0)
            .and_then(|l| l.chars().nth(cursor.1))
            .map(|c| c.is_whitespace())
            .unwrap_or(true);
        match m {
            Motion::WordForward if !on_blank => Motion::WordEnd,
            Motion::WordForwardBig if !on_blank => Motion::WordEndBig,
            _ => m,
        }
    }

    /// Resolve the second key of a `g`-chord. The operator (if any) is
    /// still live, so `dgg` / `dge` work as operator targets, while `gu`
    /// / `gU` / `g~` / `gw` / `gq` *introduce* an operator that awaits a
    /// further motion.
    fn handle_g(&mut self, k: KeyEvent) -> KeyOutcome {
        match k.code {
            KeyCode::Char('g') => self.run_motion(Motion::FirstLine),
            KeyCode::Char('e') => self.run_motion(Motion::WordEndBack),
            KeyCode::Char('E') => self.run_motion(Motion::WordEndBackBig),
            KeyCode::Char('u') if self.ops.op.is_none() => {
                self.ops.op = Some(Operator::Lower);
                KeyOutcome::Consumed
            }
            KeyCode::Char('U') if self.ops.op.is_none() => {
                self.ops.op = Some(Operator::Upper);
                KeyOutcome::Consumed
            }
            KeyCode::Char('~') if self.ops.op.is_none() => {
                self.ops.op = Some(Operator::ToggleCase);
                KeyOutcome::Consumed
            }
            KeyCode::Char('w') if self.ops.op.is_none() => {
                self.ops.op = Some(Operator::ReflowKeep);
                KeyOutcome::Consumed
            }
            KeyCode::Char('q') if self.ops.op.is_none() => {
                self.ops.op = Some(Operator::ReflowMove);
                KeyOutcome::Consumed
            }
            KeyCode::Char('J') if self.ops.op.is_none() => {
                let n = self.ops.eff_count();
                self.join_lines(n, false);
                self.ops = OpState::default();
                KeyOutcome::Consumed
            }
            _ => {
                self.ops = OpState::default();
                KeyOutcome::Consumed
            }
        }
    }

    fn handle_normal_ctrl(&mut self, k: KeyEvent) -> KeyOutcome {
        // Motion-flavoured Ctrl chords (Ctrl-d / Ctrl-u) are dispatched
        // by the caller before this lands; left here are the non-motion
        // chords: Ctrl-R redo and Ctrl-C app passthrough.
        match k.code {
            KeyCode::Char('r') => {
                self.textarea.redo();
            }
            // Ctrl-V enters block-wise visual.
            KeyCode::Char('v') => self.enter_visual(VisualKind::Block),
            // Ctrl-C falls through to the app (which quits) only when
            // the user has already left Insert. That matches the
            // pty-editor path's behavior: ^C in insert mode is the
            // editor's, ^C in normal is the app's.
            KeyCode::Char('c') => return KeyOutcome::PassThrough,
            _ => {}
        }
        self.ops = OpState::default();
        KeyOutcome::Consumed
    }

    // ---------- Operator application ----------

    /// Apply `op` to a normalised [`Region`]. The single sink for both
    /// Normal-mode operators (`d{motion}`, `ciw`, `gqap`, …) and the
    /// visual-mode operators that route through [`Self::visual_region`].
    fn apply_operator(&mut self, op: Operator, region: Region) {
        let text = self.region_text(&region);
        match op {
            Operator::Yank => {
                self.yank = Some(Yank {
                    text,
                    line_wise: region.linewise,
                });
                let ranges = self.region_ranges(&region);
                self.arm_yank_highlight(ranges);
                self.textarea.cancel_selection();
                self.textarea.move_cursor(CursorMove::Jump(
                    region.start.0 as u16,
                    region.start.1 as u16,
                ));
                self.sync_goal();
            }
            Operator::Delete => {
                self.yank = Some(Yank {
                    text,
                    line_wise: region.linewise,
                });
                self.delete_region(&region);
                self.sync_goal();
            }
            Operator::Change => {
                if region.linewise {
                    self.yank = Some(Yank {
                        text,
                        line_wise: true,
                    });
                    self.change_lines(region.start.0, region.end.0);
                } else {
                    if region.start != region.end {
                        self.yank = Some(Yank {
                            text,
                            line_wise: false,
                        });
                        self.delete_region(&region);
                    }
                    self.mode = BodyMode::Insert;
                }
            }
            Operator::Lower | Operator::Upper | Operator::ToggleCase => {
                let body = if region.linewise {
                    text.trim_end_matches('\n')
                } else {
                    text.as_str()
                };
                let new = transform_case(body, op);
                self.replace_region(&region, &new);
                self.textarea.cancel_selection();
                self.textarea.move_cursor(CursorMove::Jump(
                    region.start.0 as u16,
                    region.start.1 as u16,
                ));
                self.sync_goal();
            }
            Operator::Indent | Operator::Dedent => {
                self.shift_lines(region.start.0, region.end.0, matches!(op, Operator::Indent));
            }
            Operator::ReflowKeep | Operator::ReflowMove => {
                self.reflow_region(
                    region.start.0,
                    region.end.0,
                    matches!(op, Operator::ReflowMove),
                );
            }
        }
    }

    /// Build a linewise region of `n` lines from the cursor row and apply
    /// `op` to it — the doubled-operator form (`dd`, `cc`, `2yy`, `guu`).
    fn apply_linewise_lines(&mut self, op: Operator, n: usize) {
        let (row, _) = self.textarea.cursor();
        let last = self.textarea.lines().len().saturating_sub(1);
        let bot = (row + n.saturating_sub(1)).min(last);
        self.apply_operator(
            op,
            Region {
                start: (row, 0),
                end: (bot, 0),
                linewise: true,
            },
        );
    }

    fn region_text(&self, region: &Region) -> String {
        let lines = self.textarea.lines();
        if region.linewise {
            extract_lines(lines, region.start.0, region.end.0)
        } else {
            extract_range(lines, region.start, region.end)
        }
    }

    fn region_ranges(&self, region: &Region) -> Vec<(u16, u16, u16)> {
        if region.linewise {
            self.line_ranges(region.start.0, region.end.0)
        } else {
            self.char_ranges(region.start.0, region.start.1, region.end.0, region.end.1)
        }
    }

    /// Cut a charwise or linewise region. Linewise routes through
    /// [`Self::cut_lines`] so the trailing-newline bookkeeping stays in
    /// one place; charwise is a single `cut()` (one undo step).
    fn delete_region(&mut self, region: &Region) {
        self.textarea.cancel_selection();
        if region.linewise {
            self.cut_lines(region.start.0, region.end.0);
        } else if region.start != region.end {
            self.textarea.move_cursor(CursorMove::Jump(
                region.start.0 as u16,
                region.start.1 as u16,
            ));
            self.textarea.start_selection();
            self.textarea
                .move_cursor(CursorMove::Jump(region.end.0 as u16, region.end.1 as u16));
            self.textarea.cut();
        }
    }

    /// Cut whole lines `top..=bot` (including the joining newline) as a
    /// single history step, handling the interior / last-line / single-
    /// line cases so no stray empty row is left behind.
    fn cut_lines(&mut self, top: usize, bot: usize) {
        let line_count = self.textarea.lines().len();
        self.textarea.cancel_selection();
        if bot + 1 < line_count {
            self.textarea.move_cursor(CursorMove::Jump(top as u16, 0));
            self.textarea.start_selection();
            self.textarea
                .move_cursor(CursorMove::Jump((bot + 1) as u16, 0));
            self.textarea.cut();
        } else if top > 0 {
            let prev_end = self.textarea.lines()[top - 1].chars().count() as u16;
            let bot_end = self.textarea.lines()[bot].chars().count() as u16;
            self.textarea
                .move_cursor(CursorMove::Jump((top - 1) as u16, prev_end));
            self.textarea.start_selection();
            self.textarea
                .move_cursor(CursorMove::Jump(bot as u16, bot_end));
            self.textarea.cut();
        } else {
            let bot_end = self.textarea.lines()[bot].chars().count() as u16;
            self.textarea.move_cursor(CursorMove::Jump(0, 0));
            self.textarea.start_selection();
            self.textarea
                .move_cursor(CursorMove::Jump(bot as u16, bot_end));
            self.textarea.cut();
        }
    }

    /// `cc` / `S`: collapse lines `top..=bot` to a single empty line and
    /// drop into Insert (vim keeps the row, unlike `dd`).
    fn change_lines(&mut self, top: usize, bot: usize) {
        let bot_end = self.textarea.lines()[bot].chars().count() as u16;
        self.textarea.cancel_selection();
        self.textarea.move_cursor(CursorMove::Jump(top as u16, 0));
        self.textarea.start_selection();
        self.textarea
            .move_cursor(CursorMove::Jump(bot as u16, bot_end));
        self.textarea.cut();
        self.textarea.cancel_selection();
        self.mode = BodyMode::Insert;
    }

    /// Replace a region's text in place (case ops, reflow, indent). Uses
    /// `insert_str` over a selection, which is 2 history steps (delete +
    /// insert) — accepted v1 drift.
    fn replace_region(&mut self, region: &Region, new: &str) {
        self.textarea.cancel_selection();
        if region.linewise {
            let bot_end = self.textarea.lines()[region.end.0].chars().count() as u16;
            self.textarea
                .move_cursor(CursorMove::Jump(region.start.0 as u16, 0));
            self.textarea.start_selection();
            self.textarea
                .move_cursor(CursorMove::Jump(region.end.0 as u16, bot_end));
            self.textarea.insert_str(new);
        } else if region.start != region.end {
            self.textarea.move_cursor(CursorMove::Jump(
                region.start.0 as u16,
                region.start.1 as u16,
            ));
            self.textarea.start_selection();
            self.textarea
                .move_cursor(CursorMove::Jump(region.end.0 as u16, region.end.1 as u16));
            self.textarea.insert_str(new);
        }
    }

    /// `>` / `<`: add or remove one [`SHIFTWIDTH`] of leading whitespace
    /// per line, replacing the whole block in one `insert_str`.
    fn shift_lines(&mut self, top: usize, bot: usize, indent: bool) {
        let joined = {
            let lines = self.textarea.lines();
            (top..=bot)
                .map(|r| shift_line(&lines[r], indent))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let bot_end = self.textarea.lines()[bot].chars().count() as u16;
        self.textarea.cancel_selection();
        self.textarea.move_cursor(CursorMove::Jump(top as u16, 0));
        self.textarea.start_selection();
        self.textarea
            .move_cursor(CursorMove::Jump(bot as u16, bot_end));
        self.textarea.insert_str(joined);
        let col = self.textarea.lines()[top]
            .chars()
            .position(|c| !c.is_whitespace())
            .unwrap_or(0);
        self.textarea.cancel_selection();
        self.textarea
            .move_cursor(CursorMove::Jump(top as u16, col as u16));
        self.sync_goal();
    }

    /// `gw` / `gq`: rewrap the paragraph(s) in `top..=bot` to the
    /// configured width. `gq` parks the cursor on the last reflowed line;
    /// `gw` restores the pre-op cursor (clamped).
    fn reflow_region(&mut self, top: usize, bot: usize, move_to_end: bool) {
        let saved = self.textarea.cursor();
        let text = {
            let lines = self.textarea.lines();
            (top..=bot)
                .map(|r| lines[r].clone())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let wrapped = reflow_paragraph(&text, self.text_width as usize);
        let new_bot = top + wrapped.len().saturating_sub(1);
        let joined = wrapped.join("\n");
        let bot_end = self.textarea.lines()[bot].chars().count() as u16;
        self.textarea.cancel_selection();
        self.textarea.move_cursor(CursorMove::Jump(top as u16, 0));
        self.textarea.start_selection();
        self.textarea
            .move_cursor(CursorMove::Jump(bot as u16, bot_end));
        self.textarea.insert_str(joined);
        self.textarea.cancel_selection();
        if move_to_end {
            self.textarea
                .move_cursor(CursorMove::Jump(new_bot as u16, 0));
        } else {
            let last = self.textarea.lines().len().saturating_sub(1);
            let r = saved.0.min(last);
            let cmax = self.textarea.lines()[r].chars().count().saturating_sub(1);
            self.textarea
                .move_cursor(CursorMove::Jump(r as u16, saved.1.min(cmax) as u16));
        }
        self.sync_goal();
    }

    // ---------- Single-key edits ----------

    fn delete_chars_at_cursor(&mut self, n: usize) {
        let (row, col) = self.textarea.cursor();
        let len = self.textarea.lines()[row].chars().count();
        if col >= len {
            return;
        }
        let region = Region {
            start: (row, col),
            end: (row, (col + n).min(len)),
            linewise: false,
        };
        self.yank = Some(Yank {
            text: self.region_text(&region),
            line_wise: false,
        });
        self.delete_region(&region);
        self.sync_goal();
    }

    fn delete_chars_before_cursor(&mut self, n: usize) {
        let (row, col) = self.textarea.cursor();
        if col == 0 {
            return;
        }
        let region = Region {
            start: (row, col.saturating_sub(n)),
            end: (row, col),
            linewise: false,
        };
        self.yank = Some(Yank {
            text: self.region_text(&region),
            line_wise: false,
        });
        self.delete_region(&region);
        self.sync_goal();
    }

    fn toggle_case_at_cursor(&mut self, n: usize) {
        for _ in 0..n.max(1) {
            let (row, col) = self.textarea.cursor();
            let chars: Vec<char> = self.textarea.lines()[row].chars().collect();
            if col >= chars.len() {
                break;
            }
            let toggled = toggle_case_char(chars[col]);
            self.textarea.cancel_selection();
            self.textarea
                .move_cursor(CursorMove::Jump(row as u16, col as u16));
            self.textarea.start_selection();
            self.textarea
                .move_cursor(CursorMove::Jump(row as u16, (col + 1) as u16));
            self.textarea.insert_str(toggled.to_string());
        }
        self.sync_goal();
    }

    /// `J` (`with_space`) / `gJ`. With count `n`, joins `n` lines (so at
    /// least one join). Removes the next line's leading indent and, for
    /// `J`, inserts a single separating space unless either side is empty.
    fn join_lines(&mut self, n: usize, with_space: bool) {
        let joins = n.saturating_sub(1).max(1);
        for _ in 0..joins {
            let (row, _) = self.textarea.cursor();
            if row + 1 >= self.textarea.lines().len() {
                break;
            }
            let cur_len = self.textarea.lines()[row].chars().count();
            let next = self.textarea.lines()[row + 1].clone();
            let lead = next.chars().take_while(|c| c.is_whitespace()).count();
            let cur_ends_blank = self.textarea.lines()[row]
                .chars()
                .last()
                .map(|c| c.is_whitespace())
                .unwrap_or(true);
            let next_empty = next.trim().is_empty();
            self.textarea.cancel_selection();
            self.textarea
                .move_cursor(CursorMove::Jump(row as u16, cur_len as u16));
            self.textarea.start_selection();
            self.textarea
                .move_cursor(CursorMove::Jump((row + 1) as u16, lead as u16));
            self.textarea.cut();
            if with_space && cur_len > 0 && !cur_ends_blank && !next_empty {
                self.textarea.insert_str(" ");
                self.textarea.move_cursor(CursorMove::Back);
            }
        }
        self.sync_goal();
    }

    /// `s` — substitute `n` chars (`cl`): delete then Insert.
    fn substitute_chars(&mut self, n: usize) {
        let (row, col) = self.textarea.cursor();
        let len = self.textarea.lines()[row].chars().count();
        let region = Region {
            start: (row, col),
            end: (row, (col + n).min(len)),
            linewise: false,
        };
        if region.start != region.end {
            self.yank = Some(Yank {
                text: self.region_text(&region),
                line_wise: false,
            });
            self.delete_region(&region);
        }
        self.mode = BodyMode::Insert;
    }

    /// `S` — substitute `n` lines (`cc`).
    fn substitute_lines(&mut self, n: usize) {
        let (row, _) = self.textarea.cursor();
        let last = self.textarea.lines().len().saturating_sub(1);
        let bot = (row + n.saturating_sub(1)).min(last);
        let region = Region {
            start: (row, 0),
            end: (bot, 0),
            linewise: true,
        };
        self.yank = Some(Yank {
            text: self.region_text(&region),
            line_wise: true,
        });
        self.change_lines(row, bot);
    }

    /// `C` — change to end of line (`c$`).
    fn change_to_eol(&mut self) {
        let (row, col) = self.textarea.cursor();
        let len = self.textarea.lines()[row].chars().count();
        let region = Region {
            start: (row, col),
            end: (row, len),
            linewise: false,
        };
        if region.start != region.end {
            self.yank = Some(Yank {
                text: self.region_text(&region),
                line_wise: false,
            });
            self.delete_region(&region);
        }
        self.mode = BodyMode::Insert;
    }

    /// `D` — delete to end of line (`d$`).
    fn delete_to_eol(&mut self) {
        let (row, col) = self.textarea.cursor();
        let len = self.textarea.lines()[row].chars().count();
        let region = Region {
            start: (row, col),
            end: (row, len),
            linewise: false,
        };
        self.yank = Some(Yank {
            text: self.region_text(&region),
            line_wise: false,
        });
        self.delete_region(&region);
        self.sync_goal();
    }

    /// `r{char}` — replace the next `n` chars (clamped to EOL) with the
    /// typed char, leaving the cursor on the last replaced cell.
    fn do_replace_char(&mut self, n: usize, k: KeyEvent) {
        let KeyCode::Char(ch) = k.code else {
            return; // Esc / other cancels
        };
        let (row, col) = self.textarea.cursor();
        let len = self.textarea.lines()[row].chars().count();
        if col >= len {
            return;
        }
        let n = n.min(len - col).max(1);
        let repl: String = std::iter::repeat_n(ch, n).collect();
        self.textarea.cancel_selection();
        self.textarea
            .move_cursor(CursorMove::Jump(row as u16, col as u16));
        self.textarea.start_selection();
        self.textarea
            .move_cursor(CursorMove::Jump(row as u16, (col + n) as u16));
        self.textarea.insert_str(repl);
        self.textarea.move_cursor(CursorMove::Back);
        self.sync_goal();
    }

    /// `f F t T` — resolve the find against the current line. With an
    /// operator pending, apply it to the spanned region; otherwise move.
    fn do_find(&mut self, fk: FindKind, op: Option<Operator>, count: usize, k: KeyEvent) {
        let KeyCode::Char(ch) = k.code else {
            return;
        };
        let cursor = self.textarea.cursor();
        let chars: Vec<char> = self.textarea.lines()[cursor.0].chars().collect();
        let Some(span) = find_span(cursor, &chars, fk, ch, count) else {
            return;
        };
        if let Some(op) = op {
            let region = motion::span_to_region(self.textarea.lines(), span);
            self.apply_operator(op, region);
        } else {
            self.textarea.cancel_selection();
            self.textarea
                .move_cursor(CursorMove::Jump(span.end.0 as u16, span.end.1 as u16));
            self.sync_goal();
        }
    }

    /// `i{obj}` / `a{obj}` after an operator — resolve the text object and
    /// apply. Text objects require a pending operator (no visual entry
    /// from Normal here).
    fn do_text_object(&mut self, op: Option<Operator>, around: bool, k: KeyEvent) {
        let KeyCode::Char(c) = k.code else { return };
        let (Some(obj), Some(op)) = (textobj::key_to_text_object(c), op) else {
            return;
        };
        let kind = if around {
            TextObjKind::Around
        } else {
            TextObjKind::Inner
        };
        let cursor = self.textarea.cursor();
        if let Some(span) = textobj::resolve_text_object(self.textarea.lines(), cursor, obj, kind) {
            let region = motion::span_to_region(self.textarea.lines(), span);
            self.apply_operator(op, region);
        }
    }

    /// Re-establish the vim goal column from the live cursor column.
    /// Called after any horizontal / explicit-column move so a following
    /// `j`/`k` rides the column the user just landed on.
    fn sync_goal(&mut self) {
        let (_, col) = self.textarea.cursor();
        self.goal_col = GoalCol::Col(col);
        self.goal_anchor = col;
    }

    /// Vim-style vertical motion that preserves the goal column across
    /// lines of differing length (tui-textarea's `Up`/`Down` clamps and
    /// forgets it). Reseeds the goal when an untracked path moved the
    /// cursor since the last tracked move, then `Jump`s to the goal
    /// column clamped to the target line's last on-a-char position.
    fn vertical(&mut self, up: bool, count: usize) {
        let (row, col) = self.textarea.cursor();
        // Untracked move (edit, paste, insert exit) since the last goal
        // sync? Reseed so we don't carry a stale goal.
        if col != self.goal_anchor {
            self.goal_col = GoalCol::Col(col);
        }
        let last_row = self.textarea.lines().len().saturating_sub(1);
        let target_row = if up {
            row.saturating_sub(count)
        } else {
            (row + count).min(last_row)
        };
        // Normal/Visual: the block cursor sits ON a char, so the last
        // valid column is line_len - 1 (0 for an empty line).
        let max_col = self.textarea.lines()[target_row]
            .chars()
            .count()
            .saturating_sub(1);
        let new_col = match self.goal_col {
            GoalCol::Eol => max_col,
            GoalCol::Col(n) => n.min(max_col),
        };
        self.textarea
            .move_cursor(CursorMove::Jump(target_row as u16, new_col as u16));
        self.goal_anchor = new_col;
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

    /// Vim word motion via the shared [`words`] scanner. Both the small
    /// (`w b e`) and WORD (`W B E`) variants route here so the composer
    /// and the reader stay byte-for-byte in agreement on word boundaries
    /// — tui-textarea's own `WordForward` differs subtly and the reader
    /// can't reach it at all.
    fn word_move(&mut self, motion: WordMotion, big: bool) {
        let (row, col) = self.textarea.cursor();
        let (nr, nc) = words::word_motion(self.textarea.lines(), row, col, motion, big);
        self.textarea
            .move_cursor(CursorMove::Jump(nr as u16, nc as u16));
    }

    // ---------- Visual mode ----------

    fn enter_visual(&mut self, kind: VisualKind) {
        self.ops = OpState::default();
        self.visual_anchor = self.textarea.cursor();
        self.block_anchor = if kind == VisualKind::Block {
            Some(self.visual_anchor)
        } else {
            None
        };
        self.mode = BodyMode::Visual(kind);
        self.refresh_visual_selection();
    }

    fn exit_visual_to_normal(&mut self) {
        self.ops = OpState::default();
        self.textarea.cancel_selection();
        self.block_anchor = None;
        self.mode = BodyMode::Normal;
    }

    /// The current char/line visual selection as an operator [`Region`].
    /// Char-wise is inclusive of the cursor cell (vim semantics); block
    /// returns `None` (it routes through its own rectangle path).
    fn visual_region(&self, kind: VisualKind) -> Option<Region> {
        let anchor = self.visual_anchor;
        let cur = self.textarea.cursor();
        match kind {
            VisualKind::Char => {
                let (lo, hi) = if anchor <= cur {
                    (anchor, cur)
                } else {
                    (cur, anchor)
                };
                let len = self
                    .textarea
                    .lines()
                    .get(hi.0)
                    .map(|l| l.chars().count())
                    .unwrap_or(0);
                Some(Region {
                    start: lo,
                    end: (hi.0, (hi.1 + 1).min(len)),
                    linewise: false,
                })
            }
            VisualKind::Line => {
                let (top, bot) = (anchor.0.min(cur.0), anchor.0.max(cur.0));
                Some(Region {
                    start: (top, 0),
                    end: (bot, 0),
                    linewise: true,
                })
            }
            VisualKind::Block => None,
        }
    }

    /// Apply a non-`d`/`y`/`c` operator (`> < gu gU g~ gw gq`) to the
    /// current selection and leave visual. Block selections are skipped.
    fn visual_apply(&mut self, kind: VisualKind, op: Operator) {
        if let Some(region) = self.visual_region(kind) {
            self.apply_operator(op, region);
        }
        self.exit_visual_to_normal();
    }

    /// Reshape the selection to the text object under the cursor (`viw`,
    /// `va"`, `vip`). Anchor moves to the object start, cursor to its end.
    fn visual_select_object(&mut self, around: bool, k: KeyEvent) {
        let KeyCode::Char(c) = k.code else { return };
        let Some(obj) = textobj::key_to_text_object(c) else {
            return;
        };
        let objkind = if around {
            TextObjKind::Around
        } else {
            TextObjKind::Inner
        };
        let cursor = self.textarea.cursor();
        if let Some(span) =
            textobj::resolve_text_object(self.textarea.lines(), cursor, obj, objkind)
        {
            self.visual_anchor = span.start;
            self.textarea
                .move_cursor(CursorMove::Jump(span.end.0 as u16, span.end.1 as u16));
            self.refresh_visual_selection();
        }
    }

    /// Switch visual kind in place, keeping the anchor. Char/Line drive
    /// the textarea selection; Block drives our own `block_anchor`.
    fn swap_visual_kind(&mut self, new: VisualKind) {
        if new == VisualKind::Block {
            self.block_anchor = Some(self.visual_anchor);
        } else {
            self.block_anchor = None;
        }
        self.mode = BodyMode::Visual(new);
        self.refresh_visual_selection();
    }

    /// Rectangle corners `(r0, r1, c0, c1)` (rows/cols normalized
    /// independently) for the active block selection, or `None` when not
    /// block-selecting. `c1` is inclusive.
    fn block_corners(&self) -> Option<(usize, usize, usize, usize)> {
        let (ar, ac) = self.block_anchor?;
        let (cr, cc) = self.textarea.cursor();
        Some((ar.min(cr), ar.max(cr), ac.min(cc), ac.max(cc)))
    }

    /// Cell ranges (`(row, col_start, col_end_excl)`, char coords) for the
    /// live block selection — drives both the flash and the painter in
    /// `compose.rs`. Empty when not block-selecting.
    pub fn block_selection_ranges(&self) -> Vec<(u16, u16, u16)> {
        let Some((r0, r1, c0, c1)) = self.block_corners() else {
            return Vec::new();
        };
        let lines = self.textarea.lines();
        (r0..=r1)
            .filter_map(|r| {
                let len = lines.get(r)?.chars().count();
                let lo = c0.min(len);
                let hi = (c1 + 1).min(len);
                (hi > lo).then_some((r as u16, lo as u16, hi as u16))
            })
            .collect()
    }

    fn handle_visual(&mut self, k: KeyEvent, kind: VisualKind) -> KeyOutcome {
        if k.code == KeyCode::Esc {
            self.exit_visual_to_normal();
            return KeyOutcome::Consumed;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            // Ctrl-V toggles block-wise: same kind exits, else swaps in.
            if matches!(k.code, KeyCode::Char('v')) {
                if kind == VisualKind::Block {
                    self.exit_visual_to_normal();
                } else {
                    self.swap_visual_kind(VisualKind::Block);
                }
                return KeyOutcome::Consumed;
            }
            // Ctrl-C exits visual like Esc (and then the app handler
            // sees a clean Normal-mode keypress next).
            if matches!(k.code, KeyCode::Char('c')) {
                self.exit_visual_to_normal();
                return KeyOutcome::Consumed;
            }
            return KeyOutcome::Consumed;
        }
        // Text object after `i`/`a`: reshape the selection to the object.
        if let Some(around) = self.ops.await_obj.take() {
            self.visual_select_object(around, k);
            return KeyOutcome::Consumed;
        }
        // `g`-chord resolution: a prior `g` armed `await_g`. `gg`/`ge`/`gE`
        // extend the selection; `gu`/`gU`/`g~`/`gw`/`gq` apply that
        // operator to it; anything else clears the latch.
        if self.ops.await_g {
            self.ops.await_g = false;
            match k.code {
                KeyCode::Char('g') => {
                    motion::apply(self, Motion::FirstLine);
                    self.refresh_visual_selection();
                    return KeyOutcome::Consumed;
                }
                KeyCode::Char('e') => {
                    motion::apply(self, Motion::WordEndBack);
                    self.refresh_visual_selection();
                    return KeyOutcome::Consumed;
                }
                KeyCode::Char('E') => {
                    motion::apply(self, Motion::WordEndBackBig);
                    self.refresh_visual_selection();
                    return KeyOutcome::Consumed;
                }
                KeyCode::Char('u') => {
                    self.visual_apply(kind, Operator::Lower);
                    return KeyOutcome::Consumed;
                }
                KeyCode::Char('U') => {
                    self.visual_apply(kind, Operator::Upper);
                    return KeyOutcome::Consumed;
                }
                KeyCode::Char('~') => {
                    self.visual_apply(kind, Operator::ToggleCase);
                    return KeyOutcome::Consumed;
                }
                KeyCode::Char('w') => {
                    self.visual_apply(kind, Operator::ReflowKeep);
                    return KeyOutcome::Consumed;
                }
                KeyCode::Char('q') => {
                    self.visual_apply(kind, Operator::ReflowMove);
                    return KeyOutcome::Consumed;
                }
                _ => {}
            }
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
                    self.swap_visual_kind(VisualKind::Char);
                }
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('V') => {
                if kind == VisualKind::Line {
                    self.exit_visual_to_normal();
                } else {
                    self.swap_visual_kind(VisualKind::Line);
                }
                return KeyOutcome::Consumed;
            }

            // Selection actions
            KeyCode::Char('y') => {
                if kind == VisualKind::Block {
                    self.yank_block();
                } else {
                    self.yank_selection(kind);
                }
                self.exit_visual_to_normal();
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('d') | KeyCode::Char('x') => {
                if kind == VisualKind::Block {
                    self.delete_block();
                } else {
                    self.cut_selection(kind);
                }
                self.exit_visual_to_normal();
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('c') => {
                if kind == VisualKind::Block {
                    // Capture corners before `delete_block` moves the
                    // cursor (which would otherwise collapse the row range
                    // when the anchor is the top row), then block-insert at
                    // the left edge — vim's block-change.
                    if let Some(corners) = self.block_corners() {
                        self.delete_block();
                        self.begin_block_insert(corners, BlockInsertKind::Change);
                    }
                } else {
                    self.cut_selection(kind);
                    self.textarea.cancel_selection();
                    self.block_anchor = None;
                    self.mode = BodyMode::Insert;
                }
                return KeyOutcome::Consumed;
            }
            // Block-insert entries (block-visual only).
            KeyCode::Char('I') if kind == VisualKind::Block => {
                if let Some(corners) = self.block_corners() {
                    self.begin_block_insert(corners, BlockInsertKind::Insert);
                }
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('A') if kind == VisualKind::Block => {
                if let Some(corners) = self.block_corners() {
                    self.begin_block_insert(corners, BlockInsertKind::Append);
                }
                return KeyOutcome::Consumed;
            }

            // Indent / dedent the selected lines.
            KeyCode::Char('>') => {
                self.visual_apply(kind, Operator::Indent);
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('<') => {
                self.visual_apply(kind, Operator::Dedent);
                return KeyOutcome::Consumed;
            }

            // Reshape the selection to a text object (`iw`, `i"`, `ip`, …).
            KeyCode::Char('i') if kind != VisualKind::Block => {
                self.ops.await_obj = Some(false);
                return KeyOutcome::Consumed;
            }
            KeyCode::Char('a') if kind != VisualKind::Block => {
                self.ops.await_obj = Some(true);
                return KeyOutcome::Consumed;
            }

            // Arm a `g`-chord; resolved at the top of the next call.
            KeyCode::Char('g') => {
                self.ops.await_g = true;
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
            // Block selection is painted by us (see `block_selection_ranges`),
            // not the textarea's single-range selection — leave it cleared.
            VisualKind::Block => {}
        }
    }

    // ---------- Block ops ----------

    /// Rectangular text of the active block selection (rows joined by
    /// `\n`, each row sliced to `[c0, c1]`). Empty when not block-selecting.
    fn block_text(&self, r0: usize, r1: usize, c0: usize, c1: usize) -> String {
        let lines = self.textarea.lines();
        (r0..=r1)
            .map(|r| {
                let chars: Vec<char> = lines[r].chars().collect();
                let lo = c0.min(chars.len());
                let hi = (c1 + 1).min(chars.len());
                chars[lo..hi].iter().collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn yank_block(&mut self) {
        let Some((r0, r1, c0, c1)) = self.block_corners() else {
            return;
        };
        let text = self.block_text(r0, r1, c0, c1);
        self.yank = Some(Yank {
            text,
            line_wise: false,
        });
        let ranges = self.block_selection_ranges();
        self.arm_yank_highlight(ranges);
    }

    fn delete_block(&mut self) {
        let Some((r0, r1, c0, c1)) = self.block_corners() else {
            return;
        };
        let text = self.block_text(r0, r1, c0, c1);
        self.yank = Some(Yank {
            text,
            line_wise: false,
        });
        // Cut each row's `[c0, c1]` span. Within-line cuts never shift
        // other rows, so order is free; rows shorter than `c0` are
        // untouched. Each cut is its own history step (v1: N undos).
        for r in r0..=r1 {
            let len = self.textarea.lines()[r].chars().count();
            if c0 >= len {
                continue;
            }
            let hi = (c1 + 1).min(len);
            self.textarea.cancel_selection();
            self.textarea
                .move_cursor(CursorMove::Jump(r as u16, c0 as u16));
            self.textarea.start_selection();
            self.textarea
                .move_cursor(CursorMove::Jump(r as u16, hi as u16));
            self.textarea.cut();
        }
        self.textarea.cancel_selection();
        self.textarea
            .move_cursor(CursorMove::Jump(r0 as u16, c0 as u16));
    }

    /// Start a block insert (`I` / `A` / `c`). Records the replay plan in
    /// `block_insert`, moves the cursor to the top-row insert column, and
    /// drops into Insert mode. The actual replay across the other rows
    /// happens when the Esc that ends the insert lands in `handle_insert`.
    fn begin_block_insert(&mut self, corners: (usize, usize, usize, usize), what: BlockInsertKind) {
        let (r0, r1, c0, c1) = corners;
        let lines = self.textarea.lines();
        let top_len = lines[r0].chars().count();
        let (spec, top_col) = match what {
            BlockInsertKind::Insert | BlockInsertKind::Change => {
                (BlockColSpec::Left(c0), c0.min(top_len))
            }
            BlockInsertKind::Append => (BlockColSpec::Append(c1), (c1 + 1).min(top_len)),
        };
        let rows: Vec<usize> = (r0 + 1..=r1).collect();
        self.block_insert = Some(BlockInsert {
            rows,
            spec,
            top_row: r0,
            top_col,
            pre_len: top_len,
            pre_rows: self.textarea.lines().len(),
        });
        // The rectangle is captured in `block_insert`; drop the visual
        // anchor so leaving via Esc doesn't think we're still selecting.
        self.block_anchor = None;
        self.textarea.cancel_selection();
        self.textarea
            .move_cursor(CursorMove::Jump(r0 as u16, top_col as u16));
        self.mode = BodyMode::Insert;
    }

    /// Replay the just-typed top-row text across the other block rows.
    /// Called from the Esc that ends a block insert. Aborts (replays
    /// nothing) if the row count changed (a newline was typed) or the
    /// cursor left the top row, matching the snapshot-diff contract.
    fn finish_block_insert(&mut self, bi: BlockInsert) {
        let lines = self.textarea.lines();
        if lines.len() != bi.pre_rows {
            return;
        }
        let (cur_row, _) = self.textarea.cursor();
        if cur_row != bi.top_row {
            return;
        }
        let top_chars: Vec<char> = lines[bi.top_row].chars().collect();
        let typed_len = top_chars.len().saturating_sub(bi.pre_len);
        if typed_len == 0 {
            return;
        }
        let typed: String = top_chars[bi.top_col..bi.top_col + typed_len]
            .iter()
            .collect();
        for r in bi.rows {
            let len = self.textarea.lines()[r].chars().count();
            let col = match bi.spec {
                // `I`: insert at the left edge; skip rows too short to
                // reach it (vim pads with spaces — accepted v1 drift).
                BlockColSpec::Left(c0) => {
                    if c0 > len {
                        continue;
                    }
                    c0
                }
                // `A`: append after the right edge, or at the row's own
                // end when it's shorter than the block.
                BlockColSpec::Append(c1) => (c1 + 1).min(len),
            };
            self.textarea
                .move_cursor(CursorMove::Jump(r as u16, col as u16));
            self.textarea.insert_str(&typed);
        }
        // Park the cursor back where the live insert ended.
        self.textarea.move_cursor(CursorMove::Jump(
            bi.top_row as u16,
            (bi.top_col + typed_len) as u16,
        ));
    }

    // ---------- Line ops ----------

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
            // Block routes through `yank_block`, never here.
            VisualKind::Block => unreachable!("block yank handled by yank_block"),
        };
        self.yank = Some(Yank {
            text,
            line_wise: matches!(kind, VisualKind::Line),
        });
        let ranges = match kind {
            VisualKind::Char => self.char_ranges(sr, sc, er, ec),
            VisualKind::Line => self.line_ranges(sr, er),
            VisualKind::Block => unreachable!("block yank handled by yank_block"),
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
            // Block routes through `delete_block`, never here.
            VisualKind::Block => unreachable!("block cut handled by delete_block"),
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
            VisualKind::Block => unreachable!("block cut handled by delete_block"),
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
        self.sync_goal();
    }
    fn move_char_right(&mut self) {
        self.move_l();
        self.sync_goal();
    }
    fn move_char_up(&mut self) {
        self.vertical(true, 1);
    }
    fn move_char_down(&mut self) {
        self.vertical(false, 1);
    }
    fn move_word_forward(&mut self) {
        self.word_move(WordMotion::Forward, false);
        self.sync_goal();
    }
    fn move_word_back(&mut self) {
        self.word_move(WordMotion::Back, false);
        self.sync_goal();
    }
    fn move_word_end(&mut self) {
        self.word_move(WordMotion::End, false);
        self.sync_goal();
    }
    fn move_word_forward_big(&mut self) {
        self.word_move(WordMotion::Forward, true);
        self.sync_goal();
    }
    fn move_word_back_big(&mut self) {
        self.word_move(WordMotion::Back, true);
        self.sync_goal();
    }
    fn move_word_end_big(&mut self) {
        self.word_move(WordMotion::End, true);
        self.sync_goal();
    }
    fn move_word_end_back(&mut self) {
        self.word_move(WordMotion::EndBack, false);
        self.sync_goal();
    }
    fn move_word_end_back_big(&mut self) {
        self.word_move(WordMotion::EndBack, true);
        self.sync_goal();
    }
    fn move_line_start(&mut self) {
        self.textarea.move_cursor(CursorMove::Head);
        self.sync_goal();
    }
    fn move_first_non_blank(&mut self) {
        let (row, _) = self.textarea.cursor();
        let col = self.textarea.lines()[row]
            .chars()
            .position(|c| !c.is_whitespace())
            .unwrap_or(0);
        self.textarea
            .move_cursor(CursorMove::Jump(row as u16, col as u16));
        self.sync_goal();
    }
    fn move_line_end(&mut self) {
        self.textarea.move_cursor(CursorMove::End);
        // `$` rides the end of each line on subsequent `j`/`k` until a
        // horizontal move resets the goal (vim's `curswant = MAXCOL`).
        self.goal_col = GoalCol::Eol;
        self.goal_anchor = self.textarea.cursor().1;
    }
    fn move_first_line(&mut self) {
        self.textarea.move_cursor(CursorMove::Top);
        self.textarea.move_cursor(CursorMove::Head);
        self.sync_goal();
    }
    fn move_last_line(&mut self) {
        self.textarea.move_cursor(CursorMove::Bottom);
        self.textarea.move_cursor(CursorMove::Head);
        self.sync_goal();
    }
    fn move_half_page(&mut self, down: bool) {
        self.vertical(!down, half_page());
    }
}

fn split_for_textarea(s: &str) -> Vec<String> {
    if s.is_empty() {
        return vec![String::new()];
    }
    s.split('\n').map(str::to_string).collect()
}

/// A single `custom_highlight` registration: start `(row, byte)`, end
/// `(row, byte)` (end exclusive), the cell style, and the layer priority.
type HighlightSpec = ((usize, usize), (usize, usize), Style, u8);

/// Byte offset of the `col`-th character in `line` (0-based, by `char`,
/// matching the textarea's column model). Columns at or past the end of
/// the line clamp to `line.len()`. Used to translate our char-column
/// selection/flash ranges into the byte offsets `custom_highlight` wants.
fn char_col_to_byte(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map(|(b, _)| b)
        .unwrap_or(line.len())
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

fn toggle_case_char(c: char) -> char {
    if c.is_uppercase() {
        c.to_lowercase().next().unwrap_or(c)
    } else if c.is_lowercase() {
        c.to_uppercase().next().unwrap_or(c)
    } else {
        c
    }
}

/// Map a string through a case operator (`gu` / `gU` / `g~`). Uses the
/// full Unicode case mappings so e.g. `ß` → `SS` survives.
fn transform_case(s: &str, op: Operator) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match op {
            Operator::Lower => out.extend(c.to_lowercase()),
            Operator::Upper => out.extend(c.to_uppercase()),
            Operator::ToggleCase => {
                if c.is_uppercase() {
                    out.extend(c.to_lowercase());
                } else if c.is_lowercase() {
                    out.extend(c.to_uppercase());
                } else {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// One step of indent (`>`) or dedent (`<`). Blank lines are left alone;
/// dedent removes up to [`SHIFTWIDTH`] leading spaces or a single tab.
fn shift_line(line: &str, indent: bool) -> String {
    if line.trim().is_empty() {
        return line.to_string();
    }
    if indent {
        let mut s = " ".repeat(SHIFTWIDTH);
        s.push_str(line);
        return s;
    }
    let mut removed = 0;
    let mut cut = 0;
    for (i, c) in line.char_indices() {
        if c == ' ' && removed < SHIFTWIDTH {
            removed += 1;
            cut = i + c.len_utf8();
        } else if c == '\t' && removed == 0 {
            cut = i + c.len_utf8();
            break;
        } else {
            break;
        }
    }
    line[cut..].to_string()
}

/// Resolve an `f`/`F`/`t`/`T` find on a single line's chars to a span
/// from the cursor to the target. Forward finds are inclusive, backward
/// finds exclusive (so `dF` / `dT` stop before the cursor cell). `None`
/// when the char isn't found `count` times.
fn find_span(
    cursor: (usize, usize),
    chars: &[char],
    fk: FindKind,
    ch: char,
    count: usize,
) -> Option<MotionSpan> {
    let (row, col) = cursor;
    let count = count.max(1);
    match fk {
        FindKind::Forward | FindKind::Till => {
            let mut seen = 0;
            let mut hit = None;
            for (i, &c) in chars.iter().enumerate().skip(col + 1) {
                if c == ch {
                    seen += 1;
                    if seen == count {
                        hit = Some(i);
                        break;
                    }
                }
            }
            let i = hit?;
            let target = if matches!(fk, FindKind::Till) {
                i.saturating_sub(1)
            } else {
                i
            };
            Some(MotionSpan {
                start: (row, col),
                end: (row, target),
                kind: MotionKind::CharInclusive,
            })
        }
        FindKind::Back | FindKind::TillBack => {
            let mut seen = 0;
            let mut hit = None;
            for i in (0..col).rev() {
                if chars[i] == ch {
                    seen += 1;
                    if seen == count {
                        hit = Some(i);
                        break;
                    }
                }
            }
            let i = hit?;
            let target = if matches!(fk, FindKind::TillBack) {
                i + 1
            } else {
                i
            };
            Some(MotionSpan {
                start: (row, col),
                end: (row, target),
                kind: MotionKind::CharExclusive,
            })
        }
    }
}

/// Greedy reflow to `width` display columns. Blank lines separate
/// paragraphs (preserved); words are never split, so a word wider than
/// `width` overflows onto its own line. Width 0 falls back to one column.
fn reflow_paragraph(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim().is_empty() {
            out.push(String::new());
            i += 1;
            continue;
        }
        let mut words: Vec<&str> = Vec::new();
        while i < lines.len() && !lines[i].trim().is_empty() {
            words.extend(lines[i].split_whitespace());
            i += 1;
        }
        let mut line = String::new();
        for w in words {
            if line.is_empty() {
                line.push_str(w);
            } else if line.width() + 1 + w.width() <= width {
                line.push(' ');
                line.push_str(w);
            } else {
                out.push(std::mem::take(&mut line));
                line.push_str(w);
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    if out.is_empty() {
        out.push(String::new());
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

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn feed(ed: &mut BodyEditor, keys: &[KeyEvent]) {
        for ke in keys {
            ed.handle_key(*ke, 72);
        }
    }

    #[test]
    fn capital_y_yanks_line_like_yy() {
        let mut ed = BodyEditor::new("alpha\nbeta");
        feed(&mut ed, &[k('Y'), k('p')]);
        assert_eq!(ed.text(), "alpha\nalpha\nbeta");
    }

    #[test]
    fn vertical_preserves_goal_column_across_blank_line() {
        let mut ed = BodyEditor::new("foo bar\n\nfoo");
        // Cursor to column 2 (second 'o'), then down across the blank
        // line: vim curswant keeps column 2 instead of collapsing to 0.
        feed(&mut ed, &[k('l'), k('l'), k('j'), k('j')]);
        assert_eq!(ed.cursor(), (2, 2));
    }

    #[test]
    fn dollar_rides_end_of_each_line_on_vertical_move() {
        let mut ed = BodyEditor::new("foobar\nab\nhello");
        feed(&mut ed, &[k('$'), k('j')]);
        assert_eq!(ed.cursor(), (1, 1)); // EOL of "ab"
        ed.handle_key(k('j'), 72);
        assert_eq!(ed.cursor(), (2, 4)); // EOL of "hello"
    }

    #[test]
    fn horizontal_move_resets_goal_column() {
        let mut ed = BodyEditor::new("foobar\nab\nhello");
        // `$` then `h` drops EOL-sticky; the goal becomes the concrete
        // column, so the next `j` lands there rather than at EOL.
        feed(&mut ed, &[k('$'), k('j'), k('h'), k('j')]);
        assert_eq!(ed.cursor(), (2, 0));
    }

    #[test]
    fn ctrl_v_enters_block_visual() {
        let mut ed = BodyEditor::new("abcd\nefgh");
        ed.handle_key(ctrl('v'), 72);
        assert_eq!(ed.mode, BodyMode::Visual(VisualKind::Block));
    }

    #[test]
    fn block_yank_is_rectangular() {
        let mut ed = BodyEditor::new("abcd\nefgh\nijkl");
        // Ctrl-V at (0,0), down twice, right once → rect cols 0..=1 × rows 0..=2.
        feed(&mut ed, &[ctrl('v'), k('j'), k('j'), k('l'), k('y')]);
        let y = ed.yank.as_ref().expect("yank set");
        assert_eq!(y.text, "ab\nef\nij");
        assert!(!y.line_wise);
        assert_eq!(ed.mode, BodyMode::Normal);
    }

    #[test]
    fn block_delete_removes_rectangle() {
        let mut ed = BodyEditor::new("abcd\nefgh\nijkl");
        feed(&mut ed, &[ctrl('v'), k('j'), k('j'), k('l'), k('d')]);
        assert_eq!(ed.text(), "cd\ngh\nkl");
        let y = ed.yank.as_ref().expect("yank set");
        assert_eq!(y.text, "ab\nef\nij");
    }

    #[test]
    fn block_insert_replays_across_rows() {
        let mut ed = BodyEditor::new("abcd\nefgh\nijkl");
        // Ctrl-V, down twice (column 0 across 3 rows), I, type X, Esc.
        feed(&mut ed, &[ctrl('v'), k('j'), k('j'), k('I'), k('X'), esc()]);
        assert_eq!(ed.text(), "Xabcd\nXefgh\nXijkl");
        assert_eq!(ed.mode, BodyMode::Normal);
    }

    #[test]
    fn block_append_inserts_after_right_edge() {
        let mut ed = BodyEditor::new("ab\ncd\nef");
        // Single-column block at col 0 across 3 rows; A appends after col 0.
        feed(&mut ed, &[ctrl('v'), k('j'), k('j'), k('A'), k('X'), esc()]);
        assert_eq!(ed.text(), "aXb\ncXd\neXf");
    }

    #[test]
    fn block_insert_aborts_replay_on_newline() {
        let mut ed = BodyEditor::new("abcd\nefgh\nijkl");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        // Pressing Enter changes the row count → replay is skipped; only
        // the top-row live edit lands, the other rows stay untouched.
        feed(
            &mut ed,
            &[
                ctrl('v'),
                k('j'),
                k('j'),
                k('I'),
                k('X'),
                enter,
                k('Y'),
                esc(),
            ],
        );
        assert_eq!(ed.text(), "X\nYabcd\nefgh\nijkl");
    }

    #[test]
    fn i_then_type_then_esc_lands_in_normal_with_text() {
        let mut ed = BodyEditor::new("");
        feed(&mut ed, &[k('i'), k('H'), k('i')]);
        assert_eq!(ed.mode, BodyMode::Insert);
        assert_eq!(ed.text(), "Hi");
        ed.handle_key(esc(), 72);
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
        let out = ed.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), 72);
        assert!(matches!(out, KeyOutcome::PassThrough));
        // Insert eats the colon as literal text.
        ed.handle_key(k('i'), 72);
        let out = ed.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), 72);
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
            ed.handle_key(k(c), 72);
        }
        ed.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL), 72);
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
            ed.handle_key(k(c), 72);
        }
        ed.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), 72);
        assert_eq!(ed.text(), "");
    }

    // ---------- operator engine ----------

    #[test]
    fn dw_deletes_word_and_trailing_space() {
        let mut ed = BodyEditor::new("foo bar baz");
        feed(&mut ed, &[k('d'), k('w')]);
        assert_eq!(ed.text(), "bar baz");
    }

    #[test]
    fn dw_at_last_word_stops_at_eol() {
        let mut ed = BodyEditor::new("foo bar\nbaz");
        // cursor on 'b' of "bar" via word-forward, then dw: stays on line 0.
        feed(&mut ed, &[k('w'), k('d'), k('w')]);
        assert_eq!(ed.text(), "foo \nbaz");
    }

    #[test]
    fn counts_multiply_d2w_and_2dw() {
        let mut ed = BodyEditor::new("a b c d e");
        feed(&mut ed, &[k('d'), k('2'), k('w')]);
        assert_eq!(ed.text(), "c d e");
        let mut ed = BodyEditor::new("a b c d e");
        feed(&mut ed, &[k('2'), k('d'), k('w')]);
        assert_eq!(ed.text(), "c d e");
    }

    #[test]
    fn de_is_inclusive() {
        let mut ed = BodyEditor::new("foo bar");
        feed(&mut ed, &[k('d'), k('e')]);
        assert_eq!(ed.text(), " bar");
    }

    #[test]
    fn d_dollar_deletes_to_eol() {
        let mut ed = BodyEditor::new("hello world");
        feed(&mut ed, &[k('l'), k('l'), k('d'), k('$')]);
        assert_eq!(ed.text(), "he");
    }

    #[test]
    fn dd_and_2dd_are_linewise() {
        let mut ed = BodyEditor::new("one\ntwo\nthree");
        feed(&mut ed, &[k('2'), k('d'), k('d')]);
        assert_eq!(ed.text(), "three");
    }

    #[test]
    fn dgg_and_d_cap_g_linewise() {
        let mut ed = BodyEditor::new("a\nb\nc\nd");
        feed(&mut ed, &[k('j'), k('j'), k('d'), k('g'), k('g')]);
        assert_eq!(ed.text(), "d");
        let mut ed = BodyEditor::new("a\nb\nc\nd");
        feed(&mut ed, &[k('j'), k('d'), k('G')]);
        assert_eq!(ed.text(), "a");
    }

    #[test]
    fn cw_acts_like_ce_and_enters_insert() {
        let mut ed = BodyEditor::new("foo bar");
        feed(&mut ed, &[k('c'), k('w')]);
        assert_eq!(ed.text(), " bar"); // trailing space NOT eaten
        assert_eq!(ed.mode, BodyMode::Insert);
    }

    #[test]
    fn cc_clears_line_keeps_row_and_inserts() {
        let mut ed = BodyEditor::new("alpha\nbeta");
        feed(&mut ed, &[k('c'), k('c')]);
        assert_eq!(ed.text(), "\nbeta");
        assert_eq!(ed.mode, BodyMode::Insert);
        assert_eq!(ed.cursor(), (0, 0));
    }

    #[test]
    fn text_object_diw_and_di_quote_and_dip() {
        let mut ed = BodyEditor::new("foo bar baz");
        feed(&mut ed, &[k('w'), k('d'), k('i'), k('w')]);
        assert_eq!(ed.text(), "foo  baz");

        let mut ed = BodyEditor::new("say \"hi\" now");
        feed(&mut ed, &[k('w'), k('w'), k('d'), k('i'), k('"')]);
        assert_eq!(ed.text(), "say \"\" now");

        let mut ed = BodyEditor::new("a\nb\n\nc");
        feed(&mut ed, &[k('d'), k('i'), k('p')]);
        assert_eq!(ed.text(), "\nc");
    }

    #[test]
    fn di_paren_inner() {
        let mut ed = BodyEditor::new("f(abc)g");
        feed(&mut ed, &[k('l'), k('l'), k('d'), k('i'), k('(')]);
        assert_eq!(ed.text(), "f()g");
    }

    #[test]
    fn yw_yanks_without_mutating_and_arms_highlight() {
        let mut ed = BodyEditor::new("foo bar");
        feed(&mut ed, &[k('y'), k('w')]);
        assert_eq!(ed.text(), "foo bar");
        assert_eq!(ed.yank.as_ref().unwrap().text, "foo ");
        assert!(ed.yank_highlight.is_some());
        feed(&mut ed, &[k('$'), k('p')]);
        assert_eq!(ed.text(), "foo barfoo ");
    }

    #[test]
    fn indent_and_dedent() {
        let mut ed = BodyEditor::new("x\ny\nz");
        feed(&mut ed, &[k('>'), k('>')]);
        assert_eq!(ed.text(), "    x\ny\nz");
        feed(&mut ed, &[k('<'), k('<')]);
        assert_eq!(ed.text(), "x\ny\nz");
        // >j indents two lines.
        let mut ed = BodyEditor::new("x\ny\nz");
        feed(&mut ed, &[k('>'), k('j')]);
        assert_eq!(ed.text(), "    x\n    y\nz");
    }

    #[test]
    fn case_operators() {
        let mut ed = BodyEditor::new("Hello World");
        feed(&mut ed, &[k('g'), k('u'), k('u')]);
        assert_eq!(ed.text(), "hello world");
        let mut ed = BodyEditor::new("Hello World");
        feed(&mut ed, &[k('g'), k('U'), k('U')]);
        assert_eq!(ed.text(), "HELLO WORLD");
        let mut ed = BodyEditor::new("Hello");
        feed(&mut ed, &[k('g'), k('~'), k('~')]);
        assert_eq!(ed.text(), "hELLO");
        // guw lowercases one word.
        let mut ed = BodyEditor::new("FOO BAR");
        feed(&mut ed, &[k('g'), k('u'), k('w')]);
        assert_eq!(ed.text(), "foo BAR");
    }

    #[test]
    fn tilde_toggles_and_advances_with_count() {
        let mut ed = BodyEditor::new("abc");
        feed(&mut ed, &[k('~')]);
        assert_eq!(ed.text(), "Abc");
        assert_eq!(ed.cursor(), (0, 1));
        let mut ed = BodyEditor::new("abcd");
        feed(&mut ed, &[k('3'), k('~')]);
        assert_eq!(ed.text(), "ABCd");
    }

    #[test]
    fn gqq_reflows_to_width() {
        let mut ed = BodyEditor::new("one two three four five");
        // width 8 → wrap.
        ed.handle_key(k('g'), 8);
        ed.handle_key(k('q'), 8);
        ed.handle_key(k('q'), 8);
        assert_eq!(ed.text(), "one two\nthree\nfour\nfive");
    }

    #[test]
    fn gq_moves_to_end_gw_restores_cursor() {
        let mut ed = BodyEditor::new("aa bb cc dd");
        ed.handle_key(k('g'), 5);
        ed.handle_key(k('q'), 5);
        ed.handle_key(k('q'), 5);
        // gq parks on the last reflowed line.
        let (last, _) = ed.cursor();
        assert_eq!(last as usize, ed.text().lines().count() - 1);

        let mut ed = BodyEditor::new("aa bb cc dd");
        feed(&mut ed, &[k('l'), k('l'), k('l')]); // cursor col 3
        ed.handle_key(k('g'), 5);
        ed.handle_key(k('w'), 5);
        ed.handle_key(k('w'), 5);
        // gw restores the original row.
        assert_eq!(ed.cursor().0, 0);
    }

    #[test]
    fn count_movement_3w() {
        let mut ed = BodyEditor::new("a b c d e");
        feed(&mut ed, &[k('3'), k('w')]);
        assert_eq!(ed.cursor(), (0, 6)); // start of "d"
    }

    #[test]
    fn visual_text_object_parity_viwd_equals_diw() {
        let mut a = BodyEditor::new("foo bar baz");
        feed(&mut a, &[k('w'), k('d'), k('i'), k('w')]);
        let mut b = BodyEditor::new("foo bar baz");
        feed(&mut b, &[k('w'), k('v'), k('i'), k('w'), k('d')]);
        assert_eq!(a.text(), b.text());
    }

    #[test]
    fn replace_char_and_replace_mode() {
        let mut ed = BodyEditor::new("abc");
        feed(&mut ed, &[k('r'), k('X')]);
        assert_eq!(ed.text(), "Xbc");
        // 2rX replaces two chars.
        let mut ed = BodyEditor::new("abcd");
        feed(&mut ed, &[k('2'), k('r'), k('z')]);
        assert_eq!(ed.text(), "zzcd");
        // R overtype.
        let mut ed = BodyEditor::new("abcd");
        feed(&mut ed, &[k('R'), k('X'), k('Y')]);
        assert_eq!(ed.text(), "XYcd");
        assert_eq!(ed.mode, BodyMode::Replace);
        ed.handle_key(esc(), 72);
        assert_eq!(ed.mode, BodyMode::Normal);
    }

    #[test]
    fn join_lines_with_and_without_space() {
        let mut ed = BodyEditor::new("foo\n  bar");
        feed(&mut ed, &[k('J')]);
        assert_eq!(ed.text(), "foo bar");
        let mut ed = BodyEditor::new("foo\n  bar");
        feed(&mut ed, &[k('g'), k('J')]);
        assert_eq!(ed.text(), "foobar");
        // 3J joins three lines.
        let mut ed = BodyEditor::new("a\nb\nc\nd");
        feed(&mut ed, &[k('3'), k('J')]);
        assert_eq!(ed.text(), "a b c\nd");
    }

    #[test]
    fn s_cap_s_cap_c_cap_d_shortcuts() {
        let mut ed = BodyEditor::new("abc");
        feed(&mut ed, &[k('s')]);
        assert_eq!(ed.text(), "bc");
        assert_eq!(ed.mode, BodyMode::Insert);

        let mut ed = BodyEditor::new("hello world");
        feed(&mut ed, &[k('l'), k('l'), k('D')]);
        assert_eq!(ed.text(), "he");

        let mut ed = BodyEditor::new("hello world");
        feed(&mut ed, &[k('l'), k('l'), k('C')]);
        assert_eq!(ed.text(), "he");
        assert_eq!(ed.mode, BodyMode::Insert);

        let mut ed = BodyEditor::new("alpha\nbeta");
        feed(&mut ed, &[k('S')]);
        assert_eq!(ed.text(), "\nbeta");
        assert_eq!(ed.mode, BodyMode::Insert);
    }

    #[test]
    fn find_char_motion_and_operator() {
        // dt) deletes up to but not including ')'.
        let mut ed = BodyEditor::new("foo(bar)baz");
        feed(&mut ed, &[k('d'), k('t'), k(')')]);
        assert_eq!(ed.text(), ")baz");
        // df) deletes through ')'.
        let mut ed = BodyEditor::new("foo(bar)baz");
        feed(&mut ed, &[k('d'), k('f'), k(')')]);
        assert_eq!(ed.text(), "baz");
        // plain f) moves the cursor onto ')'.
        let mut ed = BodyEditor::new("foo(bar)baz");
        feed(&mut ed, &[k('f'), k(')')]);
        assert_eq!(ed.cursor(), (0, 7));
    }

    #[test]
    fn operator_cancel_redispatches_key() {
        // `dz` is invalid: d cancels, z is a no-op, x then deletes a char.
        let mut ed = BodyEditor::new("abc");
        feed(&mut ed, &[k('d'), k('z'), k('x')]);
        assert_eq!(ed.text(), "bc");
    }

    #[test]
    fn dw_and_diw_are_single_undo() {
        let mut ed = BodyEditor::new("foo bar");
        feed(&mut ed, &[k('d'), k('w'), k('u')]);
        assert_eq!(ed.text(), "foo bar");
        let mut ed = BodyEditor::new("foo bar");
        feed(&mut ed, &[k('d'), k('i'), k('w'), k('u')]);
        assert_eq!(ed.text(), "foo bar");
    }

    #[test]
    fn ge_moves_to_previous_word_end() {
        let mut ed = BodyEditor::new("foo bar baz");
        feed(&mut ed, &[k('$'), k('g'), k('e')]);
        assert_eq!(ed.cursor(), (0, 6)); // end of "bar"
    }

    // ---------- soft-wrap ----------

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    #[test]
    fn char_col_to_byte_ascii_and_multibyte() {
        // ASCII: byte offset == char index.
        assert_eq!(char_col_to_byte("hello", 0), 0);
        assert_eq!(char_col_to_byte("hello", 3), 3);
        // At/past end clamps to the byte length.
        assert_eq!(char_col_to_byte("hello", 5), 5);
        assert_eq!(char_col_to_byte("hello", 99), 5);
        // Multibyte: "é" is 2 bytes, "界" is 3, so columns advance by chars
        // while byte offsets jump by encoded width.
        let s = "aéb界c";
        assert_eq!(char_col_to_byte(s, 0), 0); // a
        assert_eq!(char_col_to_byte(s, 1), 1); // é
        assert_eq!(char_col_to_byte(s, 2), 3); // b
        assert_eq!(char_col_to_byte(s, 3), 4); // 界
        assert_eq!(char_col_to_byte(s, 4), 7); // c
        assert_eq!(char_col_to_byte(s, 5), 8); // end
    }

    /// Render the editor's textarea into a fixed-size test buffer and
    /// return one row's visible text (trailing blanks trimmed).
    fn render_rows(ed: &BodyEditor, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| f.render_widget(&ed.textarea, Rect::new(0, 0, w, h)))
            .unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn long_line_soft_wraps_onto_multiple_rows() {
        // Default wrap (WordOrGlyph): a line wider than the pane spills onto
        // the next visual row instead of scrolling off the right edge.
        let ed = BodyEditor::new("the quick brown fox jumps");
        let rows = render_rows(&ed, 12, 4);
        assert!(rows[0].starts_with("the quick"), "row 0: {:?}", rows[0]);
        assert!(
            !rows[1].is_empty(),
            "long line should continue on row 1, got {rows:?}"
        );
        // No row exceeds the pane width (nothing ran off-screen).
        assert!(rows.iter().all(|r| r.chars().count() <= 12));
    }

    #[test]
    fn wrap_off_keeps_one_logical_row() {
        // With wrap disabled the logical line stays on a single visual row
        // (horizontal scroll), so row 1 is blank.
        let mut ed = BodyEditor::new("the quick brown fox jumps");
        ed.set_wrap(ComposeWrap::Off);
        let rows = render_rows(&ed, 12, 4);
        assert!(rows[1].is_empty(), "no wrap → row 1 empty, got {rows:?}");
    }

    #[test]
    fn cursor_style_tracks_mode() {
        let mut ed = BodyEditor::new("hello world");
        // Normal: REVERSED block.
        assert!(
            ed.textarea
                .cursor_style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        // Insert: UNDERLINED.
        feed(&mut ed, &[k('i')]);
        assert!(
            ed.textarea
                .cursor_style()
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        // Back to Normal.
        feed(&mut ed, &[esc()]);
        assert!(
            ed.textarea
                .cursor_style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn block_selection_registers_reversed_overlay() {
        // Ctrl-V block-select two columns on row 0, then ensure the overlay
        // renders as REVERSED cells via custom_highlight (not a bespoke
        // painter). "ab" at cols 0..=1.
        let mut ed = BodyEditor::new("abcd\nefgh");
        feed(&mut ed, &[ctrl('v'), k('j'), k('l')]);
        ed.apply_overlays();
        let mut term = Terminal::new(TestBackend::new(8, 4)).unwrap();
        term.draw(|f| f.render_widget(&ed.textarea, Rect::new(0, 0, 8, 4)))
            .unwrap();
        let buf = term.backend().buffer().clone();
        // Column 0 of both selected rows should carry REVERSED.
        for y in 0..=1u16 {
            let m = buf.cell((0, y)).unwrap().modifier;
            assert!(
                m.contains(Modifier::REVERSED),
                "cell (0,{y}) should be REVERSED, modifier = {m:?}"
            );
        }
    }
}
