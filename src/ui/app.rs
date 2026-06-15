use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui_image::picker::Picker;

use crate::config::{Account, Config};
use crate::mail::addressbook::{self, AddressBook, AddressBookResult};
use crate::mail::addressbook_external::{self, ExtResult};
use crate::mail::compose::{SendOutcome, SendResult};
use crate::mail::flags::{self, FlagOp};
use crate::mail::html::{self, Block};
use crate::mail::parse;
use crate::store::AccountSpec;
use crate::store::index::{FolderStat, Index, MessageRow};
use crate::store::scan::{self, AccountFolderStats, ScanResult};
use crate::store::sync::SyncResult;
use crate::store::thread::{ThreadedRow, build_threads};
use crate::store::watch::{self, SelfWrites, Watcher, WatcherConfig, WatcherEvent};
use crate::ui::address_complete::{self, AddressCompleteState};
use crate::ui::clipboard::ClipboardResult;
use crate::ui::compose::{ComposeField, ComposeScreen};
use crate::ui::events::AppEvent;
use crate::ui::images::{self, ImageKey, ResolvedImage};
use crate::ui::motion::MotionTarget;
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
    /// Reader-pane attachment picker (`gf`). Digit input accumulates in
    /// `App.attachment_pick_buf`; `<Enter>` opens the chosen attachment in
    /// the external viewer. Mirrors `LinkPick`'s digit-capture grammar.
    AttachmentPick,
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
/// (`V`) snaps both anchor and cursor to whole rendered lines; block-wise
/// (`Ctrl-V`) selects the rectangle between anchor and cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualKind {
    Char,
    Line,
    Block,
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
    pub attachments: Vec<parse::Attachment>,
}

#[derive(Debug)]
pub enum ScanState {
    Scanning,
    Ready(Vec<ThreadedRow>),
    Failed(String),
}

/// One reversible message-view mutation, recorded on the undo stack so
/// `u` / `Ctrl-r` can replay the inverse. Identity is the stable
/// `msgid`, never a maildir path — paths drift on every mbsync run.
///
/// `Move` and `Flag` cover the entirety of the message-view mutation
/// surface (`a` / `d` / `D` / `:archive` / `:spam` / `:trash` / `:mv` and
/// `m` / `*` / `x`). `Batch` groups several leaf actions so a multi-row
/// operation like `D` (trash thread) lands as a single undo step.
/// Composer text editing is intentionally out of scope — `$EDITOR` owns
/// its own undo buffer inside the pty.
/// The stable identity of an indexed message: its Message-ID plus the
/// account and folder it currently lives in. The same Message-ID can
/// exist in several places at once — Gmail copies one message into both
/// Inbox and All Mail, and the same list mail is delivered to two
/// accounts — so msgid alone no longer identifies a row. Flag toggles,
/// cross-folder moves, and their undo all target the full triple so an
/// action can never land on a same-msgid copy in a different account or
/// folder (which previously relocated mail into the wrong account).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsgRef {
    pub msgid: String,
    pub account: String,
    pub folder: String,
}

impl MsgRef {
    pub fn of(row: &MessageRow) -> Self {
        Self {
            msgid: row.msgid.clone(),
            account: row.account.clone(),
            folder: row.folder.clone(),
        }
    }

    /// True iff `row` is the exact copy this ref names.
    pub fn matches(&self, row: &MessageRow) -> bool {
        row.msgid == self.msgid && row.account == self.account && row.folder == self.folder
    }
}

#[derive(Debug, Clone)]
pub enum UndoAction {
    Move {
        msgid: String,
        src_account: String,
        src_folder: String,
        /// Recorded for status text ("undid: move to Archive") and for
        /// the redo direction. Identifies the post-action location.
        dst_folder: String,
    },
    Flag {
        msgid: String,
        /// Account + folder the flagged message lives in. A flag toggle
        /// doesn't move the message, so these are stable across the
        /// undo/redo cycle and pin the exact copy to re-flag.
        account: String,
        folder: String,
        flag: char,
        /// Whether the flag was set *before* the user's action. Drives
        /// `FlagOp::Add` (restore set) or `FlagOp::Remove` (restore
        /// unset) on undo; explicit Add/Remove instead of Toggle so a
        /// concurrent sync flipping the flag in between can't double
        /// our work.
        was_set: bool,
    },
    /// Group of leaf actions that should be undone/redone as a unit.
    /// Children are applied in forward order; on undo, the inverse is
    /// applied in reverse order and the collected inverses form a new
    /// `Batch` for the opposite stack. Children whose inverse fails are
    /// skipped — their failures surface in the status row but don't
    /// strand the rest of the group.
    Batch(Vec<UndoAction>),
}

/// In-memory undo/redo stacks for message-view mutations. Bounded so a
/// long session doesn't accumulate stale entries; lost on restart by
/// design (the index is truth, the stack is convenience).
#[derive(Debug, Default)]
pub struct UndoStack {
    undo: VecDeque<UndoAction>,
    redo: VecDeque<UndoAction>,
}

impl UndoStack {
    const CAP: usize = 50;

    pub fn new() -> Self {
        Self::default()
    }

    /// Record a new user-initiated action. Clears the redo trail —
    /// pressing `u` then taking a fresh action invalidates the
    /// redo branch (standard editor convention).
    pub fn record(&mut self, action: UndoAction) {
        if self.undo.len() == Self::CAP {
            self.undo.pop_front();
        }
        self.undo.push_back(action);
        self.redo.clear();
    }

    pub fn pop_undo(&mut self) -> Option<UndoAction> {
        self.undo.pop_back()
    }

    pub fn pop_redo(&mut self) -> Option<UndoAction> {
        self.redo.pop_back()
    }

    /// Push the inverse onto the redo stack after a successful undo.
    /// Does not clear; redo→undo→redo cycles must be lossless.
    pub fn push_redo(&mut self, action: UndoAction) {
        if self.redo.len() == Self::CAP {
            self.redo.pop_front();
        }
        self.redo.push_back(action);
    }

    /// Push the inverse onto the undo stack after a successful redo.
    /// Does not clear redo.
    pub fn push_undo(&mut self, action: UndoAction) {
        if self.undo.len() == Self::CAP {
            self.undo.pop_front();
        }
        self.undo.push_back(action);
    }

    #[cfg(test)]
    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    #[cfg(test)]
    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }
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
    /// Digit buffer for the reader attachment picker (`gf`), mirroring
    /// `link_pick_buf`. Active only in `Mode::AttachmentPick`.
    pub attachment_pick_buf: String,
    /// Pending `g` prefix in Normal mode. The user has typed `g`; the
    /// next key decides what it composes — today only `/` for global
    /// search, future room for `gg`/`G` etc.
    pub pending_g: bool,
    /// Pending `y` sequence in Normal mode (Reader focus only). `None`
    /// when idle; `y` arms it with an empty buffer and each subsequent key
    /// is pushed on. Recognised sequences: `yl` (link), `yip` (inner
    /// paragraph), `yap` (a paragraph — block + trailing newline). `yi` /
    /// `ya` are kept as live prefixes; anything else clears the buffer and
    /// falls through to the rest of the Reader keymap.
    pub pending_y: Option<String>,
    /// Pending numeric count in Normal mode (`3j`, `5w`). `None` when
    /// idle; digits accumulate here and the next motion multiplies by it.
    /// `0` is a motion (line-start) unless a count is already building.
    pub pending_count: Option<usize>,
    /// Pending `z` prefix for scroll-positioning chords (`zz` / `zt` /
    /// `zb`). Mirrors `pending_g`.
    pub pending_z: bool,
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
    /// `[reader].osc8_links`: whether the reader emits OSC 8 terminal
    /// hyperlinks around links. Read once at startup; the reader draw
    /// consults it each frame.
    pub osc8_links: bool,
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
    /// In-flight `:send` workers. `:send` closes the compose tab
    /// immediately and pushes the worker's receiver here so completion
    /// surfaces in the cmdline status row whichever tab the user is on.
    /// `label` is a short subject-or-recipients identifier so the
    /// status message disambiguates when multiple sends overlap.
    pub pending_sends: Vec<PendingSend>,
    /// In-flight clipboard-fallback worker. `Some` while a yank pipe is
    /// running; cleared on completion. Concurrent yanks pile up
    /// receivers and the most recent one wins for status display —
    /// fine because real fallback commands return in milliseconds.
    pub clipboard_rx: Option<Receiver<ClipboardResult>>,
    /// Native address-book cache (recipients harvested from each
    /// account's Sent folder at startup). Empty until
    /// `address_book_rx` reports back; the compose popup just sees no
    /// native matches in the meantime.
    pub address_book: AddressBook,
    /// Startup-walk receiver for the native address book. `Some` until
    /// the worker drops its single message; then cleared. Only one
    /// startup run per session.
    pub address_book_rx: Option<Receiver<AddressBookResult>>,
    /// Per-keystroke external `query_command` receiver. Exactly one in
    /// flight at a time; subsequent queries land in
    /// `address_pending_query` and dispatch when the in-flight worker
    /// completes (or the debounce elapses, whichever is later).
    pub address_ext_rx: Option<Receiver<ExtResult>>,
    /// Query string the currently-in-flight external worker is
    /// resolving. `None` when nothing is running. Used to dedupe the
    /// dispatcher and to filter stale results.
    pub address_in_flight_query: Option<String>,
    /// Query the user has typed and we have *not yet* dispatched —
    /// either because the debounce hasn't fired, or because an in-flight
    /// worker is still running on an older query.
    pub address_pending_query: Option<String>,
    /// Earliest time the debounce permits dispatching
    /// `address_pending_query`. The main loop clamps `recv_timeout` to
    /// this deadline so the worker fires on schedule without busy
    /// polling.
    pub address_debounce_until: Option<Instant>,
    /// Clone of the unified event channel so the sync worker (and any
    /// future workers spawned from cmdline dispatch) can push
    /// `AppEvent::Wake` events. `None` in tests where no event loop is
    /// running.
    pub event_tx: Option<Sender<AppEvent>>,
    /// Set by `finalize_finished_editors` when an embedded `$EDITOR`
    /// exits — nvim/vim typically leaves the host cursor in whatever
    /// shape `guicursor` last requested (often a bar). The main draw
    /// loop drains this and emits `CSI 0 SP q` so the rest of the app
    /// (and the user's shell after exit) doesn't inherit a stuck shape.
    pub cursor_style_reset_pending: bool,
    /// Last DECSCUSR shape param emitted for the native compose body
    /// editor (steady block in Normal/Visual, steady bar in Insert).
    /// Tracked here so the main draw loop only emits the escape on
    /// actual transitions instead of every frame. `None` means we
    /// haven't emitted a native-editor shape yet (or it has since been
    /// reset to the terminal default).
    pub native_cursor_shape_emitted: Option<u16>,
    /// Undo/redo for message-view mutations (moves + flag flips).
    /// Recorded by the user-initiated wrappers (`toggle_flag_selected_user`,
    /// `move_selected_to_user`) — internal callsites like auto-Seen
    /// bypass it on purpose.
    pub undo_stack: UndoStack,
}

/// A user-visible screen with its own tab in the strip and its own
/// per-screen state. `Compose` is boxed to push the heavy
/// `BodyEditor` / overlay state onto the heap; `InboxScreen` is left
/// inline because most of its bulk (`search`, `parsed`, image cache)
/// is already heap-owned via internal `Box`/`Vec`/`HashMap`. The
/// remaining size delta between the two variants is intrinsic to the
/// design — the inbox carries the live scan/watcher state — so the
/// `large_enum_variant` lint is allowed locally rather than spent on
/// boxing both variants.
#[allow(clippy::large_enum_variant)]
pub enum Screen {
    Inbox(InboxScreen),
    Compose(Box<ComposeScreen>),
}

