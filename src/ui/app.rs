use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui_image::picker::Picker;

use crate::config::{Account, Config};
use crate::mail::compose::SendOutcome;
use crate::mail::flags::{self, FlagOp};
use crate::mail::html::{self, Block};
use crate::mail::parse;
use crate::store::AccountSpec;
use crate::store::index::{FolderStat, Index, MessageRow};
use crate::store::scan::{self, AccountFolderStats, ScanResult};
use crate::store::sync::SyncResult;
use crate::store::thread::{ThreadedRow, build_threads};
use crate::store::watch::{self, SelfWrites, Watcher, WatcherConfig, WatcherEvent};
use crate::ui::clipboard::ClipboardResult;
use crate::ui::compose::{ComposeScreen, ComposeStatus};
use crate::ui::events::AppEvent;
use crate::ui::images::{self, ImageKey, ResolvedImage};
use crate::ui::search::SearchState;
use crate::ui::text_input::TextInput;
use crate::ui::{cmdline, compose, folders, list, reader, tabs};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Folders,
    List,
    Reader,
}

/// Modal layer. `Normal` is the ambient navigation mode — keys are routed
/// by pane focus (List vs Reader), not by a separate Reader sub-mode.
/// `Command`, `LinkPick`, and `Search` exist because they capture text /
/// digit input. `Search`'s local-vs-global flavour lives on
/// `InboxScreen.search.kind` so the mode enum stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Command,
    LinkPick,
    Search,
    /// Reader-pane vim-style visual mode. The kind (char-wise vs
    /// line-wise) and the anchor live on `InboxScreen.visual`; the mode
    /// variant only carries the dispatch flag. Strict pairing: when
    /// `Mode::Visual` is set, `InboxScreen.visual` is `Some`, and vice
    /// versa. Entry/exit go through `enter_visual` / `exit_visual` so the
    /// pair stays consistent.
    Visual,
}

/// Vim-style visual sub-kind. Char-wise (`v`) extends cell-by-cell; line-wise
/// (`V`) snaps both anchor and cursor to whole rendered lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualKind {
    Char,
    Line,
}

/// Anchor pin for visual mode. Cursor coords live on `InboxScreen`
/// (`reader_cursor_line`, `reader_cursor_col`); selection runs from
/// anchor → cursor. Both endpoints index into `LaidOutBody.line_text`,
/// i.e. the body-relative coord space (no header rows).
#[derive(Debug, Clone, Copy)]
pub struct VisualState {
    pub kind: VisualKind,
    pub anchor_line: u16,
    pub anchor_col: u16,
}

#[derive(Debug, Clone)]
pub struct ParsedBody {
    // Carried so a future invalidation pass can sanity-check the cache
    // against the selected row; `last_parsed_msgid` on the inbox is what
    // currently drives re-parsing.
    #[allow(dead_code)]
    pub msgid: String,
    pub blocks: Vec<Block>,
    pub raw_html: Option<String>,
    pub plain_fallback: Option<String>,
    pub cid_parts: HashMap<String, Vec<u8>>,
}

#[derive(Debug)]
pub enum ScanState {
    Scanning,
    Ready(Vec<ThreadedRow>),
    Failed(String),
}

/// Top-level UI state. Holds the modal layer, cmdline buffer, status row,
/// and the list of screens (Step 1 has only the inbox; tabs land in Step 2).
/// App also owns process-global resources screens consult — the image
/// picker, the sqlite cache path, the self-writes registry — passed by
/// reference into per-screen operations rather than duplicated per screen.
pub struct App {
    pub mode: Mode,
    pub cmdline: TextInput,
    pub link_pick_buf: String,
    /// Pending `g` prefix in Normal mode. The user has typed `g`; the
    /// next key decides what it composes — today only `/` for global
    /// search, future room for `gg`/`G` etc.
    pub pending_g: bool,
    /// Pending `y` prefix in Normal mode (Reader focus only). Mirrors
    /// `pending_g`: `y` arms it; the next key decides — `p` for paragraph,
    /// `l` for link. Any other key clears it and falls through.
    pub pending_y: bool,
    /// Transient status / error displayed in the cmdline row. Cleared
    /// when the user enters a new command or moves selection.
    pub status_error: Option<String>,
    pub quit: bool,
    pub screens: Vec<Screen>,
    /// Index into `screens` of the currently-displayed tab. Always 0 in
    /// Step 1 (inbox is the only screen); Step 2 introduces tab switching.
    #[allow(dead_code)]
    pub active: usize,
    /// Capability picker; `None` when `[images].protocol = "off"` or
    /// stdio isn't a tty. When None the reader always renders the
    /// `[image: alt]` placeholder.
    pub picker: Option<Picker>,
    /// Capped at max_height_cells from `[images]`. Surfaced to the reader
    /// so layout caps reservation height the same way the decode does.
    pub image_max_height_cells: u16,
    /// SQLite cache path. Kept so flag flips can briefly re-open the
    /// index and mirror the new `path` / `flags` without holding a
    /// long-lived `Connection` on the UI thread.
    pub cache_path: PathBuf,
    /// Self-write registry the future Step 7 notify watcher will consult
    /// to suppress its own rename events. Populated whenever a flag flip
    /// renames a maildir file under us.
    pub self_writes: SelfWrites,
    /// In-flight `:sync` worker. `Some` while the configured
    /// `[sync].command` is running; cleared on completion. A second
    /// `:sync` while one is in flight errors out rather than queueing.
    pub sync_rx: Option<Receiver<SyncResult>>,
    /// In-flight clipboard-fallback worker. `Some` while a yank pipe is
    /// running; cleared on completion. Concurrent yanks pile up
    /// receivers and the most recent one wins for status display —
    /// fine because real fallback commands return in milliseconds.
    pub clipboard_rx: Option<Receiver<ClipboardResult>>,
    /// Clone of the unified event channel so the sync worker (and any
    /// future workers spawned from cmdline dispatch) can push
    /// `AppEvent::Wake` events. `None` in tests where no event loop is
    /// running.
    pub event_tx: Option<Sender<AppEvent>>,
}

/// A user-visible screen with its own tab in the strip and its own
/// per-screen state.
pub enum Screen {
    Inbox(InboxScreen),
    Compose(ComposeScreen),
}

/// Per-screen state for the maildir reader: pane visibility/focus, scan
/// worker channel + parsed rows, current selection, parsed body cache,
/// decoded inline images. Everything that used to live on `App` and
/// described "the inbox view" lives here.
pub struct InboxScreen {
    pub focus: Pane,
    pub sidebar_visible: bool,
    pub list_visible: bool,
    pub reader_visible: bool,
    pub reader_scroll: u16,
    /// Body-relative cursor used by reader yanks (`yp` / `yl`). Indexes
    /// into `LaidOutBody.lines` (i.e. excludes the header rows the
    /// reader prepends). Invariant for the current session: clamped
    /// into the visible viewport on every render, so it effectively
    /// tracks "topmost visible body line" until visual mode adds
    /// independent movement.
    pub reader_cursor_line: u16,
    /// Body-relative cursor column. Char index into
    /// `LaidOutBody.line_text[reader_cursor_line]`. For ASCII this maps
    /// 1:1 with display columns; CJK / emoji breaks the parity but the
    /// reader's existing layout already treats one char as one cell
    /// (see `LineBuilder::line_width`), so visual selection stays
    /// internally consistent.
    pub reader_cursor_col: u16,
    /// Vim-style visual-mode anchor. `Some` iff `Mode::Visual` is the
    /// active mode (strict pairing — see `Mode::Visual` doc). Char-wise
    /// vs line-wise lives on `kind`; the cursor end of the selection is
    /// `(reader_cursor_line, reader_cursor_col)`.
    pub visual: Option<VisualState>,
    /// Header-row count (From/Subject/Folder/Flags + the blank
    /// separator) from the last reader draw. Stashed so the yank
    /// helpers can translate the absolute viewport scroll into body
    /// coordinates without recomputing the header block.
    pub last_reader_header_offset: u16,
    /// Body-line count (excluding headers) from the last reader draw.
    /// Used to clamp `reader_cursor_line` within the body's actual
    /// range.
    pub last_reader_body_only_lines: u16,
    /// Inner reader-pane width (after border) from the last draw.
    /// Stashed so yank helpers can re-run layout at the same width
    /// the cursor was clamped against — otherwise `line_block_idx`
    /// indexing would drift when the keymap doesn't know the live
    /// pane width.
    pub last_reader_inner_width: u16,
    /// Inner reader-pane rect (after the pane border) from the last
    /// frame the reader actually drew. `None` when the reader pane is
    /// hidden this frame. Used by the mouse handler to translate
    /// terminal-cell coordinates into body-relative (line, col) for
    /// drag-selection.
    pub last_reader_inner: Option<Rect>,
    /// Anchor for an in-progress mouse drag in the reader. Set on
    /// `MouseDown(Left)` inside the reader's inner area; promoted to a
    /// visual-char selection on the first `Drag`; cleared on `Up`.
    /// `None` between gestures. Body-relative (line, col).
    pub mouse_drag_anchor: Option<(u16, u16)>,
    pub scan: ScanState,
    pub selected: usize,
    /// Boxed so its size doesn't bloat `Screen::Inbox` past the
    /// `large_enum_variant` threshold (same reason `search` is boxed).
    pub parsed: Option<Box<ParsedBody>>,
    /// Last-known msgid we tried to parse a body for, so a parse failure
    /// doesn't loop forever.
    last_parsed_msgid: Option<String>,
    /// Decoded images per message body, keyed by msgid so navigating
    /// back-and-forth between two threads doesn't re-decode each side.
    /// Pruned to {current, previous} on each msgid change.
    image_cache: HashMap<String, HashMap<ImageKey, ResolvedImage>>,
    /// Previous msgid kept on the cache for cheap back-and-forth.
    prev_parsed_msgid: Option<String>,
    /// Image rects drawn by the reader on the previous frame. The
    /// reader Clears these next frame when the body changes so kitty /
    /// iTerm2 placements don't ghost across messages.
    pub last_image_rects: Vec<Rect>,
    /// Body-line count from the previous reader draw, used by `G` to
    /// pick a bottom-scroll position without re-running layout in the
    /// keymap. A lower bound (counts pre-wrap `Line`s, so heavy wrap
    /// undershoots — the user can `j` from there).
    pub last_reader_body_lines: u16,
    /// Inner reader-pane height (after border) from the previous draw.
    /// Pairs with `last_reader_body_lines` for the `G` calc.
    pub last_reader_inner_height: u16,
    /// Set by `ensure_body` when the body changed this tick; reader
    /// consumes it to drive the Clear pass.
    pub body_changed_this_tick: bool,
    /// Per-scope, per-folder (total, unread) roll-up surfaced to the
    /// sidebar. The first group is always the unified "[all]" view
    /// (`scope = None`); subsequent groups are one per configured account
    /// in alphabetical order. Populated from the scan result and patched
    /// locally on flag flips and cross-folder moves so the counts stay
    /// live without a re-scan.
    pub folder_stats: Vec<AccountFolderStats>,
    /// Which account the list pane is currently filtered by. `None` =
    /// "[all]" (unified across accounts, today's default); `Some(name)`
    /// scopes the list to that account.
    pub current_account: Option<String>,
    /// Which folder the list pane is currently rendering. `INBOX` is
    /// the default; `Alt-j`/`Alt-k` (and `j`/`k` in the Folders pane)
    /// walk the flat sidebar entries (`[all]` group first, then one
    /// `[account]` group per account) and re-fetch from the index.
    pub current_folder: String,
    scan_rx: Option<Receiver<ScanResult>>,
    /// `notify`-backed maildir watcher. `None` when `[watch].enabled =
    /// false`, no accounts are configured, or `notify::Watcher::new`
    /// failed (degraded mode: startup full rescan only, no live updates).
    /// Lives on `InboxScreen` so its `Drop` releases inotify FDs when
    /// the screen goes away.
    watcher: Option<Watcher>,
    /// Receiver half of the watcher's flush channel. Drained each tick
    /// into `pending_dirty`.
    watch_rx: Option<Receiver<WatcherEvent>>,
    /// Folder keys waiting to be rescanned. Accumulates while a
    /// rescan is in flight so we never overlap two rescans; the next
    /// `poll_watch` after the in-flight rescan completes fires one
    /// combined rescan over the union.
    pending_dirty: HashSet<(String, String)>,
    /// Receiver for the per-folder rescan worker kicked from
    /// `poll_watch`. Separate from `scan_rx` so the startup full scan
    /// and watcher-driven rescans don't fight over the same channel.
    rescan_rx: Option<Receiver<ScanResult>>,
    /// The dirty set the in-flight `rescan_rx` covers, so
    /// `apply_rescan` knows whether the current list view was
    /// re-walked and needs its rows replaced.
    rescan_in_flight: HashSet<(String, String)>,
    /// One-shot warning surfaced to the cmdline status row when the
    /// watcher failed to start (commonly `fs.inotify.max_user_watches`
    /// exhausted). `None` once consumed by `App::new`.
    pub watcher_warning: Option<String>,
    /// Active `/` or `g/` search. While `Some`, the list pane renders
    /// `search.results` (flat, no threading) instead of `scan.threads`,
    /// and the selection / flag / move ops operate against the search
    /// row. `None` is the ambient inbox view. Boxed so the heavy
    /// haystack doesn't bloat `Screen::Inbox` (clippy `large_enum_variant`).
    pub search: Option<Box<SearchState>>,
}

