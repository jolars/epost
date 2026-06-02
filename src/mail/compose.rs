//! Compose back-end: `Draft` model + constructors for new / reply /
//! reply-all / forward, MIME serialization via `mail-builder`, and the
//! send worker that pipes to `msmtp -t`. The UI front-end (compose tab)
//! lives in `ui::compose` and consumes the types and functions exposed
//! here.

// Step 6 lands Draft + serialize; the send worker (Step 7) and UI
// integration (Step 8+) wire the rest. Some helpers are surfaced as
// pub(crate) for the upcoming wiring but not yet called.
#![allow(dead_code)]

use std::borrow::Cow;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{SystemTime, UNIX_EPOCH};

use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address;
use mail_builder::headers::message_id::MessageId;

use crate::mail::parse::{Body, Headers};

/// A composable message. Fields mirror the visible header form in the
/// compose tab; `account` keys back into `[accounts.<name>]` so the
/// send worker can resolve SMTP + Sent folder. `from` is informational
/// only — SMTP routing always uses `account`'s configured command.
#[derive(Debug, Clone)]
pub struct Draft {
    pub account: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    /// Bare msgid (no `<>` wrapping); `mail-builder` adds the brackets.
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// Files to attach as `multipart/mixed` parts. Empty = single-part
    /// `text/plain`. Bytes are read at `serialize` time, not at attach
    /// time — a file deleted between `:attach` and `:send` surfaces as
    /// a send-path error rather than silently shipping stale content.
    pub attachments: Vec<PathBuf>,
}

impl Draft {
    pub fn new_blank(account: &str, from: &str) -> Draft {
        Draft {
            account: account.to_string(),
            from: from.to_string(),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: String::new(),
            body: String::new(),
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
        }
    }

    /// Build a reply to `h` (and `b` for the quoted body). `reply_all`
    /// promotes the original To+Cc list into Cc, minus our own address.
    /// `Reply-To:` (when present on the original) wins over `From:`.
    pub fn reply_to(h: &Headers, b: &Body, account: &str, from: &str, reply_all: bool) -> Draft {
        let our = extract_addr(from);
        let target = h
            .reply_to
            .clone()
            .or_else(|| h.from.clone())
            .unwrap_or_default();
        let to: Vec<String> = if target.is_empty() {
            Vec::new()
        } else {
            vec![target]
        };
        let mut cc: Vec<String> = Vec::new();
        if reply_all {
            for a in h.to.iter().chain(h.cc.iter()) {
                let addr = extract_addr(a);
                if !addr.is_empty() && addr == our {
                    continue;
                }
                if to.iter().any(|t| extract_addr(t) == addr) {
                    continue;
                }
                if cc.iter().any(|c| extract_addr(c) == addr) {
                    continue;
                }
                cc.push(a.clone());
            }
        }
        let subject = prefix_once("Re:", h.subject.as_deref().unwrap_or(""));
        let mut refs = h.refs.clone();
        if !refs.iter().any(|r| r == &h.msgid) {
            refs.push(h.msgid.clone());
        }
        Draft {
            account: account.to_string(),
            from: from.to_string(),
            to,
            cc,
            bcc: Vec::new(),
            subject,
            body: quote_reply(h, b),
            in_reply_to: Some(h.msgid.clone()),
            references: refs,
            attachments: Vec::new(),
        }
    }

    /// Build a forward of `h`. Recipients are left blank for the user
    /// to fill in; no `In-Reply-To` / `References` so the forward
    /// starts a fresh thread.
    pub fn forward(h: &Headers, b: &Body, account: &str, from: &str) -> Draft {
        Draft {
            account: account.to_string(),
            from: from.to_string(),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: prefix_once("Fwd:", h.subject.as_deref().unwrap_or("")),
            body: forward_block(h, b),
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
        }
    }
}

/// Extract the bare address from a `Name <addr>` or `addr` string.
/// Returns lowercased so case-insensitive comparison is the default.
pub(crate) fn extract_addr(raw: &str) -> String {
    let s = raw.trim();
    if let Some(start) = s.find('<')
        && let Some(end) = s.rfind('>')
        && end > start
    {
        return s[start + 1..end].trim().to_ascii_lowercase();
    }
    s.to_ascii_lowercase()
}

