use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

#[derive(Debug, Clone)]
pub struct MessageRow {
    pub msgid: String,
    pub account: String,
    pub folder: String,
    pub path: PathBuf,
    pub date: i64,
    pub from_addr: Option<String>,
    pub subject: Option<String>,
    pub in_reply: Option<String>,
    pub refs: Vec<String>,
    pub flags: String,
}

/// Per-folder roll-up surfaced to the sidebar. Unified across accounts
/// (matches how `list_folder("INBOX")` already merges accounts), so a
/// user with two accounts sees one `INBOX` row whose counts sum both.
#[derive(Debug, Clone)]
pub struct FolderStat {
    pub folder: String,
    pub total: u64,
    pub unread: u64,
}

pub struct Index {
    conn: Connection,
}

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating index dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite index at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("enabling WAL")?;
        // Multiple writers race: the main UI thread upserts on flag flips
        // and moves, while the scan and watcher-driven rescan workers
        // upsert from their own threads. WAL serializes writers, so
        // without a busy timeout the loser fails instantly with
        // SQLITE_BUSY ("database is locked"). 5s is well above any
        // realistic per-transaction write time.
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .context("setting busy_timeout")?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory sqlite")?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    pub fn upsert(&mut self, row: &MessageRow) -> Result<()> {
        let path_s = row.path.to_string_lossy();
        let refs = row.refs.join(" ");
        self.conn
            .execute(
                "INSERT INTO msg(msgid, account, folder, path, date, from_addr, subject, in_reply, refs, flags) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) \
                 ON CONFLICT(msgid, account, folder) DO UPDATE SET \
                   path=excluded.path, \
                   date=excluded.date, \
                   from_addr=excluded.from_addr, \
                   subject=excluded.subject, \
                   in_reply=excluded.in_reply, \
                   refs=excluded.refs, \
                   flags=excluded.flags",
                params![
                    row.msgid,
                    row.account,
                    row.folder,
                    path_s,
                    row.date,
                    row.from_addr,
                    row.subject,
                    row.in_reply,
                    refs,
                    row.flags,
                ],
            )
            .context("upserting msg row")?;
        Ok(())
    }

    /// Drop rows for `(account, folder)` whose `msgid` is not in `keep`.
    /// Returns the number of rows pruned. Used after a per-folder rescan
    /// to reflect files deleted on disk between scans.
    pub fn prune_folder(
        &mut self,
        account: &str,
        folder: &str,
        keep: &std::collections::HashSet<String>,
    ) -> Result<usize> {
        let tx = self.conn.transaction().context("begin prune_folder")?;
        let mut to_drop: Vec<String> = Vec::new();
        {
            let mut stmt = tx
                .prepare("SELECT msgid FROM msg WHERE account = ?1 AND folder = ?2")
                .context("preparing prune_folder select")?;
            let rows = stmt
                .query_map(params![account, folder], |r| r.get::<_, String>(0))
                .context("executing prune_folder select")?;
            for row in rows {
                let msgid = row.context("collecting prune_folder msgid")?;
                if !keep.contains(&msgid) {
                    to_drop.push(msgid);
                }
            }
        }
        let dropped = to_drop.len();
        if !to_drop.is_empty() {
            // Scoped to (account, folder): the same msgid can now live in
            // other folders/accounts (Gmail's Inbox + All Mail, or a list
            // mail in two accounts), and pruning INBOX must not touch those.
            let mut del = tx
                .prepare("DELETE FROM msg WHERE msgid = ?1 AND account = ?2 AND folder = ?3")
                .context("preparing prune_folder delete")?;
            for msgid in &to_drop {
                del.execute(params![msgid, account, folder])
                    .context("executing prune_folder delete")?;
            }
        }
        tx.commit().context("committing prune_folder")?;
        Ok(dropped)
    }

    /// `account = None` means "all accounts" — the unified inbox view
    /// today's UI defaults to. `Some(name)` filters by `account`.
    pub fn list_folder(&self, account: Option<&str>, folder: &str) -> Result<Vec<MessageRow>> {
        match account {
            None => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT msgid, account, folder, path, date, from_addr, subject, in_reply, refs, flags \
                         FROM msg WHERE folder = ?1 ORDER BY date DESC",
                    )
                    .context("preparing list_folder")?;
                let rows = stmt
                    .query_map(params![folder], row_from_sqlite)
                    .context("executing list_folder")?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .context("collecting list_folder rows")
            }
            Some(acc) => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT msgid, account, folder, path, date, from_addr, subject, in_reply, refs, flags \
                         FROM msg WHERE account = ?1 AND folder = ?2 ORDER BY date DESC",
                    )
                    .context("preparing list_folder (scoped)")?;
                let rows = stmt
                    .query_map(params![acc, folder], row_from_sqlite)
                    .context("executing list_folder (scoped)")?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .context("collecting list_folder rows")
            }
        }
    }

    /// Pull every row in `(account, folders)` ordered by date DESC. Used
    /// by the `/` and `g/` search modes to seed an in-memory haystack
    /// for fuzzy matching: `account = None` crosses accounts, `folders =
    /// None` crosses every folder. The full row set fits comfortably in
    /// memory for typical mailboxes (≤50K × ~500 B) and avoids re-querying
    /// per keystroke.
    pub fn list_scope(
        &self,
        account: Option<&str>,
        folders: Option<&[String]>,
    ) -> Result<Vec<MessageRow>> {
        let cols = "msgid, account, folder, path, date, from_addr, subject, in_reply, refs, flags";
        // Build the WHERE + params on the fly. SQLite caps SQLITE_MAX_VARIABLE_NUMBER
        // at 999 by default; the global_folders config tops out at a handful in
        // practice, so a single IN list is fine.
        let mut sql = format!("SELECT {cols} FROM msg");
        let mut where_parts: Vec<String> = Vec::new();
        let mut params_vec: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(acc) = account {
            where_parts.push(format!("account = ?{}", params_vec.len() + 1));
            params_vec.push(acc.to_string().into());
        }
        if let Some(folders) = folders {
            if folders.is_empty() {
                // No folders is no scope — return empty rather than every row.
                return Ok(Vec::new());
            }
            let placeholders: Vec<String> = (0..folders.len())
                .map(|i| format!("?{}", params_vec.len() + 1 + i))
                .collect();
            where_parts.push(format!("folder IN ({})", placeholders.join(",")));
            for f in folders {
                params_vec.push(f.clone().into());
            }
        }
        if !where_parts.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&where_parts.join(" AND "));
        }
        sql.push_str(" ORDER BY date DESC");
        let mut stmt = self.conn.prepare(&sql).context("preparing list_scope")?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|v| v as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), row_from_sqlite)
            .context("executing list_scope")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("collecting list_scope rows")
    }

    /// Per-folder counts (total and unread). `account = None` aggregates
    /// across accounts (today's default — two INBOXes merge into one row);
    /// `Some(name)` restricts to that account. "Unread" is the absence of
    /// the maildir `S` flag; the column stores the suffix verbatim so a
    /// `LIKE '%S%'` check is precise (flags are uppercase ASCII letters,
    /// no false matches).
    pub fn folder_stats(&self, account: Option<&str>) -> Result<Vec<FolderStat>> {
        let row_to_stat = |r: &rusqlite::Row<'_>| {
            let folder: String = r.get(0)?;
            let total: i64 = r.get(1)?;
            let unread: i64 = r.get(2)?;
            Ok(FolderStat {
                folder,
                total: total.max(0) as u64,
                unread: unread.max(0) as u64,
            })
        };
        match account {
            None => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT folder, COUNT(*) AS total, \
                                SUM(CASE WHEN flags LIKE '%S%' THEN 0 ELSE 1 END) AS unread \
                         FROM msg GROUP BY folder",
                    )
                    .context("preparing folder_stats")?;
                let rows = stmt
                    .query_map([], row_to_stat)
                    .context("executing folder_stats")?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .context("collecting folder_stats rows")
            }
            Some(acc) => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT folder, COUNT(*) AS total, \
                                SUM(CASE WHEN flags LIKE '%S%' THEN 0 ELSE 1 END) AS unread \
                         FROM msg WHERE account = ?1 GROUP BY folder",
                    )
                    .context("preparing folder_stats (scoped)")?;
                let rows = stmt
                    .query_map(params![acc], row_to_stat)
                    .context("executing folder_stats (scoped)")?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .context("collecting folder_stats rows")
            }
        }
    }

    /// Look up a single row by its full identity `(msgid, account,
    /// folder)`. msgid alone no longer identifies a row — the same
    /// Message-ID can live in several folders/accounts at once — so undo
    /// /redo must re-locate the exact copy it acted on. Paths and flag
    /// suffixes still drift on every sync, but the identity triple is
    /// stable, so this remains the canonical "find this message where it
    /// lives now" entry point.
    pub fn get(&self, msgid: &str, account: &str, folder: &str) -> Result<Option<MessageRow>> {
        use rusqlite::OptionalExtension;
        self.conn
            .query_row(
                "SELECT msgid, account, folder, path, date, from_addr, subject, in_reply, refs, flags \
                 FROM msg WHERE msgid = ?1 AND account = ?2 AND folder = ?3",
                params![msgid, account, folder],
                row_from_sqlite,
            )
            .optional()
            .context("get by identity")
    }

    /// Delete the single row identified by `(msgid, account, folder)`.
    /// A cross-folder move is delete-then-upsert: under the composite key
    /// the destination row is a *new* row, so the source row must be
    /// removed explicitly or the message would linger in both folders.
    pub fn delete(&mut self, msgid: &str, account: &str, folder: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM msg WHERE msgid = ?1 AND account = ?2 AND folder = ?3",
                params![msgid, account, folder],
            )
            .context("delete by identity")?;
        Ok(())
    }
}

