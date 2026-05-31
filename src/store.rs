pub mod index;
pub mod scan;
pub mod sync;
pub mod thread;
pub mod watch;

use std::path::{Path, PathBuf};

use crate::config::{Account, resolve_inbox_root};
use crate::mail::layout::Layout;

/// Per-account fan-out parameters for the scan worker and the inotify
/// watcher. Bundles the pieces both consumers need (name, maildir root,
/// on-disk folder layout, resolved INBOX path) so signatures don't grow
/// to quadruples.
#[derive(Debug, Clone)]
pub struct AccountSpec {
    pub name: String,
    pub root: PathBuf,
    pub layout: Layout,
    /// Resolved on-disk root for INBOX `cur/new/tmp`. Equal to `root`
    /// under the traditional convention (INBOX at the maildir root);
    /// otherwise a same-account subdir (e.g. `<root>/Inbox` for
    /// mbsync's default-pattern setup). Computed once at spec
    /// construction so workers don't re-stat on every walk.
    pub inbox_root: PathBuf,
}

impl AccountSpec {
    /// Build a spec from a config-side `Account`, resolving the
    /// INBOX path via the user's optional `inbox_folder` override and
    /// the on-disk fallback chain.
    pub fn from_account(name: &str, account: &Account) -> Self {
        let root = account.maildir.clone();
        let inbox_root = resolve_inbox_root(&root, account.inbox_folder.as_deref());
        Self {
            name: name.to_string(),
            root,
            layout: account.layout,
            inbox_root,
        }
    }

    /// Resolve a folder label to its on-disk root. `"INBOX"` routes
    /// through the resolved `inbox_root`; every other label goes
    /// through the layout's regular mapping.
    pub fn folder_path(&self, label: &str) -> PathBuf {
        if label == "INBOX" {
            self.inbox_root.clone()
        } else {
            self.layout.folder_path(&self.root, label)
        }
    }

    /// Discover every non-INBOX folder under the account root,
    /// filtering out whatever subdir was resolved as INBOX so it
    /// isn't double-listed as both `"INBOX"` and (e.g.) `"Inbox"`.
    pub fn discover_non_inbox_folders(&self) -> Vec<(String, PathBuf)> {
        let inbox = self.inbox_root.as_path();
        self.layout
            .discover_folders(&self.root)
            .into_iter()
            .filter(|(_, p)| p.as_path() != inbox)
            .collect()
    }

    /// Project a config `(name, Account)` pair into a fresh spec.
    /// Convenience for the `cfg.accounts.iter().map(...)` pattern at
    /// the worker / watcher boundaries.
    pub fn from_pair(pair: (&String, &Account)) -> Self {
        Self::from_account(pair.0, pair.1)
    }

    /// `<root>/cur` exists? Used by the watcher to decide whether
    /// there's anything to watch directly at the account root (vs.
    /// only via subfolders). Kept here so the path-vs-Path Layout
    /// stuff stays bundled.
    #[allow(dead_code)]
    pub fn root_is_maildir(root: &Path) -> bool {
        root.join("cur").is_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn mk_maildir(root: &std::path::Path, dir: &str) {
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(root.join(dir).join(sub)).unwrap();
        }
    }

    #[test]
    fn discover_excludes_resolved_inbox_subdir() {
        // mbsync default-pattern verbatim setup: no maildir at root,
        // INBOX lives at `<root>/Inbox`, plus `Sent`/`Archive` at the
        // same level. Discovery must not list `Inbox` as a separate
        // folder — that's what `INBOX` already covers.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        mk_maildir(&root, "Inbox");
        mk_maildir(&root, "Sent");
        mk_maildir(&root, "Archive");

        let acc = Account {
            maildir: root.clone(),
            from: "x".into(),
            layout: Layout::Verbatim,
            inbox_folder: None,
            sent_folder: None,
            archive_folder: None,
            spam_folder: None,
            trash_folder: None,
            smtp: None,
        };
        let spec = AccountSpec::from_account("a", &acc);

        // INBOX resolved to the subdir.
        assert_eq!(spec.inbox_root, root.join("Inbox"));

        let labels: Vec<String> = spec
            .discover_non_inbox_folders()
            .into_iter()
            .map(|(l, _)| l)
            .collect();
        // Sorted by Layout::discover_folders; Inbox is filtered out.
        assert_eq!(labels, vec!["Archive".to_string(), "Sent".to_string()]);
    }

    #[test]
    fn discover_keeps_inbox_subdir_when_root_is_the_inbox() {
        // Traditional layout: maildir at root IS the INBOX. A subdir
        // literally named `Inbox` here would be a regular folder
        // (unlikely but valid), and discovery should NOT filter it.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        mk_maildir(&root, ""); // root is a maildir
        mk_maildir(&root, "Inbox"); // also a regular subfolder

        let acc = Account {
            maildir: root.clone(),
            from: "x".into(),
            layout: Layout::Verbatim,
            inbox_folder: None,
            sent_folder: None,
            archive_folder: None,
            spam_folder: None,
            trash_folder: None,
            smtp: None,
        };
        let spec = AccountSpec::from_account("a", &acc);

        // inbox_root resolved to root (root has cur/), not the Inbox subdir.
        assert_eq!(spec.inbox_root, root);
        let labels: Vec<String> = spec
            .discover_non_inbox_folders()
            .into_iter()
            .map(|(l, _)| l)
            .collect();
        // The literal `Inbox` subdir survives discovery as a regular folder.
        assert_eq!(labels, vec!["Inbox".to_string()]);
    }
}
