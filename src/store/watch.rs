//! Self-write registry and `notify`-backed maildir watcher (Step 7).
//!
//! Two cooperating pieces:
//! 1. `SelfWrites` — a thread-safe set of paths we are about to mutate.
//!    The flag-flow `_recorded` wrappers register both source and
//!    destination *before* their rename so the inotify event that lands
//!    afterwards can be matched and dropped: it is our own write, not an
//!    external change.
//! 2. `Watcher` — wraps `notify::RecommendedWatcher`. It watches every
//!    account maildir's `INBOX` (`{cur,new}`) plus each discovered
//!    sub-folder's `{cur,new}` and every "discovery root" (the account
//!    root, plus — under the verbatim layout — each folder root that
//!    may contain nested sub-folders). Events are mapped to `(account,
//!    folder)` dirty marks, accumulated over a debounce window
//!    (`config.watch.debounce_ms`), and emitted as one
//!    `WatcherEvent::FoldersDirty(set)` per quiet period.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::mail::layout::Layout;
use crate::store::AccountSpec;
use crate::ui::events::AppEvent;

// ---------- SelfWrites ----------

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
        g.insert(normalize(&path.into()));
    }

    /// Returns true iff `path` was registered by us, removing it so a
    /// later genuinely-external write on the same path is not swallowed.
    /// Called by the notify watcher to skip echoes of our own renames,
    /// and by the `_recorded` flag-flow wrappers to clean up after a
    /// failed rename.
    pub fn consume(&self, path: &Path) -> bool {
        let mut g = self.0.lock().expect("SelfWrites poisoned");
        g.remove(&normalize(path))
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

// ---------- Watcher ----------

/// `(account_name, folder_label)`. `folder_label` is `"INBOX"` for the
/// root maildir; otherwise the per-layout label form (Maildir++ strips
/// the leading dot from `.Sent`; verbatim uses the `/`-joined relative
/// path like `Sent/2024`).
pub type FolderKey = (String, String);

#[derive(Debug)]
pub enum WatcherEvent {
    /// One or more folders had external file activity in the most
    /// recent debounce window. The set is the union of every
    /// `(account, folder)` that fired at least one non-self-write event.
    FoldersDirty(HashSet<FolderKey>),
}

#[derive(Debug, Clone)]
pub struct WatcherConfig {
    pub debounce: Duration,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(250),
        }
    }
}

/// Owns the `notify::RecommendedWatcher` (RAII: dropping releases all
/// inotify FDs and stops notify's internal thread) and the debounce
/// helper thread. Dropping signals shutdown and joins the debounce
/// thread before letting the watcher itself drop.
pub struct Watcher {
    inner: Arc<Mutex<RecommendedWatcher>>,
    shutdown: Arc<AtomicBool>,
    state: Arc<(Mutex<DirtyState>, Condvar)>,
    debounce_thread: Option<JoinHandle<()>>,
    register_tx: Sender<RegisterMsg>,
}

enum RegisterMsg {
    /// Install watches for a specific folder root's `{cur,new}` and (in
    /// verbatim layout) add the folder root to the discovery-roots map
    /// so new nested sub-folders get detected.
    AddFolder {
        account: String,
        label: String,
        folder_root: PathBuf,
        layout: Layout,
    },
}

#[derive(Default)]
struct DirtyState {
    dirty: HashSet<FolderKey>,
    last_event_at: Option<Instant>,
}

