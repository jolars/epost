//! Reader-yank clipboard sink.
//!
//! Two paths, chosen exclusively by `[reader].clipboard`:
//!
//! * **OSC 52** (default): emits `ESC ] 52 ; c ; <base64> ESC \` to
//!   stdout. The terminal interprets it as a clipboard-set control
//!   sequence — no cells painted, no display side-effect. Distinct from
//!   the OSC 8 trick in `reader::emit_osc8_hyperlinks`: that one has to
//!   land in specific cells because it annotates display; this one only
//!   needs to reach the tty stream.
//!
//! * **Shell-out fallback**: when the user sets `[reader].clipboard =
//!   ["wl-copy"]` (or similar), the text is piped to that command's
//!   stdin on a `std::thread` worker. Mirrors `store::sync::start_worker`
//!   so the polling pattern is identical: `mpsc::Receiver` + an
//!   `AppEvent::Wake` on completion.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::config::Config;
use crate::ui::events::AppEvent;

/// What `yank` returned and what (if anything) the caller needs to
/// store. The OSC 52 path completes synchronously; the fallback path
/// hands back a receiver for `poll_clipboard` to drain.
pub enum YankOutcome {
    /// OSC 52 emitted directly. No further work for the caller.
    Sent,
    /// Fallback worker dispatched. Store this receiver on `App`; drain
    /// in `poll_clipboard` each tick.
    Spawned(Receiver<ClipboardResult>),
    /// Synchronous failure (couldn't even kick the worker, or stdout
    /// write failed). Surface immediately.
    Failed(String),
}

/// Result reported by the fallback worker.
pub type ClipboardResult = Result<usize, String>;

/// Copy `text` to the clipboard. Selects OSC 52 vs the fallback worker
/// based on `[reader].clipboard`. `event_tx` is cloned into the worker
/// so completion wakes the main loop without waiting on the idle
/// heartbeat (same shape as `store::sync::start_worker`).
pub fn yank(text: &str, cfg: &Config, event_tx: Option<&Sender<AppEvent>>) -> YankOutcome {
    if let Some(cmd) = cfg.reader.clipboard.as_ref().filter(|c| !c.is_empty()) {
        let rx = spawn_fallback(cmd.clone(), text.to_string(), event_tx.cloned());
        return YankOutcome::Spawned(rx);
    }
    match emit_osc52(text) {
        Ok(_) => YankOutcome::Sent,
        Err(e) => YankOutcome::Failed(e),
    }
}

/// Build the OSC 52 escape sequence for `text`. Factored out so tests
/// can golden-check the byte shape without touching stdout. Always
/// targets the `c` (clipboard) selection; we don't surface PRIMARY
/// because terminals are inconsistent about it.
fn build_osc52(text: &str) -> String {
    let b64 = STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{b64}\x1b\\")
}

fn emit_osc52(text: &str) -> Result<(), String> {
    let seq = build_osc52(text);
    let mut out = std::io::stdout().lock();
    out.write_all(seq.as_bytes())
        .map_err(|e| format!("write osc52: {e}"))?;
    out.flush().map_err(|e| format!("flush osc52: {e}"))?;
    Ok(())
}

fn spawn_fallback(
    cmd: Vec<String>,
    text: String,
    event_tx: Option<Sender<AppEvent>>,
) -> Receiver<ClipboardResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run_blocking(&cmd, &text);
        let _ = tx.send(result);
        if let Some(wake) = event_tx {
            let _ = wake.send(AppEvent::Wake);
        }
    });
    rx
}

fn run_blocking(cmd: &[String], text: &str) -> ClipboardResult {
    if cmd.is_empty() {
        return Err("clipboard command not configured".into());
    }
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("command not found: {}", cmd[0]),
            _ => format!("spawn: {e}"),
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("write stdin: {e}"))?;
        // Drop closes the pipe; required so `wait` doesn't deadlock
        // against a child that's waiting on EOF.
        drop(stdin);
    }
    let output = child.wait_with_output().map_err(|e| format!("wait: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let truncated: String = stderr.chars().take(500).collect();
        return Err(format!(
            "exit {}: {}",
            output.status.code().unwrap_or(-1),
            truncated.trim()
        ));
    }
    Ok(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn recv_blocking(rx: Receiver<ClipboardResult>) -> ClipboardResult {
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
    fn osc52_envelope_has_expected_bytes() {
        let seq = build_osc52("hi");
        // ESC ] 52 ; c ; <base64> ESC \
        assert!(seq.starts_with("\x1b]52;c;"), "bad prefix: {seq:?}");
        assert!(seq.ends_with("\x1b\\"), "bad terminator: {seq:?}");
        let b64 = &seq["\x1b]52;c;".len()..seq.len() - "\x1b\\".len()];
        assert_eq!(b64, STANDARD.encode("hi".as_bytes()));
    }

    #[test]
    fn osc52_envelope_handles_multibyte_utf8() {
        // Body has emoji + a multi-byte glyph; base64 should encode the
        // raw UTF-8 bytes so the receiving terminal can decode back to
        // the original string.
        let text = "héllo 🦀";
        let seq = build_osc52(text);
        let b64 = &seq["\x1b]52;c;".len()..seq.len() - "\x1b\\".len()];
        let decoded = STANDARD.decode(b64).expect("decode");
        assert_eq!(decoded, text.as_bytes());
    }

    #[test]
    fn fallback_pipes_text_to_stdin() {
        let cmd = vec![
            "/bin/sh".into(),
            "-c".into(),
            "cat > /dev/null && exit 0".into(),
        ];
        let rx = spawn_fallback(cmd, "anything".into(), None);
        assert!(matches!(recv_blocking(rx), Ok(8)));
    }

    #[test]
    fn fallback_reports_exit_code_and_stderr() {
        let cmd = vec![
            "/bin/sh".into(),
            "-c".into(),
            "cat > /dev/null; printf 'boom' 1>&2; exit 3".into(),
        ];
        let rx = spawn_fallback(cmd, "data".into(), None);
        match recv_blocking(rx) {
            Err(s) => {
                assert!(s.contains("exit 3"), "got: {s}");
                assert!(s.contains("boom"), "got: {s}");
            }
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn fallback_missing_binary_errors() {
        let rx = spawn_fallback(vec!["/no/such/binary-epost-test".into()], "x".into(), None);
        let out = recv_blocking(rx);
        assert!(
            matches!(out, Err(ref s) if s.contains("not found")),
            "{out:?}"
        );
    }
}
