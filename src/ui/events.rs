//! Unified event channel for the main loop. Replaces the
//! poll-with-timeout pattern: any source that wants the UI to wake up
//! (input arrives, embedded editor exits, pty has new bytes, scan/send
//! worker reports) sends an `AppEvent` and the main loop runs one
//! cycle. This is the same architecture aerc uses via vaxis's
//! `ui.Invalidate()` — react in real time, never on a timer.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use anyhow::{Context, Result};
use crossterm::event::{self, Event};

#[derive(Debug)]
pub enum AppEvent {
    /// A raw crossterm input event (key, resize, mouse, …).
    Input(Event),
    /// Something asked the UI to re-tick: poll workers, finalize
    /// finished editors, draw. The payload is informational only —
    /// the main loop does the same work for every wake.
    Wake,
}

/// Spawn a thread that blocks on `crossterm::event::read()` and
/// forwards every event into the returned receiver. Returns the
/// `Sender` half too so other subsystems (editor pty, workers) can
/// inject `Wake` events into the same stream.
pub fn channel() -> Result<(Sender<AppEvent>, Receiver<AppEvent>)> {
    let (tx, rx) = mpsc::channel();
    let input_tx = tx.clone();
    thread::Builder::new()
        .name("epost-input".into())
        .spawn(move || {
            while let Ok(ev) = event::read() {
                if input_tx.send(AppEvent::Input(ev)).is_err() {
                    break;
                }
            }
        })
        .context("spawning input reader")?;
    Ok((tx, rx))
}
