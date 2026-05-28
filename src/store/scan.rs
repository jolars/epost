use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use anyhow::{Context, Result};

use crate::mail::parse;
use crate::store::index::{Index, MessageRow};
use crate::store::thread::{ThreadedRow, build_threads};

pub type ScanResult = std::result::Result<Vec<ThreadedRow>, String>;

pub fn start_worker(accounts: Vec<(String, PathBuf)>, cache_path: PathBuf) -> Receiver<ScanResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run(&cache_path, &accounts).map_err(|e| format!("{e:#}"));
        let _ = tx.send(result);
    });
    rx
}

fn run(cache_path: &Path, accounts: &[(String, PathBuf)]) -> Result<Vec<ThreadedRow>> {
    let mut idx = Index::open(cache_path)?;
    for (name, root) in accounts {
        scan_account(name, root, &mut idx)?;
    }
    let rows = idx.list_folder("INBOX")?;
    Ok(build_threads(rows))
}

pub fn scan_account(account: &str, root: &Path, index: &mut Index) -> Result<ScanReport> {
    let mut report = ScanReport::default();
    scan_folder(account, root, "INBOX", index, &mut report)?;

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
        scan_folder(account, &sub, &label, index, &mut report)?;
    }
    Ok(report)
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
) -> Result<()> {
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
                    report.scanned += 1;
                }
                None => report.skipped += 1,
            }
        }
    }
    Ok(())
}

pub fn folder_label(subdir: &str) -> String {
    subdir.strip_prefix('.').unwrap_or(subdir).to_string()
}

pub fn extract_flags(path: &Path) -> String {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return String::new(),
    };
    name.rsplit_once(":2,")
        .map(|(_, flags)| flags.to_string())
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