impl App {
    pub fn new(
        cfg: &Config,
        cache_path: PathBuf,
        picker: Option<Picker>,
        event_tx: Option<Sender<AppEvent>>,
    ) -> Self {
        let self_writes = SelfWrites::new();
        let inbox = InboxScreen::new(cfg, &cache_path, self_writes.clone(), event_tx.clone());
        let watcher_warning = inbox.watcher_warning.clone();
        Self {
            mode: Mode::Normal,
            cmdline: TextInput::new(),
            link_pick_buf: String::new(),
            pending_g: false,
            pending_y: false,
            status_error: watcher_warning,
            quit: false,
            screens: vec![Screen::Inbox(inbox)],
            active: 0,
            picker,
            image_max_height_cells: cfg.images.max_height_cells,
            cache_path,
            self_writes,
            sync_rx: None,
            clipboard_rx: None,
            event_tx,
        }
    }

    /// Borrow the inbox screen. The inbox is always pinned at index 0
    /// (other tabs are compose tabs); the helper centralizes that
    /// assumption so the rest of the UI doesn't open-code the match.
    pub fn inbox(&self) -> &InboxScreen {
        match &self.screens[0] {
            Screen::Inbox(s) => s,
            Screen::Compose(_) => unreachable!("inbox is pinned at index 0"),
        }
    }

    pub fn inbox_mut(&mut self) -> &mut InboxScreen {
        match &mut self.screens[0] {
            Screen::Inbox(s) => s,
            Screen::Compose(_) => unreachable!("inbox is pinned at index 0"),
        }
    }

    /// Push a new compose tab, mark it active, return its index.
    pub fn open_compose(&mut self, screen: ComposeScreen) -> usize {
        self.screens.push(Screen::Compose(screen));
        let idx = self.screens.len() - 1;
        self.active = idx;
        idx
    }

    /// Close the currently active tab unless it's the inbox (index 0).
    /// Returns Ok(()) on close, Err(msg) when blocked.
    pub fn close_active_tab(&mut self) -> Result<(), &'static str> {
        if self.active == 0 {
            return Err("cannot close the inbox tab");
        }
        let idx = self.active;
        self.screens.remove(idx);
        if self.active >= self.screens.len() {
            self.active = self.screens.len() - 1;
        }
        Ok(())
    }

    /// Borrow the currently-active compose screen, if any.
    pub fn active_compose_mut(&mut self) -> Option<&mut ComposeScreen> {
        match self.screens.get_mut(self.active)? {
            Screen::Compose(s) => Some(s),
            _ => None,
        }
    }

    /// Drain any pending send-worker result on each compose tab.
    /// Transitions `ComposeStatus` and surfaces a one-shot message to
    /// the cmdline status row. Mirrors the `poll_scan` pattern.
    pub fn poll_compose_sends(&mut self) {
        let Self {
            screens,
            status_error,
            ..
        } = self;
        for screen in screens.iter_mut() {
            let Screen::Compose(c) = screen else { continue };
            let Some(rx) = c.send_rx.as_ref() else {
                continue;
            };
            match rx.try_recv() {
                Ok(Ok(SendOutcome::Sent)) => {
                    c.status = ComposeStatus::Sent;
                    c.send_rx = None;
                    *status_error = Some("sent".into());
                }
                Ok(Ok(SendOutcome::SentNoCopy(msg))) => {
                    c.status = ComposeStatus::Sent;
                    c.send_rx = None;
                    *status_error = Some(format!("sent (no Sent copy: {msg})"));
                }
                Ok(Err(e)) => {
                    c.status = ComposeStatus::Failed(e.clone());
                    c.send_rx = None;
                    *status_error = Some(format!("send failed: {e}"));
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    c.status = ComposeStatus::Failed("send worker died".into());
                    c.send_rx = None;
                    *status_error = Some("send: worker died".into());
                }
            }
        }
    }

    /// Drain the in-flight `:sync` worker. Mirrors `poll_compose_sends`:
    /// on completion clears `sync_rx` and writes a one-shot message to
    /// the cmdline status row. The maildir watcher reconciles the new
    /// files separately; success here only means the sync command
    /// exited cleanly.
    pub fn poll_sync(&mut self) {
        let Some(rx) = self.sync_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.sync_rx = None;
                self.status_error = Some("synced".into());
            }
            Ok(Err(e)) => {
                self.sync_rx = None;
                self.status_error = Some(format!("sync failed: {e}"));
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.sync_rx = None;
                self.status_error = Some("sync: worker died".into());
            }
        }
    }

    /// Drain the in-flight clipboard fallback worker (when a yank chose
    /// the shell-out path). Mirrors `poll_sync`: on completion clears
    /// `clipboard_rx` and writes a one-shot message to the cmdline row.
    pub fn poll_clipboard(&mut self) {
        let Some(rx) = self.clipboard_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(bytes)) => {
                self.clipboard_rx = None;
                self.status_error = Some(format!("yanked {bytes} bytes"));
            }
            Ok(Err(e)) => {
                self.clipboard_rx = None;
                self.status_error = Some(format!("clipboard failed: {e}"));
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.clipboard_rx = None;
                self.status_error = Some("clipboard: worker died".into());
            }
        }
    }

    /// Cmdline / status helpers that reach into the inbox screen — kept
    /// on App so `cmdline::dispatch` doesn't have to know about screens.
    pub fn inbox_parsed(&self) -> Option<&ParsedBody> {
        self.inbox().parsed.as_deref()
    }

    pub fn poll_scan(&mut self) {
        self.inbox_mut().poll_scan();
    }

    /// Drain the inotify watcher channel into `pending_dirty`, harvest
    /// any completed per-folder rescan, and kick a fresh rescan when
    /// there's work and no in-flight worker. Called every main-loop
    /// tick alongside `poll_scan`.
    pub fn poll_watch(&mut self, cfg: &Config) {
        let Self {
            screens,
            cache_path,
            status_error,
            ..
        } = self;
        let Some(Screen::Inbox(inbox)) = screens.get_mut(0) else {
            unreachable!("inbox is pinned at index 0")
        };
        inbox.poll_watch(cfg, cache_path, status_error);
    }

    /// Tab navigation. With only the inbox screen in Step 1 / Step 2,
    /// cycling is a no-op; the wiring is in place so Step 8's compose
    /// tabs participate without further keymap changes.
    pub fn next_tab(&mut self) {
        if self.screens.len() > 1 {
            self.active = (self.active + 1) % self.screens.len();
        }
    }

    pub fn prev_tab(&mut self) {
        let n = self.screens.len();
        if n > 1 {
            self.active = (self.active + n - 1) % n;
        }
    }

    pub fn set_tab(&mut self, idx: usize) {
        if idx < self.screens.len() {
            self.active = idx;
        }
    }

    /// Re-reads and parses the body for the currently-selected message
    /// when it differs from the cached body. Splits self so the inbox
    /// can borrow App-global resources (picker / cache path / self-writes
    /// registry) and write into `status_error` simultaneously.
    pub fn ensure_body_for_selection(&mut self) {
        let Self {
            screens,
            picker,
            image_max_height_cells,
            cache_path,
            self_writes,
            status_error,
            ..
        } = self;
        let Some(Screen::Inbox(inbox)) = screens.get_mut(0) else {
            unreachable!("inbox is pinned at index 0")
        };
        inbox.ensure_body(
            picker.as_ref(),
            *image_max_height_cells,
            cache_path,
            self_writes,
            status_error,
        );
    }

    /// Toggle a maildir flag (`S` / `F` / `T`) on the selected row.
    /// Drives the user-facing flag bindings: `m` → Seen, `*` → Flagged,
    /// `d` → Trashed.
    pub fn toggle_flag_selected(&mut self, flag: char) {
        let Self {
            screens,
            cache_path,
            self_writes,
            status_error,
            ..
        } = self;
        let Some(Screen::Inbox(inbox)) = screens.get_mut(0) else {
            unreachable!("inbox is pinned at index 0")
        };
        inbox.toggle_flag(flag, cache_path, self_writes, status_error);
    }

    /// Move the selected row's message file into `target_folder` of the
    /// owning account. Drives `:archive` / `:spam` / `:trash` / `:mv`.
    /// The folder name is the Maildir++ label (e.g. `"Archive"`), not the
    /// on-disk `.Archive` directory. The target maildir is created if
    /// missing.
    pub fn move_selected_to(&mut self, target_folder: &str, cfg: &Config) {
        let Self {
            screens,
            cache_path,
            self_writes,
            status_error,
            ..
        } = self;
        let Some(Screen::Inbox(inbox)) = screens.get_mut(0) else {
            unreachable!("inbox is pinned at index 0")
        };
        inbox.move_selected_to(
            target_folder,
            &cfg.accounts,
            cache_path,
            self_writes,
            status_error,
        );
    }

    // --- Convenience pass-throughs so keys.rs doesn't reach into inbox
    // for every keystroke. Step 1: all key dispatch is inbox-only. ---

    pub fn cycle_focus(&mut self, forward: bool) {
        self.inbox_mut().cycle_focus(forward);
    }

    /// Enter `/` (local) search: snapshot the current scope's rows from
    /// the index, install a fresh `SearchState`, switch focus to List,
    /// flip `Mode::Search`. Failure to open the index surfaces in the
    /// cmdline status row and leaves search inactive.
    pub fn enter_search_local(&mut self) {
        let Self {
            screens,
            cache_path,
            status_error,
            mode,
            ..
        } = self;
        let Some(Screen::Inbox(inbox)) = screens.get_mut(0) else {
            unreachable!("inbox is pinned at index 0")
        };
        if inbox.enter_search_local(cache_path, status_error) {
            *mode = Mode::Search;
        }
    }

    /// Enter `g/` (global) search using `[search].global_folders`
    /// (or every folder when the list is empty/unset), filtered to the
    /// currently-selected account scope.
    pub fn enter_search_global(&mut self, cfg: &Config) {
        let Self {
            screens,
            cache_path,
            status_error,
            mode,
            ..
        } = self;
        let Some(Screen::Inbox(inbox)) = screens.get_mut(0) else {
            unreachable!("inbox is pinned at index 0")
        };
        if inbox.enter_search_global(&cfg.search.global_folders, cache_path, status_error) {
            *mode = Mode::Search;
        }
    }

    /// Exit search via `Esc`: drop results, restore the cursor to the
    /// pre-search msgid when possible, return to Normal.
    pub fn exit_search_cancel(&mut self) {
        self.inbox_mut().exit_search_cancel();
        self.mode = Mode::Normal;
    }

    /// Exit search via `Enter`: keep results active in the list pane,
    /// return to Normal, and focus the Reader pane when visible so a
    /// single Enter takes the user from search field straight into the
    /// message body (vs. committing here and requiring a second Enter
    /// for the List→Reader focus shift). When the reader is hidden,
    /// focus stays on List. `Esc` from Reader → List → clears search,
    /// matching the rest of the keymap.
    pub fn exit_search_commit(&mut self) {
        self.mode = Mode::Normal;
        let inbox = self.inbox_mut();
        if inbox.reader_visible {
            inbox.focus = Pane::Reader;
        }
    }

    /// Drop active search (post-commit Esc, or any sidebar scope change).
    pub fn clear_search(&mut self) {
        self.inbox_mut().clear_search();
    }

    /// Switch the inbox to render `(account, folder)`. `account = None`
    /// is the unified `[all]` view. Drives `:account` from cmdline.
    pub fn switch_to_scope(&mut self, account: Option<String>, folder: &str) {
        let Self {
            screens,
            cache_path,
            status_error,
            ..
        } = self;
        let Some(Screen::Inbox(inbox)) = screens.get_mut(0) else {
            unreachable!("inbox is pinned at index 0")
        };
        inbox.switch_to_scope(account, folder, cache_path, status_error);
    }

    /// Enter visual mode at the current cursor position. Pairs
    /// `Mode::Visual` with `InboxScreen.visual = Some(_)` — see the
    /// invariant note on `Mode::Visual`. No-op when the Reader pane
    /// isn't focused (visual on a list / sidebar pane is undefined).
    pub fn enter_visual(&mut self, kind: VisualKind) {
        let inbox = self.inbox_mut();
        if inbox.focus != Pane::Reader {
            return;
        }
        inbox.visual = Some(VisualState {
            kind,
            anchor_line: inbox.reader_cursor_line,
            anchor_col: inbox.reader_cursor_col,
        });
        self.mode = Mode::Visual;
    }

    /// Leave visual mode without yanking. Clears the anchor and flips
    /// back to `Mode::Normal`; the cursor stays where it is so a
    /// follow-up `yp` lands on the same block.
    pub fn exit_visual(&mut self) {
        self.inbox_mut().visual = None;
        self.mode = Mode::Normal;
    }

    pub fn focus_left(&mut self) {
        self.inbox_mut().focus_left();
    }

    pub fn focus_right(&mut self) {
        self.inbox_mut().focus_right();
    }

    pub fn focus_up(&mut self) {
        self.inbox_mut().focus_up();
    }

    pub fn focus_down(&mut self) {
        self.inbox_mut().focus_down();
    }

    pub fn select_next(&mut self) {
        self.inbox_mut().select_next();
    }

    pub fn select_prev(&mut self) {
        self.inbox_mut().select_prev();
    }

    /// Advance / retreat through the sidebar's flat list of selectable
    /// `(scope, folder)` entries, skipping non-selectable group headers
    /// (`[all]`, `[<account>]`). Order mirrors what `folders::draw`
    /// renders: `[all]` group first, then accounts alphabetically; within
    /// each group INBOX is pinned first, the rest alphabetical. Wraps at
    /// the ends. No-op when no folders are known yet.
    pub fn cycle_folder(&mut self, forward: bool) {
        let Self {
            screens,
            cache_path,
            status_error,
            ..
        } = self;
        let Some(Screen::Inbox(inbox)) = screens.get_mut(0) else {
            unreachable!("inbox is pinned at index 0")
        };
        let order = crate::ui::folders::selectable_entries(&inbox.folder_stats);
        if order.is_empty() {
            return;
        }
        let n = order.len();
        let current_idx = order
            .iter()
            .position(|(scope, folder)| {
                scope.as_deref() == inbox.current_account.as_deref()
                    && folder == &inbox.current_folder
            })
            .unwrap_or(0);
        let next_idx = if forward {
            (current_idx + 1) % n
        } else {
            (current_idx + n - 1) % n
        };
        let (target_scope, target_folder) = order[next_idx].clone();
        inbox.switch_to_scope(target_scope, &target_folder, cache_path, status_error);
    }
}

