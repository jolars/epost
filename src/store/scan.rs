use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::{Context, Result};

use crate::mail::flags;
#[cfg(test)]
use crate::mail::layout::Layout;
use crate::mail::parse;
use crate::store::AccountSpec;
use crate::store::index::{FolderStat, Index, MessageRow};
use crate::store::thread::{ThreadedRow, build_threads};
use crate::ui::events::AppEvent;

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

/// Eager startup worker: scans only INBOX for each configured account.
/// The catch-up pass for every other discovered folder is spawned by
/// `start_catchup_worker` once this result lands, so the user sees
/// their INBOX threaded list as fast as the INBOX walk alone can run
/// — independent of how big `Archive`, `Sent`, etc. are.
pub fn start_inbox_worker(
    accounts: Vec<AccountSpec>,
    cache_path: PathBuf,
    current_scope: (Option<String>, String),
) -> Receiver<ScanResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result =
            run_inbox_only(&cache_path, &accounts, &current_scope).map_err(|e| format!("{e:#}"));
        let _ = tx.send(result);
    });
    rx
}

/// Background catch-up worker: walks every *non-INBOX* folder for each
/// account so the sidebar counts and any future scope switch see fresh
/// data. Spawned by `InboxScreen` once the eager `start_inbox_worker`
/// result lands. Returns the same `ScanData` shape so the apply path
/// can reuse the rescan plumbing.
pub fn start_catchup_worker(
    accounts: Vec<AccountSpec>,
    cache_path: PathBuf,
    current_scope: (Option<String>, String),
) -> Receiver<ScanResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result =
            run_catchup(&cache_path, &accounts, &current_scope).map_err(|e| format!("{e:#}"));
        let _ = tx.send(result);
    });
    rx
}

/// Outcome of an off-thread folder switch. Carries the `generation` that
/// requested it so a late result from a switch the user has already cycled
/// past can be discarded; `result` is the threaded rows for the target
/// scope or a formatted error string. Unlike [`ScanData`] there are no
/// folder-stat groups: a scope switch walks nothing on disk, so the
/// sidebar roll-up is unchanged.
pub struct SwitchOutcome {
    pub generation: u64,
    pub result: std::result::Result<Vec<ThreadedRow>, String>,
}

/// Load one scope's threaded list off the UI thread. The index read +
/// JWZ thread build is O(folder) and used to block the event loop inside
/// `switch_to_scope`; this mirrors the `start_*_worker` shape so the UI
/// just polls a receiver. On completion the worker also pushes an
/// `AppEvent::Wake` (when an event channel is plumbed in) so the result
/// surfaces without waiting for the idle heartbeat.
pub fn start_switch_worker(
    account: Option<String>,
    folder: String,
    cache_path: PathBuf,
    generation: u64,
    event_tx: Option<Sender<AppEvent>>,
) -> Receiver<SwitchOutcome> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = switch_run(&cache_path, account.as_deref(), &folder);
        let _ = tx.send(SwitchOutcome { generation, result });
        if let Some(wake) = event_tx {
            let _ = wake.send(AppEvent::Wake);
        }
    });
    rx
}

fn switch_run(
    cache_path: &Path,
    account: Option<&str>,
    folder: &str,
) -> std::result::Result<Vec<ThreadedRow>, String> {
    let idx = Index::open(cache_path).map_err(|e| format!("open index: {e:#}"))?;
    let rows = idx
        .list_folder(account, folder)
        .map_err(|e| format!("list {folder}: {e:#}"))?;
    Ok(build_threads(rows))
}

fn run_inbox_only(
    cache_path: &Path,
    accounts: &[AccountSpec],
    current_scope: &(Option<String>, String),
) -> Result<ScanData> {
    let mut idx = Index::open(cache_path)?;
    let mut report = ScanReport::default();
    for spec in accounts {
        // The INBOX binding is always `folders[0]`. Scan its `path`
        // (the resolved inbox root) and pin the index label to the
        // canonical `"INBOX"`.
        let inbox = &spec.folders[0];
        let found = scan_folder(&spec.name, &inbox.path, &inbox.label, &mut idx, &mut report)?;
        idx.prune_folder(&spec.name, &inbox.label, &found)?;
    }
    let (scope_account, scope_folder) = current_scope;
    let rows = idx.list_folder(scope_account.as_deref(), scope_folder)?;
    let groups = build_groups(&idx, accounts)?;
    Ok(ScanData {
        threads: build_threads(rows),
        groups,
    })
}