/// One in-flight send worker. Owned by `App` (not the compose tab) so
/// `:send` can close the tab immediately and the result still surfaces
/// when the worker reports back. `cancel_tx` fires the "undo send"
/// window; firing it before `[compose].send_delay_secs` elapses aborts
/// the send (worker returns `SendOutcome::Cancelled`).
pub struct PendingSend {
    pub rx: Receiver<SendResult>,
    pub cancel_tx: Sender<()>,
    pub label: String,
    /// File the composer was loaded from in `Drafts/cur/`, snapshotted
    /// at `:send` time. Deleted on a successful send so the draft
    /// doesn't linger after the message goes out. Left alone on
    /// `SentNoCopy` and on send failure so the user has a recovery
    /// copy. `None` for fresh-compose / reply / forward sends.
    pub origin_draft_path: Option<PathBuf>,
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
    /// `LaidOutBody.line_text[reader_cursor_line]` — a *logical* position,
    /// not a display cell. Movement (`h`/`l`/`0`/`$`) and text extraction
    /// work in this char space; the reader's paint path converts to the
    /// display cell via `reader::cell_col` (and the mouse converts a
    /// clicked cell back with `reader::char_at_cell`), so zero-width and
    /// wide characters land overlays on the right cells.
    pub reader_cursor_col: u16,
    /// Vim "curswant": the column vertical motion (`j`/`k`, page moves)
    /// tries to keep across lines of differing length. `u16::MAX` is the
    /// end-of-line sentinel (set by `$`), so vertical motion rides each
    /// line's end. Horizontal / explicit-column moves reset it to the
    /// resulting column. `reader_cursor_col` is the *live* (trued-up)
    /// column; this is the *goal* a vertical move re-sources from.
    pub reader_goal_col: u16,
    /// Vim-style visual-mode anchor. `Some` iff `Mode::Visual` is the
    /// active mode (strict pairing — see `Mode::Visual` doc). Char-wise
    /// vs line-wise lives on `kind`; the cursor end of the selection is
    /// `(reader_cursor_line, reader_cursor_col)`.
    pub visual: Option<VisualState>,
    /// Body-relative line index of each attachment row from the last
    /// reader draw, indexed by attachment (so `[i]` is the body line that
    /// renders attachment `i`). Stashed so `gx`/`gs` can resolve "the
    /// attachment under the cursor" without re-running layout. Empty when
    /// the open message has no attachments.
    pub last_attachment_lines: Vec<u16>,
    /// Plain text of each body line (excluding headers) from the last
    /// reader draw — a clone of the laid-out `LaidOutBody.line_text`.
    /// Stashed so the mouse handler can map a clicked display cell back
    /// to a char index (`reader_cursor_col` is a char index), which
    /// needs the line content and would otherwise require re-running
    /// layout. Empty when the reader drew no body this frame.
    pub last_reader_body_line_text: Vec<String>,
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
    /// Transient highlight on the just-yanked region. `Some` between
    /// `yp` / `yl` / visual-mode `y` firing and `expires_at` lapsing.
    /// The painter flips `Modifier::REVERSED` on covered cells each
    /// frame; `tick` clears the entry once expired. Mirrors vim's
    /// `vim-highlightedyank`. Lives next to the visual state because
    /// both are reader-pane transients keyed by the same coord space.
    pub yank_highlight: Option<crate::ui::reader::YankHighlight>,
    pub scan: ScanState,
    pub selected: usize,
    /// Anchor row for a list-pane multi-select ("visual line" over the
    /// message list). `Some` while the user is building a selection with
    /// `v` / `V`; the other end is `selected`, so the range covers
    /// `min(anchor, selected)..=max(anchor, selected)`. Distinct from
    /// `visual` (reader text selection). Survives into `Mode::Command` so
    /// `:mv` / `:archive` / `:trash` can act on the range, vim-style.
    /// Cleared on scope switch, search entry/exit, and after any range
    /// action consumes it.
    pub list_visual: Option<usize>,
    /// Persisted top-row offset for the messages list. The list pane
    /// renders only the visible window (`rows[offset..offset+height]`)
    /// rather than building a `ListItem` per folder row every frame, so
    /// it owns the scroll math: `list::clamp_offset` slides this the
    /// minimum amount to keep `selected` within a `SCROLL_PADDING` band
    /// of the viewport edges (vim `scrolloff`). Reset to 0 on scope
    /// switch.
    pub list_offset: usize,
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
    /// Inner message-list height (after border) from the previous draw.
    /// Drives the Ctrl-d/u/f/b page-step in the list pane.
    pub last_list_inner_height: u16,
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
    /// Receiver for the background catch-up worker that walks every
    /// non-INBOX folder after the eager INBOX scan returns. Separate
    /// channel so a long catch-up doesn't block (or get blocked by)
    /// the watcher's per-folder rescans. `None` once consumed.
    catchup_rx: Option<Receiver<ScanResult>>,
    /// Event channel clone, handed to the folder-switch worker so it can
    /// `Wake` the main loop when its result lands. `None` in tests where
    /// no channel is plumbed in (the result is then drained by the next
    /// `poll_switch` on the idle heartbeat).
    event_tx: Option<Sender<AppEvent>>,
    /// In-flight off-thread folder switch. `switch_to_scope` spawns the
    /// worker (index read + JWZ build for the target scope) and parks the
    /// receiver here; `poll_switch` drains it. Replaced on every switch,
    /// which drops the prior receiver so a superseded worker's result is
    /// discarded at the channel.
    switch_rx: Option<Receiver<scan::SwitchOutcome>>,
    /// Monotonic switch counter, bumped on every `switch_to_scope`. The
    /// apply step discards any result whose generation no longer matches
    /// — latest target wins when the user cycles folders faster than the
    /// loads finish (Alt-j Alt-j Alt-j). Belt-and-suspenders alongside
    /// the receiver-replacement above.
    switch_generation: u64,
    /// `(account, folder)` pairs whose maildir contents have been
    /// walked at least once this session. Drives the lazy-on-switch
    /// path: scope-switching to a pair not in this set kicks an
    /// immediate rescan instead of waiting for the catch-up worker.
    /// Populated by the eager INBOX scan, the catch-up worker, and
    /// every rescan-result apply.
    scanned_folders: HashSet<(String, String)>,
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
    /// Bumped on every optimistic in-memory mutation a racing rescan
    /// could clobber — a row dropped by a cross-folder move
    /// (`drop_row_after_move`) or a flag patched in place
    /// (`apply_flag_change`). A rescan worker walks the disk
    /// asynchronously, so one kicked *before* such a mutation can carry
    /// pre-mutation state (e.g. a just-trashed message still present in
    /// its old folder). `apply_rescan` discards any result whose
    /// `rescan_kick_epoch` no longer matches and re-queues the folders,
    /// so the stale walk can't resurrect a removed row or revert a flag.
    optimistic_epoch: u64,
    /// `optimistic_epoch` captured when the in-flight `rescan_rx` was
    /// kicked. Compared on result to detect a mutation that raced the
    /// walk.
    rescan_kick_epoch: u64,
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
        // Kick off the native-address-book startup walk now so the
        // popup has data by the time the user opens their first
        // compose tab. Worker pushes a single result into
        // `address_book_rx`; the per-tick `poll_address_book` drains
        // it into `address_book`.
        let address_book_rx = {
            let specs = account_specs(cfg);
            if specs.is_empty() {
                None
            } else {
                Some(addressbook::start_addressbook_worker(specs))
            }
        };
        Self {
            mode: Mode::Normal,
            cmdline: TextInput::new(),
            link_pick_buf: String::new(),
            attachment_pick_buf: String::new(),
            pending_g: false,
            pending_y: None,
            pending_count: None,
            pending_z: false,
            status_error: watcher_warning,
            quit: false,
            screens: vec![Screen::Inbox(inbox)],
            active: 0,
            picker,
            image_max_height_cells: cfg.images.max_height_cells,
            osc8_links: cfg.reader.osc8_links,
            cache_path,
            self_writes,
            sync_rx: None,
            pending_sends: Vec::new(),
            clipboard_rx: None,
            address_book: AddressBook::new(),
            address_book_rx,
            address_ext_rx: None,
            address_in_flight_query: None,
            address_pending_query: None,
            address_debounce_until: None,
            event_tx,
            cursor_style_reset_pending: false,
            native_cursor_shape_emitted: None,
            undo_stack: UndoStack::new(),
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
        self.screens.push(Screen::Compose(Box::new(screen)));
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

    /// Read-only borrow of the currently-active compose screen, if any.
    pub fn active_compose(&self) -> Option<&ComposeScreen> {
        match self.screens.get(self.active)? {
            Screen::Compose(s) => Some(s),
            _ => None,
        }
    }

    /// Drain finished send workers from `pending_sends`. `:send` closes
    /// the compose tab synchronously, so the worker's result has no tab
    /// to land on — it surfaces in the cmdline status row instead.
    /// Successful sends overwrite any in-flight "sending: …" message
    /// the user might still be looking at.
    pub fn poll_pending_sends(&mut self) {
        let mut i = 0;
        while i < self.pending_sends.len() {
            match self.pending_sends[i].rx.try_recv() {
                Ok(result) => {
                    let pending = self.pending_sends.swap_remove(i);
                    // Clean up the originating draft only on full send
                    // success — `SentNoCopy` and `Err` both leave the
                    // user with something they may want to revisit, so
                    // the draft stays put.
                    if let (Some(path), Ok(SendOutcome::Sent)) =
                        (pending.origin_draft_path.as_ref(), &result)
                    {
                        self.self_writes.record(path);
                        match std::fs::remove_file(path) {
                            Ok(()) => {}
                            Err(_) => {
                                // The send went through; missing draft
                                // file is best-effort cleanup. Drop the
                                // self-write record so a future write
                                // at the same path isn't suppressed.
                                self.self_writes.consume(path);
                            }
                        }
                    }
                    self.status_error = Some(format_send_status(&pending.label, result));
                }
                Err(TryRecvError::Empty) => {
                    i += 1;
                }
                Err(TryRecvError::Disconnected) => {
                    let pending = self.pending_sends.swap_remove(i);
                    self.status_error = Some(format!("send ({}): worker died", pending.label));
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

    /// Per-tick maintenance for address completion: ingest the
    /// startup-walk result into the native cache, drain any external
    /// `query_command` worker that finished, and dispatch a pending
    /// query if the debounce has elapsed and no worker is in flight.
    /// Mirrors `poll_sync` / `poll_clipboard`'s receiver-drain shape;
    /// the dispatch path is the extra bit specific to this subsystem.
    pub fn poll_address_book(&mut self, cfg: &Config) {
        // Drain the startup walk's single result. Worker exits after
        // sending; either branch clears the receiver.
        if let Some(rx) = self.address_book_rx.as_ref() {
            match rx.try_recv() {
                Ok(result) => {
                    self.address_book.set_native(result.contacts);
                    self.address_book_rx = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.address_book_rx = None;
                }
            }
        }

        // Drain external `query_command` result, if any.
        if let Some(rx) = self.address_ext_rx.as_ref() {
            match rx.try_recv() {
                Ok(ExtResult { query, outcome }) => {
                    self.address_ext_rx = None;
                    self.address_in_flight_query = None;
                    match outcome {
                        Ok(contacts) => self.apply_external_results(&query, contacts),
                        Err(e) => {
                            self.status_error = Some(format!("query_command: {e}"));
                        }
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.address_ext_rx = None;
                    self.address_in_flight_query = None;
                }
            }
        }

        // Dispatch a queued query when debounce has elapsed and no
        // worker is in flight. Note: keeping the deadline armed while
        // a worker is running means the next query won't fire until
        // the current one completes; that bounds the dispatch rate at
        // one in-flight worker without per-keystroke process kills.
        let ready = self
            .address_debounce_until
            .is_some_and(|d| Instant::now() >= d);
        if !ready || self.address_ext_rx.is_some() || self.address_pending_query.is_none() {
            return;
        }
        let Some(raw) = cfg.compose.address_book.query_command.as_deref() else {
            // query_command got cleared after we armed the timer; drop
            // the pending request silently.
            self.address_pending_query = None;
            self.address_debounce_until = None;
            return;
        };
        let argv: Vec<String> = raw.split_whitespace().map(String::from).collect();
        if argv.is_empty() {
            self.address_pending_query = None;
            self.address_debounce_until = None;
            return;
        }
        let query = self.address_pending_query.take().expect("checked above");
        self.address_debounce_until = None;
        self.address_in_flight_query = Some(query.clone());
        self.address_ext_rx = Some(addressbook_external::start_query_worker(
            argv,
            query,
            self.event_tx.clone(),
        ));
    }

    /// Refresh the active compose tab's popup state after a TextInput
    /// edit. Called by the host loop after `compose::handle_key`
    /// consumes a key; closes the popup when focus moved off a
    /// recipient field or the token is too short, opens/refreshes it
    /// otherwise. Arms the external debounce when `query_command` is
    /// configured and the user has typed a new prefix.
    pub fn refresh_address_complete(&mut self, cfg: &Config) {
        let Self {
            screens,
            active,
            address_book,
            address_in_flight_query,
            address_pending_query,
            address_debounce_until,
            ..
        } = self;
        let Some(Screen::Compose(c)) = screens.get_mut(*active) else {
            return;
        };
        // Higher-priority overlays preempt the popup.
        if c.confirm_close.is_some() || c.from_picker.is_some() {
            c.address_complete = None;
            return;
        }
        // The popup is an Insert-mode affordance: in header Normal mode the
        // keys are motions/edits, not text the user is composing, so there's
        // nothing to complete.
        if c.header_mode != crate::ui::compose_header::HeaderMode::Insert {
            c.address_complete = None;
            return;
        }
        let field = c.focused;
        let input = match field {
            ComposeField::To => &c.to,
            ComposeField::Cc => &c.cc,
            ComposeField::Bcc => &c.bcc,
            _ => {
                c.address_complete = None;
                return;
            }
        };
        let Some((token_start, token)) = address_complete::extract_token(input) else {
            c.address_complete = None;
            c.address_complete_suppressed = None;
            return;
        };
        let token_lc = token.to_ascii_lowercase();
        if token_lc.chars().count() < cfg.compose.address_book.min_chars {
            c.address_complete = None;
            c.address_complete_suppressed = None;
            return;
        }
        // Esc-dismissal park: refuse to reopen on the same prefix the
        // user just dismissed. Token divergence (next keystroke)
        // clears the park.
        match c.address_complete_suppressed.as_deref() {
            Some(parked) if parked == token_lc => {
                c.address_complete = None;
                return;
            }
            Some(_) => c.address_complete_suppressed = None,
            None => {}
        }
        // Preserve previously-fetched external items for the *same*
        // token so a fresh native re-query doesn't drop them.
        let prev_external: Vec<addressbook::Contact> = c
            .address_complete
            .as_ref()
            .filter(|s| s.field == field && s.token == token_lc)
            .map(|s| {
                s.items
                    .iter()
                    .filter(|c| c.source == addressbook::Source::External)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let native = address_book.query_native(&token_lc, address_complete::MAX_ITEMS * 2);
        let items = addressbook::merge(prev_external, native, address_complete::MAX_ITEMS);
        // Preserve selection by email when possible so an item the
        // user was about to accept doesn't slide under their cursor.
        let new_selected = c
            .address_complete
            .as_ref()
            .and_then(|s| s.items.get(s.selected))
            .and_then(|prev| items.iter().position(|x| x.email_lc == prev.email_lc))
            .unwrap_or(0);
        c.address_complete = Some(AddressCompleteState {
            field,
            token_start,
            token: token_lc.clone(),
            items,
            selected: new_selected,
        });

        // External dispatch: only arm when configured, and only when
        // the token differs from whatever the running worker is
        // already chasing (or what's already queued).
        if cfg
            .compose
            .address_book
            .query_command
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        {
            let in_flight = address_in_flight_query.as_deref() == Some(token_lc.as_str());
            let pending = address_pending_query.as_deref() == Some(token_lc.as_str());
            if !in_flight && !pending {
                *address_pending_query = Some(token_lc);
                *address_debounce_until = Some(
                    Instant::now() + Duration::from_millis(cfg.compose.address_book.debounce_ms),
                );
            }
        }
    }

    /// Splice an external worker's result into the active compose
    /// tab's popup, if it's still relevant (same field, same token).
    /// Stale results are silently dropped — the user has already
    /// typed past whatever query produced them.
    fn apply_external_results(&mut self, query: &str, external: Vec<addressbook::Contact>) {
        let Self {
            screens,
            active,
            address_book,
            ..
        } = self;
        let Some(Screen::Compose(c)) = screens.get_mut(*active) else {
            return;
        };
        let Some(state) = c.address_complete.as_mut() else {
            return;
        };
        if state.token != query {
            return;
        }
        let native = address_book.query_native(&state.token, address_complete::MAX_ITEMS * 2);
        let items = addressbook::merge(external, native, address_complete::MAX_ITEMS);
        // Preserve selection by email.
        let new_selected = state
            .items
            .get(state.selected)
            .and_then(|prev| items.iter().position(|x| x.email_lc == prev.email_lc))
            .unwrap_or(0);
        state.items = items;
        state.selected = new_selected;
    }

    /// Earliest wake the address-book subsystem cares about — the
    /// debounce deadline when one is armed and a worker isn't in
    /// flight. Returned to the main loop so `recv_timeout` clamps to
    /// it and the query dispatcher fires within the configured window.
    pub fn address_debounce_remaining(&self) -> Option<Duration> {
        let deadline = self.address_debounce_until?;
        if self.address_ext_rx.is_some() {
            // A worker is already running; the deadline will be
            // re-evaluated when the worker completes and pushes Wake.
            return None;
        }
        Some(deadline.saturating_duration_since(Instant::now()))
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

    pub fn poll_scan(&mut self, cfg: &Config) {
        let Self {
            screens,
            cache_path,
            status_error,
            ..
        } = self;
        let Some(Screen::Inbox(inbox)) = screens.get_mut(0) else {
            unreachable!("inbox is pinned at index 0")
        };
        inbox.poll_scan(cfg, cache_path, status_error);
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

    /// Drain the off-thread folder-switch worker into the list view.
    /// Called every main-loop tick alongside `poll_scan` / `poll_watch`.
    pub fn poll_switch(&mut self) {
        let Self {
            screens,
            status_error,
            ..
        } = self;
        let Some(Screen::Inbox(inbox)) = screens.get_mut(0) else {
            unreachable!("inbox is pinned at index 0")
        };
        inbox.poll_switch(status_error);
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
    /// `d` → Trashed. Pushes an undo entry on success — the internal
    /// auto-Seen path lives on `InboxScreen::try_mark_seen` and bypasses
    /// this method on purpose (so opening a message doesn't pollute the
    /// undo stack).
    pub fn toggle_flag_selected(&mut self, flag: char) {
        let Some(target) = self.inbox().selected_message_row().map(MsgRef::of) else {
            return;
        };
        if let Some(action) = self.toggle_flag_msgid(flag, &target) {
            self.undo_stack.record(action);
        }
    }

    /// Msgid-targeted flag toggle that returns the would-be `UndoAction`
    /// instead of recording it — multi-row callers (the list-visual
    /// range) collect these into a single `UndoAction::Batch` so the
    /// whole selection undoes in one `u`. Mirrors [`App::move_msgid_to`].
    /// Single-row callers should prefer [`App::toggle_flag_selected`],
    /// which records for them.
    pub fn toggle_flag_msgid(&mut self, flag: char, target: &MsgRef) -> Option<UndoAction> {
        let was_set = self
            .inbox()
            .find_row(target)
            .map(|r| r.flags.contains(flag))?;
        let ok = {
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
            inbox.toggle_flag_msgid(flag, target, cache_path, self_writes, status_error)
        };
        if ok {
            Some(UndoAction::Flag {
                msgid: target.msgid.clone(),
                account: target.account.clone(),
                folder: target.folder.clone(),
                flag,
                was_set,
            })
        } else {
            None
        }
    }

    /// Start or cancel a list-pane multi-select. Anchored at the current
    /// selection; the other end follows `selected` as the user moves with
    /// `j` / `k`. No-op unless the List pane is focused — a "selection"
    /// over the reader or sidebar is meaningless.
    pub fn toggle_list_visual(&mut self) {
        let inbox = self.inbox_mut();
        if inbox.focus != Pane::List {
            return;
        }
        inbox.list_visual = if inbox.list_visual.is_some() {
            None
        } else {
            Some(inbox.selected)
        };
    }

    /// Move the selected row's message file into `target_folder` of the
    /// owning account. Drives `:archive` / `:spam` / `:trash` / `:mv`.
    /// The folder name is the Maildir++ label (e.g. `"Archive"`), not the
    /// on-disk `.Archive` directory. The target maildir is created if
    /// missing. Pushes an undo entry on success.
    pub fn move_selected_to(&mut self, target_folder: &str, cfg: &Config) {
        let Some(target) = self.inbox().selected_message_row().map(MsgRef::of) else {
            return;
        };
        if let Some(action) = self.move_msgid_to(&target, target_folder, cfg) {
            self.undo_stack.record(action);
        }
    }

    /// Move-by-msgid variant that returns the would-be `UndoAction`
    /// instead of recording it. Multi-row callers (trash-thread) collect
    /// these into a single `UndoAction::Batch` so the whole operation
    /// undoes in one `u`. Single-row callers should prefer
    /// [`App::move_selected_to`], which records for them.
    pub fn move_msgid_to(
        &mut self,
        target: &MsgRef,
        target_folder: &str,
        cfg: &Config,
    ) -> Option<UndoAction> {
        // The move's source identity *is* `target`; confirm the row is
        // actually present before acting.
        self.inbox().find_row(target)?;
        let ok = {
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
            inbox.move_msgid_to(
                target_folder,
                target,
                &cfg.accounts,
                cache_path,
                self_writes,
                status_error,
            )
        };
        if ok {
            Some(UndoAction::Move {
                msgid: target.msgid.clone(),
                src_account: target.account.clone(),
                src_folder: target.folder.clone(),
                dst_folder: target_folder.to_string(),
            })
        } else {
            None
        }
    }

    /// Pop the most recent message-view mutation off the undo stack and
    /// apply its inverse. The inverse is pushed onto the redo stack so
    /// `Ctrl-r` can re-execute the original. Failure modes (msgid not in
    /// index, src folder no longer configured, rename failed) surface in
    /// `status_error` and discard the action — they don't re-push.
    pub fn undo(&mut self, cfg: &Config) {
        let Some(action) = self.undo_stack.pop_undo() else {
            self.status_error = Some("nothing to undo".into());
            return;
        };
        if let Some(inverse) = self.apply_inverse(action, cfg, "undid") {
            self.undo_stack.push_redo(inverse);
        }
    }

    /// Mirror of [`App::undo`] for the `Ctrl-r` direction.
    pub fn redo(&mut self, cfg: &Config) {
        let Some(action) = self.undo_stack.pop_redo() else {
            self.status_error = Some("nothing to redo".into());
            return;
        };
        if let Some(inverse) = self.apply_inverse(action, cfg, "redid") {
            self.undo_stack.push_undo(inverse);
        }
    }

    /// Shared body of [`App::undo`] / [`App::redo`]: locate the message
    /// by msgid via the index (paths drift on sync, msgid is stable),
    /// replay the inverse rename, update folder stats + in-memory views,
    /// return the inverse action so the caller can push it onto the
    /// opposite stack. `verb` shapes the status-line text only.
    fn apply_inverse(
        &mut self,
        action: UndoAction,
        cfg: &Config,
        verb: &str,
    ) -> Option<UndoAction> {
        if let UndoAction::Batch(children) = action {
            // Undo children in reverse order — the last action applied
            // is the first to be reversed. Inverses accumulate in the
            // order they're produced, then get reversed once at the end
            // so the resulting Batch replays correctly in the opposite
            // direction. Per-child failures surface in `status_error`
            // but don't strand the rest of the batch.
            let total = children.len();
            let mut inverses: Vec<UndoAction> = Vec::with_capacity(total);
            for child in children.into_iter().rev() {
                if let Some(inv) = self.apply_inverse(child, cfg, verb) {
                    inverses.push(inv);
                }
            }
            inverses.reverse();
            let done = inverses.len();
            self.status_error = Some(format!("{verb}: {done} of {total}"));
            return Some(UndoAction::Batch(inverses));
        }
        // Re-locate the exact copy this action touched. A flag toggle
        // leaves the message in place, so it lives at (account, folder).
        // A move put it at (src_account, dst_folder) — account is
        // unchanged cross-folder, and dst_folder is where it sits now —
        // so that triple locates it for the reverse move.
        let (msgid, locate_account, locate_folder) = match &action {
            UndoAction::Flag {
                msgid,
                account,
                folder,
                ..
            } => (msgid.clone(), account.clone(), folder.clone()),
            UndoAction::Move {
                msgid,
                src_account,
                dst_folder,
                ..
            } => (msgid.clone(), src_account.clone(), dst_folder.clone()),
            UndoAction::Batch(_) => unreachable!("handled above"),
        };
        let current = match Index::open(&self.cache_path)
            .and_then(|idx| idx.get(&msgid, &locate_account, &locate_folder))
        {
            Ok(Some(row)) => row,
            Ok(None) => {
                self.status_error = Some(format!("{verb}: {msgid} no longer indexed"));
                return None;
            }
            Err(e) => {
                self.status_error = Some(format!("{verb}: index lookup failed: {e:#}"));
                return None;
            }
        };
        match action {
            UndoAction::Flag {
                msgid,
                account,
                folder,
                flag,
                was_set,
            } => {
                let op = if was_set { FlagOp::Add } else { FlagOp::Remove };
                match flags::set_flag_recorded(&current.path, flag, op, &self.self_writes) {
                    Ok((new_path, new_flags)) => {
                        let Self {
                            screens,
                            cache_path,
                            status_error,
                            ..
                        } = self;
                        let Some(Screen::Inbox(inbox)) = screens.get_mut(0) else {
                            unreachable!("inbox is pinned at index 0")
                        };
                        inbox.apply_flag_undo(
                            &current,
                            &new_path,
                            &new_flags,
                            cache_path,
                            status_error,
                        );
                        *status_error = Some(format!("{verb}: flag {flag}"));
                        Some(UndoAction::Flag {
                            msgid,
                            account,
                            folder,
                            flag,
                            was_set: !was_set,
                        })
                    }
                    Err(e) => {
                        self.status_error = Some(format!("{verb}: {e}"));
                        None
                    }
                }
            }
            UndoAction::Move {
                msgid,
                src_account,
                src_folder,
                dst_folder,
            } => {
                let Some(account_cfg) = cfg.accounts.get(&src_account) else {
                    self.status_error = Some(format!("{verb}: unknown account {src_account}"));
                    return None;
                };
                let spec = AccountSpec::from_account(&src_account, account_cfg);
                let Some(binding) = spec.binding_by_label(&src_folder) else {
                    self.status_error = Some(format!(
                        "{verb}: {src_folder} not configured on {src_account}"
                    ));
                    return None;
                };
                let folder_root = binding.path.clone();
                let target_cur = folder_root.join("cur");
                if let Err(e) = flags::ensure_maildir(&folder_root) {
                    self.status_error = Some(format!("{verb}: create {src_folder}: {e}"));
                    return None;
                }
                // Idempotent watcher registration so external writes to a
                // freshly-created source folder still surface.
                if let Some(w) = self.inbox().watcher.as_ref() {
                    w.register_folder(&src_account, &src_folder, &folder_root, account_cfg.layout);
                }
                match flags::move_to_folder_recorded(
                    &current.path,
                    &target_cur,
                    &current.flags,
                    &self.self_writes,
                ) {
                    Ok(new_path) => {
                        let original_dst_account = current.account.clone();
                        let original_dst_folder = current.folder.clone();
                        let Self {
                            screens,
                            cache_path,
                            status_error,
                            ..
                        } = self;
                        let Some(Screen::Inbox(inbox)) = screens.get_mut(0) else {
                            unreachable!("inbox is pinned at index 0")
                        };
                        inbox.apply_move_undo(
                            &current,
                            &src_folder,
                            &new_path,
                            cache_path,
                            status_error,
                        );
                        *status_error = Some(format!("{verb}: move to {dst_folder}"));
                        // Inverse: the "src" of the next-direction action is
                        // where the message is *now* (= the original dst);
                        // the "dst" is where it was before this undo (= the
                        // original src). Account doesn't change cross-folder.
                        Some(UndoAction::Move {
                            msgid,
                            src_account: original_dst_account,
                            src_folder: original_dst_folder,
                            dst_folder: src_folder,
                        })
                    }
                    Err(e) => {
                        self.status_error = Some(format!("{verb}: {e}"));
                        None
                    }
                }
            }
            UndoAction::Batch(_) => unreachable!("Batch is dispatched at the top of apply_inverse"),
        }
    }

    // --- Convenience pass-throughs so keys.rs doesn't reach into inbox
    // for every keystroke. Step 1: all key dispatch is inbox-only. ---

    pub fn cycle_focus(&mut self, forward: bool) {
        self.inbox_mut().cycle_focus(forward);
    }

    /// Enter `g/` (local) search: snapshot the current scope's rows from
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

    /// Enter `/` (global) search using `[search].global_folders`
    /// (or every folder when the list is empty/unset), filtered to the
    /// currently-selected account scope. This is the default broad search;
    /// `g/` narrows to the current folder.
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
            ..
        } = self;
        let Some(Screen::Inbox(inbox)) = screens.get_mut(0) else {
            unreachable!("inbox is pinned at index 0")
        };
        inbox.switch_to_scope(account, folder, cache_path);
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

    pub fn select_last(&mut self) {
        self.inbox_mut().select_last();
    }

    /// Half/full-page navigation (Ctrl-d/u/f/b), routed by focus: the
    /// reader scrolls its body, the list moves its selection. No-op when
    /// the sidebar is focused.
    pub fn page_move(&mut self, down: bool, full: bool) {
        let inbox = self.inbox_mut();
        match inbox.focus {
            // Reader page keys move the cursor (scroll follows) so the
            // cursor stays addressable for `gx`/`gs` and visual entry.
            Pane::Reader => inbox.page_cursor(down, full),
            Pane::List => inbox.select_page(down, full),
            Pane::Folders => {}
        }
    }

    /// Ctrl-e / Ctrl-y: nudge the reader viewport one line without moving
    /// the cursor. The draw-time clamp re-seats the cursor onto the
    /// viewport edge if this scroll would push it off-screen (vim's
    /// scroll-line semantics). No-op unless the Reader pane is focused.
    pub fn scroll_reader_line(&mut self, down: bool) {
        let inbox = self.inbox_mut();
        if inbox.focus != Pane::Reader {
            return;
        }
        if down {
            let max = inbox
                .last_reader_body_lines
                .saturating_sub(inbox.last_reader_inner_height);
            inbox.reader_scroll = inbox.reader_scroll.saturating_add(1).min(max);
        } else {
            inbox.reader_scroll = inbox.reader_scroll.saturating_sub(1);
        }
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
        inbox.switch_to_scope(target_scope, &target_folder, cache_path);
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

        let accounts: Vec<AccountSpec> = account_specs(cfg);
        let scan_rx = if accounts.is_empty() {
            None
        } else {
            Some(scan::start_inbox_worker(
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
        // Kept for the folder-switch worker; the watcher block below
        // consumes the original `event_tx`.
        let switch_event_tx = event_tx.clone();
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
                    log::info!("app: watcher started for {} accounts", accounts.len());
                    watcher = Some(w);
                    watch_rx = Some(rx);
                }
                Err(e) => {
                    log::error!("app: watcher failed to start: {e:#}");
                    watcher_warning = Some(format!("watcher disabled: {e:#}"));
                }
            }
        } else {
            log::warn!(
                "app: watcher NOT started (watch.enabled={}, accounts={}, event_tx={})",
                cfg.watch.enabled,
                accounts.len(),
                switch_event_tx.is_some()
            );
        }

        Self {
            focus,
            sidebar_visible,
            list_visible,
            reader_visible,
            reader_scroll: 0,
            reader_cursor_line: 0,
            reader_cursor_col: 0,
            reader_goal_col: 0,
            visual: None,
            last_attachment_lines: Vec::new(),
            last_reader_body_line_text: Vec::new(),
            last_reader_header_offset: 0,
            last_reader_body_only_lines: 0,
            last_reader_inner_width: 0,
            last_reader_inner: None,
            mouse_drag_anchor: None,
            yank_highlight: None,
            scan,
            selected: 0,
            list_visual: None,
            list_offset: 0,
            parsed: None,
            last_parsed_msgid: None,
            image_cache: HashMap::new(),
            prev_parsed_msgid: None,
            last_image_rects: Vec::new(),
            last_reader_body_lines: 0,
            last_reader_inner_height: 0,
            last_list_inner_height: 0,
            body_changed_this_tick: false,
            folder_stats: Vec::new(),
            current_account: None,
            current_folder: "INBOX".to_string(),
            scan_rx,
            catchup_rx: None,
            event_tx: switch_event_tx,
            switch_rx: None,
            switch_generation: 0,
            scanned_folders: HashSet::new(),
            watcher,
            watch_rx,
            pending_dirty: HashSet::new(),
            rescan_rx: None,
            rescan_in_flight: HashSet::new(),
            optimistic_epoch: 0,
            rescan_kick_epoch: 0,
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
        // Horizontal: move the live column, clamp it to the current
        // line's last on-a-char position (so `l` stops at EOL instead of
        // silently overshooting), and reset the goal to it. Vertical and
        // horizontal are never combined by callers, but handling each
        // axis independently keeps the goal honest if they ever are.
        if dx != 0 {
            let raw = if dx >= 0 {
                self.reader_cursor_col.saturating_add(dx as u16)
            } else {
                self.reader_cursor_col
                    .saturating_sub((-dx).min(u16::MAX as i32) as u16)
            };
            self.reader_cursor_col = self
                .last_reader_body_line_text
                .get(self.reader_cursor_line as usize)
                .map(|l| raw.min(l.chars().count().saturating_sub(1) as u16))
                .unwrap_or(raw);
            self.reader_goal_col = self.reader_cursor_col;
        }
        // Vertical: re-source the live column from the goal (vim
        // curswant) so it survives short/empty lines. The goal itself is
        // left alone; the draw-time clamp (reader::draw) trues the live
        // column to the new line, but the next vertical move re-sources
        // from the goal so that clamping is harmless.
        if dy != 0 {
            let max_line = self.last_reader_body_only_lines.saturating_sub(1) as i32;
            self.reader_cursor_line =
                (self.reader_cursor_line as i32 + dy).clamp(0, max_line) as u16;
            self.reader_cursor_col = self.reader_goal_col;
        }
        self.follow_cursor();
    }

    pub fn move_reader_cursor_to_top(&mut self) {
        self.reader_cursor_line = 0;
        self.reader_cursor_col = 0;
        self.reader_goal_col = 0;
        self.follow_cursor();
    }

    pub fn move_reader_cursor_to_bottom(&mut self) {
        let max_line = self.last_reader_body_only_lines.saturating_sub(1);
        self.reader_cursor_line = max_line;
        // Vim `G` lands at the start of the last line, not its end —
        // matches `gg`, which parks at column 0.
        self.reader_cursor_col = 0;
        self.reader_goal_col = 0;
        self.follow_cursor();
    }

    pub fn move_reader_cursor_to_line_start(&mut self) {
        self.reader_cursor_col = 0;
        self.reader_goal_col = 0;
    }

    pub fn move_reader_cursor_to_line_end(&mut self) {
        // Sentinel: clamped to real line end at draw time. As the goal
        // it makes subsequent `j`/`k` ride each line's end (curswant =
        // MAXCOL) until a horizontal move resets it.
        self.reader_cursor_col = u16::MAX;
        self.reader_goal_col = u16::MAX;
    }

    /// Vim word motion over the laid-out body. Reads the per-frame
    /// `last_reader_body_line_text` directly (no re-layout) and drives the
    /// cursor through the shared [`words`](crate::ui::words) scanner, so
    /// the reader and composer agree on word boundaries. Used by both the
    /// reader-Normal keymap and the `MotionTarget` impl (reader-Visual).
    pub fn reader_word(&mut self, motion: crate::ui::words::WordMotion, big: bool) {
        if self.last_reader_body_line_text.is_empty() {
            return;
        }
        let (nr, nc) = crate::ui::words::word_motion(
            &self.last_reader_body_line_text,
            self.reader_cursor_line as usize,
            self.reader_cursor_col as usize,
            motion,
            big,
        );
        self.reader_cursor_line = nr as u16;
        self.reader_cursor_col = nc as u16;
        self.reader_goal_col = nc as u16;
        self.follow_cursor();
    }

    /// Vim `}` / `{` — move the cursor to the next / previous blank line
    /// (paragraph boundary) over the laid-out body, clamping at the ends.
    pub fn reader_paragraph(&mut self, forward: bool) {
        let lines = &self.last_reader_body_line_text;
        if lines.is_empty() {
            return;
        }
        let is_blank = |r: usize| lines.get(r).map(|l| l.trim().is_empty()).unwrap_or(true);
        let cur = self.reader_cursor_line as usize;
        let target = if forward {
            let mut r = cur + 1;
            while r < lines.len() && !is_blank(r) {
                r += 1;
            }
            r.min(lines.len().saturating_sub(1))
        } else if cur == 0 {
            0
        } else {
            let mut r = cur - 1;
            while r > 0 && !is_blank(r) {
                r -= 1;
            }
            r
        };
        self.reader_cursor_line = target as u16;
        self.reader_cursor_col = 0;
        self.reader_goal_col = 0;
        self.follow_cursor();
    }

    /// Vim `H` / `M` / `L` — park the cursor at the top / middle / bottom
    /// of the visible body viewport.
    pub fn reader_cursor_to_viewport(&mut self, pos: ViewportPos) {
        let inner_h = self.last_reader_inner_height;
        let body_lines = self.last_reader_body_only_lines;
        if inner_h == 0 || body_lines == 0 {
            return;
        }
        let header = self.last_reader_header_offset;
        let top_body = self.reader_scroll.saturating_sub(header);
        let bot_body = (top_body + inner_h.saturating_sub(1)).min(body_lines.saturating_sub(1));
        let target = match pos {
            ViewportPos::Top => top_body,
            ViewportPos::Middle => (top_body + bot_body) / 2,
            ViewportPos::Bottom => bot_body,
        };
        self.reader_cursor_line = target.min(body_lines.saturating_sub(1));
        self.reader_cursor_col = 0;
        self.reader_goal_col = 0;
        self.follow_cursor();
    }

    /// Vim `zt` / `zz` / `zb` — scroll so the cursor sits at the top /
    /// centre / bottom of the viewport, without moving the cursor.
    pub fn reader_scroll_cursor(&mut self, pos: ViewportPos) {
        let inner_h = self.last_reader_inner_height;
        if inner_h == 0 {
            return;
        }
        let abs = self
            .last_reader_header_offset
            .saturating_add(self.reader_cursor_line);
        let want = match pos {
            ViewportPos::Top => abs,
            ViewportPos::Middle => abs.saturating_sub(inner_h / 2),
            ViewportPos::Bottom => abs.saturating_sub(inner_h.saturating_sub(1)),
        };
        let total = self
            .last_reader_header_offset
            .saturating_add(self.last_reader_body_only_lines);
        let max_scroll = total.saturating_sub(inner_h);
        self.reader_scroll = want.min(max_scroll);
    }

    /// Attachment count for the open message (0 when no body parsed).
    pub fn attachment_count(&self) -> usize {
        self.parsed.as_deref().map_or(0, |p| p.attachments.len())
    }

    /// Resolve the 0-based attachment index whose inline row the reader
    /// cursor currently sits on, from the last draw's stashed line map.
    /// Falls back to the sole attachment when the cursor is elsewhere but
    /// exactly one attachment exists; returns `None` otherwise.
    pub fn attachment_under_cursor(&self) -> Option<usize> {
        if let Some(i) = self
            .last_attachment_lines
            .iter()
            .position(|&line| line == self.reader_cursor_line)
        {
            return Some(i);
        }
        (self.attachment_count() == 1).then_some(0)
    }
}

/// Reader motion impl. Cursor columns intentionally overshoot — the
/// laid-out body isn't available at keymap time, so `reader::draw`
/// clamps `reader_cursor_col` against `LaidOutBody.line_text[row]` once
/// it has the live width in hand. Word motions stay unimplemented (the
/// default no-op): word boundaries would need the body text too, and
/// the reader doesn't surface that to the keymap layer yet.
impl MotionTarget for InboxScreen {
    fn move_char_left(&mut self) {
        self.move_reader_cursor(0, -1);
    }
    fn move_char_right(&mut self) {
        self.move_reader_cursor(0, 1);
    }
    fn move_char_up(&mut self) {
        self.move_reader_cursor(-1, 0);
    }
    fn move_char_down(&mut self) {
        self.move_reader_cursor(1, 0);
    }
    fn move_line_start(&mut self) {
        self.move_reader_cursor_to_line_start();
    }
    fn move_line_end(&mut self) {
        self.move_reader_cursor_to_line_end();
    }
    fn move_first_line(&mut self) {
        self.move_reader_cursor_to_top();
    }
    fn move_last_line(&mut self) {
        self.move_reader_cursor_to_bottom();
    }
    fn move_half_page(&mut self, down: bool) {
        // Half-viewport step. Falls back to a tiny jump when the height
        // hasn't been measured yet (first frame).
        let step = (self.last_reader_inner_height / 2).max(1) as i32;
        self.move_reader_cursor(if down { step } else { -step }, 0);
    }
    // Word motions: now backed by the shared scanner over the laid-out
    // body text, unlike the no-op default — `reader_word` reads
    // `last_reader_body_line_text`, which the keymap layer can't see but
    // `self` carries.
    fn move_word_forward(&mut self) {
        self.reader_word(crate::ui::words::WordMotion::Forward, false);
    }
    fn move_word_back(&mut self) {
        self.reader_word(crate::ui::words::WordMotion::Back, false);
    }
    fn move_word_end(&mut self) {
        self.reader_word(crate::ui::words::WordMotion::End, false);
    }
    fn move_word_forward_big(&mut self) {
        self.reader_word(crate::ui::words::WordMotion::Forward, true);
    }
    fn move_word_back_big(&mut self) {
        self.reader_word(crate::ui::words::WordMotion::Back, true);
    }
    fn move_word_end_big(&mut self) {
        self.reader_word(crate::ui::words::WordMotion::End, true);
    }
    fn move_word_end_back(&mut self) {
        self.reader_word(crate::ui::words::WordMotion::EndBack, false);
    }
    fn move_word_end_back_big(&mut self) {
        self.reader_word(crate::ui::words::WordMotion::EndBack, true);
    }

    // Re-open the inherent impl block so the rest of InboxScreen's
    // methods stay attached. Rust allows multiple `impl T` blocks.
}

/// Where a viewport-relative motion (`H`/`M`/`L`) or scroll-positioning
/// chord (`zt`/`zz`/`zb`) anchors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewportPos {
    Top,
    Middle,
    Bottom,
}

impl InboxScreen {
    /// Move the reader cursor by a page (scroll follows). `full` is a
    /// whole viewport with two lines of overlap (`Ctrl-f`/`Ctrl-b`), else
    /// a half viewport (`Ctrl-d`/`Ctrl-u`) — both vim-style.
    pub fn page_cursor(&mut self, down: bool, full: bool) {
        let h = self.last_reader_inner_height;
        let step = if full {
            h.saturating_sub(2).max(1)
        } else {
            (h / 2).max(1)
        } as i32;
        self.move_reader_cursor(if down { step } else { -step }, 0);
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
        // so `yp`/`yl` operate on the new message's first content. Scroll
        // is reset alongside the cursor: a new message opens at the top,
        // like every mail client. Leaving the old scroll in place left the
        // viewport at the previous message's bottom while the cursor reset
        // to line 0, so the draw-time keep-in-view clamp would drag the
        // cursor down to the viewport edge — landing "beyond" the content.
        // Visual mode anchors against the old body's coords too, so drop it.
        self.reader_scroll = 0;
        self.reader_cursor_line = 0;
        self.reader_cursor_col = 0;
        self.reader_goal_col = 0;
        self.visual = None;
        self.mouse_drag_anchor = None;
        self.yank_highlight = None;
        let Some(path) = self.selected_path() else {
            self.parsed = None;
            self.evict_image_cache(old_msgid.as_deref(), &msgid);
            return;
        };
        // Folder of the selected row, captured before the read so a
        // failure can mark it dirty for a path-refreshing rescan.
        let stale_folder = self
            .selected_message_row()
            .map(|r| (r.account.clone(), r.folder.clone()));
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
                    attachments: body.attachments,
                }));
                self.evict_image_cache(old_msgid.as_deref(), &msgid);
                // Mark the *selected* copy seen — not just any row with this
                // msgid, which could be a same-id copy in another account.
                if let Some(target) = self.selected_message_row().map(MsgRef::of) {
                    self.try_mark_seen(&target, cache_path, self_writes, status_error);
                }
            }
            Err(e) => {
                self.parsed = None;
                *status_error = Some(format!("parse failed: {e:#}"));
                self.evict_image_cache(old_msgid.as_deref(), &msgid);
                // The file was almost certainly renamed out from under us
                // by an mbsync/`notmuch new` sync, leaving a stale path in
                // the index. Mark the folder dirty so the next rescan
                // refreshes the path, and clear `last_parsed_msgid` so the
                // body re-loads once it does — otherwise it stays broken
                // (msgid unchanged → no retry) until the user navigates
                // away and back. If the message was truly removed, the
                // rescan prunes the row and selection moves on, so this
                // can't spin forever on one msgid.
                if let Some(key) = stale_folder {
                    self.pending_dirty.insert(key);
                }
                self.last_parsed_msgid = None;
            }
        }
    }

    /// Add the `S` (Seen) flag to the selected row if it isn't already
    /// set. Used by `ensure_body` on a successful body parse, so opening
    /// a message once is enough to mark it read. Errors are surfaced via
    /// `status_error`; the rescan reconciles on the next sync.
    fn try_mark_seen(
        &mut self,
        target: &MsgRef,
        cache_path: &Path,
        self_writes: &SelfWrites,
        status_error: &mut Option<String>,
    ) {
        let Some(row) = self.find_row(target) else {
            return;
        };
        if row.flags.contains('S') {
            return;
        }
        let path = row.path.clone();
        match flags::set_flag_recorded(&path, 'S', FlagOp::Add, self_writes) {
            Ok((new_path, new_flags)) => {
                self.apply_flag_change(target, &new_path, &new_flags, cache_path, status_error);
            }
            Err(e) => {
                *status_error = Some(format!("mark read: {e}"));
            }
        }
    }

    /// Returns `true` iff the rename succeeded — the App-level wrapper
    /// uses that to decide whether to push an undo entry. Early returns
    /// for "no selection" / "no row" / rename failure all yield `false`.
    /// Toggle `flag` on the message identified by `msgid` rather than the
    /// current selection. Returns `true` on a successful rename so the
    /// App wrapper can decide whether to record undo. Multi-row callers
    /// (list-visual range) drive several of these from one operation; the
    /// selection-targeted [`InboxScreen::toggle_flag`] delegates here.
    pub fn toggle_flag_msgid(
        &mut self,
        flag: char,
        target: &MsgRef,
        cache_path: &Path,
        self_writes: &SelfWrites,
        status_error: &mut Option<String>,
    ) -> bool {
        let Some(row) = self.find_row(target) else {
            return false;
        };
        let path = row.path.clone();
        match flags::set_flag_recorded(&path, flag, FlagOp::Toggle, self_writes) {
            Ok((new_path, new_flags)) => {
                self.apply_flag_change(target, &new_path, &new_flags, cache_path, status_error);
                true
            }
            Err(e) => {
                *status_error = Some(format!("toggle {flag}: {e}"));
                false
            }
        }
    }

    /// Rename `msgid`'s file into `<account_maildir>/.<folder>/cur/`,
    /// preserving its flag suffix. On success: records both paths on the
    /// self-write registry (so the future Step 7 watcher will skip its own
    /// echo), drops the row from the in-memory inbox view, and mirrors the
    /// new folder/path into the SQLite index. Returns `true` iff the rename
    /// succeeded so the App-level wrapper can decide whether to push an
    /// undo entry. The msgid is taken explicitly (rather than read from
    /// `selected_msgid`) so multi-row callers like trash-thread can drive
    /// several moves from one operation without rotating the selection.
    pub fn move_msgid_to(
        &mut self,
        target_folder: &str,
        target: &MsgRef,
        accounts: &HashMap<String, Account>,
        cache_path: &Path,
        self_writes: &SelfWrites,
        status_error: &mut Option<String>,
    ) -> bool {
        let Some(row) = self.find_row(target) else {
            return false;
        };
        let account_name = row.account.clone();
        let path = row.path.clone();
        let row_flags = row.flags.clone();

        let Some(account) = accounts.get(&account_name) else {
            *status_error = Some(format!("move: unknown account {account_name}"));
            return false;
        };
        // Hard guard against cross-account moves. epost has no "move mail
        // between accounts" operation — every move resolves its
        // destination against the message's *own* account maildir, so the
        // source file must already live under that account's root. If it
        // doesn't, the (account, path) pair is inconsistent and proceeding
        // would relocate a file into a foreign account's tree (and, on the
        // next sync, upload it to the wrong provider — exactly how the
        // Gmail injection happened). Refuse instead.
        if !path.starts_with(&account.maildir) {
            *status_error = Some(format!(
                "move refused: {} is not under account {account_name}'s maildir (cross-account move)",
                path.display()
            ));
            return false;
        }
        // Bindings are config-derived and cheap to rebuild; doing it
        // here avoids threading the AccountSpec map through every
        // move callsite. The binding's `path` carries the on-disk
        // root (role's `disk_name` resolved via layout, or an extra's
        // literal). `target_folder` is the index/sidebar label, so
        // role names like "Archive" route correctly to weirdly-named
        // disk folders.
        let spec = AccountSpec::from_account(&account_name, account);
        let Some(binding) = spec.binding_by_label(target_folder) else {
            *status_error = Some(format!(
                "move: {target_folder} not configured on {account_name}"
            ));
            return false;
        };
        let folder_root = binding.path.clone();
        let target_cur = folder_root.join("cur");

        if let Err(e) = flags::ensure_maildir(&folder_root) {
            *status_error = Some(format!("move: create {target_folder}: {e}"));
            return false;
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
                    target,
                    target_folder,
                    &new_path,
                    cache_path,
                    status_error,
                );
                *status_error = Some(format!("moved to {target_folder}"));
                true
            }
            Err(e) => {
                *status_error = Some(format!("move to {target_folder}: {e}"));
                false
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
        source: &MsgRef,
        target_folder: &str,
        new_path: &Path,
        cache_path: &Path,
        status_error: &mut Option<String>,
    ) {
        // Pull the pre-move snapshot from whichever in-memory view has
        // the row. Scan is preferred when both hold it (local search +
        // current folder), but global search may only have it in the
        // haystack.
        let Some(snapshot_base) = self.find_row(source).cloned() else {
            return;
        };
        // A rescan walking the disk concurrently may still see this
        // message in its old folder; bump the epoch so its result is
        // discarded rather than resurrecting the row we're about to drop.
        self.optimistic_epoch = self.optimistic_epoch.wrapping_add(1);
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
            && let Some(i) = rows.iter().position(|t| source.matches(&t.row))
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
            s.drop_msg(source);
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

        // Under the composite key the destination is a *new* row, so the
        // source row must be deleted explicitly — an upsert alone would
        // leave the message indexed in both folders.
        if let Err(e) = move_in_index(cache_path, source, &snapshot) {
            *status_error = Some(format!("index mirror failed: {e:#}"));
        }
    }

    /// Post-rename bookkeeping for an undo flag flip. Same shape as
    /// [`InboxScreen::apply_flag_change`], but the pre-state comes from
    /// the index lookup (`current`) instead of `find_row`, so it also
    /// handles cross-scope undo where the row isn't in any in-memory
    /// view. Stats are adjusted against `current.account` /
    /// `current.folder`; in-memory rows are patched when present.
    fn apply_flag_undo(
        &mut self,
        current: &MessageRow,
        new_path: &Path,
        new_flags: &str,
        cache_path: &Path,
        status_error: &mut Option<String>,
    ) {
        let target = MsgRef::of(current);
        let was_unread = !current.flags.contains('S');
        let now_unread = !new_flags.contains('S');
        let unread_delta: i64 = match (was_unread, now_unread) {
            (true, false) => -1,
            (false, true) => 1,
            _ => 0,
        };
        if unread_delta != 0 {
            adjust_unread(
                &mut self.folder_stats,
                &current.account,
                &current.folder,
                unread_delta,
            );
        }
        if let ScanState::Ready(rows) = &mut self.scan
            && let Some(t) = rows.iter_mut().find(|t| target.matches(&t.row))
        {
            t.row.path = new_path.to_path_buf();
            t.row.flags = new_flags.to_string();
        }
        if let Some(s) = self.search.as_mut() {
            s.patch_msg(&target, new_path, new_flags);
        }
        let mut snap = current.clone();
        snap.path = new_path.to_path_buf();
        snap.flags = new_flags.to_string();
        if let Err(e) = mirror_to_index(cache_path, &snap) {
            *status_error = Some(format!("index mirror failed: {e:#}"));
        }
    }

    /// Post-rename bookkeeping for an undo move. Same shape as
    /// [`InboxScreen::drop_row_after_move`], but the pre-state comes
    /// from the index lookup (`current`) rather than `find_row` —
    /// after the user's forward move the row was already dropped from
    /// the in-memory view, so we can't look it up there. Marks
    /// `src_folder` dirty so the next `poll_watch` tick rescans it,
    /// which is how the row reappears in scan when the current scope
    /// is the undo destination.
    fn apply_move_undo(
        &mut self,
        current: &MessageRow,
        src_folder: &str,
        new_path: &Path,
        cache_path: &Path,
        status_error: &mut Option<String>,
    ) {
        // `current` is where the message sits *now* (the forward move's
        // destination); we're moving it back to `src_folder`.
        let from = MsgRef::of(current);
        let was_unread = !current.flags.contains('S');
        let mut snap = current.clone();
        snap.folder = src_folder.to_string();
        snap.path = new_path.to_path_buf();

        // Drop from scan if the row happens to be in view (uncommon —
        // the forward move already dropped it — but defend against the
        // case where a rescan re-surfaced it under the dst label).
        if let ScanState::Ready(rows) = &mut self.scan
            && let Some(i) = rows.iter().position(|t| from.matches(&t.row))
        {
            rows.remove(i);
            if self.search.is_none() {
                if rows.is_empty() {
                    self.selected = 0;
                } else if self.selected >= rows.len() {
                    self.selected = rows.len() - 1;
                }
            }
        }
        if let Some(s) = self.search.as_mut() {
            s.drop_msg(&from);
            if s.results.is_empty() {
                self.selected = 0;
            } else if self.selected >= s.results.len() {
                self.selected = s.results.len() - 1;
            }
        }
        adjust_total(
            &mut self.folder_stats,
            &current.account,
            &current.folder,
            -1,
        );
        adjust_total(&mut self.folder_stats, &current.account, src_folder, 1);
        if was_unread {
            adjust_unread(
                &mut self.folder_stats,
                &current.account,
                &current.folder,
                -1,
            );
            adjust_unread(&mut self.folder_stats, &current.account, src_folder, 1);
        }
        // Delete the now-stale destination row and insert the row back
        // under `src_folder` (composite key: distinct rows).
        if let Err(e) = move_in_index(cache_path, &from, &snap) {
            *status_error = Some(format!("index mirror failed: {e:#}"));
        }
        // Queue a rescan of the destination folder so the row reappears
        // in scan when the user's current scope == src_folder. Watcher's
        // self-write suppression eats the rename event, so we have to
        // mark dirty ourselves.
        self.pending_dirty
            .insert((current.account.clone(), src_folder.to_string()));
    }

    /// Patch the in-memory row's `path` / `flags` after a successful
    /// rename, then mirror the change into the SQLite index. Maildir is
    /// truth (DESIGN invariant 3), so if the index update fails we leave
    /// the in-memory row patched and let the next rescan reconcile.
    fn apply_flag_change(
        &mut self,
        target: &MsgRef,
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
        let Some(pre) = self.find_row(target).cloned() else {
            return;
        };
        // Same race as a move: a rescan kicked before this flip can carry
        // the pre-flip flags and revert the in-memory patch below. Bump
        // the epoch so `apply_rescan` discards a stale walk.
        self.optimistic_epoch = self.optimistic_epoch.wrapping_add(1);
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
            && let Some(t) = rows.iter_mut().find(|t| target.matches(&t.row))
        {
            t.row.path = new_path.to_path_buf();
            t.row.flags = new_flags.to_string();
            snapshot_for_index = Some(t.row.clone());
        }
        // Patch search haystack (if there).
        if let Some(s) = self.search.as_mut() {
            s.patch_msg(target, new_path, new_flags);
            if snapshot_for_index.is_none()
                && let Some(r) = s.haystack.iter().find(|r| target.matches(r))
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
    pub fn switch_to_scope(&mut self, account: Option<String>, folder: &str, cache_path: &Path) {
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
        // Set the target scope eagerly so the top-bar badge updates this
        // frame. The folder load (index read + JWZ thread build) is
        // O(folder) and used to block the event loop right here; it now
        // runs on a std::thread worker and lands via `poll_switch`.
        self.current_account = account.clone();
        self.current_folder = folder.to_string();
        // If this folder hasn't been walked yet this session (eager
        // INBOX-only startup → catch-up may still be in flight, or the
        // user navigated faster than the background pass), enqueue it
        // for the rescan worker. `poll_watch` will pick it up next
        // tick. For the unified `[all]` scope, queue every account's copy
        // of the folder since the view aggregates across accounts.
        self.queue_lazy_scan(account.as_deref(), folder);
        // Placeholder until the worker reports — same as startup. Keeps
        // the user from acting on the previous scope's rows mid-switch.
        self.scan = ScanState::Scanning;
        self.selected = 0;
        self.list_offset = 0;
        self.list_visual = None;
        self.reader_scroll = 0;
        self.reader_cursor_line = 0;
        self.reader_cursor_col = 0;
        self.reader_goal_col = 0;
        self.visual = None;
        self.mouse_drag_anchor = None;
        self.yank_highlight = None;
        self.parsed = None;
        self.last_parsed_msgid = None;
        self.prev_parsed_msgid = None;
        self.image_cache.clear();
        // Drives the reader's next-frame Clear pass so kitty/iTerm
        // image placements from the previous body don't ghost over
        // the new scope's first message.
        self.body_changed_this_tick = true;
        // Bump the generation so a late result from a switch the user has
        // already cycled past is discarded by `poll_switch`. Replacing
        // `switch_rx` drops the prior receiver, so the superseded worker's
        // send fails and its result never arrives anyway — the generation
        // is the explicit guard documenting the latest-wins contract.
        self.switch_generation = self.switch_generation.wrapping_add(1);
        self.switch_rx = Some(scan::start_switch_worker(
            account,
            folder.to_string(),
            cache_path.to_path_buf(),
            self.switch_generation,
            self.event_tx.clone(),
        ));
    }

    /// Drain the off-thread folder-switch worker. Applies the threaded
    /// rows only when the result's `generation` still matches the latest
    /// switch — a stale result (the user cycled past this scope before the
    /// worker finished) is dropped so the newest target wins. Errors
    /// surface to the cmdline status row. Called every tick alongside
    /// `poll_scan`.
    pub fn poll_switch(&mut self, status_error: &mut Option<String>) {
        let Some(rx) = self.switch_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(outcome) => {
                self.switch_rx = None;
                if outcome.generation != self.switch_generation {
                    // Superseded by a newer switch; ignore its rows.
                    return;
                }
                match outcome.result {
                    Ok(threads) => {
                        self.scan = ScanState::Ready(threads);
                        self.selected = 0;
                        self.list_offset = 0;
                        self.body_changed_this_tick = true;
                    }
                    Err(msg) => {
                        *status_error = Some(format!("switch scope: {msg}"));
                        self.scan = ScanState::Failed(msg);
                    }
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                // The held receiver always belongs to the latest switch
                // (we replace it on every switch), so a disconnect here is
                // the active worker dying.
                self.switch_rx = None;
                self.scan = ScanState::Failed("switch worker died before reporting".into());
            }
        }
    }

    /// Enqueue a rescan for `(account, folder)` if it hasn't been
    /// walked yet this session. For the unified `[all]` scope queue
    /// every known account's copy of the folder, since the view
    /// aggregates across accounts and any one of them being stale
    /// poisons the result. Known accounts come from `folder_stats` —
    /// after the eager INBOX scan that always contains every
    /// configured account, even those with empty folders.
    fn queue_lazy_scan(&mut self, account: Option<&str>, folder: &str) {
        let candidates: Vec<String> = match account {
            Some(a) => vec![a.to_string()],
            None => self
                .folder_stats
                .iter()
                .filter_map(|g| g.scope.clone())
                .collect(),
        };
        for acc in candidates {
            let key = (acc, folder.to_string());
            if !self.scanned_folders.contains(&key) {
                self.pending_dirty.insert(key);
            }
        }
    }

    pub fn poll_scan(
        &mut self,
        cfg: &Config,
        cache_path: &Path,
        status_error: &mut Option<String>,
    ) {
        // 1. Drain the eager INBOX scan. On success, mark every
        //    configured account's INBOX as scanned-this-session and
        //    hand off to the background catch-up worker for the rest.
        if let Some(rx) = self.scan_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(data)) => {
                    self.scan = ScanState::Ready(data.threads);
                    self.folder_stats = data.groups;
                    self.selected = 0;
                    self.scan_rx = None;
                    for name in cfg.accounts.keys() {
                        self.scanned_folders
                            .insert((name.clone(), "INBOX".to_string()));
                    }
                    if self.catchup_rx.is_none() && !cfg.accounts.is_empty() {
                        self.catchup_rx = Some(scan::start_catchup_worker(
                            account_specs(cfg),
                            cache_path.to_path_buf(),
                            (self.current_account.clone(), self.current_folder.clone()),
                        ));
                    }
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

        // 2. Drain the background catch-up. Folder stats are always
        //    replaced; the threaded list is re-queried from the index
        //    iff the current scope's folder was covered (the worker's
        //    `data.threads` is for the scope it captured at spawn, which
        //    can be stale if the user navigated since).
        if let Some(rx) = self.catchup_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(data)) => {
                    let covered = scan::enumerate_folders(&account_specs(cfg));
                    self.apply_catchup(data, &covered, cache_path, status_error);
                    self.catchup_rx = None;
                }
                Ok(Err(msg)) => {
                    *status_error = Some(format!("catchup: {msg}"));
                    self.catchup_rx = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.catchup_rx = None;
                }
            }
        }
    }

    /// Install the catch-up worker's payload. `folder_stats` is always
    /// replaced (counts and folder list both shift). The threaded list
    /// for the current scope is re-read from the index when the catch-up
    /// covered the folder — `data.threads` is for the scope captured at
    /// worker spawn time, which may be stale if the user navigated
    /// during the walk. `covered` is the full set the catch-up touched
    /// (so the "[all]" view-touched check is honest about which
    /// accounts' folders ran).
    fn apply_catchup(
        &mut self,
        data: scan::ScanData,
        covered: &HashSet<(String, String)>,
        cache_path: &Path,
        status_error: &mut Option<String>,
    ) {
        self.folder_stats = data.groups;
        self.scanned_folders.extend(covered.iter().cloned());
        let view_touched = covered.iter().any(|(account, folder)| {
            folder == &self.current_folder
                && match &self.current_account {
                    None => true,
                    Some(scope) => scope == account,
                }
        });
        if !view_touched {
            return;
        }
        let idx = match Index::open(cache_path) {
            Ok(i) => i,
            Err(e) => {
                *status_error = Some(format!("catchup apply: open index: {e:#}"));
                return;
            }
        };
        let rows = match idx.list_folder(self.current_account.as_deref(), &self.current_folder) {
            Ok(r) => r,
            Err(e) => {
                *status_error = Some(format!("catchup apply: list: {e:#}"));
                return;
            }
        };
        let old_msgid = self.selected_msgid();
        self.scan = ScanState::Ready(build_threads(rows));
        self.selected = match old_msgid {
            Some(mid) => self
                .threaded()
                .iter()
                .position(|r| r.row.msgid == mid)
                .unwrap_or(0),
            None => 0,
        };
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
                    Ok(WatcherEvent::FoldersDirty(set)) => {
                        log::debug!("poll_watch: received FoldersDirty {set:?}");
                        self.pending_dirty.extend(set);
                    }
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
                    self.rescan_rx = None;
                    if self.optimistic_epoch != self.rescan_kick_epoch {
                        // A move/flag mutation landed after this walk was
                        // kicked: the payload may carry pre-mutation disk
                        // state (a trashed row still in its old folder, a
                        // reverted flag). Drop it and re-queue the folders
                        // so step 3 kicks a fresh, post-mutation walk.
                        log::debug!(
                            "poll_watch: discarding stale rescan (epoch {} != {}); re-queuing {:?}",
                            self.rescan_kick_epoch,
                            self.optimistic_epoch,
                            covered
                        );
                        self.pending_dirty.extend(covered);
                    } else {
                        self.apply_rescan(data, &covered);
                    }
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
            log::debug!(
                "poll_watch: kicking rescan for {:?} (scope account={:?} folder={:?})",
                dirty,
                self.current_account,
                self.current_folder
            );
            let accounts: HashMap<String, AccountSpec> = cfg
                .accounts
                .iter()
                .map(|(n, a)| (n.clone(), AccountSpec::from_account(n, a)))
                .collect();
            self.rescan_in_flight = dirty.clone();
            self.rescan_kick_epoch = self.optimistic_epoch;
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
        self.scanned_folders.extend(dirty.iter().cloned());
        let view_touched = dirty.iter().any(|(account, folder)| {
            folder == &self.current_folder
                && match &self.current_account {
                    None => true,
                    Some(scope) => scope == account,
                }
        });
        if !view_touched {
            log::debug!(
                "apply_rescan: dirty {dirty:?} does not touch current view (account={:?} folder={:?}); list left unchanged",
                self.current_account,
                self.current_folder
            );
            return;
        }
        log::debug!(
            "apply_rescan: view touched by {dirty:?}; refreshing list ({} rows)",
            data.threads.len()
        );
        let old_ref = self.selected_message_row().map(MsgRef::of);
        self.scan = ScanState::Ready(data.threads);
        self.selected = match (old_ref, &self.scan) {
            (Some(target), ScanState::Ready(rows)) => rows
                .iter()
                .position(|r| target.matches(&r.row))
                .unwrap_or(0),
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
        self.list_visual = None;
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
        self.list_visual = None;
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
        self.list_visual = None;
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
        self.list_visual = None;
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

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.list_len().saturating_sub(1);
    }

    /// Move the list selection by a page. `full` picks a whole viewport
    /// (Ctrl-f/b, with two rows of overlap for context, vim-style); else
    /// a half viewport (Ctrl-d/u). The `List` widget recomputes its own
    /// offset to keep `selected` visible, so this only moves the cursor.
    /// A live visual-line range follows `selected` as usual.
    pub fn select_page(&mut self, down: bool, full: bool) {
        let len = self.list_len();
        if len == 0 {
            return;
        }
        let h = self.last_list_inner_height.max(1) as usize;
        let step = if full {
            h.saturating_sub(2).max(1)
        } else {
            (h / 2).max(1)
        };
        self.selected = if down {
            (self.selected + step).min(len - 1)
        } else {
            self.selected.saturating_sub(step)
        };
    }

    /// Look up a row by msgid across both the scan view and the search
    /// haystack (when search is active). Used by flag-toggle / move
    /// callsites that must keep the in-memory rows consistent regardless
    /// of which view is current.
    pub fn find_row(&self, target: &MsgRef) -> Option<&MessageRow> {
        if let ScanState::Ready(rows) = &self.scan
            && let Some(t) = rows.iter().find(|t| target.matches(&t.row))
        {
            return Some(&t.row);
        }
        self.search
            .as_ref()
            .and_then(|s| s.haystack.iter().find(|r| target.matches(r)))
    }

    /// Row at list position `i` in whichever view is active (search
    /// results when a search is running, otherwise the threaded scan).
    /// Mirrors [`InboxScreen::selected_message_row`]'s view routing for
    /// an arbitrary index — used to enumerate a list-visual range.
    pub fn row_at(&self, i: usize) -> Option<&MessageRow> {
        if let Some(s) = self.search.as_ref() {
            return s.selected_row(i);
        }
        self.threaded().get(i).map(|t| &t.row)
    }
}

/// Project the in-config accounts into the `AccountSpec` shape the
/// scan / catch-up workers and `enumerate_folders` consume. Kept as a
/// single helper so the projection (resolving INBOX path, etc.)
/// stays consistent across the eager scan, the catch-up, and the
/// watcher's per-folder rescan paths.
fn format_send_status(label: &str, result: SendResult) -> String {
    match result {
        Ok(SendOutcome::Sent) => format!("sent: {label}"),
        Ok(SendOutcome::SentNoCopy(msg)) => format!("sent: {label} (no Sent copy: {msg})"),
        Ok(SendOutcome::Cancelled) => format!("cancelled: {label}"),
        Err(e) => format!("send failed ({label}): {e}"),
    }
}

fn account_specs(cfg: &Config) -> Vec<AccountSpec> {
    cfg.accounts
        .iter()
        .map(|(name, a)| AccountSpec::from_account(name, a))
        .collect()
}

fn mirror_to_index(cache_path: &Path, row: &MessageRow) -> anyhow::Result<()> {
    let mut idx = Index::open(cache_path)?;
    idx.upsert(row)?;
    Ok(())
}

/// Reflect a cross-folder move in the index: delete the `source` row and
/// upsert the relocated `row`. Under the composite `(msgid, account,
/// folder)` key the destination is a distinct row, so a bare upsert
/// would leave the message indexed in both the source and destination
/// folders. No-op-safe when `source` already matches `row`'s identity
/// (the delete drops it, the upsert reinstates it).
fn move_in_index(cache_path: &Path, source: &MsgRef, row: &MessageRow) -> anyhow::Result<()> {
    let mut idx = Index::open(cache_path)?;
    idx.delete(&source.msgid, &source.account, &source.folder)?;
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
        attachment_pick_buf,
        osc8_links,
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
                reader::draw(
                    f,
                    rect,
                    inbox,
                    *mode,
                    link_pick_buf,
                    attachment_pick_buf,
                    *osc8_links,
                );
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
    fn reader_vertical_preserves_goal_column_across_blank_line() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.last_reader_body_line_text = vec!["foo bar".into(), String::new(), "foo".into()];
        inbox.last_reader_body_only_lines = 3;
        // Move to column 2 of the first line.
        inbox.move_reader_cursor(0, 1);
        inbox.move_reader_cursor(0, 1);
        assert_eq!(inbox.reader_cursor_col, 2);
        // Down onto the blank line: the live column overshoots and the
        // draw-time clamp would true it to 0 (simulated here).
        inbox.move_reader_cursor(1, 0);
        inbox.reader_cursor_col = 0;
        // Down onto "foo": the goal restores column 2.
        inbox.move_reader_cursor(1, 0);
        assert_eq!(inbox.reader_cursor_line, 2);
        assert_eq!(inbox.reader_cursor_col, 2);
    }

    #[test]
    fn reader_dollar_rides_end_of_each_line() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.last_reader_body_line_text = vec!["foobar".into(), "ab".into(), "hello".into()];
        inbox.last_reader_body_only_lines = 3;
        inbox.move_reader_cursor_to_line_end();
        assert_eq!(inbox.reader_goal_col, u16::MAX);
        // Each vertical move re-sources the EOL sentinel; the draw clamp
        // (not run here) trues the live column to each line's end.
        inbox.move_reader_cursor(1, 0);
        assert_eq!(inbox.reader_cursor_col, u16::MAX);
        assert_eq!(inbox.reader_goal_col, u16::MAX);
        inbox.move_reader_cursor(1, 0);
        assert_eq!(inbox.reader_goal_col, u16::MAX);
    }

    #[test]
    fn reader_horizontal_move_resets_goal_column() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.last_reader_body_line_text = vec!["foobar".into(), "hello".into()];
        inbox.last_reader_body_only_lines = 2;
        inbox.move_reader_cursor_to_line_end();
        // `h` after `$` drops EOL-sticky and pins the goal to a column.
        inbox.reader_cursor_col = 5; // simulate the draw clamp to EOL of "foobar"
        inbox.move_reader_cursor(0, -1);
        assert_eq!(inbox.reader_goal_col, 4);
        inbox.move_reader_cursor(1, 0);
        assert_eq!(inbox.reader_cursor_col, 4);
    }

    #[test]
    fn reader_paragraph_jumps_to_blank_lines() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.last_reader_body_line_text = vec![
            "a".into(),
            "b".into(),
            String::new(),
            "c".into(),
            "d".into(),
        ];
        inbox.last_reader_body_only_lines = 5;
        // `}` from line 0 lands on the blank line (2).
        inbox.reader_paragraph(true);
        assert_eq!(inbox.reader_cursor_line, 2);
        // `}` again clamps to the last line.
        inbox.reader_paragraph(true);
        assert_eq!(inbox.reader_cursor_line, 4);
        // `{` walks back to the blank line.
        inbox.reader_paragraph(false);
        assert_eq!(inbox.reader_cursor_line, 2);
    }

    #[test]
    fn reader_viewport_positions_top_middle_bottom() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.last_reader_body_line_text = (0..20).map(|i| format!("line{i}")).collect();
        inbox.last_reader_body_only_lines = 20;
        inbox.last_reader_inner_height = 10;
        inbox.last_reader_header_offset = 0;
        inbox.reader_scroll = 0;
        inbox.reader_cursor_to_viewport(ViewportPos::Top);
        assert_eq!(inbox.reader_cursor_line, 0);
        inbox.reader_cursor_to_viewport(ViewportPos::Bottom);
        assert_eq!(inbox.reader_cursor_line, 9);
        inbox.reader_cursor_to_viewport(ViewportPos::Middle);
        assert!(matches!(inbox.reader_cursor_line, 4 | 5));
    }

