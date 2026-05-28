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
    #[serde(default = "default_view")]
    pub default_view: String,
    #[serde(default)]
    pub load_images: bool,
}

impl Default for Reader {
    fn default() -> Self {
        Self {
            default_view: default_view(),
            load_images: false,
        }
    }
}

fn default_view() -> String {
    "html".to_string()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub maildir: PathBuf,
    pub from: String,
    #[serde(default)]
    pub sent_folder: Option<String>,
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
            "#,
        )
        .unwrap();
        assert!(cfg.ui.reader);
        assert_eq!(cfg.accounts["dev"].from, "Dev <dev@example.invalid>");
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
}