impl InboxScreen {
    pub fn new(
        cfg: &Config,
        cache_path: &Path,
        self_writes: SelfWrites,
        event_tx: Option<Sender<AppEvent>>,
    ) -> Self {
        let sidebar_visible = cfg.ui.sidebar;
        let list_visible = cfg.ui.list;
        let reader_visible = cfg.ui.reader;
        let focus = initial_focus(sidebar_visible, list_visible, reader_visible);

        let accounts: Vec<AccountSpec> = cfg
            .accounts
            .iter()
            .map(|(name, a)| AccountSpec {
                name: name.clone(),
                root: a.maildir.clone(),
                layout: a.layout,
            })
            .collect();
        let scan_rx = if accounts.is_empty() {
            None
        } else {
            Some(scan::start_worker(
                accounts.clone(),
                cache_path.to_path_buf(),
                (None, "INBOX".to_string()),
            ))
        };
        let scan = if scan_rx.is_some() {
            ScanState::Scanning
        } else {
            ScanState::Ready(Vec::new())
        };

        // Start the inotify watcher when the user hasn't disabled it,
        // accounts are configured, and an event channel is available
        // (in tests no event channel is plumbed in). On failure surface
        // a one-shot warning and degrade to "no live updates" rather
        // than crashing — startup full rescan already ran.
        let mut watcher: Option<Watcher> = None;
        let mut watch_rx: Option<Receiver<WatcherEvent>> = None;
        let mut watcher_warning: Option<String> = None;
        if cfg.watch.enabled
            && !accounts.is_empty()
            && let Some(tx) = event_tx
        {
            let wcfg = WatcherConfig {
                debounce: Duration::from_millis(cfg.watch.debounce_ms),
            };
            match watch::start(&accounts, self_writes, wcfg, tx) {
                Ok((w, rx)) => {
                    watcher = Some(w);
                    watch_rx = Some(rx);
                }
                Err(e) => {
                    watcher_warning = Some(format!("watcher disabled: {e:#}"));
                }
            }
        }

        Self {
            focus,
            sidebar_visible,
            list_visible,
            reader_visible,
            reader_scroll: 0,
            reader_cursor_line: 0,
            reader_cursor_col: 0,
            visual: None,
            last_reader_header_offset: 0,
            last_reader_body_only_lines: 0,
            last_reader_inner_width: 0,
            last_reader_inner: None,
            mouse_drag_anchor: None,
            scan,
            selected: 0,
            parsed: None,
            last_parsed_msgid: None,
            image_cache: HashMap::new(),
            prev_parsed_msgid: None,
            last_image_rects: Vec::new(),
            last_reader_body_lines: 0,
            last_reader_inner_height: 0,
            body_changed_this_tick: false,
            folder_stats: Vec::new(),
            current_account: None,
            current_folder: "INBOX".to_string(),
            scan_rx,
            watcher,
            watch_rx,
            pending_dirty: HashSet::new(),
            rescan_rx: None,
            rescan_in_flight: HashSet::new(),
            watcher_warning,
            search: None,
        }
    }

    pub fn resolved_image(&self, key: &ImageKey) -> Option<&ResolvedImage> {
        let msgid = self.last_parsed_msgid.as_deref()?;
        self.image_cache.get(msgid)?.get(key)
    }

    /// Swap the active visual-mode kind in place. Preserves anchor and
    /// cursor; the next render redraws the highlight in the new shape.
    /// No-op when not in visual mode.
    pub fn set_visual_kind(&mut self, kind: VisualKind) {
        if let Some(v) = self.visual.as_mut() {
            v.kind = kind;
        }
    }

    /// Move the body-relative cursor by `(dy, dx)`. Lines clamp into
    /// `[0, last_reader_body_only_lines)` immediately; columns are
    /// allowed to overshoot — `reader::draw` clamps them to the real
    /// line length once `LaidOutBody.line_text` is in hand. After the
    /// move, scrolls the reader to bring the cursor back into view.
    pub fn move_reader_cursor(&mut self, dy: i32, dx: i32) {
        if self.last_reader_body_only_lines == 0 {
            return;
        }
        let max_line = self.last_reader_body_only_lines.saturating_sub(1) as i32;
        let new_line = (self.reader_cursor_line as i32 + dy).clamp(0, max_line) as u16;
        // Columns clamp at the floor only; the ceiling depends on
        // line_text, which the keymap doesn't have. Draw-time clamp
        // brings overshoots back in. Saturate add so u16::MAX + N
        // doesn't wrap to zero.
        let new_col = if dx >= 0 {
            self.reader_cursor_col.saturating_add(dx as u16)
        } else {
            self.reader_cursor_col
                .saturating_sub((-dx).min(u16::MAX as i32) as u16)
        };
        self.reader_cursor_line = new_line;
        self.reader_cursor_col = new_col;
        self.follow_cursor();
    }

    pub fn move_reader_cursor_to_top(&mut self) {
        self.reader_cursor_line = 0;
        self.reader_cursor_col = 0;
        self.follow_cursor();
    }

    pub fn move_reader_cursor_to_bottom(&mut self) {
        let max_line = self.last_reader_body_only_lines.saturating_sub(1);
        self.reader_cursor_line = max_line;
        self.reader_cursor_col = u16::MAX;
        self.follow_cursor();
    }

    pub fn move_reader_cursor_to_line_start(&mut self) {
        self.reader_cursor_col = 0;
    }

    pub fn move_reader_cursor_to_line_end(&mut self) {
        // Sentinel: clamped to real line end at draw time.
        self.reader_cursor_col = u16::MAX;
    }

    /// Adjust `reader_scroll` so the cursor sits inside the body
    /// viewport. Pure scroll-follow — does not move the cursor.
    pub fn follow_cursor(&mut self) {
        let inner_h = self.last_reader_inner_height;
        let header = self.last_reader_header_offset;
        let abs_line = header.saturating_add(self.reader_cursor_line);
        if abs_line < self.reader_scroll {
            self.reader_scroll = abs_line;
        } else if inner_h > 0 && abs_line >= self.reader_scroll.saturating_add(inner_h) {
            self.reader_scroll = abs_line.saturating_add(1).saturating_sub(inner_h);
        }
    }

    /// Re-read and re-parse the body for the currently-selected message
    /// when it differs from the cached one. Parse failures surface in
    /// `status_error` and leave `parsed = None` without retrying.
    /// On success also decodes every reachable `cid:` / `data:` image
    /// into `self.image_cache[msgid]`; decode failures are listed in
    /// `status_error` but don't block the rest of the body.
    pub fn ensure_body(
        &mut self,
        picker: Option<&Picker>,
        max_height: u16,
        cache_path: &Path,
        self_writes: &SelfWrites,
        status_error: &mut Option<String>,
    ) {
        self.body_changed_this_tick = false;
        let Some(msgid) = self.selected_msgid() else {
            self.parsed = None;
            self.last_parsed_msgid = None;
            return;
        };
        if self.last_parsed_msgid.as_deref() == Some(msgid.as_str()) {
            return;
        }
        let old_msgid = self.last_parsed_msgid.take();
        self.last_parsed_msgid = Some(msgid.clone());
        self.body_changed_this_tick = true;
        // New body means the old cursor position is meaningless — reset
        // so `yp`/`yl` operate on the new message's first content.
        // `reader_scroll` is intentionally left alone here: the
        // selection-change flow elsewhere already preserves scroll
        // semantics, and changing that is out of scope for the yank
        // work. Visual mode anchors against the old body's coords too,
        // so drop it.
        self.reader_cursor_line = 0;
        self.reader_cursor_col = 0;
        self.visual = None;
        self.mouse_drag_anchor = None;
        let Some(path) = self.selected_path() else {
            self.parsed = None;
            self.evict_image_cache(old_msgid.as_deref(), &msgid);
            return;
        };
        match parse::read_body(&path) {
            Ok(body) => {
                let blocks = body.html.as_deref().map(html::parse).unwrap_or_default();
                self.decode_images(
                    &msgid,
                    &blocks,
                    &body.cid_parts,
                    picker,
                    max_height,
                    status_error,
                );
                self.parsed = Some(Box::new(ParsedBody {
                    msgid: msgid.clone(),
                    blocks,
                    raw_html: body.html,
                    plain_fallback: body.plain,
                    cid_parts: body.cid_parts,
                }));
                self.evict_image_cache(old_msgid.as_deref(), &msgid);
                self.try_mark_seen(&msgid, cache_path, self_writes, status_error);
            }
            Err(e) => {
                self.parsed = None;
                *status_error = Some(format!("parse failed: {e:#}"));
                self.evict_image_cache(old_msgid.as_deref(), &msgid);
            }
        }
    }