    #[test]
    fn reader_scroll_cursor_centers_and_tops() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.last_reader_body_line_text = (0..40).map(|i| format!("line{i}")).collect();
        inbox.last_reader_body_only_lines = 40;
        inbox.last_reader_inner_height = 10;
        inbox.last_reader_header_offset = 0;
        inbox.reader_cursor_line = 20;
        // zt: cursor row becomes the scroll top.
        inbox.reader_scroll_cursor(ViewportPos::Top);
        assert_eq!(inbox.reader_scroll, 20);
        // zz: centred — scroll = cursor - height/2.
        inbox.reader_scroll_cursor(ViewportPos::Middle);
        assert_eq!(inbox.reader_scroll, 15);
    }

    #[test]
    fn reader_ge_lands_on_previous_word_end() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.last_reader_body_line_text = vec!["foo bar baz".into()];
        inbox.last_reader_body_only_lines = 1;
        inbox.reader_cursor_line = 0;
        inbox.reader_cursor_col = 10; // on the last 'z'
        inbox.reader_word(crate::ui::words::WordMotion::EndBack, false);
        assert_eq!(inbox.reader_cursor_col, 6); // end of "bar"
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

    fn parsed_with_attachments(n: usize) -> Box<ParsedBody> {
        Box::new(ParsedBody {
            msgid: "x@y".into(),
            blocks: Vec::new(),
            raw_html: None,
            plain_fallback: None,
            cid_parts: std::collections::HashMap::new(),
            attachments: (0..n)
                .map(|i| parse::Attachment {
                    filename: format!("f{i}"),
                    bytes: vec![0u8; 4],
                })
                .collect(),
        })
    }

    #[test]
    fn attachment_under_cursor_maps_line_then_falls_back() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.parsed = Some(parsed_with_attachments(3));
        // Chip rows at body lines 0,1,2 (as the layout emits them).
        inbox.last_attachment_lines = vec![0, 1, 2];
        inbox.reader_cursor_line = 1;
        assert_eq!(inbox.attachment_under_cursor(), Some(1));
        inbox.reader_cursor_line = 2;
        assert_eq!(inbox.attachment_under_cursor(), Some(2));
        // Cursor off any chip row with >1 attachment → no resolution.
        inbox.reader_cursor_line = 9;
        assert_eq!(inbox.attachment_under_cursor(), None);

        // With exactly one attachment, a cursor anywhere falls back to it.
        inbox.parsed = Some(parsed_with_attachments(1));
        inbox.last_attachment_lines = vec![0];
        inbox.reader_cursor_line = 42;
        assert_eq!(inbox.attachment_under_cursor(), Some(0));
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
    fn move_reader_cursor_to_bottom_lands_at_line_start() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.last_reader_body_only_lines = 7;
        inbox.last_reader_inner_height = 3;
        inbox.last_reader_header_offset = 0;
        inbox.move_reader_cursor_to_bottom();
        assert_eq!(inbox.reader_cursor_line, 6);
        // Vim `G` lands at the start of the last line, not its end.
        assert_eq!(inbox.reader_cursor_col, 0);
    }

    #[test]
    fn page_cursor_half_and_full_step_and_clamp() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.last_reader_inner_height = 10;
        inbox.last_reader_body_only_lines = 100;
        inbox.last_reader_header_offset = 0;
        // Half page down = inner_height / 2; cursor moves, scroll follows.
        inbox.page_cursor(true, false);
        assert_eq!(inbox.reader_cursor_line, 5);
        // Full page down = inner_height - 2 (vim-style overlap).
        inbox.page_cursor(true, true);
        assert_eq!(inbox.reader_cursor_line, 5 + 8);
        // Half page back up.
        inbox.page_cursor(false, false);
        assert_eq!(inbox.reader_cursor_line, 8);
        // Down clamps the cursor to the last body line (99).
        inbox.reader_cursor_line = 95;
        inbox.page_cursor(true, true);
        assert_eq!(inbox.reader_cursor_line, 99);
        // Up saturates at 0.
        inbox.reader_cursor_line = 3;
        inbox.page_cursor(false, true);
        assert_eq!(inbox.reader_cursor_line, 0);
    }

    #[test]
    fn select_page_steps_selection_and_clamps_to_ends() {
        let mut inbox = inbox_with_panes(true, true, true);
        inbox.last_list_inner_height = 10;
        let rows: Vec<MessageRow> = (0..30)
            .map(|i| MessageRow {
                msgid: format!("m{i}"),
                account: "dev".into(),
                folder: "INBOX".into(),
                path: std::path::PathBuf::from("/x"),
                date: i as i64,
                from_addr: None,
                subject: Some(format!("s{i}")),
                in_reply: None,
                refs: Vec::new(),
                flags: String::new(),
            })
            .collect();
        inbox.scan = ScanState::Ready(build_threads(rows));
        inbox.selected = 0;
        // Half page = 5.
        inbox.select_page(true, false);
        assert_eq!(inbox.selected, 5);
        // Full page = inner_height - 2 = 8.
        inbox.select_page(true, true);
        assert_eq!(inbox.selected, 13);
        // Up a half page.
        inbox.select_page(false, false);
        assert_eq!(inbox.selected, 8);
        // Down clamps at the last row (29).
        inbox.selected = 27;
        inbox.select_page(true, true);
        assert_eq!(inbox.selected, 29);
        // Up saturates at 0.
        inbox.selected = 4;
        inbox.select_page(false, true);
        assert_eq!(inbox.selected, 0);
    }

    #[test]
    fn page_move_routes_by_focus() {
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
        {
            let inbox = app.inbox_mut();
            inbox.last_reader_inner_height = 10;
            inbox.last_reader_body_lines = 100;
            inbox.last_reader_body_only_lines = 100;
            inbox.last_reader_header_offset = 0;
            inbox.last_list_inner_height = 10;
            let rows: Vec<MessageRow> = (0..30)
                .map(|i| MessageRow {
                    msgid: format!("m{i}"),
                    account: "dev".into(),
                    folder: "INBOX".into(),
                    path: std::path::PathBuf::from("/x"),
                    date: i as i64,
                    from_addr: None,
                    subject: None,
                    in_reply: None,
                    refs: Vec::new(),
                    flags: String::new(),
                })
                .collect();
            inbox.scan = ScanState::Ready(build_threads(rows));
        }
        // Reader focus moves the body cursor (scroll follows), leaves
        // selection alone.
        app.inbox_mut().focus = Pane::Reader;
        app.page_move(true, false);
        assert_eq!(app.inbox().reader_cursor_line, 5);
        assert_eq!(app.inbox().selected, 0);
        // List focus moves selection, leaves the reader cursor alone.
        app.inbox_mut().focus = Pane::List;
        app.page_move(true, false);
        assert_eq!(app.inbox().selected, 5);
        assert_eq!(app.inbox().reader_cursor_line, 5);
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

    pub(super) fn drop_message(tmp: &TempDir, dir: &str, basename: &str) -> std::path::PathBuf {
        let inbox = tmp.path().join("Mail").join("personal");
        fs::create_dir_all(inbox.join("cur")).unwrap();
        fs::create_dir_all(inbox.join("new")).unwrap();
        let p = inbox.join(dir).join(basename);
        fs::write(&p, MSG).unwrap();
        p
    }

    pub(super) fn one_account_config(tmp: &TempDir) -> Config {
        let mut cfg = Config::default();
        cfg.accounts.insert(
            "personal".into(),
            Account {
                maildir: tmp.path().join("Mail").join("personal"),
                from: "Tester <tester@example.invalid>".into(),
                layout: crate::mail::layout::Layout::Maildirpp,
                inbox: None,
                sent: None,
                archive: None,
                spam: None,
                trash: None,

                drafts: None,

                extra_folders: Vec::new(),
                smtp: None,
                primary: false,
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
                    layout: crate::mail::layout::Layout::Maildirpp,
                    inbox: None,
                    sent: None,
                    archive: None,
                    spam: None,
                    trash: None,

                    drafts: None,

                    extra_folders: Vec::new(),
                    smtp: None,
                    primary: false,
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

    /// Spin until the scan worker either reports rows or fails AND the
    /// background catch-up worker (kicked once the eager INBOX scan
    /// lands) has finished. Production startup deliberately returns to
    /// the user on the eager INBOX result alone, but tests want every
    /// folder scanned before they assert — so this helper waits past
    /// both stages.
    pub(super) fn drain_scan(app: &mut App, cfg: &Config) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            app.poll_scan(cfg);
            let inbox_done = matches!(app.inbox().scan, ScanState::Ready(_) | ScanState::Failed(_));
            let catchup_done = app.inbox().catchup_rx.is_none();
            if inbox_done && catchup_done {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("scan worker never reported");
    }

    /// Pump `poll_switch` until the in-flight folder-switch worker reports.
    /// `switch_to_scope` is async now (rows land via `poll_switch`), so any
    /// test asserting on the post-switch list must drain first. A no-op
    /// (same-scope) switch leaves `switch_rx` unset and returns at once.
    pub(super) fn drain_switch(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            app.poll_switch();
            if app.inbox().switch_rx.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("switch worker never reported");
    }

    #[test]
    fn cross_account_same_msgid_move_targets_only_selected_account() {
        // Regression for the real-mail corruption: one Message-ID
        // delivered to two accounts is two distinct rows now. Moving the
        // row selected in account A must relocate A's file and leave B's
        // copy untouched. The pre-fix code resolved the target by msgid
        // alone (first match) and could move — and on sync, upload — the
        // wrong account's message into the wrong mailbox.
        let tmp = TempDir::new().unwrap();
        let mut cfg = Config::default();
        for name in ["alpha", "beta"] {
            cfg.accounts.insert(
                name.into(),
                Account {
                    maildir: tmp.path().join("Mail").join(name),
                    from: format!("Tester <{name}@example.invalid>"),
                    layout: crate::mail::layout::Layout::Maildirpp,
                    inbox: None,
                    sent: None,
                    archive: Some("Archive".into()),
                    spam: None,
                    trash: None,
                    drafts: None,
                    extra_folders: Vec::new(),
                    smtp: None,
                    primary: false,
                },
            );
            let inbox = tmp.path().join("Mail").join(name);
            fs::create_dir_all(inbox.join("cur")).unwrap();
            fs::create_dir_all(inbox.join("new")).unwrap();
            // Identical Message-ID in both accounts; distinct dates only so
            // list order is deterministic.
            let date = if name == "beta" {
                "13:00:00"
            } else {
                "12:00:00"
            };
            let body = format!(
                "Message-ID: <shared@x>\r\n\
                 Date: Thu, 28 May 2026 {date} +0000\r\n\
                 From: a@b\r\nSubject: hi\r\n\r\nbody\r\n"
            );
            fs::write(inbox.join("cur").join(format!("{name}:2,")), body).unwrap();
        }
        let cache = tmp.path().join("index.sqlite");
        let mut app = App::new(&cfg, cache.clone(), None, None);
        drain_scan(&mut app, &cfg);

        // Unified [all] INBOX shows both copies (same msgid, two accounts).
        assert_eq!(
            app.inbox().threaded().len(),
            2,
            "both same-msgid copies present in [all]"
        );

        // Select alpha's copy specifically and archive it.
        let alpha_idx = app
            .inbox()
            .threaded()
            .iter()
            .position(|t| t.row.account == "alpha")
            .expect("alpha row present");
        app.inbox_mut().selected = alpha_idx;
        cmdline::dispatch("archive", &mut app, &cfg);

        let dir_count = |p: std::path::PathBuf| fs::read_dir(p).map(|d| d.count()).unwrap_or(0);
        let mail = tmp.path().join("Mail");
        assert_eq!(
            dir_count(mail.join("alpha").join(".Archive").join("cur")),
            1,
            "alpha's file must land in alpha's Archive"
        );
        assert_eq!(
            dir_count(mail.join("alpha").join("cur")),
            0,
            "alpha's INBOX file must be gone"
        );
        assert_eq!(
            dir_count(mail.join("beta").join("cur")),
            1,
            "beta's INBOX copy must be untouched"
        );
        assert!(
            !mail.join("beta").join(".Archive").exists()
                || dir_count(mail.join("beta").join(".Archive").join("cur")) == 0,
            "beta must not have been archived"
        );

        // msgid is stored without the angle brackets (the parser strips them).
        let idx = crate::store::index::Index::open(&cache).unwrap();
        assert!(idx.get("shared@x", "alpha", "Archive").unwrap().is_some());
        assert!(idx.get("shared@x", "alpha", "INBOX").unwrap().is_none());
        assert!(idx.get("shared@x", "beta", "INBOX").unwrap().is_some());
    }

    #[test]
    fn move_refused_when_file_outside_account_maildir() {
        // Hard guard: epost never moves mail across accounts. If a row's
        // path somehow points outside its account's maildir, the move is
        // refused rather than relocating real mail into a foreign tree.
        let tmp = TempDir::new().unwrap();
        let src = drop_message(&tmp, "cur", "1779.M0P1.host:2,S");
        let cfg = account_with_folders(&tmp, Some("Archive"), None);
        let cache = tmp.path().join("index.sqlite");
        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);

        // Corrupt the in-memory row's path to point outside the account.
        let foreign = tmp.path().join("elsewhere").join("cur").join("x:2,S");
        if let ScanState::Ready(rows) = &mut app.inbox_mut().scan {
            rows[0].row.path = foreign;
        }
        let target = app.inbox().selected_message_row().map(MsgRef::of).unwrap();

        let action = app.move_msgid_to(&target, "Archive", &cfg);
        assert!(action.is_none(), "cross-account move must be refused");
        assert!(
            app.status_error
                .as_deref()
                .unwrap_or("")
                .contains("refused"),
            "status should explain the refusal, got {:?}",
            app.status_error
        );
        assert!(src.exists(), "original file must be left untouched");
    }

    #[test]
    fn auto_mark_seen_on_first_body_parse() {
        let tmp = TempDir::new().unwrap();
        let src = drop_message(&tmp, "new", "1779.M0P1.host");
        let cfg = one_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);
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
        drain_scan(&mut app, &cfg);

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
        drain_scan(&mut app, &cfg);

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
        drain_scan(&mut app, &cfg);

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
        drain_scan(&mut app, &cfg);

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
        drain_scan(&mut app, &cfg);

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

    pub(super) fn account_with_folders(
        tmp: &TempDir,
        archive: Option<&str>,
        trash: Option<&str>,
    ) -> Config {
        let mut cfg = Config::default();
        cfg.accounts.insert(
            "personal".into(),
            Account {
                maildir: tmp.path().join("Mail").join("personal"),
                from: "Tester <tester@example.invalid>".into(),
                layout: crate::mail::layout::Layout::Maildirpp,
                inbox: None,
                sent: None,
                archive: archive.map(str::to_string),
                spam: None,
                trash: trash.map(str::to_string),
                drafts: None,
                extra_folders: Vec::new(),
                smtp: None,
                primary: false,
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
        drain_scan(&mut app, &cfg);
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
        let got = idx
            .get(&msgid, "personal", "Archive")
            .unwrap()
            .expect("row in index");
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
        drain_scan(&mut app, &cfg);

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
        drain_scan(&mut app, &cfg);

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
        drain_scan(&mut app, &cfg);

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
            err.contains("archive"),
            "error must mention the missing config key, got {err:?}"
        );
    }

    #[test]
    fn mv_to_custom_folder() {
        let tmp = TempDir::new().unwrap();
        drop_message(&tmp, "cur", "1779.M0P1.host:2,S");
        let mut cfg = account_with_folders(&tmp, None, None);
        // `:mv X` only routes to folders the account declares; with
        // role-based config that means `extra_folders` for anything
        // that isn't a canonical role.
        cfg.accounts.get_mut("personal").unwrap().extra_folders = vec!["Receipts".into()];
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);

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
        drain_scan(&mut app, &cfg);

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
        drain_scan(&mut app, &cfg);

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

        let mut cfg = one_account_config(&tmp);
        // Bind the Sent role so the catch-up walks the folder; without
        // this the new role-based filter would skip it.
        cfg.accounts.get_mut("personal").unwrap().sent = Some("Sent".into());
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);

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
        // current_folder updates synchronously; the rows land off-thread.
        app.cycle_folder(true);
        assert_eq!(app.inbox().current_folder, "Sent");
        drain_switch(&mut app);
        assert_eq!(app.inbox().threaded().len(), 1);

        // Wrap back to INBOX.
        app.cycle_folder(true);
        assert_eq!(app.inbox().current_folder, "INBOX");
        drain_switch(&mut app);
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
        drain_scan(&mut app, &cfg);

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
        drain_scan(&mut app, &cfg);

        // [all] sees 2 INBOX rows (one per account).
        assert_eq!(app.inbox().threaded().len(), 2);

        // Scope to personal — only the personal INBOX row remains.
        app.switch_to_scope(Some("personal".into()), "INBOX");
        assert_eq!(app.inbox().current_account.as_deref(), Some("personal"));
        drain_switch(&mut app);
        assert_eq!(app.inbox().threaded().len(), 1);
        let mid = app.inbox().selected_msgid().unwrap();
        assert_eq!(mid, "personal-1@x");

        // Scope to work.
        app.switch_to_scope(Some("work".into()), "INBOX");
        drain_switch(&mut app);
        assert_eq!(app.inbox().threaded().len(), 1);
        assert_eq!(app.inbox().selected_msgid().as_deref(), Some("work-1@x"));

        // Back to all.
        app.switch_to_scope(None, "INBOX");
        assert_eq!(app.inbox().current_account, None);
        drain_switch(&mut app);
        assert_eq!(app.inbox().threaded().len(), 2);
    }

    #[test]
    fn rapid_scope_switch_latest_target_wins() {
        // Two switches back-to-back without draining between (the
        // Alt-j Alt-j race). The first worker is superseded — its
        // receiver is dropped, so its result never applies — and after
        // draining only the latest target's rows land.
        let tmp = TempDir::new().unwrap();
        let cfg = two_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");
        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);

        app.switch_to_scope(Some("personal".into()), "INBOX");
        app.switch_to_scope(Some("work".into()), "INBOX");
        drain_switch(&mut app);

        assert_eq!(app.inbox().current_account.as_deref(), Some("work"));
        assert_eq!(app.inbox().threaded().len(), 1);
        assert_eq!(app.inbox().selected_msgid().as_deref(), Some("work-1@x"));
    }

    #[test]
    fn flag_flip_updates_both_all_and_per_account_groups() {
        let tmp = TempDir::new().unwrap();
        let cfg = two_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);

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
        drain_scan(&mut app, &cfg);

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
        drain_scan(&mut app, &cfg);

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
        drain_scan(&mut app, &cfg);

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

    #[test]
    fn poll_watch_discards_rescan_racing_a_local_delete() {
        // Reproduces the "deleted row lingers while the cursor moves on"
        // bug: a rescan kicked before a local trash walks the disk while
        // the message is still present, so its payload would resurrect the
        // just-removed row (selection stays pinned by msgid to the next
        // message — exactly the reported symptom). The optimistic_epoch
        // guard must discard the stale payload instead of applying it.
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
        drain_scan(&mut app, &cfg);
        assert_eq!(app.inbox().threaded().len(), 2);

        // 1. A rescan is kicked while both messages are still on disk, so
        //    its disk walk will report both rows.
        app.inbox_mut()
            .pending_dirty
            .insert(("personal".into(), "INBOX".into()));
        app.poll_watch(&cfg);
        assert!(
            app.inbox().rescan_rx.is_some(),
            "rescan should be in flight"
        );
        // Let the worker finish its walk so the (stale, two-row) result is
        // buffered in the channel, undrained.
        std::thread::sleep(Duration::from_millis(200));

        // 2. The user trashes <b>: it leaves the in-memory list + disk and
        //    the optimistic epoch advances — what drop_row_after_move does.
        {
            let ib = app.inbox_mut();
            if let ScanState::Ready(rows) = &mut ib.scan {
                rows.remove(0); // <b> is row 0 (newest by date)
            }
            ib.optimistic_epoch = ib.optimistic_epoch.wrapping_add(1);
        }
        fs::remove_file(&m2).unwrap();
        assert_eq!(app.inbox().threaded().len(), 1);

        // 3. Draining the stale rescan must NOT bring <b> back.
        app.poll_watch(&cfg);
        assert_eq!(
            app.inbox().threaded().len(),
            1,
            "stale rescan resurrected the deleted row"
        );
        assert_eq!(app.inbox().selected_msgid().as_deref(), Some("a@x"));

        // 4. The folders were re-queued; a fresh, post-delete rescan
        //    settles on the correct single-row view.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            app.poll_watch(&cfg);
            if app.inbox().rescan_rx.is_none() && app.inbox().pending_dirty.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.inbox().threaded().len(), 1);
        assert_eq!(app.inbox().selected_msgid().as_deref(), Some("a@x"));
    }

    /// Spin only the eager INBOX scan (waits for `scan_rx`); deliberately
    /// does NOT wait for the catch-up worker the way `drain_scan` does.
    /// Used by the two tests below that want to observe the
    /// INBOX-only-eager intermediate state.
    fn drain_inbox_only(app: &mut App, cfg: &Config) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            app.poll_scan(cfg);
            if app.inbox().scan_rx.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("eager INBOX scan never reported");
    }

    /// Two-folder fixture (INBOX + Sent), one message in each. Returns
    /// `(cfg, cache_path)`. Used by the lazy-scan tests below so the
    /// fixture isn't open-coded twice.
    fn one_account_two_folders(tmp: &TempDir) -> (Config, PathBuf) {
        let mut cfg = Config::default();
        cfg.accounts.insert(
            "personal".into(),
            Account {
                maildir: tmp.path().join("Mail").join("personal"),
                from: "Tester <tester@example.invalid>".into(),
                layout: crate::mail::layout::Layout::Maildirpp,
                inbox: None,
                // Bind Sent so the catch-up walks the folder; the
                // role-based filter would skip it otherwise.
                sent: Some("Sent".into()),
                archive: None,
                spam: None,
                trash: None,
                drafts: None,
                extra_folders: Vec::new(),
                smtp: None,
                primary: false,
            },
        );
        let root = tmp.path().join("Mail").join("personal");
        fs::create_dir_all(root.join("cur")).unwrap();
        fs::create_dir_all(root.join("new")).unwrap();
        fs::create_dir_all(root.join(".Sent").join("cur")).unwrap();
        fs::create_dir_all(root.join(".Sent").join("new")).unwrap();
        fs::write(
            root.join("cur").join("inbox-1:2,"),
            b"Message-ID: <inbox-1@x>\r\nDate: Thu, 1 Jan 1970 00:00:00 +0000\r\nFrom: a@b\r\nSubject: hi\r\n\r\nbody\r\n",
        )
        .unwrap();
        fs::write(
            root.join(".Sent").join("cur").join("sent-1:2,S"),
            b"Message-ID: <sent-1@x>\r\nDate: Fri, 2 Jan 1970 00:00:00 +0000\r\nFrom: a@b\r\nSubject: sent\r\n\r\nbody\r\n",
        )
        .unwrap();
        (cfg, tmp.path().join("index.sqlite"))
    }

    #[test]
    fn eager_pass_indexes_inbox_but_not_sent() {
        // The startup worker is INBOX-only; Sent must wait for the
        // catch-up. Inspect the index content via a direct eager-only
        // worker *before* any `App` exists — going through `App` would
        // spawn the catch-up worker the moment the eager result lands,
        // and that background thread races us to write Sent into the
        // shared index (an otherwise-flaky read).
        let tmp = TempDir::new().unwrap();
        let (cfg, cache) = one_account_two_folders(&tmp);

        scan::start_inbox_worker(
            account_specs(&cfg),
            cache.clone(),
            (None, "INBOX".to_string()),
        )
        .recv()
        .unwrap()
        .unwrap();

        let idx = crate::store::index::Index::open(&cache).unwrap();
        assert!(
            idx.get("inbox-1@x", "personal", "INBOX").unwrap().is_some(),
            "INBOX must land"
        );
        assert!(
            idx.get("sent-1@x", "personal", "Sent").unwrap().is_none(),
            "Sent must NOT be indexed by the eager pass alone"
        );

        // The App's in-memory track-set reflects the same split: INBOX
        // scanned this session, Sent not yet. `scanned_folders` is mutated
        // only by `poll_scan` (never by the catch-up's index writes), so
        // this stays deterministic even with the catch-up worker live.
        let mut app = App::new(&cfg, cache, None, None);
        drain_inbox_only(&mut app, &cfg);
        assert!(
            app.inbox()
                .scanned_folders
                .contains(&("personal".into(), "INBOX".into()))
        );
        assert!(
            !app.inbox()
                .scanned_folders
                .contains(&("personal".into(), "Sent".into()))
        );
    }

    #[test]
    fn catchup_pass_indexes_sent_after_eager_inbox() {
        // `drain_scan` waits past the eager pass and the catch-up.
        // After it returns, Sent must be in the index and in the
        // scanned-this-session set.
        let tmp = TempDir::new().unwrap();
        let (cfg, cache) = one_account_two_folders(&tmp);

        let mut app = App::new(&cfg, cache.clone(), None, None);
        drain_scan(&mut app, &cfg);

        let idx = crate::store::index::Index::open(&cache).unwrap();
        assert!(
            idx.get("sent-1@x", "personal", "Sent").unwrap().is_some(),
            "Sent indexed"
        );
        assert!(
            app.inbox()
                .scanned_folders
                .contains(&("personal".into(), "Sent".into()))
        );
    }

    #[test]
    fn scope_switch_to_unscanned_folder_enqueues_rescan() {
        // Cold start: only eager INBOX has run. Switching to Sent must
        // mark it dirty so `poll_watch` picks it up on the next tick.
        let tmp = TempDir::new().unwrap();
        let (cfg, cache) = one_account_two_folders(&tmp);

        let mut app = App::new(&cfg, cache, None, None);
        drain_inbox_only(&mut app, &cfg);

        // Pre-switch: Sent is not in scanned_folders, not in pending_dirty.
        assert!(
            !app.inbox()
                .scanned_folders
                .contains(&("personal".into(), "Sent".into()))
        );
        let pre = app.inbox().pending_dirty.clone();
        assert!(!pre.contains(&("personal".into(), "Sent".into())));

        app.switch_to_scope(Some("personal".into()), "Sent");

        // Post-switch: Sent is enqueued for the rescan worker.
        assert!(
            app.inbox()
                .pending_dirty
                .contains(&("personal".into(), "Sent".into())),
            "Sent must be enqueued for a lazy rescan, got {:?}",
            app.inbox().pending_dirty
        );
    }

    #[test]
    fn scope_switch_to_already_scanned_folder_does_not_enqueue() {
        // After `drain_scan` (eager + catch-up), both INBOX and Sent
        // are in scanned_folders. Re-entering Sent must not enqueue a
        // redundant rescan.
        let tmp = TempDir::new().unwrap();
        let (cfg, cache) = one_account_two_folders(&tmp);

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);

        app.inbox_mut().pending_dirty.clear();
        app.switch_to_scope(Some("personal".into()), "Sent");
        assert!(
            !app.inbox()
                .pending_dirty
                .contains(&("personal".into(), "Sent".into())),
            "scanned-folders hit must skip the rescan enqueue"
        );
    }
}