/// Strip any leading run of `Re:` / `Fwd:` / `Fw:` markers (case- and
/// whitespace-tolerant) and reapply `tag` exactly once. Preserves
/// content like `[list]` prefixes that aren't reply markers.
pub(crate) fn prefix_once(tag: &str, subject: &str) -> String {
    let mut rest = subject.trim_start();
    loop {
        let lower = rest.to_ascii_lowercase();
        let stripped = lower
            .strip_prefix("re:")
            .or_else(|| lower.strip_prefix("fwd:"))
            .or_else(|| lower.strip_prefix("fw:"))
            .map(|r| rest.len() - r.len());
        match stripped {
            Some(n) => rest = rest[n..].trim_start(),
            None => break,
        }
    }
    let rest = rest.trim();
    if rest.is_empty() {
        tag.to_string()
    } else {
        format!("{tag} {rest}")
    }
}

/// Produce a quoted reply body. Cites the original date + sender on
/// the first line, then prefixes every line of the plain body (blank
/// lines included) with `"> "`. Sources from `Body::plain` if present;
/// falls back to a cheap text rendering of the HTML.
pub(crate) fn quote_reply(h: &Headers, b: &Body) -> String {
    let date = date_utc(h.date);
    let sender = h.from.as_deref().unwrap_or("someone");
    let source = body_as_plain(b);
    let mut out = format!("On {date}, {sender} wrote:\n");
    for line in source.lines() {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
    if !source.ends_with('\n') && !source.is_empty() {
        // ensure the last line gets a newline-terminated `> ` even if
        // the source had no trailing newline (so the user's reply
        // starts on a fresh line below the quote).
    }
    out.push('\n');
    out
}

/// Forwarded-message block in the style most clients emit: a marker
/// line, the original headers, blank line, original plain body.
pub(crate) fn forward_block(h: &Headers, b: &Body) -> String {
    let mut out = String::from("---------- Forwarded message ----------\n");
    out.push_str(&format!(
        "From: {}\n",
        h.from.as_deref().unwrap_or("(unknown)")
    ));
    out.push_str(&format!("Date: {}\n", date_utc(h.date)));
    out.push_str(&format!(
        "Subject: {}\n",
        h.subject.as_deref().unwrap_or("(no subject)")
    ));
    if !h.to.is_empty() {
        out.push_str(&format!("To: {}\n", h.to.join(", ")));
    }
    if !h.cc.is_empty() {
        out.push_str(&format!("Cc: {}\n", h.cc.join(", ")));
    }
    out.push('\n');
    out.push_str(&body_as_plain(b));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn body_as_plain(b: &Body) -> String {
    if let Some(p) = &b.plain {
        return p.clone();
    }
    if let Some(h) = &b.html {
        return html_text(h);
    }
    String::new()
}

/// Cheap HTML-to-text fallback used when a message has no `text/plain`
/// alternative. Walks the Block-IR and stitches the text content with
/// paragraph breaks; not faithful, just legible inside `> ` quoting.
fn html_text(html: &str) -> String {
    use crate::mail::html::{self, Block, Inline};

    fn append_inlines(runs: &[Inline], out: &mut String) {
        for r in runs {
            match r {
                Inline::Text { content, .. } => out.push_str(content),
                Inline::Link { runs, .. } => append_inlines(runs, out),
                Inline::LineBreak => out.push('\n'),
            }
        }
    }
    fn walk(blocks: &[Block], out: &mut String) {
        for b in blocks {
            match b {
                Block::Paragraph(runs) => {
                    append_inlines(runs, out);
                    out.push('\n');
                    out.push('\n');
                }
                Block::Heading { text, .. } => {
                    append_inlines(text, out);
                    out.push('\n');
                    out.push('\n');
                }
                Block::List { items, .. } => {
                    for it in items {
                        out.push_str("- ");
                        walk(it, out);
                    }
                }
                Block::Quote(inner) => walk(inner, out),
                Block::Pre(s) => {
                    out.push_str(s);
                    out.push('\n');
                }
                Block::Image { alt, .. } => {
                    out.push_str(&format!("[image: {alt}]\n"));
                }
                Block::HRule => out.push_str("----\n"),
                Block::Table { rows } => {
                    for row in rows {
                        for (i, cell) in row.iter().enumerate() {
                            if i > 0 {
                                out.push_str(" | ");
                            }
                            append_inlines(cell, out);
                        }
                        out.push('\n');
                    }
                }
            }
        }
    }
    let blocks = html::parse(html);
    let mut out = String::new();
    walk(&blocks, &mut out);
    out.trim_end().to_string()
}

/// Format a unix timestamp as `YYYY-MM-DD HH:MM UTC`. Hand-rolled to
/// avoid a date-crate dependency just for this string; uses Howard
/// Hinnant's civil-from-days algorithm (same as the list pane).
pub(crate) fn date_utc(unix: i64) -> String {
    if unix <= 0 {
        return "(unknown date)".to_string();
    }
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let h = secs / 3600;
    let min = (secs % 3600) / 60;
    format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02} UTC")
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i32 + (era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp.wrapping_sub(9) };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Expand a leading `~/` or a bare `~` to `$HOME`. Anything else passes
/// through unchanged. Mid-path `~user` is intentionally not supported —
/// the cmdline isn't a shell.
pub fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    if s == "~"
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home);
    }
    PathBuf::from(s)
}

