// Step 1 only reads [ui]; the rest of the schema is parsed strictly to lock the
// shape in place. Future build steps consume `smtp`, `sync`, `reader`, `accounts`,
// `keys`, and `Account.{sent_folder,smtp}`.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub ui: Ui,
    #[serde(default)]
    pub smtp: Smtp,
    #[serde(default)]
    pub sync: Sync,
    #[serde(default)]
    pub reader: Reader,
    #[serde(default)]
    pub images: Images,
    #[serde(default)]
    pub compose: Compose,
    #[serde(default)]
    pub watch: Watch,
    #[serde(default)]
    pub search: Search,
    #[serde(default)]
    pub accounts: HashMap<String, Account>,
    #[serde(default)]
    pub keys: HashMap<String, HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ui {
    #[serde(default = "yes")]
    pub sidebar: bool,
    #[serde(default = "yes")]
    pub list: bool,
    #[serde(default = "yes")]
    pub reader: bool,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            sidebar: true,
            list: true,
            reader: true,
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Smtp {
    #[serde(default)]
    pub command: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sync {
    #[serde(default)]
    pub command: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reader {
    #[serde(default)]
    pub prefer: ReaderPrefer,
    #[serde(default = "default_browser")]
    pub browser: Vec<String>,
    /// Optional shell-out fallback for yank/copy. When set, reader yanks
    /// pipe the selected text to this command's stdin instead of
    /// emitting OSC 52. Use for tmux setups or terminals where OSC 52 is
    /// disabled, e.g. `["wl-copy"]` or `["xclip", "-selection", "clipboard"]`.
    #[serde(default)]
    pub clipboard: Option<Vec<String>>,
    /// Mouse-drag selection in the reader pane. Default `true`: press
    /// anchors visual-char mode at the click cell, drag extends, release
    /// yanks. Cost of enabling: while epost is running the terminal's own
    /// drag-select / middle-click paste over the app's panes is consumed
    /// by the app instead of the terminal. Set `false` to keep the
    /// terminal's native selection.
    #[serde(default = "yes")]
    pub mouse: bool,
    /// Drag-and-drop opener for `:drag <n>`. Receives the attachment
    /// tempfile path as a trailing argv. Typically `["dragon",
    /// "--and-exit"]` (X11) or a Wayland equivalent. Unset = `:drag`
    /// errors out — different distros ship different binaries, so there
    /// is no useful default.
    #[serde(default)]
    pub drag: Option<Vec<String>>,
    /// Milliseconds to flash the yanked region after a reader yank
    /// (`yip` / `yap` / `yl` / visual-mode `y`) or a compose body yank
    /// (`yy` / visual-mode `y`). Mirrors vim's `vim-highlightedyank`
    /// plugin — since the cmdline status row is easy to miss, the brief
    /// yellow-on-black flash confirms both that the yank fired and what
    /// was copied (the latter matters because `yip`/`yap` infer the
    /// paragraph from cursor position). `0` disables the flash.
    #[serde(default = "default_yank_highlight_ms")]
    pub yank_highlight_ms: u16,
}

impl Default for Reader {
    fn default() -> Self {
        Self {
            prefer: ReaderPrefer::default(),
            browser: default_browser(),
            clipboard: None,
            mouse: true,
            drag: None,
            yank_highlight_ms: default_yank_highlight_ms(),
        }
    }
}

fn default_yank_highlight_ms() -> u16 {
    150
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReaderPrefer {
    #[default]
    Html,
    Plain,
}

fn default_browser() -> Vec<String> {
    vec!["xdg-open".to_string()]
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Images {
    #[serde(default)]
    pub protocol: ImagesProtocol,
    #[serde(default = "default_max_height_cells")]
    pub max_height_cells: u16,
}

impl Default for Images {
    fn default() -> Self {
        Self {
            protocol: ImagesProtocol::default(),
            max_height_cells: default_max_height_cells(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImagesProtocol {
    #[default]
    Auto,
    Kitty,
    Iterm,
    Sixel,
    Halfblocks,
    Off,
}

fn default_max_height_cells() -> u16 {
    24
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Watch {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
}

impl Default for Watch {
    fn default() -> Self {
        Self {
            enabled: true,
            debounce_ms: default_debounce_ms(),
        }
    }
}

fn default_debounce_ms() -> u64 {
    250
}

/// `g/` (global) search settings. `global_folders` is the priority-
/// ordered list of folder labels covered by global search; rows in
/// folders earlier in the list rank higher within the same score tier.
/// An empty list means "every folder, score-only ranking."
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Search {
    #[serde(default = "default_global_folders")]
    pub global_folders: Vec<String>,
}

impl Default for Search {
    fn default() -> Self {
        Self {
            global_folders: default_global_folders(),
        }
    }
}

fn default_global_folders() -> Vec<String> {
    vec![
        "INBOX".to_string(),
        "Archive".to_string(),
        "Sent".to_string(),
    ]
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compose {
    /// Editor backend for the body region. `native` (default) uses the
    /// built-in vim-style editor inside the ratatui frame; `external`
    /// auto-spawns `$EDITOR` under a pty when a compose tab opens. The
    /// native editor can always escape to `$EDITOR` on demand via
    /// `:edit`, regardless of this setting.
    #[serde(default)]
    pub mode: ComposeMode,
    /// External editor command, used by `:edit` and by `mode = "external"`.
    /// May contain whitespace-separated arguments (e.g.
    /// `"vim -c 'set ft=mail'"`); quoting is not parsed in v1. If unset,
    /// falls back to `$VISUAL` / `$EDITOR` / `vi` at spawn time.
    #[serde(default)]
    pub editor: Option<String>,
    /// "Undo send" window in seconds. `:send` hands the MIME bytes to a
    /// worker that waits this long before invoking `msmtp`; `:cancel-send`
    /// aborts in-flight sends still inside the window. `0` disables the
    /// delay (the worker dispatches immediately, same as pre-feature
    /// behaviour). Default 10s.
    #[serde(default = "default_send_delay_secs")]
    pub send_delay_secs: u64,
    /// Inline address completion for To / Cc / Bcc. Two sources, both
    /// optional: a startup walk of each account's Sent folder (always on)
    /// and an external mutt-style `query_command`. Results merge with
    /// external first, then native, deduped by lowercase email.
    #[serde(default)]
    pub address_book: AddressBook,
    /// Wrap width for the native editor's reflow operators (`gq` / `gw`).
    /// Default 72 — the conventional plain-text mail body width.
    #[serde(default = "default_text_width")]
    pub text_width: u16,
}

impl Default for Compose {
    fn default() -> Self {
        Self {
            mode: ComposeMode::default(),
            editor: None,
            send_delay_secs: default_send_delay_secs(),
            address_book: AddressBook::default(),
            text_width: default_text_width(),
        }
    }
}

fn default_send_delay_secs() -> u64 {
    10
}

fn default_text_width() -> u16 {
    72
}

/// Address-completion knobs for the compose tab's To / Cc / Bcc fields.
/// `query_command` follows the mutt `query_command` protocol: the query
/// is passed as the trailing argv element (no `%s` substitution), stdout
/// is tab-separated `email[TAB name[TAB extra…]]` lines, the first line
/// is treated as a status header and skipped, and an empty stdout means
/// "no matches" (not an error).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddressBook {
    /// External address-book command (e.g. `"khard email --parsable"`).
    /// `None` (default) disables the external source; the native Sent
    /// scan still runs.
    #[serde(default)]
    pub query_command: Option<String>,
    /// Milliseconds between the last keystroke and the external command
    /// firing. Bounds the worst-case rate at one process per
    /// `debounce_ms`, so even slow address books (khard with a network
    /// CardDAV backend) don't get spammed.
    #[serde(default = "default_ab_debounce_ms")]
    pub debounce_ms: u64,
    /// Token length that opens the popup and triggers a query. Below
    /// this, the popup is closed. Default 2 keeps single-letter typos
    /// from popping the picker.
    #[serde(default = "default_ab_min_chars")]
    pub min_chars: usize,
}

impl Default for AddressBook {
    fn default() -> Self {
        Self {
            query_command: None,
            debounce_ms: default_ab_debounce_ms(),
            min_chars: default_ab_min_chars(),
        }
    }
}

fn default_ab_debounce_ms() -> u64 {
    150
}

fn default_ab_min_chars() -> usize {
    2
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComposeMode {
    #[default]
    Native,
    External,
}

/// Resolve the editor argv at spawn time. Done late (not at parse) so
/// `$EDITOR` set after launch is honored on subsequent edits.
pub fn resolve_editor(c: &Compose) -> Vec<String> {
    let raw = c
        .editor
        .clone()
        .or_else(|| std::env::var("VISUAL").ok())
        .or_else(|| std::env::var("EDITOR").ok())
        .unwrap_or_else(|| "vi".into());
    raw.split_whitespace().map(String::from).collect()
}

/// Resolve the "default-when-no-scope" account: the one flagged
/// `primary = true`, or the alphabetically-first account when none is
/// flagged (or several are — multi-primary is treated as
/// unconfigured so the choice stays deterministic). Returns the
/// account's key (`[accounts.<name>]`); look the `Account` itself up
/// via `cfg.accounts.get(...)`.
pub fn primary_account_name(cfg: &Config) -> Option<String> {
    let primaries: Vec<&String> = cfg
        .accounts
        .iter()
        .filter(|(_, a)| a.primary)
        .map(|(name, _)| name)
        .collect();
    if primaries.len() == 1 {
        return Some(primaries[0].clone());
    }
    // None set, or more than one set: fall back to alphabetic first so
    // the chosen default doesn't shift between runs (HashMap iteration
    // order is not stable across hashes).
    let mut names: Vec<&String> = cfg.accounts.keys().collect();
    names.sort();
    names.first().map(|s| (*s).clone())
}

/// Pick the SMTP command for a given account: account-local override
/// first, then top-level `[smtp].command`, else error.
pub fn smtp_command_for<'a>(cfg: &'a Config, account: &str) -> Result<&'a [String], String> {
    let acc = cfg
        .accounts
        .get(account)
        .ok_or_else(|| format!("unknown account: {account}"))?;
    if let Some(s) = acc.smtp.as_ref().filter(|s| !s.command.is_empty()) {
        return Ok(&s.command);
    }
    if !cfg.smtp.command.is_empty() {
        return Ok(&cfg.smtp.command);
    }
    Err("smtp.command not configured".into())
}

/// Canonical folder roles. Each role has a fixed display label that
/// epost uses everywhere — in the sidebar, in the index `folder`
/// column, and as the target of role-bound commands (`:archive`,
/// `:trash`, …). The disk-side folder name (which provider you sync
/// from decides) lives in the per-account `Account` keys; the role
/// label decouples display + commands from disk naming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FolderRole {
    Inbox,
    Archive,
    Sent,
    Spam,
    Trash,
    Drafts,
}

impl FolderRole {
    /// Canonical display label. `INBOX` stays uppercase to match
    /// IMAP convention and existing assumptions; the rest are
    /// title-case English. These are the strings written into the
    /// index's `folder` column and rendered in the sidebar.
    pub fn label(self) -> &'static str {
        match self {
            FolderRole::Inbox => "INBOX",
            FolderRole::Archive => "Archive",
            FolderRole::Sent => "Sent",
            FolderRole::Spam => "Spam",
            FolderRole::Trash => "Trash",
            FolderRole::Drafts => "Drafts",
        }
    }

    /// Canonical role order. Drives sidebar layout (Inbox first, then
    /// Drafts, Sent, Archive, Spam, Trash) and the binding-build
    /// order in `AccountSpec`.
    pub const ALL: [FolderRole; 6] = [
        FolderRole::Inbox,
        FolderRole::Drafts,
        FolderRole::Sent,
        FolderRole::Archive,
        FolderRole::Spam,
        FolderRole::Trash,
    ];
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub maildir: PathBuf,
    pub from: String,
    #[serde(default)]
    pub layout: crate::mail::layout::Layout,
    /// Sub-directory under `maildir` that holds the account's INBOX
    /// `cur/new/tmp`. mbsync's `Patterns *` default treats INBOX as
    /// just another folder name and writes it to `<maildir>/Inbox/`
    /// (or `INBOX/`), which doesn't match the traditional "INBOX at
    /// the maildir root" convention. When this key is unset the
    /// scanner auto-detects: if `<maildir>/cur` exists it assumes
    /// root-INBOX (the legacy convention); otherwise it tries
    /// `<maildir>/Inbox/` then `<maildir>/INBOX/` and finally falls
    /// back to the root.
    #[serde(default)]
    pub inbox: Option<String>,
    /// Disk-side folder name for each canonical role. The role's
    /// `label()` (e.g. `"Archive"`) is what shows up in the sidebar
    /// and the index; the string here is the actual folder name in
    /// `maildir` (e.g. `"[Gmail]/All Mail"`). Unset means the
    /// account doesn't expose that role — `:archive` etc. will
    /// error.
    #[serde(default)]
    pub archive: Option<String>,
    #[serde(default)]
    pub sent: Option<String>,
    #[serde(default)]
    pub spam: Option<String>,
    #[serde(default)]
    pub trash: Option<String>,
    #[serde(default)]
    pub drafts: Option<String>,
    /// Folders that don't fit any canonical role but should still
    /// appear in the sidebar (and be scanned). Sidebar/index label
    /// is the literal string here. Examples: Gmail's `"[Gmail]/All
    /// Mail"`, project / mailing-list buckets, etc.
    #[serde(default)]
    pub extra_folders: Vec<String>,
    #[serde(default)]
    pub smtp: Option<Smtp>,
    /// Marks this account as the default identity for new (blank) sends
    /// from the unified `[all]` scope, where there is no per-account
    /// scope to imply a sender. Reply / reply-all / forward ignore this
    /// flag and continue to use the originating message's account. If no
    /// account is flagged (or the flag is set on more than one), the
    /// resolver falls back to the alphabetically-first account so the
    /// default stays stable across runs.
    #[serde(default)]
    pub primary: bool,
}

impl Account {
    /// Resolved on-disk root for the account's INBOX `cur/new/tmp`.
    /// See `inbox` for the resolution order. Hits the filesystem
    /// (`is_dir`) so callers should cache the result rather than
    /// re-resolving in hot paths.
    pub fn inbox_root(&self) -> PathBuf {
        resolve_inbox_root(&self.maildir, self.inbox.as_deref())
    }

    /// Disk-side folder name configured for the given role, if any.
    /// `None` means the role isn't bound on this account.
    pub fn role_disk_name(&self, role: FolderRole) -> Option<&str> {
        match role {
            FolderRole::Inbox => self.inbox.as_deref(),
            FolderRole::Archive => self.archive.as_deref(),
            FolderRole::Sent => self.sent.as_deref(),
            FolderRole::Spam => self.spam.as_deref(),
            FolderRole::Trash => self.trash.as_deref(),
            FolderRole::Drafts => self.drafts.as_deref(),
        }
    }
}

/// Resolve where INBOX's `cur/new/tmp` actually lives. Order:
/// explicit `inbox` override → `<root>/cur` exists (root is itself
/// the INBOX maildir, the traditional layout) → `<root>/Inbox` →
/// `<root>/INBOX` → fall back to `<root>` so callers still get a
/// stable path even when INBOX isn't synced yet.
pub fn resolve_inbox_root(root: &Path, inbox: Option<&str>) -> PathBuf {
    if let Some(sub) = inbox {
        return root.join(sub);
    }
    if root.join("cur").is_dir() {
        return root.to_path_buf();
    }
    for candidate in ["Inbox", "INBOX"] {
        let p = root.join(candidate);
        if p.join("cur").is_dir() {
            return p;
        }
    }
    root.to_path_buf()
}

pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config from {}", path.display()))?;
    let mut cfg: Config =
        toml::from_str(&text).with_context(|| format!("parsing config at {}", path.display()))?;
    for acc in cfg.accounts.values_mut() {
        acc.maildir = expand_tilde(&acc.maildir);
    }
    Ok(cfg)
}

pub fn default_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "epost")
        .map(|d| d.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("config.toml"))
}

pub fn default_cache_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "epost")
        .map(|d| d.cache_dir().join("index.sqlite"))
        .unwrap_or_else(|| PathBuf::from("index.sqlite"))
}

fn expand_tilde(p: &Path) -> PathBuf {
    let s = match p.to_str() {
        Some(s) => s,
        None => return p.to_path_buf(),
    };
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = directories::UserDirs::new()
    {
        return home.home_dir().join(rest);
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_account_returns_flagged_one() {
        let cfg: Config = toml::from_str(
            r#"
            [accounts.work]
            maildir = "./w"
            from = "W <w@example.invalid>"

            [accounts.personal]
            maildir = "./p"
            from = "P <p@example.invalid>"
            primary = true
            "#,
        )
        .unwrap();
        assert_eq!(primary_account_name(&cfg).as_deref(), Some("personal"));
    }

    #[test]
    fn primary_account_falls_back_to_alphabetic_when_unset() {
        // Two accounts, neither flagged → alphabetic-first tiebreaker
        // so the resolver is deterministic across HashMap rehashes.
        let cfg: Config = toml::from_str(
            r#"
            [accounts.work]
            maildir = "./w"
            from = "W <w@example.invalid>"

            [accounts.personal]
            maildir = "./p"
            from = "P <p@example.invalid>"
            "#,
        )
        .unwrap();
        assert_eq!(primary_account_name(&cfg).as_deref(), Some("personal"));
    }

    #[test]
    fn primary_account_falls_back_when_multiple_flagged() {
        // Ambiguous config (multiple primaries) collapses to the
        // alphabetic-first tiebreaker rather than picking arbitrarily.
        let cfg: Config = toml::from_str(
            r#"
            [accounts.work]
            maildir = "./w"
            from = "W <w@example.invalid>"
            primary = true

            [accounts.personal]
            maildir = "./p"
            from = "P <p@example.invalid>"
            primary = true
            "#,
        )
        .unwrap();
        assert_eq!(primary_account_name(&cfg).as_deref(), Some("personal"));
    }

    #[test]
    fn primary_account_none_for_empty_config() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(primary_account_name(&cfg).is_none());
    }

    #[test]
    fn compose_address_book_defaults_to_native_only() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.compose.address_book.query_command.is_none());
        assert_eq!(cfg.compose.address_book.debounce_ms, 150);
        assert_eq!(cfg.compose.address_book.min_chars, 2);
    }

    #[test]
    fn parses_compose_address_book_overrides() {
        let cfg: Config = toml::from_str(
            r#"
            [compose.address_book]
            query_command = "khard email --parsable"
            debounce_ms = 250
            min_chars = 3
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.compose.address_book.query_command.as_deref(),
            Some("khard email --parsable")
        );
        assert_eq!(cfg.compose.address_book.debounce_ms, 250);
        assert_eq!(cfg.compose.address_book.min_chars, 3);
    }

    #[test]
    fn unknown_key_in_compose_address_book_fails() {
        let err = toml::from_str::<Config>(
            r#"
            [compose.address_book]
            query_command = "khard"
            mystery = 7
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("mystery"));
    }

    #[test]
    fn parses_minimal_dev_config() {
        let cfg: Config = toml::from_str(
            r#"
            [ui]
            sidebar = true
            list = true
            reader = true

            [smtp]
            command = ["./dev/msmtp-stub"]

            [accounts.dev]
            maildir = "./dev/maildir"
            from = "Dev <dev@example.invalid>"
            sent = "Sent"
            archive = "Archive"
            spam = "Spam"
            trash = "Trash"
            "#,
        )
        .unwrap();
        assert!(cfg.ui.reader);
        assert_eq!(cfg.accounts["dev"].from, "Dev <dev@example.invalid>");
        let dev = &cfg.accounts["dev"];
        assert_eq!(dev.archive.as_deref(), Some("Archive"));
        assert_eq!(dev.spam.as_deref(), Some("Spam"));
        assert_eq!(dev.trash.as_deref(), Some("Trash"));
    }

    #[test]
    fn account_layout_defaults_to_verbatim() {
        let cfg: Config = toml::from_str(
            r#"
            [accounts.dev]
            maildir = "./dev/maildir"
            from = "Dev <dev@example.invalid>"
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.accounts["dev"].layout,
            crate::mail::layout::Layout::Verbatim
        );
    }

    #[test]
    fn account_layout_verbatim_parses() {
        let cfg: Config = toml::from_str(
            r#"
            [accounts.work]
            maildir = "./dev/maildir-verbatim"
            from = "Work <w@example.invalid>"
            layout = "verbatim"
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.accounts["work"].layout,
            crate::mail::layout::Layout::Verbatim
        );
    }

    #[test]
    fn unknown_key_at_top_level_fails() {
        let err = toml::from_str::<Config>("wat = true\n").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("wat"),
            "expected error to mention unknown key, got: {msg}"
        );
    }

    #[test]
    fn unknown_key_in_ui_fails() {
        let err = toml::from_str::<Config>(
            r#"
            [ui]
            sidebar = true
            mystery = 42
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("mystery"));
    }

    #[test]
    fn parses_reader_prefer_and_browser() {
        let cfg: Config = toml::from_str(
            r#"
            [reader]
            prefer = "plain"
            browser = ["firefox", "--new-tab"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.reader.prefer, ReaderPrefer::Plain);
        assert_eq!(cfg.reader.browser, vec!["firefox", "--new-tab"]);
    }

    #[test]
    fn reader_defaults_to_html_and_xdg_open() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.reader.prefer, ReaderPrefer::Html);
        assert_eq!(cfg.reader.browser, vec!["xdg-open".to_string()]);
        assert!(cfg.reader.clipboard.is_none());
        assert!(cfg.reader.mouse);
    }

    #[test]
    fn reader_mouse_can_be_disabled() {
        let cfg: Config = toml::from_str(
            r#"
            [reader]
            mouse = false
            "#,
        )
        .unwrap();
        assert!(!cfg.reader.mouse);
    }

    #[test]
    fn parses_reader_clipboard_fallback() {
        let cfg: Config = toml::from_str(
            r#"
            [reader]
            clipboard = ["wl-copy"]
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.reader.clipboard.as_deref(),
            Some(["wl-copy".to_string()].as_slice())
        );
    }

    #[test]
    fn parses_reader_drag_command() {
        let cfg: Config = toml::from_str(
            r#"
            [reader]
            drag = ["dragon", "--and-exit"]
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.reader.drag.as_deref(),
            Some(["dragon".to_string(), "--and-exit".to_string()].as_slice())
        );
    }

    #[test]
    fn reader_drag_defaults_to_none() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.reader.drag.is_none());
    }

    #[test]
    fn reader_yank_highlight_ms_defaults_to_150() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.reader.yank_highlight_ms, 150);
    }

    #[test]
    fn reader_yank_highlight_ms_parses_override() {
        let cfg: Config = toml::from_str(
            r#"
            [reader]
            yank_highlight_ms = 0
            "#,
        )
        .unwrap();
        assert_eq!(cfg.reader.yank_highlight_ms, 0);
    }

    #[test]
    fn parses_images_section() {
        let cfg: Config = toml::from_str(
            r#"
            [images]
            protocol = "kitty"
            max_height_cells = 12
            "#,
        )
        .unwrap();
        assert_eq!(cfg.images.protocol, ImagesProtocol::Kitty);
        assert_eq!(cfg.images.max_height_cells, 12);
    }

    #[test]
    fn unknown_key_in_reader_fails() {
        let err = toml::from_str::<Config>(
            r#"
            [reader]
            prefer = "html"
            default_view = "html"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("default_view"));
    }

    #[test]
    fn watch_defaults_on_with_250ms_debounce() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.watch.enabled);
        assert_eq!(cfg.watch.debounce_ms, 250);
    }

    #[test]
    fn parses_watch_section() {
        let cfg: Config = toml::from_str(
            r#"
            [watch]
            enabled = false
            debounce_ms = 500
            "#,
        )
        .unwrap();
        assert!(!cfg.watch.enabled);
        assert_eq!(cfg.watch.debounce_ms, 500);
    }

    #[test]
    fn unknown_key_in_watch_fails() {
        let err = toml::from_str::<Config>(
            r#"
            [watch]
            enabled = true
            mystery = 1
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("mystery"));
    }

    #[test]
    fn search_defaults_to_inbox_archive_sent() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(
            cfg.search.global_folders,
            vec![
                "INBOX".to_string(),
                "Archive".to_string(),
                "Sent".to_string()
            ]
        );
    }

    #[test]
    fn parses_search_section() {
        let cfg: Config = toml::from_str(
            r#"
            [search]
            global_folders = ["INBOX", "Archive"]
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.search.global_folders,
            vec!["INBOX".to_string(), "Archive".to_string()]
        );
    }

    #[test]
    fn search_empty_list_is_respected() {
        let cfg: Config = toml::from_str(
            r#"
            [search]
            global_folders = []
            "#,
        )
        .unwrap();
        assert!(cfg.search.global_folders.is_empty());
    }

    #[test]
    fn unknown_key_in_search_fails() {
        let err = toml::from_str::<Config>(
            r#"
            [search]
            global_folders = ["INBOX"]
            mystery = "boom"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("mystery"));
    }

    #[test]
    fn unknown_key_in_images_fails() {
        let err = toml::from_str::<Config>(
            r#"
            [images]
            protocol = "auto"
            bogus = true
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("bogus"));
    }

    /// Helper: create `<root>/<dir>/{cur,new,tmp}` so `is_dir`
    /// resolver probes succeed.
    fn mk_maildir(root: &Path, dir: &str) {
        for sub in ["cur", "new", "tmp"] {
            std::fs::create_dir_all(root.join(dir).join(sub)).unwrap();
        }
    }

    #[test]
    fn inbox_resolver_uses_explicit_override() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        mk_maildir(root, "");
        mk_maildir(root, "Custom");

        // Override wins even when root has its own cur/.
        assert_eq!(
            resolve_inbox_root(root, Some("Custom")),
            root.join("Custom")
        );
    }

    #[test]
    fn inbox_resolver_prefers_root_when_root_is_maildir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        mk_maildir(root, "");

        assert_eq!(resolve_inbox_root(root, None), root);
    }

    #[test]
    fn inbox_resolver_falls_back_to_inbox_subdir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        // No cur/ at root. mbsync default-pattern layout: Inbox/ subdir.
        std::fs::create_dir_all(root).unwrap();
        mk_maildir(root, "Inbox");

        assert_eq!(resolve_inbox_root(root, None), root.join("Inbox"));
    }

    #[test]
    fn inbox_resolver_falls_back_to_uppercase_inbox_subdir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root).unwrap();
        mk_maildir(root, "INBOX");

        assert_eq!(resolve_inbox_root(root, None), root.join("INBOX"));
    }

    #[test]
    fn inbox_resolver_falls_back_to_root_when_nothing_matches() {
        // No maildir on disk at all — resolver still returns a stable
        // path so callers don't have to special-case the empty case.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root).unwrap();

        assert_eq!(resolve_inbox_root(root, None), root);
    }

    #[test]
    fn account_folder_path_routes_inbox_through_resolver() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        mk_maildir(root, "Inbox");

        let acc = Account {
            maildir: root.to_path_buf(),
            from: "x".into(),
            layout: crate::mail::layout::Layout::Verbatim,
            inbox: None,
            sent: None,
            archive: None,
            spam: None,
            trash: None,

            drafts: None,

            extra_folders: Vec::new(),
            smtp: None,
            primary: false,
        };
        // Inbox resolver still operates on Account directly.
        assert_eq!(acc.inbox_root(), root.join("Inbox"));
        // Non-INBOX folder-path resolution moved to AccountSpec
        // bindings; see store.rs tests.
    }
}
