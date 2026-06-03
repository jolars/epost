//! Maildir info-flag suffix parsing and spec-correct on-disk renames.
//!
//! Maildir filenames are `<base>[:2,FLAGS]`. The base name is what mbsync
//! rewrites on every sync; the suffix is sorted ASCII uppercase letters
//! (canonical `D`/`F`/`P`/`R`/`S`/`T`; vendor letters like `X`/`Y` from
//! mu / notmuch are preserved on read but never minted by us). A file
//! living in `new/` has no suffix at all; the moment we add any flag the
//! file moves to the sibling `cur/`.
//!
//! The rename primitive is shaped so a future "move between folders"
//! operation reuses the same code path with a different `target_cur_dir`.
//! Step 5 only wires the same-folder flag flip; `move_to_folder` is the
//! same primitive exposed for the cross-folder case when it lands.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::store::watch::SelfWrites;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FlagOp {
    Add,
    // `Remove` is part of the public mutation API for symmetry with `Add`;
    // Step 5 itself only auto-Adds `S` and Toggles for manual unread.
    #[allow(dead_code)]
    Remove,
    Toggle,
}

#[derive(Debug, Error)]
pub enum FlagError {
    #[error("source no longer exists: {0}")]
    SourceGone(PathBuf),
    #[error("target already exists: {0}")]
    TargetExists(PathBuf),
    #[error("path not under cur/ or new/: {0}")]
    BadLayout(PathBuf),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Parse the canonical flag string out of a maildir filename. Returns
/// `""` for a file with no `:2,` suffix (typical for a fresh `new/`
/// delivery). The result is sorted, deduped, and ASCII-uppercase only;
/// lowercase noise is dropped, but uppercase vendor letters survive.
pub fn parse_flags(filename: &str) -> String {
    let suffix = filename.rsplit_once(":2,").map(|(_, s)| s).unwrap_or("");
    canonicalize(suffix)
}

/// Apply a single-flag mutation. `current` is treated as a flag set
/// (membership only); the output is canonical regardless of input shape.
/// `Add` of an already-set flag and `Remove` of an unset flag are
/// idempotent.
pub fn apply_op(current: &str, flag: char, op: FlagOp) -> String {
    debug_assert!(
        flag.is_ascii_uppercase(),
        "flag must be ASCII uppercase, got {flag:?}",
    );
    if !flag.is_ascii_uppercase() {
        return current.to_string();
    }
    let has = current.contains(flag);
    let want = match op {
        FlagOp::Add => true,
        FlagOp::Remove => false,
        FlagOp::Toggle => !has,
    };
    if has == want {
        return current.to_string();
    }
    let mut chars: Vec<char> = current.chars().filter(|c| c.is_ascii_uppercase()).collect();
    if want {
        chars.push(flag);
    } else {
        chars.retain(|&c| c != flag);
    }
    chars.sort_unstable();
    chars.dedup();
    chars.into_iter().collect()
}

/// Compute the destination path for a maildir rename. The base name
/// (everything before any existing `:2,`) is preserved; an existing
/// suffix is dropped before the new one is appended. An empty
/// `new_flags` produces a bare base in `target_cur_dir`.
///
/// Both the same-folder flag flip (target = file's own `cur/` sibling)
/// and the cross-folder move (target = destination folder's `cur/`)
/// share this function — that's the whole reason it takes the target
/// dir as a parameter instead of deriving it.
pub fn rename_for_flags(current_path: &Path, target_cur_dir: &Path, new_flags: &str) -> PathBuf {
    dest_path(current_path, target_cur_dir, new_flags, false)
}

/// Destination path for a *cross-folder move*. Same as
/// [`rename_for_flags`] but strips mbsync's `,U=<uid>` marker from the
/// base name. That marker records the message's UID **in its current
/// folder**; carried into a different folder it collides with the
/// destination's own UID space, and mbsync aborts that mailbox with
/// `Maildir error: duplicate UID N`. Dropping it makes the moved file a
/// fresh local message that mbsync uploads and assigns a new UID. A
/// same-folder flag flip must *not* strip it (mbsync tracks the message
/// by that UID), which is why this is move-only.
pub fn rename_for_move(current_path: &Path, target_cur_dir: &Path, new_flags: &str) -> PathBuf {
    dest_path(current_path, target_cur_dir, new_flags, true)
}

fn dest_path(
    current_path: &Path,
    target_cur_dir: &Path,
    new_flags: &str,
    strip_uid: bool,
) -> PathBuf {
    let name = current_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let base = name.rsplit_once(":2,").map(|(b, _)| b).unwrap_or(name);
    let base = if strip_uid {
        strip_mbsync_uid(base)
    } else {
        base.to_string()
    };
    let new_name = if new_flags.is_empty() {
        base
    } else {
        format!("{base}:2,{new_flags}")
    };
    target_cur_dir.join(new_name)
}

/// Remove mbsync's `,U=<digits>` UID marker from a maildir base name,
/// leaving any other comma-separated fields (e.g. `,S=<size>`) intact.
/// No-op when the marker is absent.
fn strip_mbsync_uid(base: &str) -> String {
    let Some(pos) = base.find(",U=") else {
        return base.to_string();
    };
    let after = &base[pos + 3..];
    let digits = after
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after.len());
    format!("{}{}", &base[..pos], &after[digits..])
}

