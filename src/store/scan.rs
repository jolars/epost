use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use anyhow::{Context, Result};

use crate::mail::flags;
use crate::mail::parse;
use crate::store::index::{FolderStat, Index, MessageRow};
use crate::store::thread::{ThreadedRow, build_threads};

/// Successful scan payload: the threaded INBOX rows the list pane
/// renders, plus the per-folder roll-up the sidebar renders. Both are
/// derived from the same index pass so the two views are consistent.
pub struct ScanData {
    pub threads: Vec<ThreadedRow>,
    pub folder_stats: Vec<FolderStat>,
}

pub type ScanResult = std::result::Result<ScanData, String>;

pub fn start_worker(accounts: Vec<(String, PathBuf)>, cache_path: PathBuf) -> Receiver<ScanResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run(&cache_path, &accounts).map_err(|e| format!("{e:#}"));
        let _ = tx.send(result);
    });
    rx
}

fn run(cache_path: &Path, accounts: &[(String, PathBuf)]) -> Result<ScanData> {
    let mut idx = Index::open(cache_path)?;
    for (name, root) in accounts {
        scan_account(name, root, &mut idx)?;
    }
    let rows = idx.list_folder("INBOX")?;
    let folder_stats = idx.folder_stats()?;
    Ok(ScanData {
        threads: build_threads(rows),
        folder_stats,
    })
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
/// current view (`list_folder`) and whose `folder_stats` aggregates
/// across all folders. Disk I/O is restricted to the dirty folders'
/// `cur/` and `new/`; everything else is pure SQL.
pub fn rescan_folders(
    cache_path: PathBuf,
    accounts: HashMap<String, PathBuf>,
    dirty: HashSet<(String, String)>,
    list_folder: String,
) -> Receiver<ScanResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result =
            rescan_run(&cache_path, &accounts, &dirty, &list_folder).map_err(|e| format!("{e:#}"));
        let _ = tx.send(result);
    });
    rx
}

fn rescan_run(
    cache_path: &Path,
    accounts: &HashMap<String, PathBuf>,
    dirty: &HashSet<(String, String)>,
    list_folder: &str,
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
    let rows = idx.list_folder(list_folder)?;
    let folder_stats = idx.folder_stats()?;
    Ok(ScanData {
        threads: build_threads(rows),
        folder_stats,
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
        );
        let initial = rx.recv().unwrap().unwrap();
        let inbox_count_initial = initial
            .folder_stats
            .iter()
            .find(|s| s.folder == "INBOX")
            .map(|s| s.total)
            .unwrap_or(0);
        assert_eq!(inbox_count_initial, 2);

        // Delete one INBOX file, add another to Archive — but mark only
        // INBOX dirty. Archive should NOT be re-walked.
        fs::remove_file(root.join("cur").join("1.M0.h:2,S")).unwrap();
        write_eml(&root.join(".Archive").join("cur").join("4.M0.h:2,S"), "d");

        let mut dirty = HashSet::new();
        dirty.insert(("dev".to_string(), "INBOX".to_string()));
        let rx = rescan_folders(cache.clone(), accounts.clone(), dirty, "INBOX".to_string());
        let data = rx.recv().unwrap().unwrap();

        // INBOX dropped to 1 (pruned <a>).
        let inbox = data
            .folder_stats
            .iter()
            .find(|s| s.folder == "INBOX")
            .map(|s| s.total)
            .unwrap_or(99);
        assert_eq!(inbox, 1, "INBOX should reflect the deletion");
        // Archive unchanged because not dirty (<d> not picked up).
        let archive = data
            .folder_stats
            .iter()
            .find(|s| s.folder == "Archive")
            .map(|s| s.total)
            .unwrap_or(99);
        assert_eq!(archive, 1, "Archive must NOT be re-walked");
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
        let inbox = idx.list_folder("INBOX").unwrap();
        assert!(!inbox.is_empty(), "inbox empty");
        let sent = idx.list_folder("Sent").unwrap();
        assert!(!sent.is_empty(), "sent empty");
    }
}