/// Cache schema version. Bump whenever the `msg` table shape changes so
/// a stale on-disk cache is dropped and rebuilt rather than silently
/// running against the old layout. The cache is a disposable derivative
/// of the maildir (DESIGN invariant), so dropping it loses nothing.
///
/// v2: primary key widened from `msgid` alone to `(msgid, account,
/// folder)`. Under a single `msgid` key, scanning a folder that shares a
/// Message-ID with another folder (Gmail copies one message into both
/// Inbox and All Mail) or another account (the same list mail delivered
/// to two accounts) would re-key or clobber the existing row — emptying
/// the inbox view and mis-attributing messages across accounts.
const SCHEMA_VERSION: i64 = 2;

/// Create the schema, dropping a stale-version table first. `CREATE TABLE
/// IF NOT EXISTS` alone can't change the primary key of a table that
/// already exists, so a cache written by an older epost would keep the
/// narrow `msgid` key. We gate on `PRAGMA user_version` and rebuild on
/// mismatch.
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .context("reading user_version")?;
    if version != SCHEMA_VERSION {
        conn.execute_batch("DROP TABLE IF EXISTS msg")
            .context("dropping stale msg table")?;
    }
    conn.execute_batch(SCHEMA).context("creating schema")?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .context("stamping user_version")?;
    Ok(())
}