#[cfg(test)]
mod undo_stack_tests {
    //! Pure-mechanics tests for the in-memory `UndoStack`. Integration
    //! tests that exercise the on-disk rename and index mirror live in
    //! `undo_integration_tests` below.

    use super::*;

    fn flag(msgid: &str, was_set: bool) -> UndoAction {
        UndoAction::Flag {
            msgid: msgid.to_string(),
            account: "dev".into(),
            folder: "INBOX".into(),
            flag: 'S',
            was_set,
        }
    }

    #[test]
    fn record_clears_redo() {
        let mut s = UndoStack::new();
        s.record(flag("a", true));
        let _ = s.pop_undo();
        s.push_redo(flag("a", false));
        assert_eq!(s.redo_len(), 1);
        // A fresh user action should drop the dangling redo branch.
        s.record(flag("b", true));
        assert_eq!(s.redo_len(), 0);
        assert_eq!(s.undo_len(), 1);
    }

    #[test]
    fn pop_returns_lifo() {
        let mut s = UndoStack::new();
        s.record(flag("a", true));
        s.record(flag("b", false));
        let last = s.pop_undo().unwrap();
        match last {
            UndoAction::Flag { msgid, .. } => assert_eq!(msgid, "b"),
            _ => panic!("expected Flag"),
        }
    }