/// Normalise a path so lookup keys and event-parent lookups share one
/// form. `notify`'s inotify backend reports event paths as the watch
/// path *as registered* joined with the changed filename, applying only
/// minimal normalisation: a watch registered on a **relative** path is
/// reported back with the process CWD prepended (and `.`/`..` left
/// intact), so the event parent (`/cwd/./dev/…/new`) never equals the
/// verbatim relative string we stored as the key (`./dev/…/new`) and
/// every external event is silently dropped — no live updates. We saw
/// exactly this under the dev config's relative `maildir` paths.
/// Canonicalising both the stored keys and the event parents to the
/// same real, absolute, symlink-resolved form makes the lookup hit
/// regardless of how the path was spelled (relative, `.`/`..`, or a
/// symlinked maildir). Falls back to the input on error (e.g. a
/// transient permission issue) so we degrade rather than panic.
fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Normalise a maildir file path to the same form the watcher compares
/// against: the canonicalised parent directory joined with the original
/// filename. `SelfWrites` records and consumes through this so a
/// self-write recorded with a non-canonical path (relative `dev/maildir`
/// roots, a symlinked maildir) still matches the event `notify` reports
/// under the canonical watch dir. Mirrors the parent-canonicalisation
/// `handle_event` already does on the lookup side — without it, our own
/// renames leak through as "external" dirt and trigger a clobbering
/// rescan. We canonicalise only the parent (always present for both the
/// source `cur/` and the pre-created destination `cur/`); canonicalising
/// the full path would fail for the destination, which doesn't exist
/// yet at record time.
fn normalize(p: &Path) -> PathBuf {
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => canonical(parent).join(name),
        _ => p.to_path_buf(),
    }
}

type LookupMap = HashMap<PathBuf, FolderKey>;

/// Map from a watched directory (account root, or — under verbatim — a
/// folder root) to the account it belongs to. Kept alongside the
/// `lookup` map for symmetry: `lookup` is per-folder `{cur,new}`,
/// `discovery_roots` is per-folder-root.
type DiscoveryRoots = HashMap<PathBuf, String>;

impl Watcher {
    /// Register a freshly-created subfolder (e.g. one the app just
    /// created via `:mv NewFolder`). Adds watches on `{folder_root}/cur`
    /// and `{folder_root}/new` and (in verbatim layout) the folder root
    /// itself, and marks the folder dirty so any pre-existing files get
    /// picked up on the next rescan. Idempotent — re-registering an
    /// already-watched folder is a no-op.
    pub fn register_folder(&self, account: &str, label: &str, folder_root: &Path, layout: Layout) {
        let _ = self.register_tx.send(RegisterMsg::AddFolder {
            account: account.to_string(),
            label: label.to_string(),
            folder_root: folder_root.to_path_buf(),
            layout,
        });
        // Wake the debounce thread to process immediately rather than
        // waiting up to one idle tick.
        self.state.1.notify_all();
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.state.1.notify_all();
        if let Some(h) = self.debounce_thread.take() {
            let _ = h.join();
        }
        // `self.inner` drops here; that releases inotify FDs and stops
        // notify's internal thread.
        let _ = &self.inner;
    }
}