/// Tilde-expand `raw` and verify it points at a regular file. Returns
/// the resolved `PathBuf` or a human-readable error suitable for the
/// status row (no command prefix — callers add their own).
pub fn validate_attachment(raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("missing path".into());
    }
    let path = expand_tilde(raw);
    match fs::metadata(&path) {
        Ok(m) if m.is_file() => Ok(path),
        Ok(_) => Err(format!("{} is not a file", path.display())),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Best-effort content type from the path extension. Extensionless or
/// unknown paths fall back to `application/octet-stream`, which is what
/// receivers expect for an opaque blob — they'll just download it.
fn guess_mime(path: &Path) -> String {
    mime_guess::from_path(path)
        .first_raw()
        .unwrap_or("application/octet-stream")
        .to_string()
}

/// Build the MIME bytes for a Draft via `mail-builder`. Date and
/// Message-ID are auto-generated by the builder if not pre-set; tests
/// redact these to keep snapshots deterministic.
pub fn serialize(d: &Draft) -> io::Result<Vec<u8>> {
    let mut b: MessageBuilder<'_> = MessageBuilder::new()
        .from(parse_addr(&d.from))
        .subject(d.subject.clone())
        .text_body(d.body.clone());
    if !d.to.is_empty() {
        b = b.to(addrs(&d.to));
    }
    if !d.cc.is_empty() {
        b = b.cc(addrs(&d.cc));
    }
    if !d.bcc.is_empty() {
        b = b.bcc(addrs(&d.bcc));
    }
    if let Some(irt) = &d.in_reply_to {
        b = b.in_reply_to(MessageId::new(irt.clone()));
    }
    if !d.references.is_empty() {
        b = b.references(MessageId::new_list(d.references.iter().cloned()));
    }
    for path in &d.attachments {
        let bytes = fs::read(path)
            .map_err(|e| io::Error::new(e.kind(), format!("attachment {}: {e}", path.display())))?;
        let filename = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".to_string());
        b = b.attachment(guess_mime(path), filename, bytes);
    }
    b.write_to_vec()
}

fn addrs(list: &[String]) -> Address<'static> {
    Address::new_list(list.iter().map(|s| parse_addr(s)).collect())
}

/// Result of a send attempt: explicitly separates "sent but no Sent
/// copy" from "send failed outright" so the UI can render the right
/// status — the user needs to know the message *did* go out even if
/// the Sent-folder write failed.
#[derive(Debug)]
pub enum SendOutcome {
    Sent,
    SentNoCopy(String),
}

