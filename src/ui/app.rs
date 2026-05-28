use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui_image::picker::Picker;

use crate::config::Config;
use crate::mail::html::{self, Block};
use crate::mail::parse;
use crate::store::scan::{self, ScanResult};
use crate::store::thread::ThreadedRow;
use crate::ui::images::{self, ImageKey, ResolvedImage};
use crate::ui::{accounts, cmdline, folders, list, reader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Folders,
    List,
    Reader,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Reader,
    Command,
    LinkPick,
}

#[derive(Debug, Clone)]
pub struct ParsedBody {
    // Carried so a future invalidation pass can sanity-check the cache
    // against the selected row; `last_parsed_msgid` on the App is what
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

pub struct App {
    pub mode: Mode,
    pub focus: Pane,
    pub sidebar_visible: bool,
    pub list_visible: bool,
    pub reader_visible: bool,
    pub reader_scroll: u16,
    pub quit: bool,
    pub scan: ScanState,
    pub selected: usize,
    pub parsed: Option<ParsedBody>,
    pub cmdline_buf: String,
    pub link_pick_buf: String,
    /// Transient status / error displayed in the cmdline row. Cleared
    /// when the user enters a new command or moves selection.
    pub status_error: Option<String>,
    /// Last-known msgid we tried to parse a body for, so a parse failure
    /// doesn't loop forever.
    last_parsed_msgid: Option<String>,
    /// Capped at max_height_cells from `[images]`. Surfaced to the reader
    /// so layout caps reservation height the same way the decode does.
    pub image_max_height_cells: u16,
    /// Capability picker; `None` when `[images].protocol = "off"` or
    /// stdio isn't a tty. When None the reader always renders the
    /// `[image: alt]` placeholder.
    picker: Option<Picker>,
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
    /// Set by `ensure_body_for_selection` when the body changed this
    /// tick; reader consumes it to drive the Clear pass.
    pub body_changed_this_tick: bool,
    scan_rx: Option<Receiver<ScanResult>>,
}

impl App {
    pub fn new(cfg: &Config, cache_path: PathBuf, picker: Option<Picker>) -> Self {
        let sidebar_visible = cfg.ui.sidebar;
        let list_visible = cfg.ui.list;
        let reader_visible = cfg.ui.reader;
        let focus = initial_focus(sidebar_visible, list_visible, reader_visible);

        let accounts: Vec<(String, PathBuf)> = cfg
            .accounts
            .iter()
            .map(|(name, a)| (name.clone(), a.maildir.clone()))
            .collect();
        let scan_rx = if accounts.is_empty() {
            None
        } else {
            Some(scan::start_worker(accounts, cache_path))
        };
        let scan = if scan_rx.is_some() {
            ScanState::Scanning
        } else {
            ScanState::Ready(Vec::new())
        };

        Self {
            mode: Mode::Normal,
            focus,
            sidebar_visible,
            list_visible,
            reader_visible,
            reader_scroll: 0,
            quit: false,
            scan,
            selected: 0,
            parsed: None,
            cmdline_buf: String::new(),
            link_pick_buf: String::new(),
            status_error: None,
            last_parsed_msgid: None,
            image_max_height_cells: cfg.images.max_height_cells,
            picker,
            image_cache: HashMap::new(),
            prev_parsed_msgid: None,
            last_image_rects: Vec::new(),
            body_changed_this_tick: false,
            scan_rx,
        }
    }

    /// Read-only lookup the reader uses when laying out `Block::Image`.
    /// Returns `None` for any image without a successfully decoded entry
    /// (remote URLs, missing cid parts, decode failures, `protocol = off`).
    pub fn resolved_image(&self, key: &ImageKey) -> Option<&ResolvedImage> {
        let msgid = self.last_parsed_msgid.as_deref()?;
        self.image_cache.get(msgid)?.get(key)
    }

    /// Re-reads and parses the body for the currently-selected message
    /// when it differs from the cached body. Parse failures surface in
    /// `status_error` and leave `parsed = None` without retrying.
    /// On success also decodes every reachable `cid:` / `data:` image
    /// into `self.image_cache[msgid]`; decode failures are listed in
    /// `status_error` but don't block the rest of the body.
    pub fn ensure_body_for_selection(&mut self) {
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
        let Some(path) = self.selected_path() else {
            self.parsed = None;
            self.evict_image_cache(old_msgid.as_deref(), &msgid);
            return;
        };
        match parse::read_body(&path) {
            Ok(body) => {
                let blocks = body.html.as_deref().map(html::parse).unwrap_or_default();
                self.decode_images(&msgid, &blocks, &body.cid_parts);
                self.parsed = Some(ParsedBody {
                    msgid: msgid.clone(),
                    blocks,
                    raw_html: body.html,
                    plain_fallback: body.plain,
                    cid_parts: body.cid_parts,
                });
                self.evict_image_cache(old_msgid.as_deref(), &msgid);
            }
            Err(e) => {
                self.parsed = None;
                self.status_error = Some(format!("parse failed: {e:#}"));
                self.evict_image_cache(old_msgid.as_deref(), &msgid);
            }
        }
    }