    #[test]
    fn push_redo_does_not_clear() {
        // The redo direction must be lossless across multiple undos so
        // a user can `u u Ctrl-r Ctrl-r` back to their starting state.
        let mut s = UndoStack::new();
        s.push_redo(flag("a", true));
        s.push_redo(flag("b", false));
        assert_eq!(s.redo_len(), 2);
    }

    #[test]
    fn cap_evicts_oldest() {
        let mut s = UndoStack::new();
        for i in 0..UndoStack::CAP + 5 {
            s.record(flag(&format!("m{i}"), true));
        }
        assert_eq!(s.undo_len(), UndoStack::CAP);
        // The newest entry must still be on top after eviction.
        let top = s.pop_undo().unwrap();
        match top {
            UndoAction::Flag { msgid, .. } => assert_eq!(msgid, format!("m{}", UndoStack::CAP + 4)),
            _ => panic!("expected Flag"),
        }
    }
}

#[cfg(test)]
mod undo_integration_tests {
    //! End-to-end undo/redo tests against a real on-disk maildir + the
    //! same SQLite index a live binary uses. Reuses the helpers from
    //! `flag_integration_tests` for the fixture.

    use std::fs;

    use tempfile::TempDir;

    use super::flag_integration_tests::*;
    use super::*;

    fn drop_inbox_msg(tmp: &TempDir, basename: &str) -> std::path::PathBuf {
        let inbox = tmp.path().join("Mail").join("personal");
        fs::create_dir_all(inbox.join("cur")).unwrap();
        fs::create_dir_all(inbox.join("new")).unwrap();
        let p = inbox.join("cur").join(basename);
        let body = "Message-ID: <m1@example.invalid>\r\n\
                    From: a@b\r\n\
                    Subject: hi\r\n\
                    Date: Thu, 28 May 2026 12:00:00 +0000\r\n\
                    \r\n\
                    body\r\n";
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn auto_seen_does_not_pollute_undo_stack() {
        // Opening a message implicitly marks it Seen via try_mark_seen,
        // which bypasses the App-level wrapper. Confirms that internal
        // flag flips don't show up on the user-visible undo stack.
        let tmp = TempDir::new().unwrap();
        let _src = drop_inbox_msg(&tmp, "x1");
        let cfg = one_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);
        app.ensure_body_for_selection();
        assert_eq!(
            app.undo_stack.undo_len(),
            0,
            "auto-seen must not push to undo stack"
        );
    }

