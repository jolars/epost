//! Maildir folder layout: switch between Maildir++ (dot-prefixed flat
//! subfolders — `.Sent`, `.Sent.2024`) and verbatim / nested (real
//! subdirectories — `Sent/`, `Sent/2024/`). Picked per account in
//! config. Name matches mbsync's `SubFolders Verbatim`.
//!
//! Folder labels are stored as opaque strings in the index. The natural
//! form is per-layout: Maildir++ strips the leading dot but keeps inner
//! dots (`.Sent.2024` → `Sent.2024`); verbatim uses the `/`-joined
//! relative path from the account root (`Sent/2024`). The two
//! namespaces never mix — a given account is one or the other.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Layout {
    #[serde(rename = "maildir++")]
    Maildirpp,
    #[default]
    #[serde(rename = "verbatim")]
    Verbatim,
}

impl Layout {
    /// Resolve a folder label to its on-disk folder root (the directory
    /// that contains `cur/new/tmp`). `INBOX` maps to the account root.
    pub fn folder_path(&self, root: &Path, label: &str) -> PathBuf {
        if label == "INBOX" {
            return root.to_path_buf();
        }
        match self {
            Layout::Maildirpp => root.join(format!(".{label}")),
            Layout::Verbatim => {
                let mut p = root.to_path_buf();
                for seg in label.split('/').filter(|s| !s.is_empty()) {
                    p.push(seg);
                }
                p
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maildirpp_folder_path_dot_prefixes() {
        let l = Layout::Maildirpp;
        assert_eq!(l.folder_path(Path::new("/m"), "INBOX"), PathBuf::from("/m"));
        assert_eq!(
            l.folder_path(Path::new("/m"), "Sent"),
            PathBuf::from("/m/.Sent")
        );
        assert_eq!(
            l.folder_path(Path::new("/m"), "Sent.2024"),
            PathBuf::from("/m/.Sent.2024")
        );
    }

    #[test]
    fn verbatim_folder_path_uses_slashes() {
        let l = Layout::Verbatim;
        assert_eq!(l.folder_path(Path::new("/m"), "INBOX"), PathBuf::from("/m"));
        assert_eq!(
            l.folder_path(Path::new("/m"), "Sent"),
            PathBuf::from("/m/Sent")
        );
        assert_eq!(
            l.folder_path(Path::new("/m"), "Sent/2024"),
            PathBuf::from("/m/Sent/2024")
        );
    }

    #[test]
    fn deserialize_layout_accepts_both_names() {
        #[derive(Deserialize)]
        struct W {
            layout: Layout,
        }
        let a: W = toml::from_str("layout = \"maildir++\"\n").unwrap();
        let b: W = toml::from_str("layout = \"verbatim\"\n").unwrap();
        assert_eq!(a.layout, Layout::Maildirpp);
        assert_eq!(b.layout, Layout::Verbatim);
    }
}
