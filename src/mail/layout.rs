//! Maildir folder layout: switch between Maildir++ (dot-prefixed flat
//! subfolders — `.Sent`, `.Sent.2024`) and fs / nested (real
//! subdirectories — `Sent/`, `Sent/2024/`). Picked per account in config.
//!
//! Folder labels are stored as opaque strings in the index. The natural
//! form is per-layout: Maildir++ strips the leading dot but keeps inner
//! dots (`.Sent.2024` → `Sent.2024`); fs uses the `/`-joined relative
//! path from the account root (`Sent/2024`). The two namespaces never
//! mix — a given account is one or the other.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Layout {
    #[default]
    #[serde(rename = "maildir++")]
    Maildirpp,
    #[serde(rename = "fs")]
    Fs,
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
            Layout::Fs => {
                let mut p = root.to_path_buf();
                for seg in label.split('/').filter(|s| !s.is_empty()) {
                    p.push(seg);
                }
                p
            }
        }
    }

    /// Discover every subfolder under `root`. INBOX is implicit and not
    /// returned. Each entry is `(label, folder_root)` where `folder_root`
    /// contains `cur/new/tmp`. Sorted by label.
    pub fn discover_folders(&self, root: &Path) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        match self {
            Layout::Maildirpp => discover_maildirpp(root, &mut out),
            Layout::Fs => discover_fs(root, "", &mut out),
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

fn discover_maildirpp(root: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let path = e.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with('.') || name == "." || name == ".." {
            continue;
        }
        let label = name.strip_prefix('.').unwrap_or(name).to_string();
        out.push((label, path));
    }
}

fn discover_fs(root: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let path = e.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Skip the maildir's own cur/new/tmp at every depth and any
        // dotfile dirs (.git, .notmuch, etc.).
        if matches!(name, "cur" | "new" | "tmp") || name.starts_with('.') {
            continue;
        }
        let label = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        // A directory is a folder iff it contains `cur/`. Either way we
        // recurse — a folder can itself contain sub-folders.
        if path.join("cur").is_dir() {
            out.push((label.clone(), path.clone()));
        }
        discover_fs(&path, &label, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn mkmaildir(p: &Path) {
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(p.join(sub)).unwrap();
        }
    }

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
    fn fs_folder_path_uses_slashes() {
        let l = Layout::Fs;
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
    fn maildirpp_discover_strips_leading_dot() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        mkmaildir(root);
        mkmaildir(&root.join(".Sent"));
        mkmaildir(&root.join(".Sent.2024"));
        mkmaildir(&root.join(".Archive"));
        // A stray non-dot dir is ignored under maildir++.
        fs::create_dir_all(root.join("ignored")).unwrap();

        let folders = Layout::Maildirpp.discover_folders(root);
        let labels: Vec<&str> = folders.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["Archive", "Sent", "Sent.2024"]);
    }

    #[test]
    fn fs_discover_recurses_and_uses_slash_labels() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        mkmaildir(root);
        mkmaildir(&root.join("Sent"));
        mkmaildir(&root.join("Sent/2024"));
        mkmaildir(&root.join("Archive"));
        // A pure container (no cur/) that holds another folder: only
        // the inner one is a folder, but discovery still descends.
        fs::create_dir_all(root.join("Containers")).unwrap();
        mkmaildir(&root.join("Containers/Project"));
        // Dotfile dirs and dot-prefixed entries are skipped.
        fs::create_dir_all(root.join(".notmuch")).unwrap();

        let folders = Layout::Fs.discover_folders(root);
        let labels: Vec<&str> = folders.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            vec!["Archive", "Containers/Project", "Sent", "Sent/2024"]
        );
        // Folder roots resolve to the right paths.
        let lookup: std::collections::HashMap<&str, &Path> = folders
            .iter()
            .map(|(l, p)| (l.as_str(), p.as_path()))
            .collect();
        assert_eq!(lookup["Sent/2024"], root.join("Sent/2024").as_path());
        assert_eq!(
            lookup["Containers/Project"],
            root.join("Containers/Project").as_path()
        );
    }

    #[test]
    fn fs_discover_skips_maildir_internals_at_every_depth() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        mkmaildir(root);
        mkmaildir(&root.join("Sent"));
        // A nested folder literally named like an internal would be a
        // spec violation; cur/new/tmp are reserved at every depth.
        assert_eq!(
            Layout::Fs
                .discover_folders(root)
                .into_iter()
                .map(|(l, _)| l)
                .collect::<Vec<_>>(),
            vec!["Sent".to_string()]
        );
    }

    #[test]
    fn deserialize_layout_accepts_both_names() {
        #[derive(Deserialize)]
        struct W {
            layout: Layout,
        }
        let a: W = toml::from_str("layout = \"maildir++\"\n").unwrap();
        let b: W = toml::from_str("layout = \"fs\"\n").unwrap();
        assert_eq!(a.layout, Layout::Maildirpp);
        assert_eq!(b.layout, Layout::Fs);
    }
}