/// Spawn the watcher. On success, returns the RAII handle and the
/// receiver the UI polls for `FoldersDirty` events. The watcher pushes
/// a `Wake` into `wake_tx` after each flush so the main loop doesn't
/// need to spin on a timer.
pub fn start(
    accounts: &[AccountSpec],
    self_writes: SelfWrites,
    cfg: WatcherConfig,
    wake_tx: Sender<AppEvent>,
) -> Result<(Watcher, Receiver<WatcherEvent>)> {
    let lookup: Arc<Mutex<LookupMap>> = Arc::new(Mutex::new(HashMap::new()));
    let discovery_roots: Arc<Mutex<DiscoveryRoots>> = Arc::new(Mutex::new(HashMap::new()));
    let state = Arc::new((Mutex::new(DirtyState::default()), Condvar::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (out_tx, out_rx) = mpsc::channel();
    let (register_tx, register_rx) = mpsc::channel();

    let callback = {
        let lookup = lookup.clone();
        let state = state.clone();
        let self_writes = self_writes.clone();
        move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else {
                return;
            };
            handle_event(ev, &lookup, &state, &self_writes);
        }
    };

    let watcher = notify::recommended_watcher(callback).context("creating notify watcher")?;
    let watcher_arc = Arc::new(Mutex::new(watcher));

    register_initial(&watcher_arc, &lookup, &discovery_roots, accounts)?;
    log::info!(
        "watch: started — {} accounts, {} folder dirs watched",
        accounts.len(),
        lookup.lock().expect("lookup poisoned").len()
    );

    let dt_inner = watcher_arc.clone();
    let dt_lookup = lookup.clone();
    let dt_state = state.clone();
    let dt_shutdown = shutdown.clone();
    let dt_discovery = discovery_roots.clone();
    let dt_debounce = cfg.debounce;
    let handle = std::thread::Builder::new()
        .name("epost-watch-debounce".into())
        .spawn(move || {
            debounce_loop(
                dt_inner,
                dt_lookup,
                dt_state,
                dt_shutdown,
                dt_discovery,
                register_rx,
                out_tx,
                wake_tx,
                dt_debounce,
            );
        })
        .context("spawning debounce thread")?;

    Ok((
        Watcher {
            inner: watcher_arc,
            shutdown,
            state,
            debounce_thread: Some(handle),
            register_tx,
        },
        out_rx,
    ))
}

fn register_initial(
    watcher: &Arc<Mutex<RecommendedWatcher>>,
    lookup: &Arc<Mutex<LookupMap>>,
    discovery_roots: &Arc<Mutex<DiscoveryRoots>>,
    accounts: &[AccountSpec],
) -> Result<()> {
    // The `watcher` mutex may be held across `w.watch()`: notify's
    // callback never touches it. The `lookup` / `discovery_roots`
    // mutexes must NOT be held across `w.watch()` — see the comment in
    // `install_folder_watches`.
    let mut w = watcher.lock().expect("watcher poisoned");
    for spec in accounts {
        if !spec.root.is_dir() {
            log::warn!(
                "watch: account {:?} root {} is not a dir — skipping (no live updates for it)",
                spec.name,
                spec.root.display()
            );
            continue;
        }
        // Account root, non-recursive: catches new top-level sub-folder
        // creation (both `.Sub` under maildir++ and `Sub/` under verbatim).
        let root = canonical(&spec.root);
        w.watch(&root, RecursiveMode::NonRecursive)
            .with_context(|| format!("watching maildir root {}", root.display()))?;
        log::debug!(
            "watch: account {:?} root watched: {}",
            spec.name,
            root.display()
        );
        discovery_roots
            .lock()
            .expect("discovery_roots poisoned")
            .insert(root, spec.name.clone());

        // Watch every binding's `{cur,new}`. The INBOX binding
        // (`folders[0]`) and every role/extra after it share the
        // same shape, so one loop covers both. Layout drives the
        // discovery-roots side: under verbatim, each folder root is
        // itself a discovery root so nested sub-folder creation is
        // visible.
        for binding in &spec.folders {
            install_folder_watches(
                &mut w,
                lookup,
                discovery_roots,
                &spec.name,
                &binding.label,
                &binding.path,
                spec.layout,
            );
        }
    }
    Ok(())
}

/// Install non-recursive watches on `folder_root/{cur,new}`, populate
/// `lookup` for both, and — under the verbatim layout — add
/// `folder_root` to `discovery_roots` so nested sub-folder creation
/// events get picked up. Best-effort: a stale `.lock` or unusual
/// permission shouldn't fail the whole watcher.
///
/// CRITICAL: `w.watch()` must never be called while the `lookup` mutex
/// is held. notify's inotify backend services `watch()` synchronously
/// on its event-loop thread and blocks the caller until that thread
/// replies — but that same thread also delivers events into our
/// callback, which locks `lookup` (see `handle_event`). Holding `lookup`
/// across `w.watch()` therefore deadlocks: the event-loop thread waits
/// for `lookup` inside the callback while we wait for the event-loop
/// thread to ack the watch. After a cross-folder move (`d` / `:trash` /
/// `:archive`), `register_folder` drives `apply_register` here exactly
/// while the move's own inotify events are in flight, so the window is
/// wide open. We lock `lookup` / `discovery_roots` only for the brief
/// insert *after* `watch()` returns. The `watcher` mutex (held by
/// callers via `w`) is safe to hold across `watch()` — the callback
/// never touches it.
fn install_folder_watches(
    w: &mut RecommendedWatcher,
    lookup: &Arc<Mutex<LookupMap>>,
    discovery_roots: &Arc<Mutex<DiscoveryRoots>>,
    account: &str,
    label: &str,
    folder_root: &Path,
    layout: Layout,
) {
    for sd in ["cur", "new"] {
        let dir = canonical(&folder_root.join(sd));
        if !dir.is_dir() {
            log::debug!(
                "watch: {account}/{label}: {} absent — not watched",
                dir.display()
            );
            continue;
        }
        match w.watch(&dir, RecursiveMode::NonRecursive) {
            Ok(()) => {
                log::debug!("watch: {account}/{label}: watching {}", dir.display());
                lookup
                    .lock()
                    .expect("lookup poisoned")
                    .insert(dir, (account.to_string(), label.to_string()));
            }
            Err(e) => log::warn!(
                "watch: {account}/{label}: failed to watch {}: {e}",
                dir.display()
            ),
        }
    }
    if matches!(layout, Layout::Verbatim) {
        let root = canonical(folder_root);
        if root.is_dir() && w.watch(&root, RecursiveMode::NonRecursive).is_ok() {
            discovery_roots
                .lock()
                .expect("discovery_roots poisoned")
                .insert(root, account.to_string());
        }
    }
}

fn handle_event(
    ev: notify::Event,
    lookup: &Arc<Mutex<LookupMap>>,
    state: &Arc<(Mutex<DirtyState>, Condvar)>,
    self_writes: &SelfWrites,
) {
    if !is_event_interesting(&ev.kind) {
        log::trace!("watch: ignoring event kind {:?} {:?}", ev.kind, ev.paths);
        return;
    }
    log::debug!("watch: event {:?} paths={:?}", ev.kind, ev.paths);

    let mut to_mark: Vec<FolderKey> = Vec::new();

    for path in &ev.paths {
        // Suppress our own writes.
        if self_writes.consume(path) {
            log::debug!("watch: suppressed self-write {}", path.display());
            continue;
        }

        // File event: the parent dir (e.g. `.../Foo/cur`) is the
        // lookup key. Folder-creation events under unconfigured
        // paths are intentionally dropped — under role-based folder
        // config, new folders only appear when the user adds them
        // to their config.
        if let Some(folder_dir) = path.parent() {
            // Canonicalise to the same form the lookup keys were stored
            // in — see `canonical`. Without this, a watch registered on a
            // relative or symlinked path never matches the event parent.
            let folder_dir = canonical(folder_dir);
            let hit = lookup
                .lock()
                .expect("lookup poisoned")
                .get(&folder_dir)
                .cloned();
            match hit {
                Some(key) => {
                    log::debug!(
                        "watch: matched {} -> {:?}, marking dirty",
                        path.display(),
                        key
                    );
                    to_mark.push(key);
                }
                None => log::debug!(
                    "watch: no lookup entry for parent dir {} (event dropped)",
                    folder_dir.display()
                ),
            }
        }
    }

    if !to_mark.is_empty() {
        let (lock, cv) = &**state;
        let mut g = lock.lock().expect("dirty state poisoned");
        for k in to_mark {
            g.dirty.insert(k);
        }
        g.last_event_at = Some(Instant::now());
        cv.notify_all();
    }
}

/// Event kinds worth re-scanning for. We deliberately drop
/// metadata-only changes (atime, ctime, permissions) and data-only
/// modifications (writes to an existing maildir file are a spec
/// violation — flags and identity are encoded in the filename).
fn is_event_interesting(kind: &EventKind) -> bool {
    !matches!(
        kind,
        EventKind::Access(_)
            | EventKind::Modify(ModifyKind::Metadata(_))
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Other
    )
}

#[allow(clippy::too_many_arguments)]
fn debounce_loop(
    watcher: Arc<Mutex<RecommendedWatcher>>,
    lookup: Arc<Mutex<LookupMap>>,
    state: Arc<(Mutex<DirtyState>, Condvar)>,
    shutdown: Arc<AtomicBool>,
    discovery_roots: Arc<Mutex<DiscoveryRoots>>,
    register_rx: Receiver<RegisterMsg>,
    out: Sender<WatcherEvent>,
    wake_tx: Sender<AppEvent>,
    debounce: Duration,
) {
    let (lock, cv) = &*state;
    loop {
        while let Ok(msg) = register_rx.try_recv() {
            apply_register(&msg, &watcher, &lookup, &discovery_roots);
        }

        if shutdown.load(Ordering::Acquire) {
            break;
        }

        let guard = lock.lock().expect("dirty state poisoned");
        let now = Instant::now();
        if flush_due(&guard, now, debounce) {
            let mut g = guard;
            let drained = std::mem::take(&mut g.dirty);
            g.last_event_at = None;
            drop(g);
            log::debug!("watch: flushing FoldersDirty {drained:?}");
            let _ = out.send(WatcherEvent::FoldersDirty(drained));
            let _ = wake_tx.send(AppEvent::Wake);
            continue;
        }

        let wait = match guard.last_event_at {
            Some(t) => {
                let elapsed = now.saturating_duration_since(t);
                debounce
                    .saturating_sub(elapsed)
                    .max(Duration::from_millis(10))
            }
            // Idle: re-check shutdown / register periodically.
            None => Duration::from_millis(250),
        };
        let _ = cv.wait_timeout(guard, wait).expect("dirty cv poisoned");
    }
}

fn apply_register(
    msg: &RegisterMsg,
    watcher: &Arc<Mutex<RecommendedWatcher>>,
    lookup: &Arc<Mutex<LookupMap>>,
    discovery_roots: &Arc<Mutex<DiscoveryRoots>>,
) {
    match msg {
        RegisterMsg::AddFolder {
            account,
            label,
            folder_root,
            layout,
        } => {
            // Hold only the `watcher` mutex across `w.watch()`; the
            // `lookup` / `discovery_roots` mutexes are locked per-insert
            // inside `install_folder_watches`. Holding `lookup` here
            // would deadlock against notify's callback — see that fn.
            let mut w = watcher.lock().expect("watcher poisoned");
            install_folder_watches(
                &mut w,
                lookup,
                discovery_roots,
                account,
                label,
                folder_root,
                *layout,
            );
        }
    }
}

/// Pure function for the flush decision: drain the dirty set iff it has
/// at least one entry and the most recent event is at least `debounce`
/// old. Extracted for unit testing.
fn flush_due(state: &DirtyState, now: Instant, debounce: Duration) -> bool {
    if state.dirty.is_empty() {
        return false;
    }
    match state.last_event_at {
        Some(t) => now.saturating_duration_since(t) >= debounce,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_to_end_delivery_fires_folders_dirty() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("gmail");
        // Inbox lives in a subdir, mirroring the real config where the
        // maildir root itself is not a maildir.
        let inbox = root.join("Inbox");
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(inbox.join(sub)).unwrap();
        }

        let mut acc = crate::config::Account {
            maildir: root.clone(),
            from: "x".into(),
            layout: Layout::Verbatim,
            inbox: None,
            archive: None,
            sent: None,
            spam: None,
            trash: None,
            drafts: None,
            extra_folders: vec![],
            smtp: None,
            primary: false,
        };
        let _ = &mut acc;
        let spec = AccountSpec::from_account("gmail", &acc);
        // Sanity: the INBOX binding must resolve to the Inbox subdir.
        assert_eq!(spec.folders[0].path, inbox);

        let (wake_tx, _wake_rx) = mpsc::channel();
        let (_watcher, rx) = start(
            &[spec],
            SelfWrites::new(),
            WatcherConfig {
                debounce: Duration::from_millis(50),
            },
            wake_tx,
        )
        .unwrap();

        // Simulate an mbsync delivery: write to tmp/, rename into new/.
        let tmp_file = inbox.join("tmp").join("1.deliver");
        fs::write(&tmp_file, b"From: a@b\r\n\r\nhi\r\n").unwrap();
        fs::rename(&tmp_file, inbox.join("new").join("1.deliver")).unwrap();

        let ev = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("expected FoldersDirty within 3s");
        let WatcherEvent::FoldersDirty(set) = ev;
        assert!(
            set.contains(&("gmail".to_string(), "INBOX".to_string())),
            "dirty set should contain gmail/INBOX, got {set:?}"
        );
    }

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
    fn self_writes_normalize_matches_non_canonical_record() {
        // Regression for the "deleted row lingers" bug: a self-write
        // recorded with a non-canonical path (relative / `.`-laden, as the
        // move code passes it) must still be consumed when the watcher
        // presents the canonical event path notify reports under the
        // canonical watch dir. Without parent-canonicalisation here, our
        // own delete leaks through as external dirt and triggers a rescan
        // that resurrects the row we just dropped.
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let cur = tmp.path().join("Inbox").join("cur");
        fs::create_dir_all(&cur).unwrap();

        let sw = SelfWrites::new();
        // Record with a noisy `/./Inbox/./cur/` parent.
        let noisy = tmp
            .path()
            .join(".")
            .join("Inbox")
            .join(".")
            .join("cur")
            .join("1.M0.h:2,S");
        sw.record(&noisy);
        assert_eq!(sw.len(), 1);

        // Consume with the canonical path — the form the watcher derives
        // from a real inotify event.
        let canonical_event = cur.join("1.M0.h:2,S");
        assert!(
            sw.consume(&canonical_event),
            "normalised self-write must match the canonical event path"
        );
        assert_eq!(sw.len(), 0);
    }

    #[test]
    fn self_writes_clone_shares_state() {
        let a = SelfWrites::new();
        let b = a.clone();
        let p = PathBuf::from("/tmp/epost-test/cur/y");
        b.record(&p);
        assert!(a.consume(&p));
    }

    #[test]
    fn flush_due_false_when_empty() {
        let s = DirtyState::default();
        assert!(!flush_due(&s, Instant::now(), Duration::from_millis(100)));
    }

    #[test]
    fn flush_due_false_until_window_elapses() {
        let mut s = DirtyState::default();
        s.dirty.insert(("dev".into(), "INBOX".into()));
        let t0 = Instant::now();
        s.last_event_at = Some(t0);
        assert!(!flush_due(&s, t0, Duration::from_millis(100)));
        assert!(!flush_due(
            &s,
            t0 + Duration::from_millis(50),
            Duration::from_millis(100),
        ));
        assert!(flush_due(
            &s,
            t0 + Duration::from_millis(100),
            Duration::from_millis(100),
        ));
        assert!(flush_due(
            &s,
            t0 + Duration::from_millis(200),
            Duration::from_millis(100),
        ));
    }

    #[test]
    fn handle_event_marks_dirty_via_lookup() {
        let lookup = Arc::new(Mutex::new(HashMap::new()));
        lookup
            .lock()
            .unwrap()
            .insert(PathBuf::from("/m/dev/cur"), ("dev".into(), "INBOX".into()));
        let state = Arc::new((Mutex::new(DirtyState::default()), Condvar::new()));
        let sw = SelfWrites::new();

        let ev = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![PathBuf::from("/m/dev/cur/1.M0.h:2,S")],
            attrs: Default::default(),
        };
        handle_event(ev, &lookup, &state, &sw);

        let g = state.0.lock().unwrap();
        assert!(g.dirty.contains(&("dev".to_string(), "INBOX".to_string())));
        assert!(g.last_event_at.is_some());
    }

    #[test]
    fn handle_event_swallows_self_write() {
        let lookup = Arc::new(Mutex::new(HashMap::new()));
        lookup
            .lock()
            .unwrap()
            .insert(PathBuf::from("/m/dev/cur"), ("dev".into(), "INBOX".into()));
        let state = Arc::new((Mutex::new(DirtyState::default()), Condvar::new()));
        let sw = SelfWrites::new();
        let path = PathBuf::from("/m/dev/cur/1.M0.h:2,S");
        sw.record(&path);

        let ev = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![path.clone()],
            attrs: Default::default(),
        };
        handle_event(ev, &lookup, &state, &sw);

        let g = state.0.lock().unwrap();
        assert!(
            g.dirty.is_empty(),
            "self-write must not produce a dirty mark"
        );
        assert!(
            !sw.consume(&path),
            "registry entry should already be consumed"
        );
    }

    #[test]
    fn handle_event_matches_non_canonical_event_parent() {
        // Regression for the live-update bug: lookup keys are stored
        // canonicalised, but `notify` can report event paths in a
        // non-canonical form (relative watches come back CWD-prefixed
        // with `.`/`..` intact). The event parent must be canonicalised
        // before the lookup or the dirty mark is dropped and the list
        // never refreshes. Here the key is the real dir; the event path
        // carries a `/./` segment that must collapse to match.
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let cur = tmp.path().join("Inbox").join("cur");
        fs::create_dir_all(&cur).unwrap();

        let lookup = Arc::new(Mutex::new(HashMap::new()));
        lookup
            .lock()
            .unwrap()
            .insert(canonical(&cur), ("gmail".into(), "INBOX".into()));
        let state = Arc::new((Mutex::new(DirtyState::default()), Condvar::new()));
        let sw = SelfWrites::new();

        // Event path with a non-canonical `/./Inbox/./cur/` parent.
        let noisy = tmp
            .path()
            .join(".")
            .join("Inbox")
            .join(".")
            .join("cur")
            .join("1.M0.h:2,S");
        let ev = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![noisy],
            attrs: Default::default(),
        };
        handle_event(ev, &lookup, &state, &sw);

        assert!(
            state
                .0
                .lock()
                .unwrap()
                .dirty
                .contains(&("gmail".to_string(), "INBOX".to_string())),
            "non-canonical event parent must still match the canonical lookup key"
        );
    }

    #[test]
    fn handle_event_drops_access_kind() {
        let lookup = Arc::new(Mutex::new(HashMap::new()));
        lookup
            .lock()
            .unwrap()
            .insert(PathBuf::from("/m/dev/cur"), ("dev".into(), "INBOX".into()));
        let state = Arc::new((Mutex::new(DirtyState::default()), Condvar::new()));
        let sw = SelfWrites::new();

        let ev = notify::Event {
            kind: EventKind::Access(notify::event::AccessKind::Read),
            paths: vec![PathBuf::from("/m/dev/cur/1.M0.h:2,S")],
            attrs: Default::default(),
        };
        handle_event(ev, &lookup, &state, &sw);

        assert!(state.0.lock().unwrap().dirty.is_empty());
    }

    /// Build an `AccountSpec` from a maildir + a list of role bindings
    /// for use in watch tests. `roles` keys mirror `Account` fields
    /// (`"sent"`, `"archive"`, …); `extras` go on `extra_folders`.
    fn test_spec(
        name: &str,
        root: PathBuf,
        layout: Layout,
        roles: &[(&str, &str)],
        extras: &[&str],
    ) -> AccountSpec {
        let mut acc = crate::config::Account {
            maildir: root,
            from: "x".into(),
            layout,
            inbox: None,
            archive: None,
            sent: None,
            spam: None,
            trash: None,
            drafts: None,
            extra_folders: extras.iter().map(|s| s.to_string()).collect(),
            smtp: None,
            primary: false,
        };
        for (field, disk) in roles {
            let v = Some(disk.to_string());
            match *field {
                "inbox" => acc.inbox = v,
                "archive" => acc.archive = v,
                "sent" => acc.sent = v,
                "spam" => acc.spam = v,
                "trash" => acc.trash = v,
                "drafts" => acc.drafts = v,
                other => panic!("unknown role field {other}"),
            }
        }
        AccountSpec::from_account(name, &acc)
    }

    #[test]
    fn register_initial_maildirpp_walks_inbox_and_dotted_subfolders() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("dev");
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(root.join(".Sent").join(sub)).unwrap();
            fs::create_dir_all(root.join(".Archive").join(sub)).unwrap();
        }

        let watcher_arc = Arc::new(Mutex::new(
            notify::recommended_watcher(|_res: notify::Result<notify::Event>| {}).unwrap(),
        ));
        let lookup = Arc::new(Mutex::new(HashMap::new()));
        let discovery_roots = Arc::new(Mutex::new(HashMap::new()));
        let accounts = vec![test_spec(
            "dev",
            root.clone(),
            Layout::Maildirpp,
            &[("sent", "Sent"), ("archive", "Archive")],
            &[],
        )];
        register_initial(&watcher_arc, &lookup, &discovery_roots, &accounts).unwrap();

        let lk = lookup.lock().unwrap();
        assert_eq!(
            lk.get(&root.join("cur")),
            Some(&("dev".into(), "INBOX".into()))
        );
        assert_eq!(
            lk.get(&root.join("new")),
            Some(&("dev".into(), "INBOX".into()))
        );
        assert_eq!(
            lk.get(&root.join(".Sent").join("cur")),
            Some(&("dev".into(), "Sent".into()))
        );
        assert_eq!(
            lk.get(&root.join(".Archive").join("new")),
            Some(&("dev".into(), "Archive".into()))
        );
    }

    #[test]
    fn register_initial_verbatim_walks_configured_extras() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("verbatim");
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(root.join("Sent").join(sub)).unwrap();
            fs::create_dir_all(root.join("Sent").join("2024").join(sub)).unwrap();
            fs::create_dir_all(root.join("Archive").join(sub)).unwrap();
        }

        let watcher_arc = Arc::new(Mutex::new(
            notify::recommended_watcher(|_res: notify::Result<notify::Event>| {}).unwrap(),
        ));
        let lookup = Arc::new(Mutex::new(HashMap::new()));
        let discovery_roots = Arc::new(Mutex::new(HashMap::new()));
        // Nested folders go on `extra_folders` — the role-based
        // design treats anything beyond the canonical six as
        // user-declared.
        let accounts = vec![test_spec(
            "verbatim",
            root.clone(),
            Layout::Verbatim,
            &[("sent", "Sent"), ("archive", "Archive")],
            &["Sent/2024"],
        )];
        register_initial(&watcher_arc, &lookup, &discovery_roots, &accounts).unwrap();

        let lk = lookup.lock().unwrap();
        assert_eq!(
            lk.get(&root.join("cur")),
            Some(&("verbatim".into(), "INBOX".into()))
        );
        assert_eq!(
            lk.get(&root.join("Sent").join("cur")),
            Some(&("verbatim".into(), "Sent".into()))
        );
        assert_eq!(
            lk.get(&root.join("Sent").join("2024").join("cur")),
            Some(&("verbatim".into(), "Sent/2024".into()))
        );
        assert_eq!(
            lk.get(&root.join("Archive").join("new")),
            Some(&("verbatim".into(), "Archive".into()))
        );
    }
}