fn run_catchup(
    cache_path: &Path,
    accounts: &[AccountSpec],
    current_scope: &(Option<String>, String),
) -> Result<ScanData> {
    let mut idx = Index::open(cache_path)?;
    let mut report = ScanReport::default();
    for spec in accounts {
        if !spec.root.is_dir() {
            continue;
        }
        // Skip INBOX (eager-scanned); walk every other configured
        // binding using the user's label (role display name or
        // extra-folder literal) as the index folder name.
        for binding in spec.folders.iter().skip(1) {
            let found = scan_folder(
                &spec.name,
                &binding.path,
                &binding.label,
                &mut idx,
                &mut report,
            )?;
            idx.prune_folder(&spec.name, &binding.label, &found)?;
        }
    }
    let (scope_account, scope_folder) = current_scope;
    let rows = idx.list_folder(scope_account.as_deref(), scope_folder)?;
    let groups = build_groups(&idx, accounts)?;
    Ok(ScanData {
        threads: build_threads(rows),
        groups,
    })
}

/// Enumerate every `(account, folder)` the eager + catch-up workers
/// will visit. Used by `InboxScreen` to populate the "scanned this
/// session" set without re-iterating the binding list at apply time.
pub fn enumerate_folders(accounts: &[AccountSpec]) -> HashSet<(String, String)> {
    let mut out = HashSet::new();
    for spec in accounts {
        for binding in &spec.folders {
            out.insert((spec.name.clone(), binding.label.clone()));
        }
    }
    out
}

/// One `folder_stats(None)` for the "[all]" group, then one
/// `folder_stats(Some(account))` per configured account, sorted by name.
/// Order matches what the sidebar renders top-to-bottom.
///
/// Every group is back-filled with its configured folders (from each
/// account's binding list) so a folder the user configured but that
/// holds no mail still shows in the sidebar at `0/0`. The `[all]` group
/// gets the union of every account's labels; per-account groups get
/// just that account's labels.
fn build_groups(idx: &Index, accounts: &[AccountSpec]) -> Result<Vec<AccountFolderStats>> {
    let mut groups = Vec::with_capacity(accounts.len() + 1);
    let all_labels = accounts
        .iter()
        .flat_map(|s| s.folders.iter())
        .map(|b| b.label.as_str());
    groups.push(AccountFolderStats {
        scope: None,
        folders: fill_configured_folders(idx.folder_stats(None)?, all_labels),
    });
    let mut specs: Vec<&AccountSpec> = accounts.iter().collect();
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    for spec in specs {
        let labels = spec.folders.iter().map(|b| b.label.as_str());
        groups.push(AccountFolderStats {
            scope: Some(spec.name.clone()),
            folders: fill_configured_folders(idx.folder_stats(Some(&spec.name))?, labels),
        });
    }
    Ok(groups)
}

/// Append a `0/0` `FolderStat` for every configured label not already
/// present in `stats` (membership keyed on the folder name). Preserves
/// the index rows verbatim — the UI layer (`folder_sort_key`) owns
/// ordering — so a configured-but-empty folder reads as a plain `0`.
fn fill_configured_folders<'a>(
    mut stats: Vec<FolderStat>,
    labels: impl Iterator<Item = &'a str>,
) -> Vec<FolderStat> {
    for label in labels {
        if !stats.iter().any(|s| s.folder == label) {
            stats.push(FolderStat {
                folder: label.to_string(),
                total: 0,
                unread: 0,
            });
        }
    }
    stats
}

