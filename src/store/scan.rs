use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use anyhow::{Context, Result};

use crate::mail::flags;
use crate::mail::parse;
use crate::store::index::{FolderStat, Index, MessageRow};
use crate::store::thread::{ThreadedRow, build_threads};

/// Per-scope folder stats: `scope = None` is the unified "[all]" group;
/// `Some(name)` is one account. The sidebar renders one group block per
/// entry in display order: `[all]` first, then accounts alphabetically.
#[derive(Debug, Clone)]
pub struct AccountFolderStats {
    pub scope: Option<String>,
    pub folders: Vec<FolderStat>,
}

/// Successful scan payload: the threaded rows for the current scope the
/// list pane renders, plus the multi-group folder roll-up the sidebar
/// renders. Both are derived from the same index pass so the two views
/// are consistent.
pub struct ScanData {
    pub threads: Vec<ThreadedRow>,
    pub groups: Vec<AccountFolderStats>,
}

pub type ScanResult = std::result::Result<ScanData, String>;

/// `current_scope` is the `(account, folder)` pair driving the list
/// pane — `account = None` means the unified view. The grouped stats
/// returned by the worker always cover every configured account plus
/// the "[all]" group regardless of scope.
pub fn start_worker(
    accounts: Vec<(String, PathBuf)>,
    cache_path: PathBuf,
    current_scope: (Option<String>, String),
) -> Receiver<ScanResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run(&cache_path, &accounts, &current_scope).map_err(|e| format!("{e:#}"));
        let _ = tx.send(result);
    });
    rx
}

fn run(
    cache_path: &Path,
    accounts: &[(String, PathBuf)],
    current_scope: &(Option<String>, String),
) -> Result<ScanData> {
    let mut idx = Index::open(cache_path)?;
    for (name, root) in accounts {
        scan_account(name, root, &mut idx)?;
    }
    let (scope_account, scope_folder) = current_scope;
    let rows = idx.list_folder(scope_account.as_deref(), scope_folder)?;
    let groups = build_groups(&idx, accounts)?;
    Ok(ScanData {
        threads: build_threads(rows),
        groups,
    })
}

/// One `folder_stats(None)` for the "[all]" group, then one
/// `folder_stats(Some(account))` per configured account, sorted by name.
/// Order matches what the sidebar renders top-to-bottom.
fn build_groups(idx: &Index, accounts: &[(String, PathBuf)]) -> Result<Vec<AccountFolderStats>> {
    let mut groups = Vec::with_capacity(accounts.len() + 1);
    groups.push(AccountFolderStats {
        scope: None,
        folders: idx.folder_stats(None)?,
    });
    let mut names: Vec<&str> = accounts.iter().map(|(n, _)| n.as_str()).collect();
    names.sort();
    for name in names {
        groups.push(AccountFolderStats {
            scope: Some(name.to_string()),
            folders: idx.folder_stats(Some(name))?,
        });
    }
    Ok(groups)
}

pub fn scan_account(account: &str, root: &Path, index: &mut Index) -> Result<ScanReport> {
    let mut report = ScanReport::default();
    let found = scan_folder(account, root, "INBOX", index, &mut report)?;
    index.prune_folder(account, "INBOX", &found)?;

    let entries = std::fs::read_dir(root)
        .with_context(|| format!("listing maildir root {}", root.display()))?;
    let mut subdirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with('.') && n != "." && n != "..")
                .unwrap_or(false)
        })
        .collect();
    subdirs.sort();

    for sub in subdirs {
        let label = sub
            .file_name()
            .and_then(|n| n.to_str())
            .map(folder_label)
            .unwrap_or_else(|| "?".to_string());
        let found = scan_folder(account, &sub, &label, index, &mut report)?;
        index.prune_folder(account, &label, &found)?;
    }
    Ok(report)
}

/// Re-walk only the dirty (account, folder) pairs, prune the index for
/// each one, and return a fresh `ScanData` whose `threads` reflects the
/// current scope (`current_scope`) and whose `groups` rebuilds the
/// multi-account sidebar stats. Disk I/O is restricted to the dirty
/// folders' `cur/` and `new/`; everything else is pure SQL.
pub fn rescan_folders(
    cache_path: PathBuf,
    accounts: HashMap<String, PathBuf>,
    dirty: HashSet<(String, String)>,
    current_scope: (Option<String>, String),
) -> Receiver<ScanResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = rescan_run(&cache_path, &accounts, &dirty, &current_scope)
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(result);
    });
    rx
}

