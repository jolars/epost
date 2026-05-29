//! `:sync` command worker. Spawns the user-configured
//! `[sync].command` on a `std::thread`, waits for it to exit, and
//! reports the result back over an `mpsc` channel. Mirrors the
//! `mail::compose::start_send_worker` shape so the polling pattern is
//! identical. The maildir watcher (Step 7) picks up the new files the
//! sync produced on disk; this worker only reports whether the sync
//! command itself succeeded.

use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};

use crate::ui::events::AppEvent;

pub type SyncResult = Result<(), String>;

/// Spawn the configured sync command on a worker thread. Returns a
/// one-shot receiver the UI polls each tick; on completion the worker
/// also pushes an `AppEvent::Wake` (when an event channel is plumbed
/// in) so the result surfaces without waiting for the idle heartbeat.
pub fn start_worker(cmd: Vec<String>, event_tx: Option<Sender<AppEvent>>) -> Receiver<SyncResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run_blocking(&cmd);
        let _ = tx.send(result);
        if let Some(wake) = event_tx {
            let _ = wake.send(AppEvent::Wake);
        }
    });
    rx
}

fn run_blocking(cmd: &[String]) -> SyncResult {
    if cmd.is_empty() {
        return Err("command not configured".into());
    }
    let output = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("command not found: {}", cmd[0]),
            _ => format!("spawn: {e}"),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let truncated: String = stderr.chars().take(500).collect();
        return Err(format!(
            "exit {}: {}",
            output.status.code().unwrap_or(-1),
            truncated.trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn recv_blocking(rx: Receiver<SyncResult>) -> SyncResult {
        // Worker exits quickly; poll briefly so the test isn't
        // timing-sensitive on slow CI.
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
    fn empty_command_errors() {
        let rx = start_worker(Vec::new(), None);
        let out = recv_blocking(rx);
        assert!(
            matches!(out, Err(ref s) if s.contains("not configured")),
            "{out:?}"
        );
    }

    #[test]
    fn successful_command_reports_ok() {
        let cmd = vec!["/bin/sh".into(), "-c".into(), "exit 0".into()];
        let rx = start_worker(cmd, None);
        assert!(matches!(recv_blocking(rx), Ok(())));
    }

    #[test]
    fn nonzero_exit_reports_error_with_stderr() {
        let cmd = vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'boom' 1>&2; exit 7".into(),
        ];
        let rx = start_worker(cmd, None);
        let out = recv_blocking(rx);
        match out {
            Err(s) => {
                assert!(s.contains("exit 7"), "got: {s}");
                assert!(s.contains("boom"), "got: {s}");
            }
            Ok(()) => panic!("expected error"),
        }
    }

    #[test]
    fn missing_binary_reports_error() {
        let rx = start_worker(vec!["/no/such/binary-epost-test".into()], None);
        let out = recv_blocking(rx);
        assert!(
            matches!(out, Err(ref s) if s.contains("not found")),
            "{out:?}"
        );
    }
}
