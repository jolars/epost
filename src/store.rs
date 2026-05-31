pub mod index;
pub mod scan;
pub mod sync;
pub mod thread;
pub mod watch;

use std::path::PathBuf;

use crate::config::{Account, FolderRole, resolve_inbox_root};
use crate::mail::layout::Layout;

/// One scannable folder on an account: the label used in the index
/// `folder` column / sidebar display, plus the on-disk root that
/// holds `cur/new/tmp`. `role` is `Some` for canonical roles (so
/// `:archive` etc. can locate the right binding); `None` for
/// `extra_folders` entries (where the label is the literal disk
/// name).
#[derive(Debug, Clone)]
pub struct FolderBinding {
    pub label: String,
    pub path: PathBuf,
    pub role: Option<FolderRole>,
}

/// Per-account fan-out parameters for the scan worker and the inotify
/// watcher. Bundles the static-per-launch context they need (name,
/// maildir root, layout) plus the resolved binding list that drives
/// which folders get scanned, watched, and displayed — built once
/// from `Account` at startup so workers don't re-resolve on every
/// walk.
#[derive(Debug, Clone)]
pub struct AccountSpec {
    pub name: String,
    pub root: PathBuf,
    pub layout: Layout,
    /// Resolved binding list. INBOX is always first (built from the
    /// auto-detected or configured inbox root); the remaining
    /// entries are the canonical roles that the account actually
    /// binds (`archive`, `sent`, `spam`, `trash`, `drafts`) in
    /// canonical order, followed by `extra_folders` in their config
    /// order. Workers walk this list verbatim — no filesystem
    /// discovery, no folder shows up unless the user opted in.
    pub folders: Vec<FolderBinding>,
}

impl AccountSpec {
    pub fn from_account(name: &str, account: &Account) -> Self {
        let root = account.maildir.clone();
        let layout = account.layout;
        let inbox_path = resolve_inbox_root(&root, account.inbox.as_deref());
        let mut folders = Vec::new();
        folders.push(FolderBinding {
            label: FolderRole::Inbox.label().to_string(),
            path: inbox_path,
            role: Some(FolderRole::Inbox),
        });
        for role in FolderRole::ALL.iter().copied() {
            if role == FolderRole::Inbox {
                continue;
            }
            if let Some(disk_name) = account.role_disk_name(role) {
                folders.push(FolderBinding {
                    label: role.label().to_string(),
                    path: layout.folder_path(&root, disk_name),
                    role: Some(role),
                });
            }
        }
        for extra in &account.extra_folders {
            folders.push(FolderBinding {
                label: extra.clone(),
                path: layout.folder_path(&root, extra),
                role: None,
            });
        }
        Self {
            name: name.to_string(),
            root,
            layout,
            folders,
        }
    }

    /// Look up the binding rendered under `label` — either a role
    /// label (`"Archive"`) or an extra-folder literal. `None` means
    /// the account isn't configured to expose that folder.
    pub fn binding_by_label(&self, label: &str) -> Option<&FolderBinding> {
        self.folders.iter().find(|b| b.label == label)
    }

    /// Look up the binding for a canonical role. Drives `:archive` /
    /// `:trash` / sent-copy resolution.
    pub fn binding_by_role(&self, role: FolderRole) -> Option<&FolderBinding> {
        self.folders.iter().find(|b| b.role == Some(role))
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

    fn account(maildir: PathBuf, layout: Layout) -> Account {
        Account {
            maildir,
            from: "x".into(),
            layout,
            inbox: None,
            archive: None,
            sent: None,
            spam: None,
            trash: None,
            drafts: None,
            extra_folders: Vec::new(),
            smtp: None,
            primary: false,
        }
    }

    #[test]
    fn bindings_emit_inbox_only_when_no_roles_configured() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        mk_maildir(&root, "");

        let acc = account(root.clone(), Layout::Verbatim);
        let spec = AccountSpec::from_account("a", &acc);

        let labels: Vec<&str> = spec.folders.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, vec!["INBOX"]);
        assert_eq!(spec.folders[0].path, root);
        assert_eq!(spec.folders[0].role, Some(FolderRole::Inbox));
    }

    #[test]
    fn bindings_emit_roles_in_canonical_order() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        mk_maildir(&root, "");
        mk_maildir(&root, "Sent");
        mk_maildir(&root, "Archive");
        mk_maildir(&root, "Trash");

        let mut acc = account(root.clone(), Layout::Verbatim);
        // Config in non-canonical order shouldn't change emitted order.
        acc.trash = Some("Trash".into());
        acc.archive = Some("Archive".into());
        acc.sent = Some("Sent".into());

        let spec = AccountSpec::from_account("a", &acc);
        let labels: Vec<&str> = spec.folders.iter().map(|b| b.label.as_str()).collect();
        // FolderRole::ALL order: Inbox, Drafts, Sent, Archive, Spam, Trash.
        // Unset roles (Drafts, Spam) are skipped.
        assert_eq!(labels, vec!["INBOX", "Sent", "Archive", "Trash"]);
    }

    #[test]
    fn bindings_map_role_label_to_disk_path() {
        // The role label decouples display from disk naming. Here the
        // archive role points at a weirdly named disk folder, but the
        // sidebar/index see "Archive".
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        mk_maildir(&root, "");
        mk_maildir(&root, "MyWeirdArchive");

        let mut acc = account(root.clone(), Layout::Verbatim);
        acc.archive = Some("MyWeirdArchive".into());

        let spec = AccountSpec::from_account("a", &acc);
        let archive = spec.binding_by_role(FolderRole::Archive).unwrap();
        assert_eq!(archive.label, "Archive");
        assert_eq!(archive.path, root.join("MyWeirdArchive"));
    }

    #[test]
    fn extras_appear_after_roles_with_literal_labels() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        mk_maildir(&root, "");

        let mut acc = account(root.clone(), Layout::Verbatim);
        acc.archive = Some("Archive".into());
        acc.extra_folders = vec!["[Gmail]/All Mail".into(), "Receipts".into()];

        let spec = AccountSpec::from_account("a", &acc);
        let labels: Vec<&str> = spec.folders.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["INBOX", "Archive", "[Gmail]/All Mail", "Receipts"]
        );
        // Extras have no role attached — drives the role-vs-extra
        // branches in command routing.
        let extras: Vec<Option<FolderRole>> = spec
            .folders
            .iter()
            .filter(|b| b.label.contains("Gmail") || b.label == "Receipts")
            .map(|b| b.role)
            .collect();
        assert_eq!(extras, vec![None, None]);
    }

    #[test]
    fn inbox_resolved_to_subdir_when_root_not_a_maildir() {
        // mbsync default-pattern layout: root is not a maildir, INBOX
        // lives at `<root>/Inbox`. The binding's path should follow
        // the resolver.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        mk_maildir(&root, "Inbox");

        let acc = account(root.clone(), Layout::Verbatim);
        let spec = AccountSpec::from_account("a", &acc);
        assert_eq!(spec.folders[0].path, root.join("Inbox"));
    }
}