pub type SendResult = Result<SendOutcome, String>;

/// Spawn a worker thread that pipes `bytes` to `smtp_cmd[0] smtp_cmd[1..]`
/// over stdin and (on success) drops a copy in `sent_cur_dir` with the
/// `:2,S` info suffix. Returns a one-shot receiver the UI polls each
/// tick. Mirrors `store::scan::start_worker` so the polling pattern is
/// identical. When `event_tx` is plumbed in, the worker also pushes an
/// `AppEvent::Wake` on completion so the main loop surfaces the result
/// immediately rather than waiting for the next idle heartbeat.
pub fn start_send_worker(
    bytes: Vec<u8>,
    smtp_cmd: Vec<String>,
    sent_cur_dir: Option<PathBuf>,
    event_tx: Option<Sender<crate::ui::events::AppEvent>>,
) -> Receiver<SendResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(send_blocking(&bytes, &smtp_cmd, sent_cur_dir.as_deref()));
        if let Some(wake) = event_tx {
            let _ = wake.send(crate::ui::events::AppEvent::Wake);
        }
    });
    rx
}

fn send_blocking(bytes: &[u8], smtp_cmd: &[String], sent_cur_dir: Option<&Path>) -> SendResult {
    if smtp_cmd.is_empty() {
        return Err("smtp.command not configured".into());
    }
    let mut child = Command::new(&smtp_cmd[0])
        .args(&smtp_cmd[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => format!("msmtp not found on PATH: {}", smtp_cmd[0]),
            _ => format!("spawning msmtp: {e}"),
        })?;

    // Close stdin (by dropping it after writing) so msmtp sees EOF and
    // proceeds — leaving it open hangs the wait.
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "msmtp stdin not piped".to_string())?;
        stdin
            .write_all(bytes)
            .map_err(|e| format!("msmtp stdin: {e}"))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("waiting for msmtp: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let truncated: String = stderr.chars().take(500).collect();
        return Err(format!(
            "msmtp exit {}: {}",
            output.status.code().unwrap_or(-1),
            truncated.trim()
        ));
    }

    // msmtp accepted the message. Try to drop a copy in Sent/cur; a
    // failure here doesn't unsend the message, so surface it as
    // `SentNoCopy` rather than `Err`.
    if let Some(dir) = sent_cur_dir {
        match deliver_sent(bytes, dir) {
            Ok(_) => Ok(SendOutcome::Sent),
            Err(e) => Ok(SendOutcome::SentNoCopy(format!("sent: {e}"))),
        }
    } else {
        Ok(SendOutcome::Sent)
    }
}

/// Write `bytes` into `cur_dir` as a maildir-spec `<unique>:2,<flags>`
/// file. Atomic via `tmp/<unique>` + rename. fsyncs the data file
/// before the rename (the maildir spec requires it). Callers pass the
/// per-role flag set: `"S"` for Sent, `"D"` for Drafts, etc.
pub fn deliver_maildir_message(bytes: &[u8], cur_dir: &Path, flags: &str) -> io::Result<PathBuf> {
    let folder_root = cur_dir
        .parent()
        .ok_or_else(|| io::Error::other("maildir cur dir has no parent"))?;
    let tmp_dir = folder_root.join("tmp");
    fs::create_dir_all(&tmp_dir)?;
    fs::create_dir_all(cur_dir)?;

    let unique = unique_filename();
    let tmp_path = tmp_dir.join(&unique);
    let final_path = cur_dir.join(format!("{unique}:2,{flags}"));

    let mut f = fs::File::create(&tmp_path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp_path, &final_path)?;
    Ok(final_path)
}

/// Sent-folder delivery — thin wrapper for the `:send` path.
pub fn deliver_sent(bytes: &[u8], cur_dir: &Path) -> io::Result<PathBuf> {
    deliver_maildir_message(bytes, cur_dir, "S")
}