    #[test]
    fn flag_toggle_records_then_undo_restores() {
        let tmp = TempDir::new().unwrap();
        let _src = drop_inbox_msg(&tmp, "x1:2,S");
        let cfg = one_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);

        // Capture pre-state.
        let pre_flags = app
            .inbox()
            .threaded()
            .first()
            .expect("row")
            .row
            .flags
            .clone();
        assert!(pre_flags.contains('S'), "fixture must start Seen");

        // Forward action.
        app.toggle_flag_selected('S');
        let mid_flags = app
            .inbox()
            .threaded()
            .first()
            .expect("row")
            .row
            .flags
            .clone();
        assert!(!mid_flags.contains('S'), "S must be cleared after toggle");
        assert_eq!(app.undo_stack.undo_len(), 1, "toggle must record undo");
        assert_eq!(app.undo_stack.redo_len(), 0);

        // Undo restores the prior state.
        app.undo(&cfg);
        let after_flags = app
            .inbox()
            .threaded()
            .first()
            .expect("row")
            .row
            .flags
            .clone();
        assert!(
            after_flags.contains('S'),
            "undo must put S back, got {after_flags:?}"
        );
        assert_eq!(app.undo_stack.undo_len(), 0);
        assert_eq!(app.undo_stack.redo_len(), 1, "undo must push redo entry");

