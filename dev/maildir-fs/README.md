# Dev fixture maildir — fs layout

Third in-tree fixture, alongside `dev/maildir/` and `dev/maildir-work/`.
This one exercises the **fs / nested** Maildir layout (real
subdirectories, `/`-joined folder labels) as opposed to the Maildir++
dot-prefix flat encoding that the other two fixtures use. Backs
`accounts.fs` in `dev/config.toml`.

## Layout (fs / nested)

    dev/maildir-fs/
      cur/  new/  tmp/         # INBOX (just like maildir++)
      Sent/
        cur/  new/  tmp/       # folder label: "Sent"
      Archive/
        cur/  new/  tmp/       # folder label: "Archive"
        2024/
          cur/  new/  tmp/     # folder label: "Archive/2024"

A directory is a folder iff it contains `cur/`. Walker recurses into
non-folder containers too, so a pure-container dir with folder children
is fine. `cur/`, `new/`, `tmp/`, and any dot-prefixed dir name are
reserved at every depth.

## Cases covered

- [x] INBOX file at the root (one message in `cur/`)
- [x] First-level sub-folder (`Sent/`, `Archive/`)
- [x] Nested sub-folder with `/`-joined label (`Archive/2024/`)

## Cases worth adding later

- [ ] Nested sub-folder created at runtime (verifies the fs watcher's
      re-discovery path under `Create(Folder)` on a folder root, not just
      the account root)
- [ ] Empty container dir with no `cur/` but folder children (verifies
      the walker descends through pure containers)