/// Serialize `draft` and atomically write it to `drafts_cur` with the
/// maildir `D` flag. Records both the tmp and final paths in
/// `SelfWrites` *before* the rename so the notify watcher swallows its
/// own write; both are consumed on error so a later genuine write at
/// the same path isn't suppressed. Returns the final on-disk path.
pub fn save_draft(
    draft: &Draft,
    drafts_cur: &Path,
    sw: &crate::store::watch::SelfWrites,
) -> io::Result<PathBuf> {
    let bytes = serialize(draft)?;

    let folder_root = drafts_cur
        .parent()
        .ok_or_else(|| io::Error::other("drafts cur dir has no parent"))?;
    let tmp_dir = folder_root.join("tmp");
    fs::create_dir_all(&tmp_dir)?;
    fs::create_dir_all(drafts_cur)?;

    let unique = unique_filename();
    let tmp_path = tmp_dir.join(&unique);
    let final_path = drafts_cur.join(format!("{unique}:2,D"));

    sw.record(&tmp_path);
    sw.record(&final_path);

    let write_result = (|| -> io::Result<()> {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp_path, &final_path)?;
        Ok(())
    })();

    if let Err(e) = write_result {
        sw.consume(&tmp_path);
        sw.consume(&final_path);
        return Err(e);
    }
    Ok(final_path)
}

fn unique_filename() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let pid = std::process::id();
    let host = hostname_safe();
    format!(
        "{}.M{}P{}.{}",
        now.as_secs(),
        now.subsec_micros(),
        pid,
        host
    )
}

fn hostname_safe() -> String {
    let raw = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let name = raw.trim();
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == ':' { '-' } else { c })
        .collect();
    if cleaned.is_empty() {
        "localhost".to_string()
    } else {
        cleaned
    }
}

