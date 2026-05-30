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
}

impl Default for Reader {
    fn default() -> Self {
        Self {
            prefer: ReaderPrefer::default(),
            browser: default_browser(),
            clipboard: None,
            mouse: true,
        }
    }
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

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compose {
    /// Editor command for the compose body. May contain whitespace-
    /// separated arguments (e.g. `"vim -c 'set ft=mail'"`); quoting is
    /// not parsed in v1. If unset, falls back to `$VISUAL` / `$EDITOR` /
    /// `vi` at spawn time.
    #[serde(default)]
    pub editor: Option<String>,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub maildir: PathBuf,
    pub from: String,
    #[serde(default)]
    pub sent_folder: Option<String>,
    #[serde(default)]
    pub archive_folder: Option<String>,
    #[serde(default)]
    pub spam_folder: Option<String>,
    #[serde(default)]
    pub trash_folder: Option<String>,
    #[serde(default)]
    pub smtp: Option<Smtp>,
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
            sent_folder = "Sent"
            archive_folder = "Archive"
            spam_folder = "Spam"
            trash_folder = "Trash"
            "#,
        )
        .unwrap();
        assert!(cfg.ui.reader);
        assert_eq!(cfg.accounts["dev"].from, "Dev <dev@example.invalid>");
        let dev = &cfg.accounts["dev"];
        assert_eq!(dev.archive_folder.as_deref(), Some("Archive"));
        assert_eq!(dev.spam_folder.as_deref(), Some("Spam"));
        assert_eq!(dev.trash_folder.as_deref(), Some("Trash"));
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
}
