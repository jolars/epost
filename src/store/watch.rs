//! Self-write registry for the future `notify` watcher (Step 7).
//!
//! When the app mutates a maildir file itself (e.g. flag flip renames a
//! message into `:2,S`), the inotify event the watcher will eventually
//! receive is *not* an external change and should not trigger a rescan.
//! Step 5's flag flow records the affected paths here so the Step 7
//! watcher can `consume()` them and skip the redundant work.
//!
//! Step 7 doesn't exist yet; this module exposes only the registry so the
//! flag flow has a place to record its writes without retrofitting later.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct SelfWrites(Arc<Mutex<HashSet<PathBuf>>>);

impl SelfWrites {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a path the app is about to mutate. For a rename, call once
    /// for the source and once for the destination so the watcher
    /// suppresses both the delete and the create event.
    pub fn record(&self, path: impl Into<PathBuf>) {
        let mut g = self.0.lock().expect("SelfWrites poisoned");
        g.insert(path.into());
    }

    /// Returns true iff `path` was registered by us, removing it so a
    /// later genuinely-external write on the same path is not swallowed.
    /// No caller in Step 5; the Step 7 notify watcher will be the
    /// consumer.
    #[allow(dead_code)]
    pub fn consume(&self, path: &Path) -> bool {
        let mut g = self.0.lock().expect("SelfWrites poisoned");
        g.remove(path)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_writes_record_then_consume() {
        let sw = SelfWrites::new();
        let p = PathBuf::from("/tmp/epost-test/cur/x:2,S");
        sw.record(&p);
        assert_eq!(sw.len(), 1);
        assert!(sw.consume(&p));
        assert_eq!(sw.len(), 0);
        // Second consume returns false — the watcher won't accidentally
        // swallow a later genuine event on the same path.
        assert!(!sw.consume(&p));
    }

    #[test]
    fn self_writes_clone_shares_state() {
        let a = SelfWrites::new();
        let b = a.clone();
        let p = PathBuf::from("/tmp/epost-test/cur/y");
        b.record(&p);
        assert!(a.consume(&p));
    }
}