fn row_from_sqlite(r: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
    let refs: String = r.get(8)?;
    let path: String = r.get(3)?;
    Ok(MessageRow {
        msgid: r.get(0)?,
        account: r.get(1)?,
        folder: r.get(2)?,
        path: PathBuf::from(path),
        date: r.get(4)?,
        from_addr: r.get(5)?,
        subject: r.get(6)?,
        in_reply: r.get(7)?,
        refs: refs.split_whitespace().map(str::to_owned).collect(),
        flags: r.get(9)?,
    })
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS msg (
  msgid     TEXT NOT NULL,
  account   TEXT NOT NULL,
  folder    TEXT NOT NULL,
  path      TEXT NOT NULL,
  date      INTEGER NOT NULL,
  from_addr TEXT,
  subject   TEXT,
  in_reply  TEXT,
  refs      TEXT,
  flags     TEXT,
  PRIMARY KEY (msgid, account, folder)
);
CREATE INDEX IF NOT EXISTS idx_folder_date ON msg(folder, date);
CREATE INDEX IF NOT EXISTS idx_account    ON msg(account);
";

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(msgid: &str, date: i64) -> MessageRow {
        MessageRow {
            msgid: msgid.to_string(),
            account: "dev".into(),
            folder: "INBOX".into(),
            path: PathBuf::from("/tmp/x"),
            date,
            from_addr: Some("Jane <jane@example.com>".into()),
            subject: Some("hi".into()),
            in_reply: None,
            refs: vec![],
            flags: "S".into(),
        }
    }

    #[test]
    fn upsert_then_list_orders_by_date_desc() {
        let mut idx = Index::open_in_memory().unwrap();
        idx.upsert(&sample("<a>", 100)).unwrap();
        idx.upsert(&sample("<b>", 200)).unwrap();
        idx.upsert(&sample("<c>", 150)).unwrap();
        let rows = idx.list_folder(None, "INBOX").unwrap();
        assert_eq!(
            rows.iter().map(|r| r.msgid.as_str()).collect::<Vec<_>>(),
            vec!["<b>", "<c>", "<a>"]
        );
    }

    #[test]
    fn upsert_updates_mutable_fields() {
        let mut idx = Index::open_in_memory().unwrap();
        let mut r = sample("<a>", 100);
        r.path = PathBuf::from("/old");
        r.flags = "".into();
        idx.upsert(&r).unwrap();
        r.path = PathBuf::from("/new");
        r.flags = "S".into();
        idx.upsert(&r).unwrap();
        let got = idx.get("<a>", "dev", "INBOX").unwrap().unwrap();
        assert_eq!(got.path, PathBuf::from("/new"));
        assert_eq!(got.flags, "S");
    }

    #[test]
    fn same_msgid_coexists_across_folders_and_accounts() {
        // Gmail copies one message into Inbox and All Mail; the same list
        // mail lands in two accounts. All four are distinct rows under the
        // composite key — scanning one must not clobber the others.
        let mut idx = Index::open_in_memory().unwrap();
        let mut mk = |account: &str, folder: &str, path: &str| {
            let mut r = sample("<dup@x>", 100);
            r.account = account.into();
            r.folder = folder.into();
            r.path = PathBuf::from(path);
            idx.upsert(&r).unwrap();
        };
        mk("gmail", "INBOX", "/g/inbox");
        mk("gmail", "[Gmail]/All Mail", "/g/all");
        mk("posteo", "INBOX", "/p/inbox");

        // The INBOX view still shows the message for each account.
        assert_eq!(idx.list_folder(Some("gmail"), "INBOX").unwrap().len(), 1);
        assert_eq!(idx.list_folder(Some("posteo"), "INBOX").unwrap().len(), 1);
        // Each copy keeps its own path.
        assert_eq!(
            idx.get("<dup@x>", "gmail", "INBOX").unwrap().unwrap().path,
            PathBuf::from("/g/inbox")
        );
        assert_eq!(
            idx.get("<dup@x>", "gmail", "[Gmail]/All Mail")
                .unwrap()
                .unwrap()
                .path,
            PathBuf::from("/g/all")
        );
        // Deleting one copy leaves the others.
        idx.delete("<dup@x>", "gmail", "[Gmail]/All Mail").unwrap();
        assert!(idx.get("<dup@x>", "gmail", "INBOX").unwrap().is_some());
        assert!(idx.get("<dup@x>", "posteo", "INBOX").unwrap().is_some());
        assert!(
            idx.get("<dup@x>", "gmail", "[Gmail]/All Mail")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn folder_stats_groups_and_counts_unread() {
        let mut idx = Index::open_in_memory().unwrap();
        // Two INBOX rows (one read, one unread), one Sent row (read).
        let mut r = sample("<a>", 100);
        r.flags = "S".into();
        idx.upsert(&r).unwrap();
        let mut r = sample("<b>", 200);
        r.flags = "".into();
        idx.upsert(&r).unwrap();
        let mut r = sample("<c>", 300);
        r.folder = "Sent".into();
        r.flags = "S".into();
        idx.upsert(&r).unwrap();

        let mut stats = idx.folder_stats(None).unwrap();
        stats.sort_by(|a, b| a.folder.cmp(&b.folder));
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].folder, "INBOX");
        assert_eq!(stats[0].total, 2);
        assert_eq!(stats[0].unread, 1);
        assert_eq!(stats[1].folder, "Sent");
        assert_eq!(stats[1].total, 1);
        assert_eq!(stats[1].unread, 0);
    }

    /// Seed two accounts (personal + work), each with an INBOX, plus a
    /// personal Sent. Used by the scope-aware tests below.
    fn seed_two_accounts() -> Index {
        let mut idx = Index::open_in_memory().unwrap();
        let mut mk = |msgid: &str, account: &str, folder: &str, flags: &str, date: i64| {
            let mut r = sample(msgid, date);
            r.account = account.into();
            r.folder = folder.into();
            r.flags = flags.into();
            idx.upsert(&r).unwrap();
        };
        mk("<p1>", "personal", "INBOX", "S", 100);
        mk("<p2>", "personal", "INBOX", "", 200);
        mk("<p3>", "personal", "Sent", "S", 300);
        mk("<w1>", "work", "INBOX", "S", 110);
        mk("<w2>", "work", "INBOX", "", 220);
        idx
    }

    #[test]
    fn list_folder_scoped_by_account_returns_only_that_account() {
        let idx = seed_two_accounts();
        let rows = idx.list_folder(Some("personal"), "INBOX").unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.msgid.as_str()).collect();
        // INBOX-personal: <p2> (date 200) then <p1> (date 100).
        assert_eq!(ids, vec!["<p2>", "<p1>"]);
    }

    #[test]
    fn list_folder_none_returns_all_accounts() {
        let idx = seed_two_accounts();
        let rows = idx.list_folder(None, "INBOX").unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.msgid.as_str()).collect();
        // INBOX across both: ordered by date desc.
        assert_eq!(ids, vec!["<w2>", "<p2>", "<w1>", "<p1>"]);
    }

    #[test]
    fn folder_stats_scoped_by_account_excludes_others() {
        let idx = seed_two_accounts();
        let mut stats = idx.folder_stats(Some("personal")).unwrap();
        stats.sort_by(|a, b| a.folder.cmp(&b.folder));
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].folder, "INBOX");
        assert_eq!(stats[0].total, 2);
        assert_eq!(stats[0].unread, 1);
        assert_eq!(stats[1].folder, "Sent");
        assert_eq!(stats[1].total, 1);
        assert_eq!(stats[1].unread, 0);
    }

    #[test]
    fn folder_stats_none_aggregates_across_accounts() {
        let idx = seed_two_accounts();
        let mut stats = idx.folder_stats(None).unwrap();
        stats.sort_by(|a, b| a.folder.cmp(&b.folder));
        // INBOX merges both accounts: 2 + 2 = 4 total, 1 + 1 = 2 unread.
        assert_eq!(stats[0].folder, "INBOX");
        assert_eq!(stats[0].total, 4);
        assert_eq!(stats[0].unread, 2);
        // Sent is personal-only.
        assert_eq!(stats[1].folder, "Sent");
        assert_eq!(stats[1].total, 1);
    }

    #[test]
    fn list_scope_none_account_none_folders_returns_all_rows() {
        let idx = seed_two_accounts();
        let rows = idx.list_scope(None, None).unwrap();
        // 5 rows seeded total; date DESC.
        let ids: Vec<&str> = rows.iter().map(|r| r.msgid.as_str()).collect();
        assert_eq!(ids, vec!["<p3>", "<w2>", "<p2>", "<w1>", "<p1>"]);
    }

    #[test]
    fn list_scope_filters_by_account() {
        let idx = seed_two_accounts();
        let rows = idx.list_scope(Some("personal"), None).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.msgid.as_str()).collect();
        // personal across all folders: <p3> Sent, then <p2>, <p1> INBOX.
        assert_eq!(ids, vec!["<p3>", "<p2>", "<p1>"]);
    }

    #[test]
    fn list_scope_filters_by_folder_list() {
        let idx = seed_two_accounts();
        let folders = vec!["INBOX".to_string(), "Sent".to_string()];
        let rows = idx.list_scope(None, Some(&folders)).unwrap();
        // Everything (all folders are in the list).
        assert_eq!(rows.len(), 5);
        let rows = idx.list_scope(None, Some(&["Sent".to_string()])).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.msgid.as_str()).collect();
        assert_eq!(ids, vec!["<p3>"]);
    }

    #[test]
    fn list_scope_account_and_folders_combine() {
        let idx = seed_two_accounts();
        let folders = vec!["INBOX".to_string()];
        let rows = idx.list_scope(Some("work"), Some(&folders)).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.msgid.as_str()).collect();
        assert_eq!(ids, vec!["<w2>", "<w1>"]);
    }

    #[test]
    fn list_scope_empty_folder_list_returns_nothing() {
        let idx = seed_two_accounts();
        let rows = idx.list_scope(None, Some(&[])).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn prune_folder_drops_rows_not_in_keep() {
        use std::collections::HashSet;
        let mut idx = Index::open_in_memory().unwrap();
        idx.upsert(&sample("<a>", 100)).unwrap();
        idx.upsert(&sample("<b>", 200)).unwrap();
        // <c> exists in INBOX *and* Sent — two distinct rows now. Pruning
        // INBOX drops the INBOX copy but must leave the Sent copy alone.
        idx.upsert(&sample("<c>", 300)).unwrap();
        let mut other = sample("<c>", 300);
        other.folder = "Sent".into();
        idx.upsert(&other).unwrap();

        let keep: HashSet<String> = ["<a>".to_string()].into_iter().collect();
        let dropped = idx.prune_folder("dev", "INBOX", &keep).unwrap();
        assert_eq!(dropped, 2, "<b> and the INBOX copy of <c> are pruned");
        assert!(idx.get("<a>", "dev", "INBOX").unwrap().is_some());
        assert!(idx.get("<b>", "dev", "INBOX").unwrap().is_none());
        assert!(idx.get("<c>", "dev", "INBOX").unwrap().is_none());
        assert!(
            idx.get("<c>", "dev", "Sent").unwrap().is_some(),
            "Sent row untouched"
        );
    }

    #[test]
    fn refs_round_trip() {
        let mut idx = Index::open_in_memory().unwrap();
        let mut r = sample("<c>", 100);
        r.refs = vec!["<a>".into(), "<b>".into()];
        idx.upsert(&r).unwrap();
        let got = idx.get("<c>", "dev", "INBOX").unwrap().unwrap();
        assert_eq!(got.refs, vec!["<a>", "<b>"]);
    }
}