fn rescan_run(
    cache_path: &Path,
    accounts: &HashMap<String, PathBuf>,
    dirty: &HashSet<(String, String)>,
    current_scope: &(Option<String>, String),
) -> Result<ScanData> {
    let mut idx = Index::open(cache_path)?;
    let mut report = ScanReport::default();
    for (account, folder) in dirty {
        let Some(root) = accounts.get(account) else {
            continue;
        };
        let folder_root = if folder == "INBOX" {
            root.clone()
        } else {
            root.join(format!(".{folder}"))
        };
        if !folder_root.is_dir() {
            // The folder vanished entirely — drop every row we have for
            // it (empty keep-set) and move on.
            idx.prune_folder(account, folder, &HashSet::new())?;
            continue;
        }
        let found = scan_folder(account, &folder_root, folder, &mut idx, &mut report)?;
        idx.prune_folder(account, folder, &found)?;
    }
    let (scope_account, scope_folder) = current_scope;
    let rows = idx.list_folder(scope_account.as_deref(), scope_folder)?;
    let acc_vec: Vec<(String, PathBuf)> = accounts
        .iter()
        .map(|(n, p)| (n.clone(), p.clone()))
        .collect();
    let groups = build_groups(&idx, &acc_vec)?;
    Ok(ScanData {
        threads: build_threads(rows),
        groups,
    })
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ScanReport {
    pub scanned: usize,
    pub skipped: usize,
}

fn scan_folder(
    account: &str,
    folder_root: &Path,
    folder: &str,
    index: &mut Index,
    report: &mut ScanReport,
) -> Result<HashSet<String>> {
    let mut found = HashSet::new();
    for sub in ["cur", "new"] {
        let dir = folder_root.join(sub);
        if !dir.is_dir() {
            continue;
        }
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("listing {}", dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            match parse::read_headers(&path)? {
                Some(headers) => {
                    let flags = extract_flags(&path);
                    let msgid = headers.msgid.clone();
                    let row = MessageRow {
                        msgid: headers.msgid,
                        account: account.to_string(),
                        folder: folder.to_string(),
                        path: path.clone(),
                        date: headers.date,
                        from_addr: headers.from,
                        subject: headers.subject,
                        in_reply: headers.in_reply,
                        refs: headers.refs,
                        flags,
                    };
                    index.upsert(&row)?;
                    found.insert(msgid);
                    report.scanned += 1;
                }
                None => report.skipped += 1,
            }
        }
    }
    Ok(found)
}

pub fn folder_label(subdir: &str) -> String {
    subdir.strip_prefix('.').unwrap_or(subdir).to_string()
}

pub fn extract_flags(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(flags::parse_flags)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_label_strips_leading_dot() {
        assert_eq!(folder_label(".Sent"), "Sent");
        assert_eq!(folder_label(".Sent.Drafts"), "Sent.Drafts");
        assert_eq!(folder_label("cur"), "cur");
    }

    #[test]
    fn extract_flags_reads_suffix() {
        assert_eq!(
            extract_flags(Path::new("1779181200.M0P1.epost-dev:2,S")),
            "S"
        );
        assert_eq!(
            extract_flags(Path::new("1779181200.M0P1.epost-dev:2,RST")),
            "RST"
        );
    }

    #[test]
    fn extract_flags_blank_when_missing() {
        assert_eq!(extract_flags(Path::new("1778850000.M0P6.epost-dev")), "");
    }

    #[test]
    fn rescan_folders_walks_only_dirty_and_prunes_deletes() {
        use std::fs;
        use std::io::Write;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("idx.sqlite");
        let root = tmp.path().join("dev");
        // INBOX + .Archive layout.
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(root.join(".Archive").join(sub)).unwrap();
        }
        let write_eml = |p: &Path, mid: &str| {
            let mut f = fs::File::create(p).unwrap();
            writeln!(f, "Message-ID: <{mid}>").unwrap();
            writeln!(f, "Date: Thu, 1 Jan 1970 00:00:00 +0000").unwrap();
            writeln!(f, "From: a@b").unwrap();
            writeln!(f, "Subject: t").unwrap();
            writeln!(f).unwrap();
            writeln!(f, "body").unwrap();
        };
        write_eml(&root.join("cur").join("1.M0.h:2,S"), "a");
        write_eml(&root.join("cur").join("2.M0.h:2,S"), "b");
        write_eml(&root.join(".Archive").join("cur").join("3.M0.h:2,S"), "c");

        // Seed the index via full scan.
        let mut accounts = HashMap::new();
        accounts.insert("dev".to_string(), root.clone());
        let rx = start_worker(
            accounts
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            cache.clone(),
            (None, "INBOX".to_string()),
        );
        let initial = rx.recv().unwrap().unwrap();
        let inbox_count_initial = group_total(&initial.groups, None, "INBOX");
        assert_eq!(inbox_count_initial, 2);

        // Delete one INBOX file, add another to Archive — but mark only
        // INBOX dirty. Archive should NOT be re-walked.
        fs::remove_file(root.join("cur").join("1.M0.h:2,S")).unwrap();
        write_eml(&root.join(".Archive").join("cur").join("4.M0.h:2,S"), "d");

        let mut dirty = HashSet::new();
        dirty.insert(("dev".to_string(), "INBOX".to_string()));
        let rx = rescan_folders(
            cache.clone(),
            accounts.clone(),
            dirty,
            (None, "INBOX".to_string()),
        );
        let data = rx.recv().unwrap().unwrap();

        // INBOX dropped to 1 (pruned <a>).
        assert_eq!(
            group_total(&data.groups, None, "INBOX"),
            1,
            "INBOX should reflect the deletion"
        );
        // Archive unchanged because not dirty (<d> not picked up).
        assert_eq!(
            group_total(&data.groups, None, "Archive"),
            1,
            "Archive must NOT be re-walked"
        );
    }

    /// Look up `total` for a `(scope, folder)` across the grouped sidebar
    /// stats; 99 sentinel when missing so a failing assertion is obvious.
    fn group_total(groups: &[AccountFolderStats], scope: Option<&str>, folder: &str) -> u64 {
        groups
            .iter()
            .find(|g| g.scope.as_deref() == scope)
            .and_then(|g| g.folders.iter().find(|s| s.folder == folder))
            .map(|s| s.total)
            .unwrap_or(99)
    }

    #[test]
    fn rescan_with_two_accounts_returns_grouped_stats() {
        use std::fs;
        use std::io::Write;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("idx.sqlite");
        let root_a = tmp.path().join("personal");
        let root_b = tmp.path().join("work");
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(root_a.join(sub)).unwrap();
            fs::create_dir_all(root_b.join(sub)).unwrap();
        }
        let write_eml = |p: &Path, mid: &str| {
            let mut f = fs::File::create(p).unwrap();
            writeln!(f, "Message-ID: <{mid}>").unwrap();
            writeln!(f, "Date: Thu, 1 Jan 1970 00:00:00 +0000").unwrap();
            writeln!(f, "From: a@b").unwrap();
            writeln!(f, "Subject: t").unwrap();
            writeln!(f).unwrap();
            writeln!(f, "body").unwrap();
        };
        write_eml(&root_a.join("cur").join("p1:2,S"), "p1");
        write_eml(&root_a.join("cur").join("p2:2,S"), "p2");
        write_eml(&root_b.join("cur").join("w1:2,S"), "w1");

        let mut accounts: HashMap<String, PathBuf> = HashMap::new();
        accounts.insert("personal".into(), root_a.clone());
        accounts.insert("work".into(), root_b.clone());

        let rx = start_worker(
            accounts
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            cache.clone(),
            (None, "INBOX".to_string()),
        );
        let data = rx.recv().unwrap().unwrap();

        // [all] INBOX = 3, personal INBOX = 2, work INBOX = 1.
        assert_eq!(group_total(&data.groups, None, "INBOX"), 3);
        assert_eq!(group_total(&data.groups, Some("personal"), "INBOX"), 2);
        assert_eq!(group_total(&data.groups, Some("work"), "INBOX"), 1);

        // Groups must include both accounts plus the unified [all].
        let mut scopes: Vec<Option<String>> = data.groups.iter().map(|g| g.scope.clone()).collect();
        scopes.sort_by(|a, b| match (a, b) {
            (None, _) => std::cmp::Ordering::Less,
            (_, None) => std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => x.cmp(y),
        });
        assert_eq!(
            scopes,
            vec![None, Some("personal".to_string()), Some("work".to_string())]
        );
    }

    #[test]
    fn scans_dev_fixture_maildir() {
        let mut idx = Index::open_in_memory().unwrap();
        let root = Path::new("dev/maildir");
        if !root.exists() {
            return;
        }
        let report = scan_account("dev", root, &mut idx).unwrap();
        assert!(report.scanned >= 6, "scanned: {}", report.scanned);
        let inbox = idx.list_folder(None, "INBOX").unwrap();
        assert!(!inbox.is_empty(), "inbox empty");
        let sent = idx.list_folder(None, "Sent").unwrap();
        assert!(!sent.is_empty(), "sent empty");
    }
}
