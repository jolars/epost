//! Self-write registry and `notify`-backed maildir watcher (Step 7).
//!
//! Two cooperating pieces:
//! 1. `SelfWrites` — a thread-safe set of paths we are about to mutate.
//!    The flag-flow `_recorded` wrappers register both source and
//!    destination *before* their rename so the inotify event that lands
//!    afterwards can be matched and dropped: it is our own write, not an
//!    external change.
//! 2. `Watcher` — wraps `notify::RecommendedWatcher`. It watches every
//!    account maildir's `INBOX` (`{cur,new}`) plus each Maildir++
//!    `.Subfolder/{cur,new}`, plus each account root (non-recursive) so
//!    folders that appear at runtime auto-register. Events are mapped to
//!    `(account, folder)` dirty marks, accumulated over a debounce window
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
        g.insert(path.into());
    }

    /// Returns true iff `path` was registered by us, removing it so a
    /// later genuinely-external write on the same path is not swallowed.
    /// Called by the notify watcher to skip echoes of our own renames,
    /// and by the `_recorded` flag-flow wrappers to clean up after a
    /// failed rename.
    pub fn consume(&self, path: &Path) -> bool {
        let mut g = self.0.lock().expect("SelfWrites poisoned");
        g.remove(path)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

// ---------- Watcher ----------

/// `(account_name, folder_label)`. `folder_label` is `"INBOX"` for the
/// root maildir, otherwise the Maildir++ subfolder name with the leading
/// `.` stripped (e.g. `.Sent` → `"Sent"`).
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
    AddFolder {
        account: String,
        label: String,
        folder_root: PathBuf,
    },
}

#[derive(Default)]
struct DirtyState {
    dirty: HashSet<FolderKey>,
    last_event_at: Option<Instant>,
}

type LookupMap = HashMap<PathBuf, FolderKey>;
type AccountRoots = HashMap<PathBuf, String>;

