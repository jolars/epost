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
        conn.execute_batch(SCHEMA).context("creating schema")?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory sqlite")?;
        conn.execute_batch(SCHEMA).context("creating schema")?;
        Ok(Self { conn })
    }

    pub fn upsert(&mut self, row: &MessageRow) -> Result<()> {
        let path_s = row.path.to_string_lossy();
        let refs = row.refs.join(" ");
        self.conn
            .execute(
                "INSERT INTO msg(msgid, account, folder, path, date, from_addr, subject, in_reply, refs, flags) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) \
                 ON CONFLICT(msgid) DO UPDATE SET \
                   account=excluded.account, \
                   folder=excluded.folder, \
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

    pub fn list_folder(&self, folder: &str) -> Result<Vec<MessageRow>> {
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

    #[cfg(test)]
    pub fn get(&self, msgid: &str) -> Result<Option<MessageRow>> {
        use rusqlite::OptionalExtension;
        self.conn
            .query_row(
                "SELECT msgid, account, folder, path, date, from_addr, subject, in_reply, refs, flags \
                 FROM msg WHERE msgid = ?1",
                params![msgid],
                row_from_sqlite,
            )
            .optional()
            .context("get by msgid")
    }
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
  msgid     TEXT PRIMARY KEY,
  account   TEXT NOT NULL,
  folder    TEXT NOT NULL,
  path      TEXT NOT NULL,
  date      INTEGER NOT NULL,
  from_addr TEXT,
  subject   TEXT,
  in_reply  TEXT,
  refs      TEXT,
  flags     TEXT
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
        let rows = idx.list_folder("INBOX").unwrap();
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
        let got = idx.get("<a>").unwrap().unwrap();
        assert_eq!(got.path, PathBuf::from("/new"));
        assert_eq!(got.flags, "S");
    }

    #[test]
    fn refs_round_trip() {
        let mut idx = Index::open_in_memory().unwrap();
        let mut r = sample("<c>", 100);
        r.refs = vec!["<a>".into(), "<b>".into()];
        idx.upsert(&r).unwrap();
        let got = idx.get("<c>").unwrap().unwrap();
        assert_eq!(got.refs, vec!["<a>", "<b>"]);
    }
}
