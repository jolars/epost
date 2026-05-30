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
//!    root, plus — under the fs layout — each folder root that may
//!    contain nested sub-folders). Events are mapped to `(account,
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
/// root maildir; otherwise the per-layout label form (Maildir++ strips
/// the leading dot from `.Sent`; fs uses the `/`-joined relative path
/// like `Sent/2024`).
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
    /// fs layout) add the folder root to the discovery-roots map so new
    /// nested sub-folders get detected.
    AddFolder {
        account: String,
        label: String,
        folder_root: PathBuf,
        layout: Layout,
    },
    /// Re-discover one account's folders and register any that aren't
    /// already watched. Fired after a Create(Folder) event under a
    /// discovery root.
    Rediscover { account: String },
}

#[derive(Default)]
struct DirtyState {
    dirty: HashSet<FolderKey>,
    last_event_at: Option<Instant>,
}

type LookupMap = HashMap<PathBuf, FolderKey>;

/// Owning info for an account, keyed by name. Held alongside the
/// `lookup` and `discovery_roots` maps so the debounce thread can run
/// `layout.discover_folders(root)` without bouncing back to the caller.
#[derive(Clone)]
struct AccountInfo {
    root: PathBuf,
    layout: Layout,
}
type Accounts = HashMap<String, AccountInfo>;

/// Map from a watched directory (account root, or — under fs — a folder
/// root) to the account it belongs to. Used in `handle_event` to map a
/// `Create(Folder)` event back to (a) its account and (b) the right
/// reaction: Maildir++ folder events under the account root strip the
/// dot and register the new folder directly; fs events anywhere queue a
/// full re-discovery for the account.
type DiscoveryRoots = HashMap<PathBuf, String>;

impl Watcher {
    /// Register a freshly-created subfolder (e.g. one the app just
    /// created via `:mv NewFolder`). Adds watches on `{folder_root}/cur`
    /// and `{folder_root}/new` and (in fs layout) the folder root
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
    let acc_map: Arc<Mutex<Accounts>> = Arc::new(Mutex::new(
        accounts
            .iter()
            .map(|s| {
                (
                    s.name.clone(),
                    AccountInfo {
                        root: s.root.clone(),
                        layout: s.layout,
                    },
                )
            })
            .collect(),
    ));
    let state = Arc::new((Mutex::new(DirtyState::default()), Condvar::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (out_tx, out_rx) = mpsc::channel();
    let (register_tx, register_rx) = mpsc::channel();

    let callback = {
        let lookup = lookup.clone();
        let state = state.clone();
        let self_writes = self_writes.clone();
        let discovery_roots = discovery_roots.clone();
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
                &discovery_roots,
                &register_tx_cb,
            );
        }
    };

    let watcher = notify::recommended_watcher(callback).context("creating notify watcher")?;
    let watcher_arc = Arc::new(Mutex::new(watcher));

    register_initial(&watcher_arc, &lookup, &discovery_roots, accounts)?;

    let dt_inner = watcher_arc.clone();
    let dt_lookup = lookup.clone();
    let dt_state = state.clone();
    let dt_shutdown = shutdown.clone();
    let dt_discovery = discovery_roots.clone();
    let dt_accounts = acc_map.clone();
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
                dt_accounts,
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
    let mut w = watcher.lock().expect("watcher poisoned");
    let mut lk = lookup.lock().expect("lookup poisoned");
    let mut dr = discovery_roots.lock().expect("discovery_roots poisoned");
    for spec in accounts {
        if !spec.root.is_dir() {
            continue;
        }
        // Account root, non-recursive: catches new top-level sub-folder
        // creation (both `.Sub` under maildir++ and `Sub/` under fs).
        w.watch(&spec.root, RecursiveMode::NonRecursive)
            .with_context(|| format!("watching maildir root {}", spec.root.display()))?;
        dr.insert(spec.root.clone(), spec.name.clone());

        // INBOX `cur`/`new`.
        for sub in ["cur", "new"] {
            let dir = spec.root.join(sub);
            if dir.is_dir() {
                w.watch(&dir, RecursiveMode::NonRecursive)
                    .with_context(|| format!("watching {}", dir.display()))?;
                lk.insert(dir, (spec.name.clone(), "INBOX".to_string()));
            }
        }

        for (label, folder_root) in spec.layout.discover_folders(&spec.root) {
            install_folder_watches(
                &mut w,
                &mut lk,
                &mut dr,
                &spec.name,
                &label,
                &folder_root,
                spec.layout,
            );
        }
    }
    Ok(())
}