/// Same-folder flag flip: resolve `target_cur_dir` from the file's own
/// maildir folder (sibling of its `cur`/`new` parent), mutate the flag
/// set, and perform the on-disk rename.
///
/// On success returns the new path and the canonical flag string so the
/// caller can mirror both into the index without re-reading the filename.
///
/// Tested primitive only: production callers go through
/// `set_flag_recorded` so the inotify watcher can suppress echoes.
#[cfg(test)]
pub fn set_flag(
    current_path: &Path,
    flag: char,
    op: FlagOp,
) -> Result<(PathBuf, String), FlagError> {
    let (new_path, new_flags) = flag_change_target(current_path, flag, op)?;
    do_rename(current_path, &new_path)?;
    Ok((new_path, new_flags))
}

/// Validate the maildir layout of `current_path` and compute the
/// destination path + canonical flag set after applying `op` to `flag`.
/// Pure: does not touch the filesystem.
fn flag_change_target(
    current_path: &Path,
    flag: char,
    op: FlagOp,
) -> Result<(PathBuf, String), FlagError> {
    let name = current_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| FlagError::BadLayout(current_path.to_path_buf()))?;
    let parent = current_path
        .parent()
        .ok_or_else(|| FlagError::BadLayout(current_path.to_path_buf()))?;
    let parent_name = parent
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| FlagError::BadLayout(current_path.to_path_buf()))?;
    if parent_name != "cur" && parent_name != "new" {
        return Err(FlagError::BadLayout(current_path.to_path_buf()));
    }
    let folder_root = parent
        .parent()
        .ok_or_else(|| FlagError::BadLayout(current_path.to_path_buf()))?;
    let target_cur_dir = folder_root.join("cur");

    let current_flags = parse_flags(name);
    let new_flags = apply_op(&current_flags, flag, op);
    let new_path = rename_for_flags(current_path, &target_cur_dir, &new_flags);
    Ok((new_path, new_flags))
}

/// Move a message file into a different folder's `cur/`, preserving (or
/// rewriting) its flag suffix.
///
/// Tested primitive only: production callers go through
/// `move_to_folder_recorded`.
#[cfg(test)]
pub fn move_to_folder(
    current_path: &Path,
    target_cur_dir: &Path,
    new_flags: &str,
) -> Result<PathBuf, FlagError> {
    let new_path = rename_for_move(current_path, target_cur_dir, new_flags);
    do_rename(current_path, &new_path)?;
    Ok(new_path)
}