    /// Add the `S` (Seen) flag to the selected row if it isn't already
    /// set. Used by `ensure_body` on a successful body parse, so opening
    /// a message once is enough to mark it read. Errors are surfaced via
    /// `status_error`; the rescan reconciles on the next sync.
    fn try_mark_seen(
        &mut self,
        msgid: &str,
        cache_path: &Path,
        self_writes: &SelfWrites,
        status_error: &mut Option<String>,
    ) {
        let Some(row) = self.find_row(msgid) else {
            return;
        };
        if row.flags.contains('S') {
            return;
        }
        let path = row.path.clone();
        match flags::set_flag_recorded(&path, 'S', FlagOp::Add, self_writes) {
            Ok((new_path, new_flags)) => {
                self.apply_flag_change(msgid, &new_path, &new_flags, cache_path, status_error);
            }
            Err(e) => {
                *status_error = Some(format!("mark read: {e}"));
            }
        }
    }

    pub fn toggle_flag(
        &mut self,
        flag: char,
        cache_path: &Path,
        self_writes: &SelfWrites,
        status_error: &mut Option<String>,
    ) {
        let Some(msgid) = self.selected_msgid() else {
            return;
        };
        let Some(row) = self.find_row(&msgid) else {
            return;
        };
        let path = row.path.clone();
        match flags::set_flag_recorded(&path, flag, FlagOp::Toggle, self_writes) {
            Ok((new_path, new_flags)) => {
                self.apply_flag_change(&msgid, &new_path, &new_flags, cache_path, status_error);
            }
            Err(e) => {
                *status_error = Some(format!("toggle {flag}: {e}"));
            }
        }
    }

    /// Rename the selected row's file into `<account_maildir>/.<folder>/cur/`,
    /// preserving its flag suffix. On success: records both paths on the
    /// self-write registry (so the future Step 7 watcher will skip its own
    /// echo), drops the row from the in-memory inbox view, and mirrors the
    /// new folder/path into the SQLite index.
    pub fn move_selected_to(
        &mut self,
        target_folder: &str,
        accounts: &HashMap<String, Account>,
        cache_path: &Path,
        self_writes: &SelfWrites,
        status_error: &mut Option<String>,
    ) {
        let Some(msgid) = self.selected_msgid() else {
            return;
        };
        let Some(row) = self.find_row(&msgid) else {
            return;
        };
        let account_name = row.account.clone();
        let path = row.path.clone();
        let row_flags = row.flags.clone();

        let Some(account) = accounts.get(&account_name) else {
            *status_error = Some(format!("move: unknown account {account_name}"));
            return;
        };
        let folder_root = account.layout.folder_path(&account.maildir, target_folder);
        let target_cur = folder_root.join("cur");

        if let Err(e) = flags::ensure_maildir(&folder_root) {
            *status_error = Some(format!("move: create {target_folder}: {e}"));
            return;
        }

        match flags::move_to_folder_recorded(&path, &target_cur, &row_flags, self_writes) {
            Ok(new_path) => {
                // Auto-register the destination folder with the watcher
                // so MOVED_TO events from external clients into it are
                // tracked. Idempotent: a no-op when already watched.
                if let Some(w) = self.watcher.as_ref() {
                    w.register_folder(&account_name, target_folder, &folder_root, account.layout);
                }
                self.drop_row_after_move(
                    &msgid,
                    target_folder,
                    &new_path,
                    cache_path,
                    status_error,
                );
                *status_error = Some(format!("moved to {target_folder}"));
            }
            Err(e) => {
                *status_error = Some(format!("move to {target_folder}: {e}"));
            }
        }
    }

    /// Inbox bookkeeping after a successful cross-folder rename: remove
    /// the row from the in-memory view (the unified list is INBOX-only
    /// and the moved row no longer belongs there), clamp `selected`, then
    /// mirror the new folder/path into the index. Mirroring failures are
    /// surfaced but don't roll back — the next rescan reconciles.
    fn drop_row_after_move(
        &mut self,
        msgid: &str,
        target_folder: &str,
        new_path: &Path,
        cache_path: &Path,
        status_error: &mut Option<String>,
    ) {
        // Pull the pre-move snapshot from whichever in-memory view has
        // the row. Scan is preferred when both hold it (local search +
        // current folder), but global search may only have it in the
        // haystack.
        let Some(snapshot_base) = self.find_row(msgid).cloned() else {
            return;
        };
        let src_folder = snapshot_base.folder.clone();
        let account = snapshot_base.account.clone();
        let was_unread = !snapshot_base.flags.contains('S');
        let mut snapshot = snapshot_base;
        snapshot.folder = target_folder.to_string();
        snapshot.path = new_path.to_path_buf();

        // Drop from scan (if there). When the row was the active list
        // selection (no search), clamp selected against the new len.
        let mut scan_had_row = false;
        if let ScanState::Ready(rows) = &mut self.scan
            && let Some(i) = rows.iter().position(|t| t.row.msgid == msgid)
        {
            rows.remove(i);
            scan_had_row = true;
            if self.search.is_none() {
                if rows.is_empty() {
                    self.selected = 0;
                } else if self.selected >= rows.len() {
                    self.selected = rows.len() - 1;
                }
            }
        }
        // Drop from search (if active). Reclamps `selected` against the
        // new results length.
        if let Some(s) = self.search.as_mut() {
            s.drop_msgid(msgid);
            if s.results.is_empty() {
                self.selected = 0;
            } else if self.selected >= s.results.len() {
                self.selected = s.results.len() - 1;
            }
        }
        // Stats are unconditional — the move actually shifted counts on
        // disk regardless of which in-memory view held the row.
        let _ = scan_had_row;
        adjust_total(&mut self.folder_stats, &account, &src_folder, -1);
        adjust_total(&mut self.folder_stats, &account, target_folder, 1);
        if was_unread {
            adjust_unread(&mut self.folder_stats, &account, &src_folder, -1);
            adjust_unread(&mut self.folder_stats, &account, target_folder, 1);
        }

        if let Err(e) = mirror_to_index(cache_path, &snapshot) {
            *status_error = Some(format!("index mirror failed: {e:#}"));
        }
    }

    /// Patch the in-memory row's `path` / `flags` after a successful
    /// rename, then mirror the change into the SQLite index. Maildir is
    /// truth (DESIGN invariant 3), so if the index update fails we leave
    /// the in-memory row patched and let the next rescan reconcile.
    fn apply_flag_change(
        &mut self,
        msgid: &str,
        new_path: &Path,
        new_flags: &str,
        cache_path: &Path,
        status_error: &mut Option<String>,
    ) {
        // Compute the unread delta + identify (account, folder) from
        // either in-memory view. The row may live in scan only (no
        // search), haystack only (global-search result outside the
        // current folder), or both (local search). Whichever has it
        // first is fine — the values that drive stats are identical.
        let Some(pre) = self.find_row(msgid).cloned() else {
            return;
        };
        let was_unread = !pre.flags.contains('S');
        let now_unread = !new_flags.contains('S');
        let unread_delta: i64 = match (was_unread, now_unread) {
            (true, false) => -1,
            (false, true) => 1,
            _ => 0,
        };
        // Patch scan (if there).
        let mut snapshot_for_index: Option<MessageRow> = None;
        if let ScanState::Ready(rows) = &mut self.scan
            && let Some(t) = rows.iter_mut().find(|t| t.row.msgid == msgid)
        {
            t.row.path = new_path.to_path_buf();
            t.row.flags = new_flags.to_string();
            snapshot_for_index = Some(t.row.clone());
        }
        // Patch search haystack (if there).
        if let Some(s) = self.search.as_mut() {
            s.patch_row(msgid, new_path, new_flags);
            if snapshot_for_index.is_none()
                && let Some(r) = s.haystack.iter().find(|r| r.msgid == msgid)
            {
                snapshot_for_index = Some(r.clone());
            }
        }
        if unread_delta != 0 {
            adjust_unread(
                &mut self.folder_stats,
                &pre.account,
                &pre.folder,
                unread_delta,
            );
        }
        let Some(snap) = snapshot_for_index else {
            return;
        };
        if let Err(e) = mirror_to_index(cache_path, &snap) {
            *status_error = Some(format!("index mirror failed: {e:#}"));
        }
    }

    fn decode_images(
        &mut self,
        msgid: &str,
        blocks: &[Block],
        cid_parts: &HashMap<String, Vec<u8>>,
        picker: Option<&Picker>,
        max_height: u16,
        status_error: &mut Option<String>,
    ) {
        let Some(picker) = picker else {
            self.image_cache.remove(msgid);
            return;
        };
        let mut refs: Vec<ImageRef> = Vec::new();
        collect_image_refs(blocks, &mut refs);
        let mut entries: HashMap<ImageKey, ResolvedImage> = HashMap::new();
        let mut failed: Vec<String> = Vec::new();
        for r in refs {
            match r {
                ImageRef::Cid(cid) => {
                    let key = ImageKey::Cid(cid.clone());
                    if entries.contains_key(&key) {
                        continue;
                    }
                    let Some(bytes) = cid_parts.get(&cid) else {
                        failed.push(format!("cid:{cid} (missing part)"));
                        continue;
                    };
                    match images::decode(picker, bytes, max_height) {
                        Ok(img) => {
                            entries.insert(key, img);
                        }
                        Err(_) => failed.push(format!("cid:{cid}")),
                    }
                }
                ImageRef::Data(uri) => {
                    let key = ImageKey::Data(images::data_uri_key(&uri));
                    if entries.contains_key(&key) {
                        continue;
                    }
                    let Some(bytes) = images::parse_data_uri(&uri) else {
                        failed.push("data:… (unsupported)".to_string());
                        continue;
                    };
                    match images::decode(picker, &bytes, max_height) {
                        Ok(img) => {
                            entries.insert(key, img);
                        }
                        Err(_) => failed.push("data:… (decode failed)".to_string()),
                    }
                }
            }
        }
        if entries.is_empty() {
            self.image_cache.remove(msgid);
        } else {
            self.image_cache.insert(msgid.to_string(), entries);
        }
        if !failed.is_empty() {
            *status_error = Some(format!("image decode failed: {}", failed.join(", ")));
        }
    }

    fn evict_image_cache(&mut self, old: Option<&str>, current: &str) {
        // Keep current + previous; drop everything else.
        self.prev_parsed_msgid = old.map(|s| s.to_string());
        let keep: [Option<&str>; 2] = [Some(current), self.prev_parsed_msgid.as_deref()];
        self.image_cache
            .retain(|k, _| keep.contains(&Some(k.as_str())));
    }

    /// Selected row from the *underlying scan* — only meaningful when
    /// `search.is_none()`. Most callers want
    /// [`InboxScreen::selected_message_row`] (search-aware).
    pub fn selected_row(&self) -> Option<&ThreadedRow> {
        match &self.scan {
            ScanState::Ready(rows) if !rows.is_empty() => {
                let i = self.selected.min(rows.len() - 1);
                rows.get(i)
            }
            _ => None,
        }
    }