        // Redo re-clears it.
        app.redo(&cfg);
        let redone_flags = app
            .inbox()
            .threaded()
            .first()
            .expect("row")
            .row
            .flags
            .clone();
        assert!(
            !redone_flags.contains('S'),
            "redo must re-clear S, got {redone_flags:?}"
        );
        assert_eq!(app.undo_stack.undo_len(), 1);
        assert_eq!(app.undo_stack.redo_len(), 0);
    }

    #[test]
    fn empty_undo_emits_status() {
        let tmp = TempDir::new().unwrap();
        let _ = drop_inbox_msg(&tmp, "x1:2,S");
        let cfg = one_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);

        app.undo(&cfg);
        assert_eq!(app.status_error.as_deref(), Some("nothing to undo"));
        app.redo(&cfg);
        assert_eq!(app.status_error.as_deref(), Some("nothing to redo"));
    }

    #[test]
    fn fresh_action_clears_redo() {
        let tmp = TempDir::new().unwrap();
        let _ = drop_inbox_msg(&tmp, "x1:2,S");
        let cfg = one_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);

        app.toggle_flag_selected('S');
        app.undo(&cfg);
        assert_eq!(app.undo_stack.redo_len(), 1, "undo populates redo");
        // Fresh action invalidates the redo branch.
        app.toggle_flag_selected('F');
        assert_eq!(
            app.undo_stack.redo_len(),
            0,
            "new mutation must clear redo trail"
        );
        assert_eq!(app.undo_stack.undo_len(), 1);
    }

    #[test]
    fn was_set_captured_pre_toggle() {
        // Pre-state must be captured BEFORE the toggle fires, otherwise
        // the recorded `was_set` reflects post-toggle state and undo
        // becomes a no-op.
        let tmp = TempDir::new().unwrap();
        let _ = drop_inbox_msg(&tmp, "x1:2,S");
        let cfg = one_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);

        app.toggle_flag_selected('F');
        match app.undo_stack.pop_undo().expect("recorded") {
            UndoAction::Flag { flag, was_set, .. } => {
                assert_eq!(flag, 'F');
                assert!(!was_set, "F was not set before user pressed *");
            }
            _ => panic!("expected Flag"),
        }
    }

    #[test]
    fn archive_then_undo_returns_file_to_inbox() {
        let tmp = TempDir::new().unwrap();
        let _ = drop_inbox_msg(&tmp, "x1:2,S");
        let cfg = account_with_folders(&tmp, Some("Archive"), None);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);
        let msgid = app.inbox().selected_msgid().expect("a selected row");

        // Forward: archive.
        cmdline::dispatch("archive", &mut app, &cfg);
        let archived = tmp
            .path()
            .join("Mail")
            .join("personal")
            .join(".Archive")
            .join("cur")
            .join("x1:2,S");
        assert!(archived.exists(), "archive must place file in .Archive/cur");
        assert_eq!(app.undo_stack.undo_len(), 1);

        // Undo: file should be back in INBOX/cur. Path may differ from
        // the original due to filename canonicalization, but msgid lookup
        // tells us where it landed.
        app.undo(&cfg);
        assert!(!archived.exists(), "undo must remove file from Archive");
        let idx = Index::open(&app.cache_path).unwrap();
        let row = idx
            .get(&msgid, "personal", "INBOX")
            .unwrap()
            .expect("row still indexed");
        assert_eq!(row.folder, "INBOX");
        assert!(row.path.exists(), "undo-restored file must exist on disk");
        assert!(
            row.path
                .starts_with(tmp.path().join("Mail").join("personal").join("cur"))
        );
        assert_eq!(app.undo_stack.undo_len(), 0);
        assert_eq!(app.undo_stack.redo_len(), 1);
    }

    #[test]
    fn move_undo_marks_src_folder_dirty() {
        // The watcher's self-write suppression eats the rename event, so
        // the undo path is responsible for queuing a rescan. Confirms
        // src_folder lands in pending_dirty so the row re-surfaces in
        // the in-memory view.
        let tmp = TempDir::new().unwrap();
        let _ = drop_inbox_msg(&tmp, "x1:2,S");
        let cfg = account_with_folders(&tmp, Some("Archive"), None);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);

        cmdline::dispatch("archive", &mut app, &cfg);
        app.inbox_mut().pending_dirty.clear();
        app.undo(&cfg);
        assert!(
            app.inbox()
                .pending_dirty
                .contains(&("personal".to_string(), "INBOX".to_string())),
            "undo of archive must queue a rescan of INBOX"
        );
    }

    /// Helper: drop a maildir message with a custom msgid + optional
    /// In-Reply-To / References, so a test can build a small thread.
    fn drop_thread_msg(
        tmp: &TempDir,
        basename: &str,
        msgid: &str,
        in_reply: Option<&str>,
        date: &str,
    ) {
        let inbox = tmp.path().join("Mail").join("personal");
        fs::create_dir_all(inbox.join("cur")).unwrap();
        fs::create_dir_all(inbox.join("new")).unwrap();
        let mut body =
            format!("Message-ID: <{msgid}>\r\nFrom: a@b\r\nSubject: hi\r\nDate: {date}\r\n");
        if let Some(p) = in_reply {
            body.push_str(&format!("In-Reply-To: <{p}>\r\nReferences: <{p}>\r\n"));
        }
        body.push_str("\r\nbody\r\n");
        fs::write(inbox.join("cur").join(basename), body).unwrap();
    }

    #[test]
    fn trash_thread_moves_every_member_and_undoes_as_one() {
        // Three-message thread (root + reply + reply-of-reply). `D`
        // should move all three to Trash, push a single Batch undo so
        // one `u` brings the whole thread back, and clear the visible
        // list in between.
        let tmp = TempDir::new().unwrap();
        drop_thread_msg(
            &tmp,
            "r:2,S",
            "root",
            None,
            "Thu, 28 May 2026 12:00:00 +0000",
        );
        drop_thread_msg(
            &tmp,
            "a:2,S",
            "reply1",
            Some("root"),
            "Thu, 28 May 2026 12:01:00 +0000",
        );
        drop_thread_msg(
            &tmp,
            "b:2,S",
            "reply2",
            Some("reply1"),
            "Thu, 28 May 2026 12:02:00 +0000",
        );

        let cfg = account_with_folders(&tmp, None, Some("Trash"));
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);

        assert_eq!(app.inbox().threaded().len(), 3, "thread should have 3 rows");

        cmdline::trash_thread_selected(&mut app, &cfg);

        // All three files now live under .Trash/cur; the in-memory
        // INBOX view drained to empty.
        let trash_cur = tmp
            .path()
            .join("Mail")
            .join("personal")
            .join(".Trash")
            .join("cur");
        let count = fs::read_dir(&trash_cur).unwrap().count();
        assert_eq!(count, 3, "all three files should land in .Trash/cur");
        assert_eq!(app.inbox().threaded().len(), 0, "INBOX should be empty");

        // Single batched undo restores the whole thread in one step.
        assert_eq!(
            app.undo_stack.undo_len(),
            1,
            "trash-thread must record exactly one undo entry"
        );
        match app.undo_stack.pop_undo().expect("recorded") {
            UndoAction::Batch(ref children) => {
                assert_eq!(children.len(), 3, "batch should hold 3 leaves");
            }
            other => panic!("expected Batch, got {other:?}"),
        }
    }

    #[test]
    fn trash_thread_undo_restores_all_three_to_inbox() {
        // Companion to the move test: drive the full forward/undo cycle
        // through `D` and confirm `u` brings every member back to the
        // INBOX (the index resolves the new paths after rescan).
        let tmp = TempDir::new().unwrap();
        drop_thread_msg(
            &tmp,
            "r:2,S",
            "root",
            None,
            "Thu, 28 May 2026 12:00:00 +0000",
        );
        drop_thread_msg(
            &tmp,
            "a:2,S",
            "reply1",
            Some("root"),
            "Thu, 28 May 2026 12:01:00 +0000",
        );
        drop_thread_msg(
            &tmp,
            "b:2,S",
            "reply2",
            Some("reply1"),
            "Thu, 28 May 2026 12:02:00 +0000",
        );

        let cfg = account_with_folders(&tmp, None, Some("Trash"));
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);

        cmdline::trash_thread_selected(&mut app, &cfg);
        app.undo(&cfg);

        let inbox_cur = tmp.path().join("Mail").join("personal").join("cur");
        let count = fs::read_dir(&inbox_cur).unwrap().count();
        assert_eq!(count, 3, "undo must restore all three files to INBOX/cur");
        // Redo must be queued so Ctrl-r re-trashes.
        assert_eq!(
            app.undo_stack.undo_len(),
            0,
            "undo consumes the batch entry"
        );
        assert_eq!(
            app.undo_stack.redo_len(),
            1,
            "undo of batch must push a single redo entry"
        );
    }

    #[test]
    fn toggle_list_visual_anchors_on_list_focus_only() {
        let tmp = TempDir::new().unwrap();
        let cfg = one_account_config(&tmp);
        let cache = tmp.path().join("index.sqlite");
        let mut app = App::new(&cfg, cache, None, None);

        // Off the List pane it's a no-op — a row selection on the reader
        // or sidebar is meaningless.
        app.inbox_mut().focus = Pane::Reader;
        app.toggle_list_visual();
        assert_eq!(app.inbox().list_visual, None);

        // On the List pane it anchors at the cursor and toggles back off.
        app.inbox_mut().focus = Pane::List;
        app.inbox_mut().selected = 2;
        app.toggle_list_visual();
        assert_eq!(app.inbox().list_visual, Some(2));
        app.toggle_list_visual();
        assert_eq!(app.inbox().list_visual, None);
    }

    #[test]
    fn list_visual_range_trashes_all_with_single_undo() {
        // Three standalone messages (distinct ids → three list rows).
        // A `v`-anchored range over all of them should trash every row
        // and record a single batched undo, mirroring `D`.
        let tmp = TempDir::new().unwrap();
        drop_thread_msg(
            &tmp,
            "1:2,S",
            "a@x",
            None,
            "Thu, 28 May 2026 12:00:00 +0000",
        );
        drop_thread_msg(
            &tmp,
            "2:2,S",
            "b@x",
            None,
            "Thu, 28 May 2026 12:01:00 +0000",
        );
        drop_thread_msg(
            &tmp,
            "3:2,S",
            "c@x",
            None,
            "Thu, 28 May 2026 12:02:00 +0000",
        );
        let cfg = account_with_folders(&tmp, None, Some("Trash"));
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);
        assert_eq!(app.inbox().threaded().len(), 3);

        // Anchor at row 0, extend the cursor to row 2 → all three.
        app.inbox_mut().focus = Pane::List;
        app.inbox_mut().selected = 0;
        app.toggle_list_visual();
        app.inbox_mut().selected = 2;
        assert_eq!(app.inbox().list_visual, Some(0));

        cmdline::trash_selected(&mut app, &cfg);

        let trash_cur = tmp
            .path()
            .join("Mail")
            .join("personal")
            .join(".Trash")
            .join("cur");
        assert_eq!(
            fs::read_dir(&trash_cur).unwrap().count(),
            3,
            "every selected row should land in .Trash/cur"
        );
        assert_eq!(app.inbox().threaded().len(), 0, "INBOX view drained");
        assert_eq!(
            app.inbox().list_visual,
            None,
            "selection consumed by the move"
        );
        assert_eq!(
            app.undo_stack.undo_len(),
            1,
            "range trash must record exactly one batched undo entry"
        );

        app.undo(&cfg);
        let inbox_cur = tmp.path().join("Mail").join("personal").join("cur");
        assert_eq!(
            fs::read_dir(&inbox_cur).unwrap().count(),
            3,
            "a single undo must restore the whole range"
        );
    }

    #[test]
    fn list_visual_range_flags_all_and_batches_undo() {
        let tmp = TempDir::new().unwrap();
        drop_thread_msg(
            &tmp,
            "1:2,S",
            "a@x",
            None,
            "Thu, 28 May 2026 12:00:00 +0000",
        );
        drop_thread_msg(
            &tmp,
            "2:2,S",
            "b@x",
            None,
            "Thu, 28 May 2026 12:01:00 +0000",
        );
        let cfg = account_with_folders(&tmp, None, None);
        let cache = tmp.path().join("index.sqlite");

        let mut app = App::new(&cfg, cache, None, None);
        drain_scan(&mut app, &cfg);
        assert_eq!(app.inbox().threaded().len(), 2);

        app.inbox_mut().focus = Pane::List;
        app.inbox_mut().selected = 0;
        app.toggle_list_visual();
        app.inbox_mut().selected = 1;

        cmdline::flag_selection(&mut app, 'F');

        assert!(
            app.inbox()
                .threaded()
                .iter()
                .all(|t| t.row.flags.contains('F')),
            "both rows should be flagged"
        );
        assert_eq!(app.inbox().list_visual, None, "selection consumed");
        assert_eq!(
            app.undo_stack.undo_len(),
            1,
            "range flag must record one batched undo entry"
        );

        app.undo(&cfg);
        assert!(
            app.inbox()
                .threaded()
                .iter()
                .all(|t| !t.row.flags.contains('F')),
            "a single undo must clear the flag on every row"
        );
    }
}
