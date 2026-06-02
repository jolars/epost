//! `:open` and `f`-link-picker exit paths. Both shell out to a
//! user-configured command (`[reader].browser`) on a worker thread so the
//! UI stays responsive (DESIGN.md invariant 8).
//!
//! `open_message` writes the message HTML to a `tempfile::NamedTempFile`
//! after rewriting `cid:` references to point at per-part tempfiles, then
//! hands the temp HTML path to the browser command. The tempfiles are
//! kept alive until the worker thread exits so the browser has time to
//! read them; `xdg-open` returns immediately, so the worker doesn't
//! wait. `open_url` hands a single URL/path through to the same command.

use std::io::Write;
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use tempfile::Builder;

use crate::mail::parse::Attachment;
use crate::ui::app::ParsedBody;

pub fn open_message(body: &ParsedBody, cmd: &[String]) -> Result<()> {
    if cmd.is_empty() {
        return Err(anyhow!("no browser command configured"));
    }
    let html = match (&body.raw_html, &body.plain_fallback) {
        (Some(h), _) => h.clone(),
        (None, Some(p)) => format!("<pre>{}</pre>", html_escape(p)),
        (None, None) => return Err(anyhow!("message has no renderable body")),
    };

    // Write each cid part to its own tempfile, then string-replace
    // cid:<id> → file://<path> in the HTML. The tempfiles are intentionally
    // left in place (disable_cleanup) — xdg-open exits before the real
    // browser actually reads the file, so we'd race a delete otherwise.
    // The OS cleans /tmp periodically.
    let mut rewritten = html;
    for (cid, bytes) in &body.cid_parts {
        let mut tmp = Builder::new()
            .prefix("epost-cid-")
            .tempfile()
            .context("creating cid tempfile")?;
        tmp.write_all(bytes).context("writing cid tempfile")?;
        tmp.disable_cleanup(true);
        let path = tmp.path().display().to_string();
        rewritten = rewritten.replace(&format!("cid:{cid}"), &format!("file://{path}"));
        drop(tmp);
    }

    let mut html_tmp = Builder::new()
        .prefix("epost-msg-")
        .suffix(".html")
        .tempfile()
        .context("creating html tempfile")?;
    html_tmp
        .write_all(rewritten.as_bytes())
        .context("writing html tempfile")?;
    html_tmp.disable_cleanup(true);
    let html_path = html_tmp.path().to_path_buf();
    drop(html_tmp);

    let cmd = cmd.to_vec();
    std::thread::spawn(move || {
        let mut c = Command::new(&cmd[0]);
        for arg in &cmd[1..] {
            c.arg(arg);
        }
        c.arg(&html_path);
        let _ = c.status();
    });
    Ok(())
}

/// Write an attachment to a tempfile preserving its extension, then hand
/// the path to the configured browser/opener command on a worker thread.
/// The tempfile is intentionally left in place (`disable_cleanup`) — the
/// spawn returns before the viewer actually reads the file, so we'd race
/// a delete otherwise; the OS cleans /tmp periodically.
pub fn open_attachment(att: &Attachment, cmd: &[String]) -> Result<()> {
    if cmd.is_empty() {
        return Err(anyhow!("no opener command configured"));
    }
    // Carry the original extension so viewers (xdg-open, mailcap-aware
    // launchers) can dispatch on it. Fall back to `.bin` when the
    // attachment filename has none.
    let ext = std::path::Path::new(&att.filename)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| format!(".{s}"))
        .unwrap_or_else(|| ".bin".to_string());
    let mut tmp = Builder::new()
        .prefix("epost-att-")
        .suffix(&ext)
        .tempfile()
        .context("creating attachment tempfile")?;
    tmp.write_all(&att.bytes)
        .context("writing attachment tempfile")?;
    tmp.disable_cleanup(true);
    let path = tmp.path().to_path_buf();
    drop(tmp);

    let cmd = cmd.to_vec();
    std::thread::spawn(move || {
        let mut c = Command::new(&cmd[0]);
        for arg in &cmd[1..] {
            c.arg(arg);
        }
        c.arg(&path);
        let _ = c.status();
    });
    Ok(())
}

pub fn open_url(href: &str, cmd: &[String]) -> Result<()> {
    if cmd.is_empty() {
        return Err(anyhow!("no browser command configured"));
    }
    let cmd = cmd.to_vec();
    let href = href.to_string();
    std::thread::spawn(move || {
        let mut c = Command::new(&cmd[0]);
        for arg in &cmd[1..] {
            c.arg(arg);
        }
        c.arg(&href);
        let _ = c.status();
    });
    Ok(())
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;

    fn run_stub(out_dir: &str) -> Vec<String> {
        let _ = fs::remove_dir_all(out_dir);
        fs::create_dir_all(out_dir).unwrap();
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!(r#"printf '%s\n' "$@" > "{out_dir}/argv.txt""#),
            // Anything before $@ is consumed as $0 by `sh -c`; the script
            // sees the rest as "$@".
            "stub".into(),
        ]
    }

    fn read_lines(p: PathBuf) -> Vec<String> {
        let s = fs::read_to_string(p).unwrap_or_default();
        s.lines().map(|l| l.to_string()).collect()
    }

    #[test]
    fn open_url_invokes_command() {
        let out = "/tmp/epost-test-open-url";
        let cmd = run_stub(out);
        open_url("https://example.test/here", &cmd).unwrap();
        // Worker thread runs detached; poll briefly.
        let argv_path = PathBuf::from(format!("{out}/argv.txt"));
        for _ in 0..40 {
            if argv_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let lines = read_lines(argv_path);
        assert!(
            lines.iter().any(|l| l.contains("example.test")),
            "{lines:?}"
        );
    }

    #[test]
    fn open_message_writes_tempfile_and_invokes_command() {
        let out = "/tmp/epost-test-open-msg";
        let cmd = run_stub(out);
        let body = ParsedBody {
            msgid: "x@y".into(),
            blocks: Vec::new(),
            raw_html: Some("<p>hello</p>".into()),
            plain_fallback: None,
            cid_parts: HashMap::new(),
            attachments: Vec::new(),
        };
        open_message(&body, &cmd).unwrap();
        let argv_path = PathBuf::from(format!("{out}/argv.txt"));
        for _ in 0..40 {
            if argv_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let lines = read_lines(argv_path);
        let last = lines.last().cloned().unwrap_or_default();
        assert!(last.ends_with(".html"), "argv last={last:?}");
        let html_at_path = fs::read_to_string(&last).unwrap_or_default();
        assert!(html_at_path.contains("hello"), "{html_at_path}");
    }

    #[test]
    fn open_attachment_writes_tempfile_with_extension() {
        let out = "/tmp/epost-test-open-att";
        let cmd = run_stub(out);
        let att = Attachment {
            filename: "report.pdf".into(),
            bytes: b"%PDF-1.4 stub".to_vec(),
        };
        open_attachment(&att, &cmd).unwrap();
        let argv_path = PathBuf::from(format!("{out}/argv.txt"));
        for _ in 0..40 {
            if argv_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let lines = read_lines(argv_path);
        let last = lines.last().cloned().unwrap_or_default();
        assert!(last.ends_with(".pdf"), "argv last={last:?}");
        let bytes = fs::read(&last).unwrap_or_default();
        assert_eq!(bytes, b"%PDF-1.4 stub");
    }
}