/// Install non-recursive watches on `folder_root/{cur,new}`, populate
/// `lookup` for both, and — under the fs layout — add `folder_root` to
/// `discovery_roots` so nested sub-folder creation events get picked up.
/// Best-effort: a stale `.lock` or unusual permission shouldn't fail
/// the whole watcher.
fn install_folder_watches(
    w: &mut RecommendedWatcher,
    lk: &mut LookupMap,
    dr: &mut DiscoveryRoots,
    account: &str,
    label: &str,
    folder_root: &Path,
    layout: Layout,
) {
    for sd in ["cur", "new"] {
        let dir = folder_root.join(sd);
        if dir.is_dir() && w.watch(&dir, RecursiveMode::NonRecursive).is_ok() {
            lk.insert(dir, (account.to_string(), label.to_string()));
        }
    }
    if matches!(layout, Layout::Fs)
        && folder_root.is_dir()
        && w.watch(folder_root, RecursiveMode::NonRecursive).is_ok()
    {
        dr.insert(folder_root.to_path_buf(), account.to_string());
    }
}

fn handle_event(
    ev: notify::Event,
    lookup: &Arc<Mutex<LookupMap>>,
    state: &Arc<(Mutex<DirtyState>, Condvar)>,
    self_writes: &SelfWrites,
    discovery_roots: &Arc<Mutex<DiscoveryRoots>>,
    register_tx: &Sender<RegisterMsg>,
) {
    if !is_event_interesting(&ev.kind) {
        return;
    }

    let mut to_mark: Vec<FolderKey> = Vec::new();
    let mut rediscover_accounts: HashSet<String> = HashSet::new();

    for path in &ev.paths {
        // Suppress our own writes.
        if self_writes.consume(path) {
            continue;
        }

        // File event: the parent dir (e.g. `.../Foo/cur`) is the
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

        // Folder event under a known discovery root (account root, or —
        // under fs — an existing folder root). Queue re-discovery for
        // that account; the debounce thread will diff against already-
        // registered folder roots and add only the new ones.
        if let Some(parent) = path.parent() {
            let acc = discovery_roots
                .lock()
                .expect("discovery_roots poisoned")
                .get(parent)
                .cloned();
            if let Some(account) = acc {
                rediscover_accounts.insert(account);
            }
        }
    }

    for account in rediscover_accounts {
        let _ = register_tx.send(RegisterMsg::Rediscover { account });
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
    accounts: Arc<Mutex<Accounts>>,
    register_rx: Receiver<RegisterMsg>,
    out: Sender<WatcherEvent>,
    wake_tx: Sender<AppEvent>,
    debounce: Duration,
) {
    let (lock, cv) = &*state;
    loop {
        while let Ok(msg) = register_rx.try_recv() {
            apply_register(&msg, &watcher, &lookup, &discovery_roots, &accounts, &state);
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
    discovery_roots: &Arc<Mutex<DiscoveryRoots>>,
    accounts: &Arc<Mutex<Accounts>>,
    state: &Arc<(Mutex<DirtyState>, Condvar)>,
) {
    match msg {
        RegisterMsg::AddFolder {
            account,
            label,
            folder_root,
            layout,
        } => {
            let mut w = watcher.lock().expect("watcher poisoned");
            let mut lk = lookup.lock().expect("lookup poisoned");
            let mut dr = discovery_roots.lock().expect("discovery_roots poisoned");
            install_folder_watches(
                &mut w,
                &mut lk,
                &mut dr,
                account,
                label,
                folder_root,
                *layout,
            );
        }
        RegisterMsg::Rediscover { account } => {
            let info = accounts
                .lock()
                .expect("accounts poisoned")
                .get(account)
                .cloned();
            let Some(info) = info else { return };
            let folders = info.layout.discover_folders(&info.root);
            let mut new_keys: Vec<FolderKey> = Vec::new();
            {
                let mut w = watcher.lock().expect("watcher poisoned");
                let mut lk = lookup.lock().expect("lookup poisoned");
                let mut dr = discovery_roots.lock().expect("discovery_roots poisoned");
                for (label, folder_root) in folders {
                    let cur = folder_root.join("cur");
                    if lk.contains_key(&cur) {
                        // Already registered.
                        continue;
                    }
                    install_folder_watches(
                        &mut w,
                        &mut lk,
                        &mut dr,
                        account,
                        &label,
                        &folder_root,
                        info.layout,
                    );
                    new_keys.push((account.clone(), label));
                }
            }
            // Dirty-mark every newly-discovered folder so a rescan picks
            // up its pre-existing files.
            if !new_keys.is_empty() {
                let (lk2, cv) = &**state;
                let mut g = lk2.lock().expect("dirty state poisoned");
                for k in new_keys {
                    g.dirty.insert(k);
                }
                g.last_event_at = Some(Instant::now());
                cv.notify_all();
            }
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
        let dr = Arc::new(Mutex::new(HashMap::new()));
        let (rtx, _rrx) = mpsc::channel::<RegisterMsg>();

        let ev = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![PathBuf::from("/m/dev/cur/1.M0.h:2,S")],
            attrs: Default::default(),
        };
        handle_event(ev, &lookup, &state, &sw, &dr, &rtx);

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
        let dr = Arc::new(Mutex::new(HashMap::new()));
        let (rtx, _rrx) = mpsc::channel::<RegisterMsg>();

        let ev = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![path.clone()],
            attrs: Default::default(),
        };
        handle_event(ev, &lookup, &state, &sw, &dr, &rtx);

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
        let dr = Arc::new(Mutex::new(HashMap::new()));
        let (rtx, _rrx) = mpsc::channel::<RegisterMsg>();

        let ev = notify::Event {
            kind: EventKind::Access(notify::event::AccessKind::Read),
            paths: vec![PathBuf::from("/m/dev/cur/1.M0.h:2,S")],
            attrs: Default::default(),
        };
        handle_event(ev, &lookup, &state, &sw, &dr, &rtx);

        assert!(state.0.lock().unwrap().dirty.is_empty());
    }

    #[test]
    fn handle_event_new_folder_under_account_root_queues_rediscovery() {
        let lookup = Arc::new(Mutex::new(HashMap::new()));
        let state = Arc::new((Mutex::new(DirtyState::default()), Condvar::new()));
        let sw = SelfWrites::new();
        let dr = Arc::new(Mutex::new(HashMap::new()));
        dr.lock()
            .unwrap()
            .insert(PathBuf::from("/m/dev"), "dev".to_string());
        let (rtx, rrx) = mpsc::channel::<RegisterMsg>();

        let ev = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::Folder),
            paths: vec![PathBuf::from("/m/dev/.NewFolder")],
            attrs: Default::default(),
        };
        handle_event(ev, &lookup, &state, &sw, &dr, &rtx);

        let msg = rrx.try_recv().expect("expected a Rediscover message");
        match msg {
            RegisterMsg::Rediscover { account } => assert_eq!(account, "dev"),
            _ => panic!("expected Rediscover"),
        }
    }

    #[test]
    fn handle_event_new_nested_folder_under_fs_folder_root_queues_rediscovery() {
        let lookup = Arc::new(Mutex::new(HashMap::new()));
        let state = Arc::new((Mutex::new(DirtyState::default()), Condvar::new()));
        let sw = SelfWrites::new();
        let dr = Arc::new(Mutex::new(HashMap::new()));
        // The Sent folder root has been registered as a discovery root
        // (fs layout); create a 2024 sub-folder under it.
        dr.lock()
            .unwrap()
            .insert(PathBuf::from("/m/dev/Sent"), "dev".to_string());
        let (rtx, rrx) = mpsc::channel::<RegisterMsg>();

        let ev = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::Folder),
            paths: vec![PathBuf::from("/m/dev/Sent/2024")],
            attrs: Default::default(),
        };
        handle_event(ev, &lookup, &state, &sw, &dr, &rtx);

        let msg = rrx.try_recv().expect("expected a Rediscover message");
        match msg {
            RegisterMsg::Rediscover { account } => assert_eq!(account, "dev"),
            _ => panic!("expected Rediscover"),
        }
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
        let accounts = vec![AccountSpec {
            name: "dev".to_string(),
            root: root.clone(),
            layout: Layout::Maildirpp,
        }];
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
        // Only the account root is a discovery root under maildir++.
        let dr = discovery_roots.lock().unwrap();
        assert_eq!(dr.get(&root), Some(&"dev".to_string()));
        assert!(dr.get(&root.join(".Sent")).is_none());
    }

    #[test]
    fn register_initial_fs_walks_nested_and_marks_folder_roots_as_discovery() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("fs");
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
        let accounts = vec![AccountSpec {
            name: "fs".to_string(),
            root: root.clone(),
            layout: Layout::Fs,
        }];
        register_initial(&watcher_arc, &lookup, &discovery_roots, &accounts).unwrap();

        let lk = lookup.lock().unwrap();
        // INBOX.
        assert_eq!(
            lk.get(&root.join("cur")),
            Some(&("fs".into(), "INBOX".into()))
        );
        // Real-subdir sub-folder with /-joined label.
        assert_eq!(
            lk.get(&root.join("Sent").join("cur")),
            Some(&("fs".into(), "Sent".into()))
        );
        assert_eq!(
            lk.get(&root.join("Sent").join("2024").join("cur")),
            Some(&("fs".into(), "Sent/2024".into()))
        );
        assert_eq!(
            lk.get(&root.join("Archive").join("new")),
            Some(&("fs".into(), "Archive".into()))
        );
        // Under fs: every folder root is a discovery root so nested
        // sub-folder creates get caught.
        let dr = discovery_roots.lock().unwrap();
        assert_eq!(dr.get(&root), Some(&"fs".to_string()));
        assert_eq!(dr.get(&root.join("Sent")), Some(&"fs".to_string()));
        assert_eq!(
            dr.get(&root.join("Sent").join("2024")),
            Some(&"fs".to_string())
        );
        assert_eq!(dr.get(&root.join("Archive")), Some(&"fs".to_string()));
    }
}