/// Same as `set_flag`, but records both the source and destination on
/// `SelfWrites` *before* the rename so the inotify watcher can never see
/// our own write as an external event. On rename failure both
/// registrations are consumed so a future genuine event isn't swallowed.
pub fn set_flag_recorded(
    current_path: &Path,
    flag: char,
    op: FlagOp,
    sw: &SelfWrites,
) -> Result<(PathBuf, String), FlagError> {
    let (new_path, new_flags) = flag_change_target(current_path, flag, op)?;
    sw.record(current_path);
    sw.record(&new_path);
    match do_rename(current_path, &new_path) {
        Ok(()) => Ok((new_path, new_flags)),
        Err(e) => {
            sw.consume(current_path);
            sw.consume(&new_path);
            Err(e)
        }
    }
}

/// Same as `move_to_folder`, but records both src and dst on
/// `SelfWrites` before the rename (see `set_flag_recorded`).
pub fn move_to_folder_recorded(
    current_path: &Path,
    target_cur_dir: &Path,
    new_flags: &str,
    sw: &SelfWrites,
) -> Result<PathBuf, FlagError> {
    let new_path = rename_for_move(current_path, target_cur_dir, new_flags);
    sw.record(current_path);
    sw.record(&new_path);
    match do_rename(current_path, &new_path) {
        Ok(()) => Ok(new_path),
        Err(e) => {
            sw.consume(current_path);
            sw.consume(&new_path);
            Err(e)
        }
    }
}

/// Create `cur/`, `new/`, and `tmp/` under `folder_root` if missing. The
/// cross-folder move calls this before the rename when the user targets
/// a folder mbsync hasn't created locally yet.
pub fn ensure_maildir(folder_root: &Path) -> std::io::Result<()> {
    for sub in ["cur", "new", "tmp"] {
        std::fs::create_dir_all(folder_root.join(sub))?;
    }
    Ok(())
}

fn canonicalize(s: &str) -> String {
    let mut chars: Vec<char> = s.chars().filter(|c| c.is_ascii_uppercase()).collect();
    chars.sort_unstable();
    chars.dedup();
    chars.into_iter().collect()
}