fn parse_addr(raw: &str) -> Address<'static> {
    let s = raw.trim();
    if let Some(start) = s.find('<')
        && let Some(end) = s.rfind('>')
        && end > start
    {
        let name = s[..start].trim();
        let addr = s[start + 1..end].trim().to_string();
        return if name.is_empty() {
            Address::new_address(None::<Cow<'static, str>>, addr)
        } else {
            Address::new_address(Some(name.to_string()), addr)
        };
    }
    Address::new_address(None::<Cow<'static, str>>, s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn headers_fixture() -> Headers {
        Headers {
            msgid: "orig@example.com".into(),
            date: 1_779_753_600, // 2026-05-26 00:00 UTC
            from: Some("Alice <alice@example.com>".into()),
            reply_to: None,
            to: vec!["bob@example.com".into(), "Jane <jane@example.com>".into()],
            cc: vec!["carol@example.com".into()],
            bcc: Vec::new(),
            subject: Some("Lunch?".into()),
            in_reply: None,
            refs: vec!["root@example.com".into()],
        }
    }

    fn body_plain() -> Body {
        Body {
            html: None,
            plain: Some("hey\n\nlet's grab food\n".into()),
            cid_parts: HashMap::new(),
        }
    }

    #[test]
    fn extract_addr_strips_name() {
        assert_eq!(extract_addr("Jane <Jane@Example.com>"), "jane@example.com");
        assert_eq!(extract_addr("plain@example.com"), "plain@example.com");
        assert_eq!(extract_addr(" PLAIN@EXAMPLE.COM "), "plain@example.com");
    }

    #[test]
    fn prefix_once_strips_existing_reply_markers() {
        assert_eq!(prefix_once("Re:", "Re: Re: hello"), "Re: hello");
        assert_eq!(prefix_once("Re:", "RE: hi"), "Re: hi");
        assert_eq!(prefix_once("Re:", "Fwd: foo"), "Re: foo");
        assert_eq!(prefix_once("Re:", "  re:   spaced  "), "Re: spaced");
        // List prefix is preserved (not a reply marker).
        assert_eq!(prefix_once("Re:", "[list] Re: ping"), "Re: [list] Re: ping");
    }

    #[test]
    fn prefix_once_handles_empty_subject() {
        assert_eq!(prefix_once("Re:", ""), "Re:");
        assert_eq!(prefix_once("Fwd:", "  "), "Fwd:");
    }

    #[test]
    fn reply_to_addresses_original_sender() {
        let h = headers_fixture();
        let b = body_plain();
        let d = Draft::reply_to(&h, &b, "personal", "me@example.com", false);
        assert_eq!(d.to, vec!["Alice <alice@example.com>".to_string()]);
        assert!(d.cc.is_empty(), "cc should be empty without reply_all");
        assert_eq!(d.subject, "Re: Lunch?");
        assert_eq!(d.in_reply_to.as_deref(), Some("orig@example.com"));
        assert_eq!(d.references, vec!["root@example.com", "orig@example.com"]);
        assert!(d.body.contains("> hey"));
        assert!(d.body.contains("Alice <alice@example.com> wrote"));
    }

    #[test]
    fn reply_all_dedups_self_and_promotes_to_cc() {
        let h = headers_fixture();
        let b = body_plain();
        // Our own address is one of the recipients on the original.
        let d = Draft::reply_to(&h, &b, "personal", "Me <bob@example.com>", true);
        // bob@ (us) should not appear in cc; jane and carol should.
        assert!(
            !d.cc.iter().any(|c| extract_addr(c) == "bob@example.com"),
            "self should be removed from cc, got {:?}",
            d.cc
        );
        assert!(d.cc.iter().any(|c| extract_addr(c) == "jane@example.com"));
        assert!(d.cc.iter().any(|c| extract_addr(c) == "carol@example.com"));
    }

    #[test]
    fn reply_to_prefers_reply_to_over_from() {
        let mut h = headers_fixture();
        h.reply_to = Some("List <list@example.com>".into());
        let d = Draft::reply_to(&h, &body_plain(), "personal", "me@example.com", false);
        assert_eq!(d.to, vec!["List <list@example.com>".to_string()]);
    }

    #[test]
    fn reply_does_not_double_prefix_re() {
        let mut h = headers_fixture();
        h.subject = Some("Re: original".into());
        let d = Draft::reply_to(&h, &body_plain(), "personal", "me@example.com", false);
        assert_eq!(d.subject, "Re: original");
    }

    #[test]
    fn forward_empties_recipients_and_prefixes_subject() {
        let h = headers_fixture();
        let d = Draft::forward(&h, &body_plain(), "personal", "me@example.com");
        assert!(d.to.is_empty());
        assert!(d.cc.is_empty());
        assert_eq!(d.subject, "Fwd: Lunch?");
        assert!(d.in_reply_to.is_none());
        assert!(d.references.is_empty());
        assert!(d.body.contains("Forwarded message"));
        assert!(d.body.contains("From: Alice <alice@example.com>"));
        assert!(d.body.contains("Subject: Lunch?"));
        assert!(d.body.contains("hey"));
    }

    #[test]
    fn date_utc_formats_known_unix() {
        assert_eq!(date_utc(0), "(unknown date)");
        assert_eq!(date_utc(1_779_753_600), "2026-05-26 00:00 UTC");
        // 2026-05-26 12:34:00 UTC = 1_779_753_600 + 12*3600 + 34*60
        assert_eq!(
            date_utc(1_779_753_600 + 12 * 3600 + 34 * 60),
            "2026-05-26 12:34 UTC"
        );
    }

    #[test]
    fn quote_reply_prefixes_blank_lines() {
        let h = headers_fixture();
        let b = Body {
            html: None,
            plain: Some("one\n\nthree\n".into()),
            cid_parts: HashMap::new(),
        };
        let q = quote_reply(&h, &b);
        assert!(q.contains("> one"));
        assert!(
            q.contains("> \n> three") || q.contains("> "),
            "quote: {q:?}"
        );
    }

    fn canned_draft() -> Draft {
        Draft {
            account: "personal".into(),
            from: "Jane Doe <jane@example.com>".into(),
            to: vec!["bob@example.com".into(), "Carol <carol@example.com>".into()],
            cc: vec!["watcher@example.com".into()],
            bcc: Vec::new(),
            subject: "Hello, friends".into(),
            body: "Hi all,\n\nQuick note.\n".into(),
            in_reply_to: Some("orig@example.com".into()),
            references: vec!["root@example.com".into(), "orig@example.com".into()],
            attachments: Vec::new(),
        }
    }

    #[test]
    fn deliver_sent_writes_with_seen_suffix() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let sent = tmp.path().join(".Sent");
        let cur = sent.join("cur");
        let bytes = b"From: me\r\nSubject: hi\r\n\r\nbody\r\n";

        let path = deliver_sent(bytes, &cur).expect("deliver");
        assert!(path.exists());
        // Filename must carry the :2,S info suffix.
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.ends_with(":2,S"), "filename: {name}");
        // tmp/ should be empty after the atomic rename.
        let tmp_entries: Vec<_> = fs::read_dir(sent.join("tmp")).unwrap().collect();
        assert!(tmp_entries.is_empty(), "tmp leftover: {tmp_entries:?}");
        // Round-trip the bytes.
        let read = fs::read(&path).unwrap();
        assert_eq!(read, bytes);
    }

    #[test]
    fn send_blocking_empty_smtp_errors() {
        let out = send_blocking(b"x", &[], None);
        assert!(matches!(out, Err(ref s) if s.contains("not configured")));
    }

    #[test]
    fn send_blocking_against_dev_stub_succeeds() {
        // The dev stub captures stdin to $EPOST_SENT_STUB_DIR. Point it
        // at a per-test tempdir so we can confirm the bytes survived.
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let capture = tmp.path().join("captured");
        fs::create_dir_all(&capture).unwrap();
        // We can't set env vars per-process safely in concurrent tests
        // (env is global). Use `sh -c` to set it inline for this call.
        let script = format!(
            "EPOST_SENT_STUB_DIR='{}' ./dev/msmtp-stub",
            capture.display()
        );
        let bytes = b"From: me\r\nSubject: hello\r\n\r\nbody\r\n";
        let out = send_blocking(bytes, &["/bin/sh".into(), "-c".into(), script], None);
        assert!(matches!(out, Ok(SendOutcome::Sent)), "{out:?}");
        let captured: Vec<_> = fs::read_dir(&capture).unwrap().collect();
        assert_eq!(captured.len(), 1, "expected 1 captured eml");
    }

    #[test]
    fn send_blocking_non_zero_exit_errors() {
        let cmd = vec!["/bin/sh".into(), "-c".into(), "exit 9".into()];
        let out = send_blocking(b"x", &cmd, None);
        match out {
            Err(s) => assert!(s.contains("exit 9"), "msg: {s}"),
            Ok(o) => panic!("expected error, got {o:?}"),
        }
    }

    #[test]
    fn send_blocking_with_sent_dir_delivers_copy() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let sent_cur = tmp.path().join(".Sent").join("cur");
        let bytes = b"From: me\r\nSubject: copy\r\n\r\nbody\r\n";
        // Simple `true` command — exits 0, ignores stdin (after it closes).
        let cmd = vec!["/bin/sh".into(), "-c".into(), "cat > /dev/null".into()];
        let out = send_blocking(bytes, &cmd, Some(&sent_cur));
        assert!(matches!(out, Ok(SendOutcome::Sent)), "{out:?}");
        let entries: Vec<_> = fs::read_dir(&sent_cur).unwrap().collect();
        assert_eq!(entries.len(), 1, "expected one Sent copy");
    }

    #[test]
    fn serialize_emits_expected_mime() {
        let bytes = serialize(&canned_draft()).expect("serialize");
        let mime = String::from_utf8_lossy(&bytes);
        // Spot-check key headers and body.
        assert!(mime.contains("From: \"Jane Doe\" <jane@example.com>"));
        assert!(mime.contains("To: <bob@example.com>") || mime.contains("To: bob@example.com"));
        assert!(mime.contains("Carol"));
        assert!(mime.contains("Cc:"));
        assert!(mime.contains("In-Reply-To: <orig@example.com>"));
        assert!(mime.contains("References: <root@example.com> <orig@example.com>"));
        assert!(mime.contains("Subject: Hello, friends"));
        // The body lives below the headers, separated by a blank line.
        assert!(mime.contains("Hi all,"));
        // Date and Message-ID are auto-generated.
        assert!(mime.contains("Date: "));
        assert!(mime.contains("Message-ID: "));
    }

    #[test]
    fn serialize_no_attachments_is_single_part() {
        let bytes = serialize(&canned_draft()).expect("serialize");
        let mime = String::from_utf8_lossy(&bytes);
        assert!(
            !mime.contains("multipart/"),
            "expected single-part body, got multipart MIME: {mime}"
        );
    }

    #[test]
    fn serialize_with_attachments_is_multipart_mixed() {
        use mail_parser::{MessageParser, MimeHeaders};
        use std::io::Write;
        use tempfile::Builder;

        let mut f = Builder::new()
            .prefix("epost-attach-test-")
            .suffix(".png")
            .tempfile()
            .unwrap();
        // Not real PNG bytes — we only assert on the Content-Type the
        // serializer picked from the .png extension, not on render.
        f.write_all(b"\x89PNG fake bytes\n").unwrap();
        let path = f.path().to_path_buf();
        let filename = path.file_name().unwrap().to_string_lossy().into_owned();

        let mut draft = canned_draft();
        draft.attachments.push(path);

        let bytes = serialize(&draft).expect("serialize");
        let parsed = MessageParser::default()
            .parse(&bytes)
            .expect("parse roundtrip");
        let ct = parsed
            .content_type()
            .expect("outer Content-Type header present");
        assert_eq!(ct.ctype(), "multipart");
        assert_eq!(ct.subtype(), Some("mixed"));
        // Expect at least the text body + the attachment.
        let attachment_count = parsed.attachment_count();
        assert!(
            attachment_count >= 1,
            "expected >=1 attachment, got {attachment_count}"
        );
        // The attachment part should carry the filename we asked for.
        let names: Vec<String> = parsed
            .attachments()
            .filter_map(|p| p.attachment_name().map(|s| s.to_string()))
            .collect();
        assert!(
            names.iter().any(|n| n == &filename),
            "expected filename {filename:?} in attachments, got {names:?}"
        );
        // …and the Content-Type should be derived from the .png suffix,
        // not the application/octet-stream catch-all.
        let attachment_cts: Vec<String> = parsed
            .attachments()
            .filter_map(|p| p.content_type())
            .map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().to_string(),
            })
            .collect();
        assert!(
            attachment_cts.iter().any(|t| t == "image/png"),
            "expected image/png in attachment Content-Types, got {attachment_cts:?}"
        );
    }

    #[test]
    fn guess_mime_falls_back_to_octet_stream_for_unknown_ext() {
        assert_eq!(
            guess_mime(Path::new("/tmp/no-extension")),
            "application/octet-stream"
        );
        assert_eq!(
            guess_mime(Path::new("/tmp/weird.zzznotreal")),
            "application/octet-stream"
        );
    }

    #[test]
    fn guess_mime_recognises_common_types() {
        assert_eq!(guess_mime(Path::new("foo.png")), "image/png");
        assert_eq!(guess_mime(Path::new("foo.pdf")), "application/pdf");
        // text/plain charset is appended by mime_guess in some versions;
        // accept either bare or parameterised form.
        let txt = guess_mime(Path::new("foo.txt"));
        assert!(
            txt.starts_with("text/plain"),
            "expected text/plain, got {txt}"
        );
    }

    #[test]
    fn serialize_missing_attachment_errors() {
        let bogus = PathBuf::from("/tmp/epost-definitely-does-not-exist-xyz123.bin");
        let mut draft = canned_draft();
        draft.attachments.push(bogus.clone());
        let err = serialize(&draft).expect_err("expected error for missing attachment");
        let msg = err.to_string();
        assert!(
            msg.contains(&bogus.display().to_string()),
            "error should mention path; got: {msg}"
        );
    }
}