    /// Search-aware selected row. Returns the search-result row when
    /// search is active, else the scan-list row. The canonical accessor
    /// for cmdline ops (move, reply, archive) that must work in both
    /// modes.
    pub fn selected_message_row(&self) -> Option<&MessageRow> {
        if let Some(s) = self.search.as_ref() {
            return s.selected_row(self.selected);
        }
        self.selected_row().map(|t| &t.row)
    }

    pub fn selected_msgid(&self) -> Option<String> {
        self.selected_message_row().map(|r| r.msgid.clone())
    }

    pub fn selected_path(&self) -> Option<PathBuf> {
        self.selected_message_row().map(|r| r.path.clone())
    }

    /// Number of rows currently rendered in the list pane — switches
    /// between search results and scan threads. Drives `select_next`
    /// and search-result count badges.
    pub fn list_len(&self) -> usize {
        if let Some(s) = self.search.as_ref() {
            return s.results.len();
        }
        self.threaded().len()
    }

    /// Switch the list pane to render `(account, folder)`. Re-reads the
    /// rows from the sqlite index (cheap — no maildir rescan) and resets
    /// per-message state so the reader doesn't show stale body content
    /// for the old selection. `account = None` means the unified `[all]`
    /// view. No-op when already on `(account, folder)`. Always clears
    /// any active search — scope changes invalidate the cached haystack
    /// and the user expects sidebar nav to drop search state.
    pub fn switch_to_scope(
        &mut self,
        account: Option<String>,
        folder: &str,
        cache_path: &Path,
        status_error: &mut Option<String>,
    ) {
        let same_scope =
            account.as_deref() == self.current_account.as_deref() && folder == self.current_folder;
        // A no-op scope-switch still drops a stray search — the user is
        // signalling "back to the sidebar list view."
        if self.search.is_some() {
            self.search = None;
            self.selected = 0;
        }
        if same_scope {
            return;
        }
        let idx = match Index::open(cache_path) {
            Ok(i) => i,
            Err(e) => {
                *status_error = Some(format!("switch scope: open index: {e:#}"));
                return;
            }
        };
        let rows = match idx.list_folder(account.as_deref(), folder) {
            Ok(r) => r,
            Err(e) => {
                *status_error = Some(format!("switch scope: list {folder}: {e:#}"));
                return;
            }
        };
        self.scan = ScanState::Ready(build_threads(rows));
        self.current_account = account;
        self.current_folder = folder.to_string();
        self.selected = 0;
        self.reader_scroll = 0;
        self.reader_cursor_line = 0;
        self.reader_cursor_col = 0;
        self.visual = None;
        self.mouse_drag_anchor = None;
        self.parsed = None;
        self.last_parsed_msgid = None;
        self.prev_parsed_msgid = None;
        self.image_cache.clear();
        // Drives the reader's next-frame Clear pass so kitty/iTerm
        // image placements from the previous body don't ghost over
        // the new scope's first message.
        self.body_changed_this_tick = true;
    }

    pub fn poll_scan(&mut self) {
        let Some(rx) = self.scan_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(data)) => {
                self.scan = ScanState::Ready(data.threads);
                self.folder_stats = data.groups;
                self.selected = 0;
                self.scan_rx = None;
            }
            Ok(Err(msg)) => {
                self.scan = ScanState::Failed(msg);
                self.scan_rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.scan = ScanState::Failed("scan worker died before reporting".into());
                self.scan_rx = None;
            }
        }
    }

    pub fn poll_watch(
        &mut self,
        cfg: &Config,
        cache_path: &Path,
        status_error: &mut Option<String>,
    ) {
        // 1. Drain the watcher channel into pending_dirty.
        if let Some(rx) = self.watch_rx.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(WatcherEvent::FoldersDirty(set)) => self.pending_dirty.extend(set),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.watch_rx = None;
                        break;
                    }
                }
            }
        }

        // 2. Drain any in-flight rescan result.
        if let Some(rx) = self.rescan_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(data)) => {
                    let covered = std::mem::take(&mut self.rescan_in_flight);
                    self.apply_rescan(data, &covered);
                    self.rescan_rx = None;
                }
                Ok(Err(msg)) => {
                    *status_error = Some(format!("rescan: {msg}"));
                    self.rescan_rx = None;
                    self.rescan_in_flight.clear();
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.rescan_rx = None;
                    self.rescan_in_flight.clear();
                }
            }
        }

        // 3. Kick a fresh rescan when there's pending work and no
        //    in-flight worker. Coalesce all pending dirt into one call.
        if self.rescan_rx.is_none() && !self.pending_dirty.is_empty() {
            let dirty = std::mem::take(&mut self.pending_dirty);
            let accounts: HashMap<String, AccountSpec> = cfg
                .accounts
                .iter()
                .map(|(n, a)| {
                    (
                        n.clone(),
                        AccountSpec {
                            name: n.clone(),
                            root: a.maildir.clone(),
                            layout: a.layout,
                        },
                    )
                })
                .collect();
            self.rescan_in_flight = dirty.clone();
            self.rescan_rx = Some(scan::rescan_folders(
                cache_path.to_path_buf(),
                accounts,
                dirty,
                (self.current_account.clone(), self.current_folder.clone()),
            ));
        }
    }

    /// Apply a per-folder rescan payload. `folder_stats` is always
    /// replaced (counts may have shifted across both `[all]` and per-
    /// account groups). The list `scan` is replaced only when the
    /// current scope's folder was actually re-walked under an account
    /// that intersects the scope; otherwise the rescan affected other
    /// scopes and we leave the visible rows alone. Selection is
    /// preserved by `msgid` when possible, snapping to row 0 when the
    /// previously-selected message is no longer present.
    fn apply_rescan(&mut self, data: scan::ScanData, dirty: &HashSet<(String, String)>) {
        self.folder_stats = data.groups;
        let view_touched = dirty.iter().any(|(account, folder)| {
            folder == &self.current_folder
                && match &self.current_account {
                    None => true,
                    Some(scope) => scope == account,
                }
        });
        if !view_touched {
            return;
        }
        let old_msgid = self.selected_msgid();
        self.scan = ScanState::Ready(data.threads);
        self.selected = match (old_msgid, &self.scan) {
            (Some(mid), ScanState::Ready(rows)) => {
                rows.iter().position(|r| r.row.msgid == mid).unwrap_or(0)
            }
            _ => 0,
        };
    }

    pub fn cycle_focus(&mut self, forward: bool) {
        let ring = [Pane::Folders, Pane::List, Pane::Reader];
        let n = ring.len();
        let start = ring.iter().position(|p| *p == self.focus).unwrap_or(0);
        for step in 1..=n {
            let i = if forward {
                (start + step) % n
            } else {
                (start + n - step) % n
            };
            if self.pane_visible(ring[i]) {
                self.focus = ring[i];
                return;
            }
        }
    }

    fn pane_visible(&self, p: Pane) -> bool {
        match p {
            Pane::Folders => self.sidebar_visible,
            Pane::List => self.list_visible,
            Pane::Reader => self.reader_visible,
        }
    }

    /// Spatial focus moves driven by `Ctrl-h/j/k/l`. The layout is fixed
    /// (Folders left of List-stacked-over-Reader), so each direction has
    /// at most one valid target; if the target is hidden the move is a
    /// no-op.
    pub fn focus_left(&mut self) {
        if matches!(self.focus, Pane::List | Pane::Reader) && self.sidebar_visible {
            self.focus = Pane::Folders;
        }
    }

    pub fn focus_right(&mut self) {
        // From the folder sidebar, prefer List, fall back to Reader.
        if self.focus == Pane::Folders {
            if self.list_visible {
                self.focus = Pane::List;
            } else if self.reader_visible {
                self.focus = Pane::Reader;
            }
        }
    }

    pub fn focus_down(&mut self) {
        if self.focus == Pane::List && self.reader_visible {
            self.focus = Pane::Reader;
        }
    }

    pub fn focus_up(&mut self) {
        if self.focus == Pane::Reader && self.list_visible {
            self.focus = Pane::List;
        }
    }

    pub fn threaded(&self) -> &[ThreadedRow] {
        match &self.scan {
            ScanState::Ready(rows) => rows,
            _ => &[],
        }
    }

    /// Snapshot rows for the current `(account, folder)` and install a
    /// fresh local `SearchState`. Returns `true` on success; `false`
    /// when the index couldn't be opened (status_error already set).
    pub fn enter_search_local(
        &mut self,
        cache_path: &Path,
        status_error: &mut Option<String>,
    ) -> bool {
        let idx = match Index::open(cache_path) {
            Ok(i) => i,
            Err(e) => {
                *status_error = Some(format!("search: open index: {e:#}"));
                return false;
            }
        };
        let folder = self.current_folder.clone();
        let rows = match idx.list_scope(
            self.current_account.as_deref(),
            Some(std::slice::from_ref(&folder)),
        ) {
            Ok(r) => r,
            Err(e) => {
                *status_error = Some(format!("search: list scope: {e:#}"));
                return false;
            }
        };
        let prior = self.selected_msgid();
        self.search = Some(Box::new(SearchState::new(
            crate::ui::search::SearchKind::Local {
                account: self.current_account.clone(),
                folder,
            },
            rows,
            prior,
        )));
        self.selected = 0;
        self.focus = Pane::List;
        true
    }

    /// Snapshot rows for the global-search scope and install a fresh
    /// global `SearchState`. `priority_folders` is `[search].global_folders`;
    /// empty means "every folder, score-only ranking."
    pub fn enter_search_global(
        &mut self,
        priority_folders: &[String],
        cache_path: &Path,
        status_error: &mut Option<String>,
    ) -> bool {
        let idx = match Index::open(cache_path) {
            Ok(i) => i,
            Err(e) => {
                *status_error = Some(format!("search: open index: {e:#}"));
                return false;
            }
        };
        let folders_filter: Option<&[String]> = if priority_folders.is_empty() {
            None
        } else {
            Some(priority_folders)
        };
        let rows = match idx.list_scope(self.current_account.as_deref(), folders_filter) {
            Ok(r) => r,
            Err(e) => {
                *status_error = Some(format!("search: list scope: {e:#}"));
                return false;
            }
        };
        let prior = self.selected_msgid();
        self.search = Some(Box::new(SearchState::new(
            crate::ui::search::SearchKind::Global {
                account: self.current_account.clone(),
                folders: priority_folders.to_vec(),
            },
            rows,
            prior,
        )));
        self.selected = 0;
        self.focus = Pane::List;
        true
    }

    /// Drop the active search and restore the cursor to the pre-search
    /// msgid when possible. No-op when search is inactive.
    pub fn exit_search_cancel(&mut self) {
        let Some(s) = self.search.take() else {
            return;
        };
        self.selected = 0;
        if let Some(prior) = s.prior_selected_msgid
            && let ScanState::Ready(rows) = &self.scan
            && let Some(i) = rows.iter().position(|t| t.row.msgid == prior)
        {
            self.selected = i;
        }
    }

    /// Drop the active search without restoring prior selection — used
    /// when the user explicitly returns to the inbox view via `Esc` in
    /// Normal mode after committing a search.
    pub fn clear_search(&mut self) {
        if self.search.take().is_some() {
            self.selected = 0;
        }
    }

    pub fn select_next(&mut self) {
        let len = self.list_len();
        if len == 0 {
            return;
        }
        if self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Look up a row by msgid across both the scan view and the search
    /// haystack (when search is active). Used by flag-toggle / move
    /// callsites that must keep the in-memory rows consistent regardless
    /// of which view is current.
    fn find_row(&self, msgid: &str) -> Option<&MessageRow> {
        if let ScanState::Ready(rows) = &self.scan
            && let Some(t) = rows.iter().find(|t| t.row.msgid == msgid)
        {
            return Some(&t.row);
        }
        self.search
            .as_ref()
            .and_then(|s| s.haystack.iter().find(|r| r.msgid == msgid))
    }
}

