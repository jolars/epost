//! Embedded `$EDITOR` session: spawns the user's editor under a pty,
//! parses the pty output through `vt100`, and exposes the parsed
//! screen for rendering via `tui-term`. Inputs are forwarded to the
//! pty's master in `forward_key`. The UI thread polls `is_done` each
//! tick to detect the editor exit and tear the session down.
//!
//! No async / await — the pty's reader runs on a `std::thread`, the
//! child wait runs on another, and both notify the UI thread via
//! `Arc<Mutex<_>>` (for the parser screen) and `mpsc::Receiver` (for
//! exit + redraw nudges).

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

pub struct EditorSession {
    parser: Arc<Mutex<vt100::Parser>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    done_rx: Receiver<()>,
    /// Set by the reader thread on first byte read from the pty.
    /// Until then the UI keeps showing the form preview, so the
    /// transition into the editor doesn't paint a blank frame
    /// before the editor's own initial draw lands.
    primed: Arc<AtomicBool>,
    done: bool,
    rows: u16,
    cols: u16,
}

impl EditorSession {
    /// Spawn `argv` under a pty sized `rows x cols`, with `path` as
    /// the final argument. Returns once the child is running; the
    /// caller polls `is_done` to detect exit.
    pub fn start(path: &Path, argv: &[String], rows: u16, cols: u16) -> Result<Self> {
        if argv.is_empty() {
            anyhow::bail!("no editor configured");
        }
        let pty = NativePtySystem::default();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening pty")?;

        let mut cmd = CommandBuilder::new(&argv[0]);
        for arg in &argv[1..] {
            cmd.arg(arg);
        }
        cmd.arg(path);
        cmd.env("TERM", "xterm-256color");

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .context("spawning editor under pty")?;
        // Drop the slave handle in the parent so the child has the
        // only reference; otherwise EOF on the master never fires.
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let primed = Arc::new(AtomicBool::new(false));

        // Reader thread: pull bytes from the pty master and feed the
        // parser. The main loop redraws on a short timer while an
        // editor session is active, so there's no separate notify;
        // it just flips `primed` so the UI knows when to start
        // rendering the pty grid instead of the form preview.
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("cloning pty reader")?;
        {
            let parser = parser.clone();
            let primed = primed.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Ok(mut p) = parser.lock() {
                                p.process(&buf[..n]);
                            }
                            primed.store(true, Ordering::Release);
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // Waiter thread: block on child.wait(), notify the UI on exit.
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = child.wait();
            let _ = done_tx.send(());
        });

        let writer = pair.master.take_writer().context("taking pty writer")?;

        Ok(Self {
            parser,
            master: pair.master,
            writer,
            done_rx,
            primed,
            done: false,
            rows,
            cols,
        })
    }

    /// True once the editor has emitted its first byte. The UI uses
    /// this to defer the preview→pty transition by one frame, avoiding
    /// a blank flicker before the editor's first paint.
    pub fn is_primed(&self) -> bool {
        self.primed.load(Ordering::Acquire)
    }

    /// Snapshot the current screen for rendering. Holds the parser
    /// lock for the duration of the callback so the reader thread
    /// can't mutate it mid-draw.
    pub fn with_screen<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> R {
        let parser = self.parser.lock().expect("parser poisoned");
        f(parser.screen())
    }

    pub fn is_done(&mut self) -> bool {
        if self.done {
            return true;
        }
        match self.done_rx.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => {
                self.done = true;
                true
            }
            Err(TryRecvError::Empty) => false,
        }
    }

    /// Resize the pty + parser to a new geometry. No-op if the size
    /// hasn't changed; called by the compose draw whenever the body
    /// area's dimensions shift.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows == self.rows && cols == self.cols {
            return;
        }
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut p) = self.parser.lock() {
            p.screen_mut().set_size(rows, cols);
        }
        self.rows = rows;
        self.cols = cols;
    }

    /// Translate a crossterm KeyEvent into bytes for the pty. Returns
    /// true if the key was understood and forwarded. Unknown keys are
    /// silently dropped (rather than guessing wrong bytes that the
    /// editor would misinterpret).
    pub fn forward_key(&mut self, k: KeyEvent) -> bool {
        let bytes = encode_key(k);
        if bytes.is_empty() {
            return false;
        }
        let _ = self.writer.write_all(&bytes);
        let _ = self.writer.flush();
        true
    }
}

fn encode_key(k: KeyEvent) -> Vec<u8> {
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let mut out = Vec::with_capacity(8);
    if alt {
        out.push(0x1b);
    }
    match k.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Ctrl + ASCII letter / common puncts → C0 control.
                let lower = c.to_ascii_lowercase();
                let byte: Option<u8> = match lower {
                    'a'..='z' => Some((lower as u8) - b'a' + 1),
                    '@' => Some(0x00),
                    '[' => Some(0x1b),
                    '\\' => Some(0x1c),
                    ']' => Some(0x1d),
                    '^' => Some(0x1e),
                    '_' => Some(0x1f),
                    ' ' => Some(0x00),
                    _ => None,
                };
                if let Some(b) = byte {
                    out.push(b);
                } else {
                    return Vec::new();
                }
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Left => out.extend_from_slice(b"\x1b[D"),
        KeyCode::Right => out.extend_from_slice(b"\x1b[C"),
        KeyCode::Up => out.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => out.extend_from_slice(b"\x1b[B"),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyCode::F(n) => match n {
            1 => out.extend_from_slice(b"\x1bOP"),
            2 => out.extend_from_slice(b"\x1bOQ"),
            3 => out.extend_from_slice(b"\x1bOR"),
            4 => out.extend_from_slice(b"\x1bOS"),
            5 => out.extend_from_slice(b"\x1b[15~"),
            6 => out.extend_from_slice(b"\x1b[17~"),
            7 => out.extend_from_slice(b"\x1b[18~"),
            8 => out.extend_from_slice(b"\x1b[19~"),
            9 => out.extend_from_slice(b"\x1b[20~"),
            10 => out.extend_from_slice(b"\x1b[21~"),
            11 => out.extend_from_slice(b"\x1b[23~"),
            12 => out.extend_from_slice(b"\x1b[24~"),
            _ => return Vec::new(),
        },
        _ => return Vec::new(),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn kc(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn enter_is_cr() {
        assert_eq!(encode_key(k(KeyCode::Enter)), vec![b'\r']);
    }

    #[test]
    fn backspace_is_del() {
        assert_eq!(encode_key(k(KeyCode::Backspace)), vec![0x7f]);
    }

    #[test]
    fn arrow_up_is_csi_a() {
        assert_eq!(encode_key(k(KeyCode::Up)), b"\x1b[A");
    }

    #[test]
    fn ctrl_a_is_soh() {
        assert_eq!(
            encode_key(kc(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            vec![0x01]
        );
    }

    #[test]
    fn alt_a_is_esc_prefixed_a() {
        assert_eq!(
            encode_key(kc(KeyCode::Char('a'), KeyModifiers::ALT)),
            vec![0x1b, b'a']
        );
    }

    #[test]
    fn plain_char_passes_through() {
        assert_eq!(encode_key(k(KeyCode::Char('x'))), vec![b'x']);
    }

    #[test]
    fn unicode_char_is_utf8_encoded() {
        assert_eq!(encode_key(k(KeyCode::Char('é'))), "é".as_bytes());
    }
}
