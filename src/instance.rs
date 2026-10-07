//! Mailto hand-off to the first running instance for a configuration.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, ensure};

use crate::mail::mailto::Mailto;
use crate::ui::events::AppEvent;

const MAX_URI_BYTES: usize = 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(1);

pub fn socket_path(config: &Path) -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "epost")
        .context("finding the instance runtime directory")?;
    let root = dirs
        .runtime_dir()
        .unwrap_or(dirs.cache_dir())
        .join("instances");
    let absolute = std::path::absolute(config)?;
    // Home Manager replaces the config symlink on rebuild. Resolve its
    // parent, but retain its filename so the instance identity stays stable.
    let parent = absolute.parent().context("config has no parent")?;
    let key = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf())
        .join(absolute.file_name().context("config has no filename")?);
    // FNV-1a keeps the socket name short and stable across binary versions.
    let hash = key
        .as_os_str()
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    Ok(root.join(format!("{hash:016x}.sock")))
}

/// `false` means no listener exists; other errors must not launch a second
/// composer because the running instance may already have received the URI.
pub fn forward(path: &Path, uri: &str) -> Result<bool> {
    uri.parse::<Mailto>()?;
    ensure!(uri.len() <= MAX_URI_BYTES, "mailto URI is too large");
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(false);
        }
        Err(e) => return Err(e).context("connecting to the running instance"),
    };
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(&(uri.len() as u32).to_be_bytes())?;
    stream.write_all(uri.as_bytes())?;
    let mut ack = [0];
    stream.read_exact(&mut ack)?;
    ensure!(ack == [1], "running instance rejected the mailto request");
    Ok(true)
}

pub struct Instance {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    // Keep the lock file open until the listener has stopped and its socket
    // has been removed. A later instance may then reclaim a stale socket.
    _lock: File,
}

impl Instance {
    pub fn start(path: &Path, tx: Sender<AppEvent>) -> Result<Option<Self>> {
        let parent = path.parent().context("instance socket has no parent")?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path.with_extension("lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::Builder::new()
            .name("epost-mailto".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(mut stream) = stream else { break };
                    if let Err(e) = receive(&mut stream, &tx) {
                        log::warn!("rejected external mailto request: {e:#}");
                    }
                }
            })?;
        Ok(Some(Self {
            path: path.to_owned(),
            stop,
            worker: Some(worker),
            _lock: lock,
        }))
    }
}

fn receive(stream: &mut UnixStream, tx: &Sender<AppEvent>) -> Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut size = [0; 4];
    stream.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    ensure!(size <= MAX_URI_BYTES, "mailto URI is too large");
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes)?;
    let mailto = std::str::from_utf8(&bytes)?.parse()?;
    tx.send(AppEvent::Mailto(mailto))?;
    stream.write_all(&[1])?;
    Ok(())
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake the blocking accept so shutdown needs no polling worker.
        let _ = UnixStream::connect(&self.path);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn forwarded_mailto_reaches_the_existing_event_loop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.sock");
        let (tx, rx) = mpsc::channel();
        let instance = Instance::start(&path, tx).unwrap().unwrap();
        let uri = "mailto:dev+list@example.com?cc=copy@example.com&bcc=hidden@example.com&subject=H%C3%A9llo&body=First%0D%0ASecond";
        assert!(forward(&path, uri).unwrap());
        let AppEvent::Mailto(mailto) = rx.recv_timeout(IO_TIMEOUT).unwrap() else {
            panic!("expected mailto")
        };
        assert_eq!(mailto, uri.parse::<Mailto>().unwrap());
        drop(instance);
        assert!(!forward(&path, uri).unwrap());
    }

    #[test]
    fn only_one_instance_owns_the_socket_and_a_stale_socket_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.sock");
        drop(UnixListener::bind(&path).unwrap());
        let (tx, rx) = mpsc::channel();
        let first = Instance::start(&path, tx.clone()).unwrap().unwrap();
        assert!(Instance::start(&path, tx.clone()).unwrap().is_none());
        assert!(forward(&path, "mailto:first@example.com").unwrap());
        assert!(matches!(
            rx.recv_timeout(IO_TIMEOUT).unwrap(),
            AppEvent::Mailto(_)
        ));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(first);
        let _next = Instance::start(&path, tx).unwrap().unwrap();
        assert!(forward(&path, "mailto:second@example.com").unwrap());
    }

    #[test]
    fn malformed_and_oversized_requests_do_not_open_drafts_or_stop_the_listener() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.sock");
        let (tx, rx) = mpsc::channel();
        let _instance = Instance::start(&path, tx).unwrap().unwrap();
        for bytes in [b"mailto:?subject=%0ABcc:evil".as_slice(), &[0xff]] {
            let mut stream = UnixStream::connect(&path).unwrap();
            stream
                .write_all(&(bytes.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(bytes).unwrap();
            assert!(stream.read_exact(&mut [0]).is_err());
        }
        let mut stream = UnixStream::connect(&path).unwrap();
        stream
            .write_all(&((MAX_URI_BYTES + 1) as u32).to_be_bytes())
            .unwrap();
        assert!(stream.read_exact(&mut [0]).is_err());
        assert!(rx.try_recv().is_err());
        assert!(forward(&path, "mailto:valid@example.com").unwrap());
        assert!(matches!(
            rx.recv_timeout(IO_TIMEOUT).unwrap(),
            AppEvent::Mailto(_)
        ));
    }

    #[test]
    fn missing_and_stale_listeners_allow_terminal_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.sock");
        assert!(!forward(&path, "mailto:dev@example.com").unwrap());
        drop(UnixListener::bind(&path).unwrap());
        assert!(!forward(&path, "mailto:dev@example.com").unwrap());
    }

    #[test]
    fn config_symlink_replacement_does_not_change_instance_identity() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let first = dir.path().join("first.toml");
        let second = dir.path().join("second.toml");
        fs::write(&first, "").unwrap();
        fs::write(&second, "").unwrap();
        std::os::unix::fs::symlink(&first, &config).unwrap();
        let before = socket_path(&config).unwrap();
        fs::remove_file(&config).unwrap();
        std::os::unix::fs::symlink(&second, &config).unwrap();
        assert_eq!(before, socket_path(&config).unwrap());
        assert_ne!(before, socket_path(&dir.path().join("other.toml")).unwrap());
    }
}