fn mirror_to_index(cache_path: &Path, row: &MessageRow) -> anyhow::Result<()> {
    let mut idx = Index::open(cache_path)?;
    idx.upsert(row)?;
    Ok(())
}

/// Patch the unread count for `(account, folder)` in both the unified
/// `[all]` group and the per-account group. Every per-account change is
/// also a unified-view change, so we always touch both.
fn adjust_unread(groups: &mut Vec<AccountFolderStats>, account: &str, folder: &str, delta: i64) {
    for scope in [None, Some(account)] {
        let entry = ensure_folder_entry(groups, scope, folder);
        entry.unread = apply_delta(entry.unread, delta);
    }
}

/// Patch the total count for `(account, folder)` in both the unified
/// `[all]` group and the per-account group.
fn adjust_total(groups: &mut Vec<AccountFolderStats>, account: &str, folder: &str, delta: i64) {
    for scope in [None, Some(account)] {
        let entry = ensure_folder_entry(groups, scope, folder);
        entry.total = apply_delta(entry.total, delta);
    }
}

fn ensure_folder_entry<'a>(
    groups: &'a mut Vec<AccountFolderStats>,
    scope: Option<&str>,
    folder: &str,
) -> &'a mut FolderStat {
    let gi = match groups.iter().position(|g| g.scope.as_deref() == scope) {
        Some(i) => i,
        None => {
            groups.push(AccountFolderStats {
                scope: scope.map(str::to_string),
                folders: Vec::new(),
            });
            groups.len() - 1
        }
    };
    let group = &mut groups[gi];
    let fi = match group.folders.iter().position(|s| s.folder == folder) {
        Some(i) => i,
        None => {
            group.folders.push(FolderStat {
                folder: folder.to_string(),
                total: 0,
                unread: 0,
            });
            group.folders.len() - 1
        }
    };
    &mut group.folders[fi]
}

fn apply_delta(value: u64, delta: i64) -> u64 {
    if delta >= 0 {
        value.saturating_add(delta as u64)
    } else {
        value.saturating_sub(delta.unsigned_abs())
    }
}

/// Single image reference reached from a Block-IR walk. Used by
/// `InboxScreen::decode_images` to enumerate every renderable image in a
/// parsed body without exposing the walk to the rest of the app.
enum ImageRef {
    Cid(String),
    Data(String),
}

fn collect_image_refs(blocks: &[Block], out: &mut Vec<ImageRef>) {
    // Inline runs (Paragraph / Heading / Table cells) can't carry image
    // content in our IR — images are block-level only — so the walk is
    // strictly over `Block`s. If `Inline` gains an image variant later,
    // descend into the runs here.
    for b in blocks {
        match b {
            Block::Image { cid, src, .. } => {
                if let Some(c) = cid {
                    out.push(ImageRef::Cid(c.clone()));
                } else if let Some(s) = src
                    && s.starts_with("data:")
                {
                    out.push(ImageRef::Data(s.clone()));
                }
            }
            Block::List { items, .. } => {
                for it in items {
                    collect_image_refs(it, out);
                }
            }
            Block::Quote(inner) => collect_image_refs(inner, out),
            Block::Paragraph(_)
            | Block::Heading { .. }
            | Block::Table { .. }
            | Block::Pre(_)
            | Block::HRule => {}
        }
    }
}

fn initial_focus(sidebar: bool, list: bool, reader: bool) -> Pane {
    if list {
        Pane::List
    } else if sidebar {
        Pane::Folders
    } else if reader {
        Pane::Reader
    } else {
        Pane::List
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let outer = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);
    let top = outer[0];
    let body = outer[1];
    let bottom = outer[2];

    tabs::draw(f, top, app);

    // Split App so the per-screen draws can borrow the active screen
    // mutably while cmdline::draw later sees the global cmdline /
    // status / mode fields.
    let App {
        screens,
        active,
        mode,
        link_pick_buf,
        ..
    } = app;
    match screens.get_mut(*active) {
        Some(Screen::Inbox(inbox)) => {
            // Clear last frame's reader rect up-front; reader::draw will
            // set it again iff the reader pane actually renders. That
            // way a frame with the reader hidden leaves `None` for the
            // mouse handler to see, and stale rects can't sneak through.
            inbox.last_reader_inner = None;
            let (sidebar_area, right_area) = split_body(body, inbox.sidebar_visible);
            let (list_area, reader_area) =
                split_right(right_area, inbox.list_visible, inbox.reader_visible);
            if let Some(rect) = sidebar_area {
                folders::draw(f, rect, inbox);
            }
            if let Some(rect) = list_area {
                list::draw(f, rect, inbox);
            }
            if let Some(rect) = reader_area {
                reader::draw(f, rect, inbox, *mode, link_pick_buf);
            }
        }
        Some(Screen::Compose(c)) => compose::draw(f, body, c),
        None => {}
    }
    cmdline::draw(f, bottom, app);
}

fn split_body(body: Rect, sidebar: bool) -> (Option<Rect>, Rect) {
    if !sidebar {
        return (None, body);
    }
    let parts = Layout::horizontal([Constraint::Length(20), Constraint::Min(0)]).split(body);
    (Some(parts[0]), parts[1])
}

fn split_right(right: Rect, list: bool, reader: bool) -> (Option<Rect>, Option<Rect>) {
    match (list, reader) {
        (true, true) => {
            let parts =
                Layout::vertical([Constraint::Percentage(40), Constraint::Min(0)]).split(right);
            (Some(parts[0]), Some(parts[1]))
        }
        (true, false) => (Some(right), None),
        (false, true) => (None, Some(right)),
        (false, false) => (None, None),
    }
}

#[cfg(test)]
mod focus_nav_tests {
    use std::path::Path;

    use super::*;

    fn inbox_with_panes(sidebar: bool, list: bool, reader: bool) -> InboxScreen {
        let mut cfg = Config::default();
        cfg.ui.sidebar = sidebar;
        cfg.ui.list = list;
        cfg.ui.reader = reader;
        InboxScreen::new(
            &cfg,
            Path::new("/tmp/epost-test.sqlite"),
            SelfWrites::new(),
            None,
        )
    }

    #[test]
    fn focus_left_moves_from_list_to_folders() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.focus = Pane::List;
        inbox.focus_left();
        assert_eq!(inbox.focus, Pane::Folders);
    }

    #[test]
    fn focus_left_from_reader_lands_on_folders() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.focus = Pane::Reader;
        inbox.focus_left();
        assert_eq!(inbox.focus, Pane::Folders);
    }

    #[test]
    fn focus_left_noop_when_sidebar_hidden() {
        let mut inbox = inbox_with_panes(false, true, true);
        inbox.focus = Pane::List;
        inbox.focus_left();
        assert_eq!(inbox.focus, Pane::List);
    }

    #[test]
    fn focus_right_from_folders_prefers_list() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.focus = Pane::Folders;
        inbox.focus_right();
        assert_eq!(inbox.focus, Pane::List);
    }

    #[test]
    fn focus_right_from_folders_falls_back_to_reader() {
        let mut inbox = inbox_with_panes(true, false, true);
        inbox.focus = Pane::Folders;
        inbox.focus_right();
        assert_eq!(inbox.focus, Pane::Reader);
    }

    #[test]
    fn focus_right_noop_when_right_column_hidden() {
        let mut inbox = inbox_with_panes(true, false, false);
        inbox.focus = Pane::Folders;
        inbox.focus_right();
        assert_eq!(inbox.focus, Pane::Folders);
    }

    #[test]
    fn focus_down_moves_list_to_reader() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.focus = Pane::List;
        inbox.focus_down();
        assert_eq!(inbox.focus, Pane::Reader);
    }

    #[test]
    fn focus_down_noop_when_reader_hidden() {
        let mut inbox = inbox_with_panes(true, true, false);
        inbox.focus = Pane::List;
        inbox.focus_down();
        assert_eq!(inbox.focus, Pane::List);
    }

    #[test]
    fn focus_up_moves_reader_to_list() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.focus = Pane::Reader;
        inbox.focus_up();
        assert_eq!(inbox.focus, Pane::List);
    }

    #[test]
    fn focus_up_noop_when_list_hidden() {
        let mut inbox = inbox_with_panes(true, false, true);
        inbox.focus = Pane::Reader;
        inbox.focus_up();
        assert_eq!(inbox.focus, Pane::Reader);
    }

    #[test]
    fn move_reader_cursor_clamps_within_body() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.last_reader_body_only_lines = 5;
        inbox.last_reader_inner_height = 10;
        inbox.last_reader_header_offset = 4;
        // Up past the top.
        inbox.reader_cursor_line = 0;
        inbox.move_reader_cursor(-3, 0);
        assert_eq!(inbox.reader_cursor_line, 0);
        // Down past the bottom: clamps to last index (4).
        inbox.move_reader_cursor(100, 0);
        assert_eq!(inbox.reader_cursor_line, 4);
    }

    #[test]
    fn move_reader_cursor_follows_scroll_down() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.last_reader_body_only_lines = 50;
        inbox.last_reader_inner_height = 5;
        inbox.last_reader_header_offset = 4;
        inbox.reader_scroll = 0;
        // Walk the cursor past the bottom of the viewport. Absolute
        // row = header + cursor; once it ≥ scroll + height, scroll
        // bumps to keep cursor visible.
        inbox.move_reader_cursor(10, 0);
        assert_eq!(inbox.reader_cursor_line, 10);
        // Absolute row = 14; viewport height 5 → scroll should be at
        // least 10 so 14 fits in [10, 15).
        assert!(
            inbox.reader_scroll >= 10,
            "scroll was {}",
            inbox.reader_scroll
        );
    }

    #[test]
    fn move_reader_cursor_follows_scroll_up() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.last_reader_body_only_lines = 50;
        inbox.last_reader_inner_height = 5;
        inbox.last_reader_header_offset = 4;
        // User scrolled down with `j` first; now `k` should bring scroll
        // back up.
        inbox.reader_scroll = 20;
        inbox.reader_cursor_line = 30;
        inbox.move_reader_cursor(-25, 0);
        // Cursor at 5, abs row = 9. Scroll should have come down to 9
        // or lower.
        assert_eq!(inbox.reader_cursor_line, 5);
        assert!(
            inbox.reader_scroll <= 9,
            "scroll was {}",
            inbox.reader_scroll
        );
    }

    #[test]
    fn move_reader_cursor_to_bottom_sets_max() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.last_reader_body_only_lines = 7;
        inbox.last_reader_inner_height = 3;
        inbox.last_reader_header_offset = 0;
        inbox.move_reader_cursor_to_bottom();
        assert_eq!(inbox.reader_cursor_line, 6);
        assert_eq!(inbox.reader_cursor_col, u16::MAX);
    }

    #[test]
    fn enter_visual_requires_reader_focus() {
        let mut cfg = Config::default();
        cfg.ui.sidebar = true;
        cfg.ui.list = true;
        cfg.ui.reader = true;
        let mut app = App::new(
            &cfg,
            std::path::PathBuf::from("/tmp/epost-test.sqlite"),
            None,
            None,
        );
        app.inbox_mut().focus = Pane::List;
        app.enter_visual(VisualKind::Char);
        assert!(app.inbox().visual.is_none(), "should not enter from List");
        assert_eq!(app.mode, Mode::Normal);

        app.inbox_mut().focus = Pane::Reader;
        app.enter_visual(VisualKind::Char);
        assert!(app.inbox().visual.is_some(), "should enter from Reader");
        assert_eq!(app.mode, Mode::Visual);
    }

    #[test]
    fn exit_visual_clears_anchor_and_mode() {
        let mut cfg = Config::default();
        cfg.ui.sidebar = true;
        cfg.ui.list = true;
        cfg.ui.reader = true;
        let mut app = App::new(
            &cfg,
            std::path::PathBuf::from("/tmp/epost-test.sqlite"),
            None,
            None,
        );
        app.inbox_mut().focus = Pane::Reader;
        app.enter_visual(VisualKind::Line);
        app.exit_visual();
        assert!(app.inbox().visual.is_none());
        assert_eq!(app.mode, Mode::Normal);
    }
}

