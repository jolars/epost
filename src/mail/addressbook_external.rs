//! External `query_command` worker for the address-book popup. Mirrors
//! mutt's `query_command` protocol exactly so off-the-shelf integrations
//! (khard, abook, goobook, notmuch-addrlookup, …) work without
//! wrappers.
//!
//! Protocol (per mutt's docs and the de-facto standard everyone follows):
//!
//! - The command is invoked with the query as its trailing argv element.
//!   No `%s` substitution — we shell out via `Command::args`, not via a
//!   subshell.
//! - Stdout is parsed line-by-line. The **first line** is dropped as a
//!   status header (khard prints "searching for 'foo'…"; mutt always
//!   ignores it).
//! - Each remaining line is tab-split. Field 0 is the email, field 1
//!   (when present) is the display name, anything after that is
//!   ignored.
//! - Empty stdout = no matches. Not an error.
//! - Non-zero exit = error; the caller surfaces it in the status row
//!   and falls back to native-only completion for that query.
//!
//! One query in flight at a time. The UI tracks the latest pending
//! query string and dispatches the next worker after the in-flight one
//! reports back; that's simpler than killing the child process mid-run
//! and keeps the perceived latency at "one query + debounce" rather
//! than "one query".

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};

use crate::mail::addressbook::{Contact, Source};
use crate::ui::events::AppEvent;

/// One external-query outcome. `query` is echoed back so the UI can
/// discard stale results from queries the user has already typed past.
pub struct ExtResult {
    pub query: String,
    pub outcome: Result<Vec<Contact>, String>,
}

/// Spawn the external command on a worker thread. `argv` is the
/// whitespace-split `query_command` from config; `query` becomes the
/// trailing argv element. Returns the receiver; the worker also pushes
/// `AppEvent::Wake` on completion so the popup refreshes without
/// waiting for the idle heartbeat.
pub fn start_query_worker(
    argv: Vec<String>,
    query: String,
    event_tx: Option<Sender<AppEvent>>,
) -> Receiver<ExtResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let outcome = run_blocking(&argv, &query);
        let _ = tx.send(ExtResult { query, outcome });
        if let Some(wake) = event_tx {
            let _ = wake.send(AppEvent::Wake);
        }
    });
    rx
}

fn run_blocking(argv: &[String], query: &str) -> Result<Vec<Contact>, String> {
    if argv.is_empty() {
        return Err("query_command not configured".into());
    }
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .arg(query)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("command not found: {}", argv[0]),
            _ => format!("spawn: {e}"),
        })?;
    let mut stdout = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut stdout);
    }
    let mut stderr = String::new();
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut stderr);
    }
    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    if !status.success() {
        let truncated: String = stderr.chars().take(200).collect();
        return Err(format!(
            "exit {}: {}",
            status.code().unwrap_or(-1),
            truncated.trim()
        ));
    }
    Ok(parse_output(&stdout))
}

/// Parse mutt-protocol `query_command` stdout. Drops the leading status
/// line, tab-splits the rest, keeps fields 0 (email) and 1 (name).
pub fn parse_output(stdout: &str) -> Vec<Contact> {
    let mut out = Vec::new();
    // First non-empty line is the status header — skip it. Lines that
    // come before any non-empty content (pure blank lines) don't count
    // toward the skip.
    let mut header_skipped = false;
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        if !header_skipped {
            header_skipped = true;
            continue;
        }
        let mut fields = line.split('\t');
        let Some(email) = fields.next() else {
            continue;
        };
        let email = email.trim();
        if email.is_empty() || !email.contains('@') {
            continue;
        }
        let name = fields
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        out.push(Contact {
            email_lc: email.to_ascii_lowercase(),
            email: email.to_string(),
            name,
            source: Source::External,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn recv_blocking(rx: Receiver<ExtResult>) -> ExtResult {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.try_recv() {
                Ok(r) => return r,
                Err(_) if Instant::now() >= deadline => panic!("worker did not complete"),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    #[test]
    fn parses_khard_style_output() {
        let s = "searching for 'ali'…\n\
                 alice@example.com\tAlice Smith\thome\n\
                 alice.work@example.com\tAlice Smith\twork\n";
        let v = parse_output(s);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].email, "alice@example.com");
        assert_eq!(v[0].name.as_deref(), Some("Alice Smith"));
        assert_eq!(v[1].email, "alice.work@example.com");
    }

    #[test]
    fn parses_email_only_lines() {
        let s = "header line\nalice@example.com\nbob@example.com\n";
        let v = parse_output(s);
        assert_eq!(v.len(), 2);
        assert!(v[0].name.is_none());
        assert!(v[1].name.is_none());
    }

    #[test]
    fn skips_lines_without_at_sign() {
        let s = "header\n\
                 not-an-email\tBob\n\
                 bob@example.com\tBob\n";
        let v = parse_output(s);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].email, "bob@example.com");
    }

    #[test]
    fn empty_stdout_returns_no_contacts() {
        assert!(parse_output("").is_empty());
        assert!(parse_output("only a header\n").is_empty());
    }

    #[test]
    fn email_lc_normalised() {
        let s = "header\nAlice@EXAMPLE.com\tAlice\n";
        let v = parse_output(s);
        assert_eq!(v[0].email, "Alice@EXAMPLE.com");
        assert_eq!(v[0].email_lc, "alice@example.com");
    }

    #[test]
    fn worker_passes_query_as_trailing_argv() {
        // Use /bin/sh -c to echo a header + one match if argv tail
        // matches what we expect.
        let argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            // $0 is the script name, so query becomes $1. Print the
            // header then a synthetic match line using $1.
            "printf 'searching for %s\\n%s@x\\tName\\n' \"$1\" \"$1\"".into(),
            "_".into(),
        ];
        let rx = start_query_worker(argv, "alice".into(), None);
        let r = recv_blocking(rx);
        assert_eq!(r.query, "alice");
        let contacts = r.outcome.expect("worker should succeed");
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].email, "alice@x");
        assert_eq!(contacts[0].name.as_deref(), Some("Name"));
    }

    #[test]
    fn nonzero_exit_reports_error() {
        let argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo 'oops' 1>&2; exit 2".into(),
        ];
        let rx = start_query_worker(argv, "x".into(), None);
        let r = recv_blocking(rx);
        match r.outcome {
            Err(s) => assert!(s.contains("exit 2"), "got: {s}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn missing_binary_reports_error() {
        let rx = start_query_worker(
            vec!["/no/such/binary-epost-ab-test".into()],
            "x".into(),
            None,
        );
        let r = recv_blocking(rx);
        assert!(matches!(r.outcome, Err(ref s) if s.contains("not found")));
    }
}