/// Re-walk only the dirty (account, folder) pairs, prune the index for
/// each one, and return a fresh `ScanData` whose `threads` reflects the
/// current scope (`current_scope`) and whose `groups` rebuilds the
/// multi-account sidebar stats. Disk I/O is restricted to the dirty
/// folders' `cur/` and `new/`; everything else is pure SQL.
pub fn rescan_folders(
    cache_path: PathBuf,
    accounts: HashMap<String, AccountSpec>,
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
    accounts: &HashMap<String, AccountSpec>,
    dirty: &HashSet<(String, String)>,
    current_scope: &(Option<String>, String),
) -> Result<ScanData> {
    let mut idx = Index::open(cache_path)?;
    let mut report = ScanReport::default();
    for (account, folder) in dirty {
        let Some(spec) = accounts.get(account) else {
            continue;
        };
        let Some(binding) = spec.binding_by_label(folder) else {
            // Folder isn't configured on this account — nothing on
            // disk for us to walk and nothing in the index keyed by
            // it (we never wrote any). Skip rather than error.
            continue;
        };
        if !binding.path.is_dir() {
            // The folder vanished entirely — drop every row we have for
            // it (empty keep-set) and move on.
            idx.prune_folder(account, folder, &HashSet::new())?;
            continue;
        }
        let found = scan_folder(account, &binding.path, folder, &mut idx, &mut report)?;
        idx.prune_folder(account, folder, &found)?;
    }
    let (scope_account, scope_folder) = current_scope;
    let rows = idx.list_folder(scope_account.as_deref(), scope_folder)?;
    let acc_vec: Vec<AccountSpec> = accounts.values().cloned().collect();
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

pub fn extract_flags(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(flags::parse_flags)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Account;

    /// Build a minimal `Account` for tests with optional role bindings.
    /// `roles` is `(role_field, disk_name)`; supported keys mirror the
    /// `Account` fields: `"archive"`, `"sent"`, `"spam"`, `"trash"`,
    /// `"drafts"`, `"inbox"`.
    fn test_account(maildir: PathBuf, layout: Layout, roles: &[(&str, &str)]) -> Account {
        let mut acc = Account {
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
        };
        for (field, name) in roles {
            let v = Some(name.to_string());
            match *field {
                "inbox" => acc.inbox = v,
                "archive" => acc.archive = v,
                "sent" => acc.sent = v,
                "spam" => acc.spam = v,
                "trash" => acc.trash = v,
                "drafts" => acc.drafts = v,
                other => panic!("unknown role field {other}"),
            }
        }
        acc
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
        let mut accounts: HashMap<String, AccountSpec> = HashMap::new();
        accounts.insert(
            "dev".to_string(),
            AccountSpec::from_account(
                "dev",
                &test_account(root.clone(), Layout::Maildirpp, &[("archive", "Archive")]),
            ),
        );
        // Seed eager INBOX + the catch-up so both INBOX and Archive
        // are in the index before the rescan-only-dirty check.
        let rx = start_inbox_worker(
            accounts.values().cloned().collect(),
            cache.clone(),
            (None, "INBOX".to_string()),
        );
        let initial = rx.recv().unwrap().unwrap();
        let inbox_count_initial = group_total(&initial.groups, None, "INBOX");
        assert_eq!(inbox_count_initial, 2);
        let rx = start_catchup_worker(
            accounts.values().cloned().collect(),
            cache.clone(),
            (None, "INBOX".to_string()),
        );
        rx.recv().unwrap().unwrap();

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

    #[test]
    fn switch_worker_returns_target_scope_threads() {
        use std::fs;
        use std::io::Write;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("idx.sqlite");
        let root = tmp.path().join("dev");
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(root.join(sub)).unwrap();
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
        write_eml(&root.join("cur").join("1.M0.h:2,S"), "inbox-a");
        write_eml(
            &root.join(".Archive").join("cur").join("2.M0.h:2,S"),
            "arch-a",
        );
        write_eml(
            &root.join(".Archive").join("cur").join("3.M0.h:2,S"),
            "arch-b",
        );

        // Seed the index (eager INBOX + catch-up so Archive lands too).
        let mut accounts: HashMap<String, AccountSpec> = HashMap::new();
        accounts.insert(
            "dev".to_string(),
            AccountSpec::from_account(
                "dev",
                &test_account(root.clone(), Layout::Maildirpp, &[("archive", "Archive")]),
            ),
        );
        let specs: Vec<AccountSpec> = accounts.values().cloned().collect();
        start_inbox_worker(specs.clone(), cache.clone(), (None, "INBOX".to_string()))
            .recv()
            .unwrap()
            .unwrap();
        start_catchup_worker(specs, cache.clone(), (None, "INBOX".to_string()))
            .recv()
            .unwrap()
            .unwrap();

        // Switch worker reads only the requested scope. The `generation`
        // it was handed is echoed back verbatim for the apply-step guard.
        let out = start_switch_worker(Some("dev".into()), "Archive".into(), cache.clone(), 7, None)
            .recv()
            .unwrap();
        assert_eq!(out.generation, 7);
        let threads = out.result.expect("switch should succeed");
        assert_eq!(threads.len(), 2, "Archive has two messages");

        // And INBOX returns its single row, unaffected.
        let out = start_switch_worker(Some("dev".into()), "INBOX".into(), cache, 8, None)
            .recv()
            .unwrap();
        assert_eq!(out.generation, 8);
        assert_eq!(out.result.unwrap().len(), 1);
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

        let mut accounts: HashMap<String, AccountSpec> = HashMap::new();
        accounts.insert(
            "personal".into(),
            AccountSpec::from_account(
                "personal",
                &test_account(root_a.clone(), Layout::Maildirpp, &[]),
            ),
        );
        accounts.insert(
            "work".into(),
            AccountSpec::from_account(
                "work",
                &test_account(root_b.clone(), Layout::Maildirpp, &[]),
            ),
        );

        let rx = start_inbox_worker(
            accounts.values().cloned().collect(),
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
    fn configured_but_empty_folders_show_at_zero() {
        use std::fs;
        use std::io::Write;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("idx.sqlite");
        let root = tmp.path().join("personal");
        // Only INBOX has mail; Archive/Sent/Spam/Trash are configured but
        // never created on disk and hold no messages.
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        let mut f = fs::File::create(root.join("cur").join("p1:2,S")).unwrap();
        writeln!(f, "Message-ID: <p1>").unwrap();
        writeln!(f, "Date: Thu, 1 Jan 1970 00:00:00 +0000").unwrap();
        writeln!(f, "From: a@b").unwrap();
        writeln!(f, "Subject: t").unwrap();
        writeln!(f).unwrap();
        writeln!(f, "body").unwrap();

        let accounts = vec![AccountSpec::from_account(
            "personal",
            &test_account(
                root.clone(),
                Layout::Maildirpp,
                &[
                    ("archive", "Archive"),
                    ("sent", "Sent"),
                    ("spam", "Spam"),
                    ("trash", "Trash"),
                ],
            ),
        )];
        let rx = start_inbox_worker(accounts, cache, (None, "INBOX".to_string()));
        let data = rx.recv().unwrap().unwrap();

        // INBOX has its one message; every other configured role shows
        // up at 0 in both the [all] union and the account group.
        assert_eq!(group_total(&data.groups, None, "INBOX"), 1);
        for folder in ["Archive", "Sent", "Spam", "Trash"] {
            assert_eq!(
                group_total(&data.groups, None, folder),
                0,
                "[all] should list configured {folder} at 0"
            );
            assert_eq!(
                group_total(&data.groups, Some("personal"), folder),
                0,
                "personal should list configured {folder} at 0"
            );
        }
    }

    #[test]
    fn scans_dev_fixture_maildir() {
        let root = Path::new("dev/maildir");
        if !root.exists() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("idx.sqlite");
        let accounts = vec![AccountSpec::from_account(
            "dev",
            &test_account(
                root.to_path_buf(),
                Layout::Maildirpp,
                &[("sent", "Sent"), ("archive", "Archive")],
            ),
        )];
        // Eager INBOX → catch-up: same path production takes.
        let rx = start_inbox_worker(accounts.clone(), cache.clone(), (None, "INBOX".into()));
        rx.recv().unwrap().unwrap();
        let rx = start_catchup_worker(accounts, cache.clone(), (None, "INBOX".into()));
        rx.recv().unwrap().unwrap();

        let idx = Index::open(&cache).unwrap();
        let inbox = idx.list_folder(None, "INBOX").unwrap();
        assert!(!inbox.is_empty(), "inbox empty");
        let sent = idx.list_folder(None, "Sent").unwrap();
        assert!(!sent.is_empty(), "sent empty");
    }
}