#[cfg(test)]
mod flag_integration_tests {
    use std::fs;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    use super::*;
    use crate::config::Account;

    const MSG: &[u8] = b"\
Message-ID: <m1@example.invalid>\r\n\
From: Tester <tester@example.invalid>\r\n\
Subject: hi\r\n\
Date: Thu, 28 May 2026 12:00:00 +0000\r\n\
\r\n\
<p>body</p>\r\n";

    fn drop_message(tmp: &TempDir, dir: &str, basename: &str) -> std::path::PathBuf {
        let inbox = tmp.path().join("Mail").join("personal");
        fs::create_dir_all(inbox.join("cur")).unwrap();
        fs::create_dir_all(inbox.join("new")).unwrap();
        let p = inbox.join(dir).join(basename);
        fs::write(&p, MSG).unwrap();
        p
    }

    fn one_account_config(tmp: &TempDir) -> Config {
        let mut cfg = Config::default();
        cfg.accounts.insert(
            "personal".into(),
            Account {
                maildir: tmp.path().join("Mail").join("personal"),
                from: "Tester <tester@example.invalid>".into(),
                layout: Default::default(),
                sent_folder: None,
                archive_folder: None,
                spam_folder: None,
                trash_folder: None,
                smtp: None,
            },
        );
        cfg
    }

    /// Two-account fixture: personal + work, each with an INBOX. Used by
    /// the multi-account UI tests below. Drops one message into each
    /// account's INBOX so scan results are non-empty.
    fn two_account_config(tmp: &TempDir) -> Config {
        let mut cfg = Config::default();
        for name in ["personal", "work"] {
            cfg.accounts.insert(
                name.into(),
                Account {
                    maildir: tmp.path().join("Mail").join(name),
                    from: format!("Tester <{name}@example.invalid>"),
                    layout: Default::default(),
                    sent_folder: None,
                    archive_folder: None,
                    spam_folder: None,
                    trash_folder: None,
                    smtp: None,
                },
            );
            let inbox = tmp.path().join("Mail").join(name);
            fs::create_dir_all(inbox.join("cur")).unwrap();
            fs::create_dir_all(inbox.join("new")).unwrap();
            let body = format!(
                "Message-ID: <{name}-1@x>\r\n\
                 Date: Thu, 28 May 2026 12:00:00 +0000\r\n\
                 From: a@b\r\n\
                 Subject: hi\r\n\
                 \r\n\
                 body\r\n"
            );
            fs::write(inbox.join("cur").join(format!("{name}-1:2,")), body).unwrap();
        }
        cfg
    }

    /// Spin until the scan worker either reports rows or fails. The
    /// worker runs on `std::thread`; tests can't share the live event
    /// loop, so this is the equivalent of one tick of the UI's
    /// `poll_scan` until the channel resolves.
    fn drain_scan(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            app.poll_scan();
            if matches!(app.inbox().scan, ScanState::Ready(_) | ScanState::Failed(_)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("scan worker never reported");
    }

    #[test]
    fn auto_mark_seen_on_first_body_parse() {
        let tmp = TempDir::new().unwrap();
        let src = drop_message(&tmp, "new", "1779.M0P1.host");
        let cfg = one_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);
        assert!(matches!(app.inbox().scan, ScanState::Ready(_)));

        app.ensure_body_for_selection();

        let row = app.inbox().threaded().first().expect("row").row.clone();
        assert!(
            row.flags.contains('S'),
            "expected S flag after auto-mark, got {:?}",
            row.flags
        );
        let expected = tmp
            .path()
            .join("Mail")
            .join("personal")
            .join("cur")
            .join("1779.M0P1.host:2,S");
        assert_eq!(row.path, expected);
        assert!(expected.exists(), "renamed file must exist");
        assert!(!src.exists(), "original new/ file must be gone");
        assert!(
            app.status_error.is_none(),
            "auto-mark must not surface an error, got {:?}",
            app.status_error
        );
    }

    #[test]
    fn auto_mark_seen_idempotent_on_repeat_calls() {
        let tmp = TempDir::new().unwrap();
        drop_message(&tmp, "new", "1779.M0P1.host");
        let cfg = one_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        app.ensure_body_for_selection();
        // Second call on the same selection: nothing to parse, nothing to
        // rename. Any status_error here would mean we tried (and failed)
        // to re-rename a file that's already gone.
        app.status_error = None;
        app.ensure_body_for_selection();
        assert!(
            app.status_error.is_none(),
            "repeat call leaked an error: {:?}",
            app.status_error
        );
    }

    #[test]
    fn manual_toggle_clears_seen() {
        let tmp = TempDir::new().unwrap();
        let src = drop_message(&tmp, "cur", "1779.M0P1.host:2,S");
        let cfg = one_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        let start = app.inbox().threaded().first().expect("row").row.clone();
        assert!(start.flags.contains('S'), "fixture must start Seen");

        app.toggle_flag_selected('S');

        let after = app.inbox().threaded().first().expect("row").row.clone();
        assert!(
            !after.flags.contains('S'),
            "toggle must clear S, got {:?}",
            after.flags
        );
        let expected = tmp
            .path()
            .join("Mail")
            .join("personal")
            .join("cur")
            .join("1779.M0P1.host");
        assert_eq!(after.path, expected);
        assert!(expected.exists());
        assert!(!src.exists());
    }

    #[test]
    fn manual_toggle_sets_flagged() {
        let tmp = TempDir::new().unwrap();
        let src = drop_message(&tmp, "cur", "1779.M0P1.host:2,S");
        let cfg = one_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        app.toggle_flag_selected('F');

        let after = app.inbox().threaded().first().expect("row").row.clone();
        assert!(
            after.flags.contains('F'),
            "expected F flag after toggle, got {:?}",
            after.flags
        );
        // Canonical ordering keeps the suffix sorted ASCII-uppercase.
        let expected = tmp
            .path()
            .join("Mail")
            .join("personal")
            .join("cur")
            .join("1779.M0P1.host:2,FS");
        assert_eq!(after.path, expected);
        assert!(expected.exists());
        assert!(!src.exists());
    }

    #[test]
    fn manual_toggle_sets_trashed_then_clears() {
        let tmp = TempDir::new().unwrap();
        drop_message(&tmp, "cur", "1779.M0P1.host:2,S");
        let cfg = one_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        app.toggle_flag_selected('T');
        let after = app.inbox().threaded().first().expect("row").row.clone();
        assert!(
            after.flags.contains('T'),
            "expected T, got {:?}",
            after.flags
        );

        app.toggle_flag_selected('T');
        let after = app.inbox().threaded().first().expect("row").row.clone();
        assert!(
            !after.flags.contains('T'),
            "expected T cleared, got {:?}",
            after.flags
        );
    }

    #[test]
    fn self_writes_recorded_on_flag_flip() {
        let tmp = TempDir::new().unwrap();
        drop_message(&tmp, "new", "1779.M0P1.host");
        let cfg = one_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        let watcher_view = app.self_writes.clone();
        app.ensure_body_for_selection();

        let new_path = tmp
            .path()
            .join("Mail")
            .join("personal")
            .join("cur")
            .join("1779.M0P1.host:2,S");
        let old_path = tmp
            .path()
            .join("Mail")
            .join("personal")
            .join("new")
            .join("1779.M0P1.host");
        assert!(
            watcher_view.consume(&new_path),
            "destination path should be recorded"
        );
        assert!(
            watcher_view.consume(&old_path),
            "source path should be recorded"
        );
    }

    fn account_with_folders(tmp: &TempDir, archive: Option<&str>, trash: Option<&str>) -> Config {
        let mut cfg = Config::default();
        cfg.accounts.insert(
            "personal".into(),
            Account {
                maildir: tmp.path().join("Mail").join("personal"),
                from: "Tester <tester@example.invalid>".into(),
                layout: Default::default(),
                sent_folder: None,
                archive_folder: archive.map(str::to_string),
                spam_folder: None,
                trash_folder: trash.map(str::to_string),
                smtp: None,
            },
        );
        cfg
    }

    #[test]
    fn archive_moves_message_out_of_inbox() {
        let tmp = TempDir::new().unwrap();
        let src = drop_message(&tmp, "cur", "1779.M0P1.host:2,S");
        let cfg = account_with_folders(&tmp, Some("Archive"), None);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache.clone(), None, None);
        drain_scan(&mut app);
        let msgid = app.inbox().selected_msgid().expect("a selected row");

        cmdline::dispatch("archive", &mut app, &cfg);

        let expected = tmp
            .path()
            .join("Mail")
            .join("personal")
            .join(".Archive")
            .join("cur")
            .join("1779.M0P1.host:2,S");
        assert!(expected.exists(), "moved file must exist at {expected:?}");
        assert!(!src.exists(), "original inbox path must be gone");
        assert!(
            app.inbox().threaded().is_empty(),
            "inbox list must drop the archived row"
        );