    /// Walks blocks, decodes resolvable images into the cache, and
    /// records failures so they can be surfaced in `status_error`.
    fn decode_images(
        &mut self,
        msgid: &str,
        blocks: &[Block],
        cid_parts: &HashMap<String, Vec<u8>>,
    ) {
        let Some(picker) = self.picker.as_ref() else {
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
                    match images::decode(picker, bytes, self.image_max_height_cells) {
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
                    match images::decode(picker, &bytes, self.image_max_height_cells) {
                        Ok(img) => {
                            entries.insert(key, img);
                        }
                        Err(_) => failed.push("data:… (decode failed)".to_string()),
                    }
                }
            }
        }
        let entries_n = entries.len();
        if entries.is_empty() {
            self.image_cache.remove(msgid);
        } else {
            self.image_cache.insert(msgid.to_string(), entries);
        }
        // Diagnostic: when a message references images, surface whether
        // we actually cached any so "why is there only a placeholder?"
        // is debuggable without instrumentation.
        let total_refs = entries_n + failed.len();
        if total_refs > 0 {
            let mut parts = vec![format!("images: {entries_n}/{total_refs} decoded")];
            if !failed.is_empty() {
                parts.push(format!("failed: {}", failed.join(", ")));
            }
            self.status_error = Some(parts.join(" — "));
        }
    }

    fn evict_image_cache(&mut self, old: Option<&str>, current: &str) {
        // Keep current + previous; drop everything else.
        self.prev_parsed_msgid = old.map(|s| s.to_string());
        let keep: [Option<&str>; 2] = [Some(current), self.prev_parsed_msgid.as_deref()];
        self.image_cache
            .retain(|k, _| keep.contains(&Some(k.as_str())));
    }

    fn selected_row(&self) -> Option<&ThreadedRow> {
        match &self.scan {
            ScanState::Ready(rows) if !rows.is_empty() => {
                let i = self.selected.min(rows.len() - 1);
                rows.get(i)
            }
            _ => None,
        }
    }

    pub fn selected_msgid(&self) -> Option<String> {
        self.selected_row().map(|r| r.row.msgid.clone())
    }

    pub fn selected_path(&self) -> Option<PathBuf> {
        self.selected_row().map(|r| r.row.path.clone())
    }

    pub fn poll_scan(&mut self) {
        let Some(rx) = self.scan_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(rows)) => {
                self.scan = ScanState::Ready(rows);
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

    pub fn cycle_focus(&mut self, forward: bool) {
        let ring = [Pane::Folders, Pane::List, Pane::Reader];
        let visible = |p: Pane| match p {
            Pane::Folders => self.sidebar_visible,
            Pane::List => self.list_visible,
            Pane::Reader => self.reader_visible,
        };
        let n = ring.len();
        let start = ring.iter().position(|p| *p == self.focus).unwrap_or(0);
        for step in 1..=n {
            let i = if forward {
                (start + step) % n
            } else {
                (start + n - step) % n
            };
            if visible(ring[i]) {
                self.focus = ring[i];
                return;
            }
        }
    }

    pub fn threaded(&self) -> &[ThreadedRow] {
        match &self.scan {
            ScanState::Ready(rows) => rows,
            _ => &[],
        }
    }

    pub fn select_next(&mut self) {
        let len = self.threaded().len();
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
}

/// Single image reference reached from a Block-IR walk. Used by
/// `App::decode_images` to enumerate every renderable image in a parsed
/// body without exposing the walk to the rest of the app.
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

    let (sidebar_area, right_area) = split_body(body, app.sidebar_visible);
    let (list_area, reader_area) = split_right(right_area, app.list_visible, app.reader_visible);

    accounts::draw(f, top, app);
    if let Some(rect) = sidebar_area {
        folders::draw(f, rect, app);
    }
    if let Some(rect) = list_area {
        list::draw(f, rect, app);
    }
    if let Some(rect) = reader_area {
        reader::draw(f, rect, &mut *app);
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