impl Watcher {
    /// Register a freshly-created subfolder (e.g. one the app just
    /// created via `:mv NewFolder`). Adds watches on `{folder_root}/cur`
    /// and `{folder_root}/new` and marks the folder dirty so any
    /// pre-existing files get picked up on the next rescan. Idempotent
    /// — re-registering an already-watched folder is a no-op.
    pub fn register_folder(&self, account: &str, label: &str, folder_root: &Path) {
        let _ = self.register_tx.send(RegisterMsg::AddFolder {
            account: account.to_string(),
            label: label.to_string(),
            folder_root: folder_root.to_path_buf(),
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
    accounts: &[(String, PathBuf)],
    self_writes: SelfWrites,
    cfg: WatcherConfig,
    wake_tx: Sender<AppEvent>,
) -> Result<(Watcher, Receiver<WatcherEvent>)> {
    let lookup: Arc<Mutex<LookupMap>> = Arc::new(Mutex::new(HashMap::new()));
    let account_roots: Arc<Mutex<AccountRoots>> = Arc::new(Mutex::new(HashMap::new()));
    let state = Arc::new((Mutex::new(DirtyState::default()), Condvar::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (out_tx, out_rx) = mpsc::channel();
    let (register_tx, register_rx) = mpsc::channel();

    let callback = {
        let lookup = lookup.clone();
        let state = state.clone();
        let self_writes = self_writes.clone();
        let account_roots = account_roots.clone();
        let register_tx_cb = register_tx.clone();
        move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else {
                return;
            };
            handle_event(
                ev,
                &lookup,
                &state,
                &self_writes,
                &account_roots,
                &register_tx_cb,
            );
        }
    };

    let watcher = notify::recommended_watcher(callback).context("creating notify watcher")?;
    let watcher_arc = Arc::new(Mutex::new(watcher));

    register_initial(&watcher_arc, &lookup, &account_roots, accounts)?;

    let dt_inner = watcher_arc.clone();
    let dt_lookup = lookup.clone();
    let dt_state = state.clone();
    let dt_shutdown = shutdown.clone();
    let dt_debounce = cfg.debounce;
    let handle = std::thread::Builder::new()
        .name("epost-watch-debounce".into())
        .spawn(move || {
            debounce_loop(
                dt_inner,
                dt_lookup,
                dt_state,
                dt_shutdown,
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
    account_roots: &Arc<Mutex<AccountRoots>>,
    accounts: &[(String, PathBuf)],
) -> Result<()> {
    let mut w = watcher.lock().expect("watcher poisoned");
    let mut lk = lookup.lock().expect("lookup poisoned");
    let mut ar = account_roots.lock().expect("account_roots poisoned");
    for (name, root) in accounts {
        if !root.is_dir() {
            continue;
        }
        // Account root, non-recursive: catches new `.Subfolder/`
        // creation at runtime.
        w.watch(root, RecursiveMode::NonRecursive)
            .with_context(|| format!("watching maildir root {}", root.display()))?;
        ar.insert(root.clone(), name.clone());

        // INBOX `cur`/`new`.
        for sub in ["cur", "new"] {
            let dir = root.join(sub);
            if dir.is_dir() {
                w.watch(&dir, RecursiveMode::NonRecursive)
                    .with_context(|| format!("watching {}", dir.display()))?;
                lk.insert(dir, (name.clone(), "INBOX".to_string()));
            }
        }

        // Each `.Sub/cur`, `.Sub/new`.
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut subs: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with('.') && n != "." && n != "..")
                    .unwrap_or(false)
            })
            .collect();
        subs.sort();
        for sub in subs {
            let label = sub
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.strip_prefix('.').unwrap_or(n).to_string())
                .unwrap_or_else(|| "?".to_string());
            for sd in ["cur", "new"] {
                let dir = sub.join(sd);
                if dir.is_dir() {
                    // Best-effort: a stale .lock or unusual permission
                    // shouldn't fail the whole watcher startup.
                    let _ = w.watch(&dir, RecursiveMode::NonRecursive);
                    lk.insert(dir, (name.clone(), label.clone()));
                }
            }
        }
    }
    Ok(())
}

fn handle_event(
    ev: notify::Event,
    lookup: &Arc<Mutex<LookupMap>>,
    state: &Arc<(Mutex<DirtyState>, Condvar)>,
    self_writes: &SelfWrites,
    account_roots: &Arc<Mutex<AccountRoots>>,
    register_tx: &Sender<RegisterMsg>,
) {
    if !is_event_interesting(&ev.kind) {
        return;
    }

    let mut to_mark: Vec<FolderKey> = Vec::new();
    let mut to_register: Vec<(String, String, PathBuf)> = Vec::new();

    for path in &ev.paths {
        // Suppress our own writes.
        if self_writes.consume(path) {
            continue;
        }

        // File event: the parent dir (e.g. `.../.Foo/cur`) is the
        // lookup key.
        if let Some(folder_dir) = path.parent() {
            let hit = lookup
                .lock()
                .expect("lookup poisoned")
                .get(folder_dir)
                .cloned();
            if let Some(key) = hit {
                to_mark.push(key);
                continue;
            }
        }

        // Folder event under an account root: e.g. `Create(Folder)` on
        // `<root>/.NewSub` when mbsync first-syncs a new folder.
        if let Some(parent) = path.parent() {
            let acc = account_roots
                .lock()
                .expect("account_roots poisoned")
                .get(parent)
                .cloned();
            if let Some(account) = acc
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
                && let Some(stripped) = name.strip_prefix('.')
            {
                let label = stripped.to_string();
                to_register.push((account, label, path.clone()));
            }
        }
    }

    for (acc, label, root) in to_register {
        let _ = register_tx.send(RegisterMsg::AddFolder {
            account: acc.clone(),
            label: label.clone(),
            folder_root: root,
        });
        // Dirty-mark the new folder so its files get picked up after
        // the debounce thread installs its watches.
        to_mark.push((acc, label));
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
    register_rx: Receiver<RegisterMsg>,
    out: Sender<WatcherEvent>,
    wake_tx: Sender<AppEvent>,
    debounce: Duration,
) {
    let (lock, cv) = &*state;
    loop {
        while let Ok(msg) = register_rx.try_recv() {
            apply_register(&msg, &watcher, &lookup);
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
) {
    let RegisterMsg::AddFolder {
        account,
        label,
        folder_root,
    } = msg;
    let mut w = watcher.lock().expect("watcher poisoned");
    let mut lk = lookup.lock().expect("lookup poisoned");
    for sub in ["cur", "new"] {
        let dir = folder_root.join(sub);
        if !dir.is_dir() {
            continue;
        }
        // Re-watching an already-watched path is harmless on inotify;
        // ignore errors so a transient permission glitch doesn't poison
        // the lookup map.
        if w.watch(&dir, RecursiveMode::NonRecursive).is_ok() {
            lk.insert(dir, (account.clone(), label.clone()));
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
        let ar = Arc::new(Mutex::new(HashMap::new()));
        let (rtx, _rrx) = mpsc::channel::<RegisterMsg>();

        let ev = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![PathBuf::from("/m/dev/cur/1.M0.h:2,S")],
            attrs: Default::default(),
        };
        handle_event(ev, &lookup, &state, &sw, &ar, &rtx);

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
        let ar = Arc::new(Mutex::new(HashMap::new()));
        let (rtx, _rrx) = mpsc::channel::<RegisterMsg>();

        let ev = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![path.clone()],
            attrs: Default::default(),
        };
        handle_event(ev, &lookup, &state, &sw, &ar, &rtx);

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
    fn handle_event_drops_access_kind() {
        let lookup = Arc::new(Mutex::new(HashMap::new()));
        lookup
            .lock()
            .unwrap()
            .insert(PathBuf::from("/m/dev/cur"), ("dev".into(), "INBOX".into()));
        let state = Arc::new((Mutex::new(DirtyState::default()), Condvar::new()));
        let sw = SelfWrites::new();
        let ar = Arc::new(Mutex::new(HashMap::new()));
        let (rtx, _rrx) = mpsc::channel::<RegisterMsg>();

        let ev = notify::Event {
            kind: EventKind::Access(notify::event::AccessKind::Read),
            paths: vec![PathBuf::from("/m/dev/cur/1.M0.h:2,S")],
            attrs: Default::default(),
        };
        handle_event(ev, &lookup, &state, &sw, &ar, &rtx);

        assert!(state.0.lock().unwrap().dirty.is_empty());
    }

    #[test]
    fn handle_event_new_subfolder_queues_register_and_dirty() {
        let lookup = Arc::new(Mutex::new(HashMap::new()));
        let state = Arc::new((Mutex::new(DirtyState::default()), Condvar::new()));
        let sw = SelfWrites::new();
        let ar = Arc::new(Mutex::new(HashMap::new()));
        ar.lock()
            .unwrap()
            .insert(PathBuf::from("/m/dev"), "dev".to_string());
        let (rtx, rrx) = mpsc::channel::<RegisterMsg>();

        let ev = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::Folder),
            paths: vec![PathBuf::from("/m/dev/.NewFolder")],
            attrs: Default::default(),
        };
        handle_event(ev, &lookup, &state, &sw, &ar, &rtx);

        let msg = rrx.try_recv().expect("expected an AddFolder message");
        match msg {
            RegisterMsg::AddFolder {
                account,
                label,
                folder_root,
            } => {
                assert_eq!(account, "dev");
                assert_eq!(label, "NewFolder");
                assert_eq!(folder_root, PathBuf::from("/m/dev/.NewFolder"));
            }
        }
        assert!(
            state
                .0
                .lock()
                .unwrap()
                .dirty
                .contains(&("dev".to_string(), "NewFolder".to_string()))
        );
    }

    #[test]
    fn register_initial_walks_inbox_and_subfolders() {
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

        // Build a watcher with a no-op handler so we can exercise
        // `register_initial` against real directories.
        let watcher_arc = Arc::new(Mutex::new(
            notify::recommended_watcher(|_res: notify::Result<notify::Event>| {}).unwrap(),
        ));
        let lookup = Arc::new(Mutex::new(HashMap::new()));
        let account_roots = Arc::new(Mutex::new(HashMap::new()));
        let accounts = vec![("dev".to_string(), root.clone())];
        register_initial(&watcher_arc, &lookup, &account_roots, &accounts).unwrap();

        let lk = lookup.lock().unwrap();
        // INBOX cur + new.
        assert_eq!(
            lk.get(&root.join("cur")),
            Some(&("dev".into(), "INBOX".into()))
        );
        assert_eq!(
            lk.get(&root.join("new")),
            Some(&("dev".into(), "INBOX".into()))
        );
        // .Sent / .Archive.
        assert_eq!(
            lk.get(&root.join(".Sent").join("cur")),
            Some(&("dev".into(), "Sent".into()))
        );
        assert_eq!(
            lk.get(&root.join(".Archive").join("new")),
            Some(&("dev".into(), "Archive".into()))
        );
        // Account root tracked.
        assert_eq!(
            account_roots.lock().unwrap().get(&root),
            Some(&"dev".to_string())
        );
    }
}