fn do_rename(from: &Path, to: &Path) -> Result<(), FlagError> {
    if from == to {
        return Ok(());
    }
    if to.exists() {
        return Err(FlagError::TargetExists(to.to_path_buf()));
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // `fs::rename` reports `NotFound` for either a missing source or a
            // missing target directory; disambiguate so a missing dest isn't
            // mis-classified as "mbsync moved it under us".
            if from.exists() {
                Err(FlagError::Io(e))
            } else {
                Err(FlagError::SourceGone(from.to_path_buf()))
            }
        }
        Err(e) => Err(FlagError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn folder(tmp: &TempDir, name: &str) -> PathBuf {
        let root = tmp.path().join(name);
        fs::create_dir_all(root.join("cur")).unwrap();
        fs::create_dir_all(root.join("new")).unwrap();
        root
    }

    fn deliver_new(folder_root: &Path, basename: &str) -> PathBuf {
        let p = folder_root.join("new").join(basename);
        fs::write(&p, b"From: x\r\nMessage-ID: <m>\r\n\r\nbody\r\n").unwrap();
        p
    }

    fn deliver_cur(folder_root: &Path, name: &str) -> PathBuf {
        let p = folder_root.join("cur").join(name);
        fs::write(&p, b"From: x\r\nMessage-ID: <m>\r\n\r\nbody\r\n").unwrap();
        p
    }

    #[test]
    fn parse_flags_empty_for_missing_suffix() {
        assert_eq!(parse_flags("1779181200.M0P1.epost-dev"), "");
        assert_eq!(parse_flags(""), "");
    }

    #[test]
    fn parse_flags_sorts_and_dedupes() {
        assert_eq!(parse_flags("x:2,SRS"), "RS");
        assert_eq!(parse_flags("x:2,DRSF"), "DFRS");
        assert_eq!(parse_flags("x:2,T"), "T");
    }

    #[test]
    fn parse_flags_drops_lowercase_keeps_vendor_uppercase() {
        assert_eq!(parse_flags("x:2,SrT"), "ST");
        assert_eq!(parse_flags("x:2,XYS"), "SXY");
    }

    #[test]
    fn parse_flags_uses_rightmost_separator() {
        // Defensive: a base name that happens to contain ":2," shouldn't fool
        // the parser. mbsync filenames don't, but other senders might.
        assert_eq!(parse_flags("weird:2,prefix:2,RS"), "RS");
    }

    #[test]
    fn apply_op_idempotent_add_of_set() {
        assert_eq!(apply_op("RS", 'S', FlagOp::Add), "RS");
    }

    #[test]
    fn apply_op_idempotent_remove_of_unset() {
        assert_eq!(apply_op("RS", 'T', FlagOp::Remove), "RS");
    }

    #[test]
    fn apply_op_add_new_flag_canonicalizes() {
        assert_eq!(apply_op("R", 'S', FlagOp::Add), "RS");
        assert_eq!(apply_op("RS", 'D', FlagOp::Add), "DRS");
    }

    #[test]
    fn apply_op_remove_existing_flag() {
        assert_eq!(apply_op("DRS", 'D', FlagOp::Remove), "RS");
        assert_eq!(apply_op("S", 'S', FlagOp::Remove), "");
    }

    #[test]
    fn apply_op_toggle_round_trips() {
        let one = apply_op("R", 'S', FlagOp::Toggle);
        assert_eq!(one, "RS");
        let two = apply_op(&one, 'S', FlagOp::Toggle);
        assert_eq!(two, "R");
    }

    #[test]
    fn rename_for_flags_new_to_cur() {
        let tmp = TempDir::new().unwrap();
        let root = folder(&tmp, "INBOX");
        let src = root.join("new").join("base");
        let new = rename_for_flags(&src, &root.join("cur"), "S");
        assert_eq!(new, root.join("cur").join("base:2,S"));
    }

    #[test]
    fn rename_for_flags_cur_existing_suffix_replaced() {
        let tmp = TempDir::new().unwrap();
        let root = folder(&tmp, "INBOX");
        let src = root.join("cur").join("base:2,R");
        let new = rename_for_flags(&src, &root.join("cur"), "RS");
        assert_eq!(new, root.join("cur").join("base:2,RS"));
    }

    #[test]
    fn rename_for_flags_drops_suffix_when_empty() {
        let tmp = TempDir::new().unwrap();
        let root = folder(&tmp, "INBOX");
        let src = root.join("cur").join("base:2,S");
        let new = rename_for_flags(&src, &root.join("cur"), "");
        assert_eq!(new, root.join("cur").join("base"));
    }

    #[test]
    fn rename_for_flags_cross_folder() {
        // Locks in the shape the future move-between-folders depends on:
        // same primitive, just a different `target_cur_dir`.
        let tmp = TempDir::new().unwrap();
        let inbox = folder(&tmp, "INBOX");
        let archive = folder(&tmp, ".Archive");
        let src = inbox.join("cur").join("base:2,R");
        let new = rename_for_flags(&src, &archive.join("cur"), "R");
        assert_eq!(new, archive.join("cur").join("base:2,R"));
    }

    #[test]
    fn set_flag_end_to_end_new_to_cur() {
        let tmp = TempDir::new().unwrap();
        let root = folder(&tmp, "INBOX");
        let src = deliver_new(&root, "1779.M0P1.host");

        let (new_path, new_flags) = set_flag(&src, 'S', FlagOp::Add).unwrap();
        assert_eq!(new_flags, "S");
        assert_eq!(new_path, root.join("cur").join("1779.M0P1.host:2,S"));
        assert!(new_path.exists());
        assert!(!src.exists());
    }

    #[test]
    fn set_flag_end_to_end_cur_add_to_existing_suffix() {
        let tmp = TempDir::new().unwrap();
        let root = folder(&tmp, "INBOX");
        let src = deliver_cur(&root, "1779.M0P1.host:2,R");

        let (new_path, new_flags) = set_flag(&src, 'S', FlagOp::Add).unwrap();
        assert_eq!(new_flags, "RS");
        assert_eq!(new_path, root.join("cur").join("1779.M0P1.host:2,RS"));
        assert!(new_path.exists());
        assert!(!src.exists());
    }

    #[test]
    fn set_flag_idempotent_when_already_set() {
        let tmp = TempDir::new().unwrap();
        let root = folder(&tmp, "INBOX");
        let src = deliver_cur(&root, "1779.M0P1.host:2,S");

        let (new_path, new_flags) = set_flag(&src, 'S', FlagOp::Add).unwrap();
        assert_eq!(new_flags, "S");
        assert_eq!(new_path, src);
        assert!(new_path.exists());
    }

    #[test]
    fn set_flag_toggle_clears_seen() {
        let tmp = TempDir::new().unwrap();
        let root = folder(&tmp, "INBOX");
        let src = deliver_cur(&root, "1779.M0P1.host:2,S");

        let (new_path, new_flags) = set_flag(&src, 'S', FlagOp::Toggle).unwrap();
        assert_eq!(new_flags, "");
        assert_eq!(new_path, root.join("cur").join("1779.M0P1.host"));
        assert!(new_path.exists());
        assert!(!src.exists());
    }

    #[test]
    fn set_flag_target_exists_returns_err() {
        let tmp = TempDir::new().unwrap();
        let root = folder(&tmp, "INBOX");
        let src = deliver_new(&root, "1779.M0P1.host");
        fs::write(root.join("cur").join("1779.M0P1.host:2,S"), b"squat").unwrap();

        let err = set_flag(&src, 'S', FlagOp::Add).unwrap_err();
        assert!(matches!(err, FlagError::TargetExists(_)), "got {err:?}");
        assert!(src.exists(), "source must be untouched on TargetExists");
    }

    #[test]
    fn set_flag_source_gone_returns_err() {
        let tmp = TempDir::new().unwrap();
        let root = folder(&tmp, "INBOX");
        let ghost = root.join("new").join("never-was");

        let err = set_flag(&ghost, 'S', FlagOp::Add).unwrap_err();
        assert!(matches!(err, FlagError::SourceGone(_)), "got {err:?}");
    }

    #[test]
    fn set_flag_bad_layout_when_parent_is_not_cur_or_new() {
        let tmp = TempDir::new().unwrap();
        let weird = tmp.path().join("Drafts");
        fs::create_dir_all(&weird).unwrap();
        let path = weird.join("orphan");
        fs::write(&path, b"x").unwrap();

        let err = set_flag(&path, 'S', FlagOp::Add).unwrap_err();
        assert!(matches!(err, FlagError::BadLayout(_)), "got {err:?}");
    }

    #[test]
    fn move_to_folder_cross_folder() {
        let tmp = TempDir::new().unwrap();
        let inbox = folder(&tmp, "INBOX");
        let archive = folder(&tmp, ".Archive");
        let src = deliver_cur(&inbox, "1779.M0P1.host:2,RS");

        let new = move_to_folder(&src, &archive.join("cur"), "RS").unwrap();
        assert_eq!(new, archive.join("cur").join("1779.M0P1.host:2,RS"));
        assert!(new.exists());
        assert!(!src.exists());
    }

    #[test]
    fn move_strips_mbsync_uid_marker() {
        // A cross-folder move must drop mbsync's `,U=<uid>` marker so the
        // moved file doesn't collide with the destination folder's UID
        // space (which surfaced as `Maildir error: duplicate UID N`).
        let tmp = TempDir::new().unwrap();
        let inbox = folder(&tmp, "INBOX");
        let archive = folder(&tmp, ".Archive");
        let src = deliver_cur(&inbox, "1779903914.2367185_6557.terra,U=4:2,S");

        let new = move_to_folder(&src, &archive.join("cur"), "S").unwrap();
        assert_eq!(
            new,
            archive
                .join("cur")
                .join("1779903914.2367185_6557.terra:2,S"),
            "the ,U=4 marker must be gone from the destination name"
        );
        assert!(new.exists());
        assert!(!src.exists());

        // A same-folder flag flip keeps the marker (mbsync tracks the
        // message by it).
        let same = rename_for_flags(&src, &inbox.join("cur"), "S");
        assert_eq!(
            same,
            inbox
                .join("cur")
                .join("1779903914.2367185_6557.terra,U=4:2,S")
        );
    }

    #[test]
    fn strip_mbsync_uid_preserves_other_fields() {
        assert_eq!(strip_mbsync_uid("a.b.host,U=42"), "a.b.host");
        assert_eq!(strip_mbsync_uid("a.b.host,U=42,S=900"), "a.b.host,S=900");
        assert_eq!(strip_mbsync_uid("a.b.host"), "a.b.host");
    }

    #[test]
    fn ensure_maildir_creates_cur_new_tmp() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join(".Archive");
        ensure_maildir(&root).unwrap();
        assert!(root.join("cur").is_dir());
        assert!(root.join("new").is_dir());
        assert!(root.join("tmp").is_dir());
    }

    #[test]
    fn set_flag_recorded_registers_both_paths_on_success() {
        let tmp = TempDir::new().unwrap();
        let root = folder(&tmp, "INBOX");
        let src = deliver_cur(&root, "1779.M0P1.host:2,R");
        let sw = SelfWrites::new();

        let (new_path, new_flags) = set_flag_recorded(&src, 'S', FlagOp::Add, &sw).unwrap();
        assert_eq!(new_flags, "RS");
        // Both src and dst recorded so the watcher swallows the
        // MOVED_FROM + MOVED_TO pair.
        assert!(sw.consume(&src));
        assert!(sw.consume(&new_path));
    }

    #[test]
    fn set_flag_recorded_consumes_on_rename_failure() {
        let tmp = TempDir::new().unwrap();
        let root = folder(&tmp, "INBOX");
        let src = deliver_new(&root, "1779.M0P1.host");
        std::fs::write(root.join("cur").join("1779.M0P1.host:2,S"), b"squat").unwrap();
        let sw = SelfWrites::new();

        let expected_dst = root.join("cur").join("1779.M0P1.host:2,S");
        let err = set_flag_recorded(&src, 'S', FlagOp::Add, &sw).unwrap_err();
        assert!(matches!(err, FlagError::TargetExists(_)));
        // Both registrations cleaned up so a later genuine event isn't lost.
        assert!(!sw.consume(&src));
        assert!(!sw.consume(&expected_dst));
    }

    #[test]
    fn move_to_folder_recorded_registers_both_paths_on_success() {
        let tmp = TempDir::new().unwrap();
        let inbox = folder(&tmp, "INBOX");
        let archive = folder(&tmp, ".Archive");
        let src = deliver_cur(&inbox, "1779.M0P1.host:2,RS");
        let sw = SelfWrites::new();

        let new = move_to_folder_recorded(&src, &archive.join("cur"), "RS", &sw).unwrap();
        assert!(sw.consume(&src));
        assert!(sw.consume(&new));
    }

    #[test]
    fn ensure_maildir_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join(".Archive");
        ensure_maildir(&root).unwrap();
        ensure_maildir(&root).unwrap();
        assert!(root.join("cur").is_dir());
    }
}
