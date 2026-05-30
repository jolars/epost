pub mod index;
pub mod scan;
pub mod sync;
pub mod thread;
pub mod watch;

use std::path::PathBuf;

use crate::mail::layout::Layout;

/// Per-account fan-out parameters for the scan worker and the inotify
/// watcher. Bundles the three pieces both consumers need (name, maildir
/// root, on-disk folder layout) so signatures don't grow to triples.
#[derive(Debug, Clone)]
pub struct AccountSpec {
    pub name: String,
    pub root: PathBuf,
    pub layout: Layout,
}