        let idx = crate::store::index::Index::open(&cache).unwrap();
        let got = idx.get(&msgid).unwrap().expect("row in index");
        assert_eq!(got.folder, "Archive");
        assert_eq!(got.path, expected);
    }

    #[test]
    fn trash_moves_to_trash_folder_preserving_flags() {
        let tmp = TempDir::new().unwrap();
        drop_message(&tmp, "cur", "1779.M0P1.host:2,FS");
        let cfg = account_with_folders(&tmp, None, Some("Trash"));
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        cmdline::dispatch("trash", &mut app, &cfg);

        let expected = tmp
            .path()
            .join("Mail")
            .join("personal")
            .join(".Trash")
            .join("cur")
            .join("1779.M0P1.host:2,FS");
        assert!(
            expected.exists(),
            "trash must preserve the suffix verbatim; expected {expected:?}"
        );
    }

    #[test]
    fn archive_creates_missing_target_folder() {
        let tmp = TempDir::new().unwrap();
        drop_message(&tmp, "cur", "1779.M0P1.host:2,S");
        let cfg = account_with_folders(&tmp, Some("Archive"), None);
        let cache = tmp.path().join("index.sqlite");

        let archive_root = tmp.path().join("Mail").join("personal").join(".Archive");
        assert!(
            !archive_root.exists(),
            "precondition: .Archive must be absent before the move"
        );

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        cmdline::dispatch("archive", &mut app, &cfg);

        assert!(archive_root.join("cur").is_dir(), "cur/ must be created");
        assert!(archive_root.join("new").is_dir(), "new/ must be created");
        assert!(archive_root.join("tmp").is_dir(), "tmp/ must be created");
    }

    #[test]
    fn archive_without_config_reports_error() {
        let tmp = TempDir::new().unwrap();
        let src = drop_message(&tmp, "cur", "1779.M0P1.host:2,S");
        let cfg = account_with_folders(&tmp, None, None);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        cmdline::dispatch("archive", &mut app, &cfg);

        assert!(src.exists(), "file must stay put when config is missing");
        assert_eq!(
            app.inbox().threaded().len(),
            1,
            "row must remain visible in the inbox list"
        );
        let err = app
            .status_error
            .as_deref()
            .expect("status_error should be set");
        assert!(
            err.contains("archive_folder"),
            "error must mention the missing config key, got {err:?}"
        );
    }

    #[test]
    fn mv_to_custom_folder() {
        let tmp = TempDir::new().unwrap();
        drop_message(&tmp, "cur", "1779.M0P1.host:2,S");
        let cfg = account_with_folders(&tmp, None, None);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        cmdline::dispatch("mv Receipts", &mut app, &cfg);

        let expected = tmp
            .path()
            .join("Mail")
            .join("personal")
            .join(".Receipts")
            .join("cur")
            .join("1779.M0P1.host:2,S");
        assert!(expected.exists(), "mv must land at {expected:?}");
        assert!(app.inbox().threaded().is_empty());
    }

    #[test]
    fn mv_without_folder_reports_error() {
        let tmp = TempDir::new().unwrap();
        let src = drop_message(&tmp, "cur", "1779.M0P1.host:2,S");
        let cfg = account_with_folders(&tmp, None, None);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        cmdline::dispatch("mv", &mut app, &cfg);

        assert!(src.exists(), "no move, no file rename");
        let err = app.status_error.as_deref().expect("error expected");
        assert!(err.contains("missing folder"), "got {err:?}");
    }

    #[test]
    fn move_records_both_paths_in_self_writes() {
        let tmp = TempDir::new().unwrap();
        let src = drop_message(&tmp, "cur", "1779.M0P1.host:2,S");
        let cfg = account_with_folders(&tmp, Some("Archive"), None);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        let watcher_view = app.self_writes.clone();
        cmdline::dispatch("archive", &mut app, &cfg);

        let dst = tmp
            .path()
            .join("Mail")
            .join("personal")
            .join(".Archive")
            .join("cur")
            .join("1779.M0P1.host:2,S");
        assert!(
            watcher_view.consume(&src),
            "source path should be on the self-write registry"
        );
        assert!(
            watcher_view.consume(&dst),
            "destination path should be on the self-write registry"
        );
    }

    #[test]
    fn cycle_folder_switches_list_and_current_folder() {
        let tmp = TempDir::new().unwrap();
        // Two folders, one message each. Distinct Message-IDs so the
        // index keeps both rows (msgid is the primary key, so reused
        // ids collapse and the second upsert overwrites the first).
        drop_message(&tmp, "cur", "1779.M0P1.host:2,S");
        let sent_root = tmp
            .path()
            .join("Mail")
            .join("personal")
            .join(".Sent")
            .join("cur");
        std::fs::create_dir_all(&sent_root).unwrap();
        let sent_msg = b"\
Message-ID: <m2@example.invalid>\r\n\
From: Tester <tester@example.invalid>\r\n\
Subject: sent hi\r\n\
Date: Thu, 28 May 2026 12:00:00 +0000\r\n\
\r\n\
<p>sent body</p>\r\n";
        std::fs::write(sent_root.join("9999.M0P9.host:2,S"), sent_msg).unwrap();

        let cfg = one_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        assert_eq!(app.inbox().current_folder, "INBOX");
        // Flatten across groups: the `[all]` group should contain both
        // folder rows.
        let folders: Vec<&str> = app
            .inbox()
            .folder_stats
            .iter()
            .flat_map(|g| g.folders.iter().map(|s| s.folder.as_str()))
            .collect();
        assert!(folders.contains(&"INBOX"));
        assert!(folders.contains(&"Sent"));

        // INBOX → Sent (next in canonical order: INBOX, then Sent).
        app.cycle_folder(true);
        assert_eq!(app.inbox().current_folder, "Sent");
        assert_eq!(app.inbox().threaded().len(), 1);

        // Wrap back to INBOX.
        app.cycle_folder(true);
        assert_eq!(app.inbox().current_folder, "INBOX");
        assert_eq!(app.inbox().threaded().len(), 1);

        // And cycle backwards lands on Sent again.
        app.cycle_folder(false);
        assert_eq!(app.inbox().current_folder, "Sent");
    }

    #[test]
    fn cycle_folder_no_op_when_stats_empty() {
        // No accounts → no scan, folder_stats stays empty. cycle_folder
        // must not panic or set a phantom current_folder.
        let cfg = Config::default();
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("index.sqlite");
        let mut app = App::new(&cfg, cache, None, None);
        app.cycle_folder(true);
        assert_eq!(app.inbox().current_folder, "INBOX");
    }

    #[test]
    fn cycle_folder_walks_groups_across_accounts() {
        let tmp = TempDir::new().unwrap();
        let cfg = two_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        // Initial: [all] INBOX selected (None scope).
        assert_eq!(app.inbox().current_account, None);
        assert_eq!(app.inbox().current_folder, "INBOX");

        // [all] INBOX → [personal] INBOX → [work] INBOX → wrap [all].
        app.cycle_folder(true);
        assert_eq!(app.inbox().current_account.as_deref(), Some("personal"));
        assert_eq!(app.inbox().current_folder, "INBOX");

        app.cycle_folder(true);
        assert_eq!(app.inbox().current_account.as_deref(), Some("work"));
        assert_eq!(app.inbox().current_folder, "INBOX");

        app.cycle_folder(true);
        assert_eq!(app.inbox().current_account, None);
        assert_eq!(app.inbox().current_folder, "INBOX");
    }

    #[test]
    fn account_scope_filters_list_view() {
        let tmp = TempDir::new().unwrap();
        let cfg = two_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        // [all] sees 2 INBOX rows (one per account).
        assert_eq!(app.inbox().threaded().len(), 2);

        // Scope to personal — only the personal INBOX row remains.
        app.switch_to_scope(Some("personal".into()), "INBOX");
        assert_eq!(app.inbox().current_account.as_deref(), Some("personal"));
        assert_eq!(app.inbox().threaded().len(), 1);
        let mid = app.inbox().selected_msgid().unwrap();
        assert_eq!(mid, "personal-1@x");

        // Scope to work.
        app.switch_to_scope(Some("work".into()), "INBOX");
        assert_eq!(app.inbox().threaded().len(), 1);
        assert_eq!(app.inbox().selected_msgid().as_deref(), Some("work-1@x"));

        // Back to all.
        app.switch_to_scope(None, "INBOX");
        assert_eq!(app.inbox().current_account, None);
        assert_eq!(app.inbox().threaded().len(), 2);
    }

    #[test]
    fn flag_flip_updates_both_all_and_per_account_groups() {
        let tmp = TempDir::new().unwrap();
        let cfg = two_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        // Pick whichever row is on top (date order), mark it read.
        app.ensure_body_for_selection();

        let groups = &app.inbox().folder_stats;
        let all_inbox_unread = groups
            .iter()
            .find(|g| g.scope.is_none())
            .and_then(|g| g.folders.iter().find(|f| f.folder == "INBOX"))
            .map(|f| f.unread)
            .unwrap();
        // Both INBOXes started unread; one is now Seen → all-unread is 1.
        assert_eq!(all_inbox_unread, 1);

        // The owning account's per-account group also shows 1 unread
        // (because each account had exactly one INBOX message that
        // started unread, and the selected one belongs to one of them).
        let selected_account = app
            .inbox()
            .selected_row()
            .map(|r| r.row.account.clone())
            .unwrap();
        let other = if selected_account == "personal" {
            "work"
        } else {
            "personal"
        };
        let read_acc_unread = groups
            .iter()
            .find(|g| g.scope.as_deref() == Some(selected_account.as_str()))
            .and_then(|g| g.folders.iter().find(|f| f.folder == "INBOX"))
            .map(|f| f.unread)
            .unwrap();
        let other_unread = groups
            .iter()
            .find(|g| g.scope.as_deref() == Some(other))
            .and_then(|g| g.folders.iter().find(|f| f.folder == "INBOX"))
            .map(|f| f.unread)
            .unwrap();
        assert_eq!(read_acc_unread, 0, "marking-read account drops to 0");
        assert_eq!(other_unread, 1, "other account unchanged");
    }

    #[test]
    fn account_command_switches_scope() {
        let tmp = TempDir::new().unwrap();
        let cfg = two_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app);

        cmdline::dispatch("account work", &mut app, &cfg);
        assert_eq!(app.inbox().current_account.as_deref(), Some("work"));
        assert_eq!(app.inbox().current_folder, "INBOX");

        cmdline::dispatch("account all", &mut app, &cfg);
        assert_eq!(app.inbox().current_account, None);

        cmdline::dispatch("account bogus", &mut app, &cfg);
        assert!(
            app.status_error
                .as_deref()
                .unwrap_or("")
                .starts_with("account: unknown")
        );
        // Scope unchanged on unknown.
        assert_eq!(app.inbox().current_account, None);
    }

    #[test]
    fn poll_watch_rescan_preserves_selection_by_msgid() {
        // Two INBOX messages, select the second by msgid. Externally
        // delete the first, mark INBOX dirty, poll_watch fires a
        // rescan — selection must follow msgid to the new index (now
        // row 0).
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("index.sqlite");

        let inbox = tmp.path().join("Mail").join("personal");
        fs::create_dir_all(inbox.join("cur")).unwrap();
        fs::create_dir_all(inbox.join("new")).unwrap();
        let m1 = inbox.join("cur").join("1.M0.h:2,S");
        let m2 = inbox.join("cur").join("2.M0.h:2,S");
        fs::write(
            &m1,
            b"Message-ID: <a@x>\r\nDate: Thu, 1 Jan 1970 00:00:00 +0000\r\n\r\n",
        )
        .unwrap();
        fs::write(
            &m2,
            b"Message-ID: <b@x>\r\nDate: Fri, 2 Jan 1970 00:00:00 +0000\r\n\r\n",
        )
        .unwrap();

        let cfg = one_account_config(&tmp);
        let mut app = App::new(&cfg, cache.clone(), None, None);
        drain_scan(&mut app);

        // The list orders by date DESC, so <b> is row 0, <a> is row 1.
        // Select <a> (row 1) so the test exercises msgid-follow.
        app.inbox_mut().selected = 1;
        let pre = app.inbox().selected_msgid().unwrap();
        assert_eq!(pre, "a@x");

        // Externally delete <b> and dirty-mark INBOX.
        fs::remove_file(&m2).unwrap();
        app.inbox_mut()
            .pending_dirty
            .insert(("personal".into(), "INBOX".into()));

        // Pump poll_watch until the rescan completes.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            app.poll_watch(&cfg);
            if app.inbox().rescan_rx.is_none() && app.inbox().threaded().len() == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.inbox().threaded().len(), 1);
        assert_eq!(app.inbox().selected_msgid().as_deref(), Some("a@x"));
        assert_eq!(app.inbox().selected, 0, "msgid stayed, index updated");
    }

    #[test]
    fn poll_watch_rescan_snaps_to_zero_when_selected_msgid_gone() {
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("index.sqlite");

        let inbox = tmp.path().join("Mail").join("personal");
        fs::create_dir_all(inbox.join("cur")).unwrap();
        fs::create_dir_all(inbox.join("new")).unwrap();
        let m1 = inbox.join("cur").join("1.M0.h:2,S");
        let m2 = inbox.join("cur").join("2.M0.h:2,S");
        fs::write(
            &m1,
            b"Message-ID: <a@x>\r\nDate: Thu, 1 Jan 1970 00:00:00 +0000\r\n\r\n",
        )
        .unwrap();
        fs::write(
            &m2,
            b"Message-ID: <b@x>\r\nDate: Fri, 2 Jan 1970 00:00:00 +0000\r\n\r\n",
        )
        .unwrap();

        let cfg = one_account_config(&tmp);
        let mut app = App::new(&cfg, cache.clone(), None, None);
        drain_scan(&mut app);

        // Select <a> (row 1, older), then delete <a> externally and
        // expect selected to snap to 0 after the rescan.
        app.inbox_mut().selected = 1;
        assert_eq!(app.inbox().selected_msgid().as_deref(), Some("a@x"));
        fs::remove_file(&m1).unwrap();
        app.inbox_mut()
            .pending_dirty
            .insert(("personal".into(), "INBOX".into()));

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            app.poll_watch(&cfg);
            if app.inbox().rescan_rx.is_none() && app.inbox().threaded().len() == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.inbox().threaded().len(), 1);
        assert_eq!(app.inbox().selected, 0);
        assert_eq!(app.inbox().selected_msgid().as_deref(), Some("b@x"));
    }
}
