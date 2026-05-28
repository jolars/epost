# Project Bootstrap: `epost` --- a Linux maildir email reader/composer

## What we are building

A GUI email client, Linux-only, written in Rust.

- **UI shell:** `egui`/`eframe` on the **wgpu** backend --- immediate-mode,
  fully keyboard-driven, vim-style modal input.
- **Window layout:** four-region panel structure --- top account bar, left
  folder sidebar, message list above the reader on the right. Side/top panels
  are toggleable; the reader fills the remaining width below the list. See
  *Window layout* below.
- **HTML body:** rendered by **Blitz** (`blitz-dom` + `blitz-html` +
  `blitz-paint`) into a wgpu texture that egui composites as an `Image` in the
  body-pane rect. Blitz uses Stylo (Servo's CSS engine) for real CSS, runs
  in-process, has **no JS engine at all**, and is *not* a native child widget
  --- so there is no overlay, no separate window, and no focus-stealing problem.
- **Storage:** real maildirs on disk are the source of truth. A SQLite index
  (via `rusqlite`) is a **disposable cache** for the unified inbox, search, and
  threading. It can be deleted and rebuilt by rescanning at any time.
- **Transport:** none of our own. We read what `mbsync` wrote; we send by
  piping a built MIME message to `msmtp -t` over stdin. Periodic syncing is
  the user's job (systemd timer, `goimapnotify`, cron --- whatever they
  already run); the app only exposes a `:sync` command bindable to a key,
  which shells out to a user-defined command (often `mbsync -a`, or
  `systemctl --user start mbsync.service` to defer to an existing unit). No
  in-app interval in v1 --- drive periodic from outside to avoid fighting
  the user's own scheduler.
- **Deployment target:** NixOS / Home Manager. The whole app is intended to be
  config-driven, so a single declarative TOML (generated from Nix or
  hand-written) fully describes the user's setup. The same config on any of
  the user's Linux machines should produce the same experience.

## Hard requirements / invariants

1. **Single render pipeline.** Blitz paints into a wgpu texture sized to the
   body-pane rect; egui samples that texture as an `Image`. There is no native
   child widget, no overlay compositing, no separate focus target. Keyboard
   input is owned by the egui/GTK window unconditionally; vim keys (`j k gg G /
   :   Enter Esc\`) are interpreted at the egui layer.
2. **Everything keys on `Message-ID`, never on file path.** `mbsync` rewrites
   maildir paths and the `:2,<flags>` info suffix on every sync, so paths go
   stale constantly. The index upserts by `msgid` and updates `path`/`flags` on
   rescan.
3. **The SQLite index is a cache.** Maildir is truth. A corrupt or deleted index
   is never a data-loss event --- full rescan on startup is the backstop.
4. **Remote content blocked by default.** Implement Blitz's `NetProvider` trait.
   Deny any request whose scheme is `http`/`https` until a per-message "load
   images" flag is set; when the flag flips, drop and re-resolve the document so
   the loads retry. Resolve `cid:` URLs against in-memory MIME parts from the
   parsed message. Allow `data:`. Deny everything else (including `file:`). This
   is privacy + tracking-pixel defense, and Blitz hands you every sub-resource
   fetch through one trait method.
5. **No JavaScript, ever.** Blitz has no JS engine --- we depend on this as a
   structural property, not a flag to turn off. Email content never executes
   code.
6. **When we change a flag ourselves** (e.g. mark read → add `S` to the info
   suffix), we rename the file per the maildir spec, then update the row
   directly. Debounce the file watcher so we don't rescan on our own writes.
7. **Configuration is declarative and read-only.** A single TOML file is the
   source of truth for accounts, panel defaults, keybinds, and renderer
   settings. **The app never writes it.** Runtime `:set ...` commands are
   strictly ephemeral --- session-only --- because the workflow we're designing
   for is a NixOS / Home Manager module that generates the config
   declaratively, and a config-as-output app cannot be config-driven. `:reload`
   re-reads the file. Parsing is strict: unknown keys fail the load with a
   pointing error rather than being silently ignored.
8. **UI never blocks.** Maildir scans, compose sends, search, and large-batch
   threading run on worker threads; the UI thread polls `mpsc` channels each
   frame and calls `Context::request_repaint()` when there's something to
   show. We use `std::thread` + channels, **not** `async`/`await` --- the
   workload is a small number of long-running jobs, not many concurrent I/O
   operations, so threads are simpler and incur no runtime cost.

## Crates

- `eframe` + `egui` (**wgpu backend**, not glow) --- UI shell. We need wgpu so
  we can share the `wgpu::Device`/`Queue` with Blitz's renderer.
- `blitz-dom`, `blitz-html`, `blitz-paint` --- HTML/CSS rendering core
- `blitz-traits` --- `NetProvider` / `NavigationProvider` traits
- `anyrender_vello_hybrid` (or `anyrender_vello`) --- Blitz's wgpu backend via
  Vello
- **Do not** depend on `blitz-shell`. It pulls in winit/AccessKit/Muda and
  assumes it owns the window; we want the core crates only.
- `rusqlite` (bundled feature) --- index
- `mail-parser` --- MIME parsing (headers, multipart, text/plain + text/html
  alternatives)
- `mail-builder` --- building outgoing MIME
- `maildirpp` (or `maildir`) --- maildir cur/new/tmp + info flags; prefer the
  maildir++ subfolder convention since we have multiple folders per account
- `notify` --- inotify file watching to mark folders dirty
- `anyhow` + `thiserror` --- errors
- `serde` + `toml` --- config (strict-parsed)
- `directories` --- XDG path resolution (`$XDG_CONFIG_HOME`, `$XDG_CACHE_HOME`)

> Note to agent: Blitz is at `0.3.0-alpha.4` as of project start (May 2026),
> with 0.3 beta targeted June 2026. Pin an exact version --- APIs churn between
> alphas. Hosted docs are thin; read source first. Start in
> `packages/blitz-dom/src/config.rs` (`DocumentConfig`),
> `packages/blitz-dom/src/document.rs` (lifecycle), and
> `packages/blitz-traits/src/net.rs` (`NetProvider` trait). Useful example
> references in the Blitz repo: `examples/wgpu_texture/` (device/queue sharing
> pattern --- note it's the *inverse* of what we want, but the sharing pattern
> is the same) and `examples/screenshot.rs` (headless render-to-pixels, closest
> to our path). No upstream example currently does
> "render-into-host-owned-wgpu-texture-and-composite" --- expect to be the first
> to wire that, and to file upstream bugs on real-world HTML-email table
> layouts.

## Module layout

```
src/
  main.rs          // eframe bootstrap, event loop
  config.rs        // accounts -> maildir paths, keybinds, msmtp cmd (TOML)
  store/
    mod.rs
    scan.rs        // walk maildirs, parse headers, upsert by msgid
    index.rs       // rusqlite schema + queries (unified inbox, thread fetch)
    thread.rs      // jwz threading over references / in-reply-to
    watch.rs       // notify (inotify) -> mark folders dirty, debounced
  mail/
    parse.rs       // mail-parser wrappers: headers, body alternatives, cid parts
    flags.rs       // maildir info flags <-> internal state, spec-correct renames
    compose.rs     // build MIME via mail-builder, pipe to `msmtp -t` stdin
    net.rs         // NetProvider impl: cid: resolver, http(s) gating, data: passthrough
  ui/
    app.rs         // top-level state: panel layout/focus/visibility, mode, selection
    keys.rs        // modal keymap: Normal / Command / Search / Insert
    accounts.rs    // top panel: account selector / global status
    folders.rs     // left sidebar: folder tree
    list.rs        // message/thread list pane
    reader.rs      // body pane: plaintext view OR Blitz-rendered HTML
    body.rs        // Blitz document lifecycle, render-to-texture, viewport/scroll
    cmdline.rs     // ":" command parser, "/" search
```

## Configuration

The TOML config is the source of truth and **read-only from the app's
perspective** --- the app never writes it. The workflow we're designing for is
a NixOS / Home Manager module that generates the config declaratively and
deploys it across the user's machines; the same config on any machine should
produce the same experience.

- **Location:** `$XDG_CONFIG_HOME/epost/config.toml` (default
  `~/.config/epost/config.toml`), resolved via the `directories` crate.
- **Lifecycle:** parsed once at startup; `:reload` re-reads.
- **Strict:** unknown keys fail the load with a pointing error.
- **Ephemeral overrides:** runtime `:set ...` affects only the current session
  and is never written back.
- **The sqlite index is *not* config.** It lives under `$XDG_CACHE_HOME/epost/`
  as a rebuildable cache (see invariant 3) and is the one piece of mutable
  state the app maintains on disk.

### Schema sketch (v1)

```toml
# ~/.config/epost/config.toml

[ui]
sidebar = true       # folder panel visible at startup
list    = true       # message list visible
reader  = true       # reader (Blitz) visible

[smtp]
command = ["msmtp", "-t"]   # default; per-account overrides take precedence

[sync]
# Optional. If set, `:sync` (and any keybind that invokes it) runs this
# command on a worker thread. If omitted, `:sync` is a no-op --- assume
# the user drives mbsync externally (systemd-user timer, goimapnotify, ...).
# No in-app interval: we deliberately do not schedule sync ourselves, to
# avoid fighting an external scheduler.
# command = ["mbsync", "-a"]
# command = ["systemctl", "--user", "start", "mbsync.service"]

[reader]
default_view = "html"  # "html" | "plain"
load_images  = false   # remote-content default for new messages

# Accounts: named tables. The name ("personal", "work") is a stable id used
# in the UI and in keybindings that reference an account.

[accounts.personal]
maildir     = "~/Mail/personal"
from        = "Jane Doe <jane@example.com>"
sent_folder = "Sent"

[accounts.work]
maildir       = "~/Mail/work"
from          = "Jane Doe <jane@work.example>"
sent_folder   = "Sent Items"
smtp.command  = ["msmtp", "-t", "-a", "work"]    # per-account override

# Keybinds: nested table per mode. Keys are sequences (vim-style notation),
# values are command strings. This OVERLAYS the baked-in defaults --- you
# only specify what you want to change. To unbind a default, set "".

[keys.normal]
"<Tab>"   = "panel-next"
"j"       = "select-next"
"k"       = "select-prev"
"<Enter>" = "focus-reader"
"i"       = "toggle-images"

[keys.reader]
"j"     = "scroll-down 80"
"k"     = "scroll-up 80"
"<Esc>" = "panel-focus list"
```

Schema lives in `src/config.rs` as plain `serde::Deserialize` structs; defaults
via `#[serde(default)]` + `Default` impls per struct. `~` expansion on path
fields is applied at parse time.

## Index schema (minimal)

```sql
CREATE TABLE IF NOT EXISTS msg (
  msgid     TEXT PRIMARY KEY,
  account   TEXT NOT NULL,
  folder    TEXT NOT NULL,
  path      TEXT NOT NULL,      -- current maildir path; mutable, never a key
  date      INTEGER NOT NULL,   -- unix seconds
  from_addr TEXT,
  subject   TEXT,
  in_reply  TEXT,               -- In-Reply-To msgid
  refs      TEXT,               -- space-joined References msgids
  flags     TEXT                -- maildir info flags, mirrored from suffix
);
CREATE INDEX IF NOT EXISTS idx_folder_date ON msg(folder, date);
CREATE INDEX IF NOT EXISTS idx_account ON msg(account);
```

- **Unified inbox:** `SELECT * FROM msg WHERE folder='inbox' ORDER BY date DESC`
- **Threading:** pull candidate set, run jwz over `in_reply`/`refs`, render as a
  tree.

## Window layout

Built on egui's panel system (`TopBottomPanel` + `SidePanel` + `CentralPanel`).
Four regions:

```
+---------+-------------------+
| Account bar (full width)    |
+---------+-------------------+
|         |   Message list    |
|         |                   |
| Folders +-------------------+
|         |                   |
|         |   Reader (Blitz)  |
|         |                   |
+---------+-------------------+
```

- **Account bar (top):** `TopBottomPanel::top`. Configured accounts,
  unified-inbox toggle, global status (sync state, unread totals). Thin. Always
  visible in v1.
- **Folder sidebar (left):** `SidePanel::left`. Folder tree under the
  currently-selected account (or "all" for the unified inbox view). Resizable
  and toggleable.
- **Message list (center, top half):** the list pane, full width of the central
  region. Threaded view; current selection drives the reader. Toggleable (hiding
  it gives the reader full height --- "zen reading").
- **Reader (center, bottom half):** the Blitz-rendered body pane. The body-pane
  rect we hand to Blitz for layout is *exactly* this region. The list/reader
  split is a resizable horizontal divider inside the central panel; the reader
  is toggleable too (hiding it gives the list full height).

### Panel focus

Normal mode tracks which panel "has" navigation focus: `Folders`, `List`, or
`Reader`. `j`/`k` navigate within the focused panel:

- `Folders` focused → move folder selection.
- `List` focused → move message/thread selection.
- `Reader` focused → enter reader sub-mode (`j`/`k` translate to Blitz viewport
  scroll-offset deltas; `Esc` returns focus to the list).

Panel-switch keys move focus between panels --- planned: `Tab` / `Shift-Tab` for
ring rotation, plus vim-window-style `Ctrl-w h/j/k/l` for direct moves. egui
still owns all keyboard input unconditionally; panel "focus" is an app-level
concept that just decides where to route the keystroke.

### Visibility

Each toggleable panel is bound to both a `:` command (e.g. `:set sidebar`,
`:set list`, `:set reader`) and a keybind (TBD --- candidates: leader-chord like
`<leader>tf` for "toggle folders", or vim-fold-style `zS`/`zL`/`zR`). Hidden
panels reclaim their space; the reader rect expands accordingly and the next
Blitz repaint picks up the new viewport size.

## The Blitz rendering model (the riskiest piece --- build this first)

Blitz is **in-process** and **not** a separate widget. It parses HTML into a
`HtmlDocument`, resolves style/layout, and paints via `anyrender` into a Vello
scene; we render that scene into a wgpu texture we allocate from eframe's wgpu
device, and feed the texture to egui as an `Image`.

- **Construction:**
  `HtmlDocument::from_html(&html, DocumentConfig {   net_provider: Some(Arc::new(MailNetProvider { ... })),   style_threading: StyleThreading::Sequential, ..Default::default() })`.
  Sequential style threading is required if multiple documents may resolve
  concurrently from different threads --- relevant once we render preview
  snippets in the list pane.
- **Render path:** each frame the body pane is visible, set the document's
  viewport to the body-pane rect's size, call `doc.resolve()`, paint via
  `blitz-paint` into a `VelloScene`, render the scene into a wgpu texture we own
  (`anyrender_vello_hybrid` sharing eframe's `wgpu::Device`/`Queue`), and feed
  the texture to egui as an `Image` placed at the body-pane rect.
- **Scrolling:** reader sub-mode (`l`/`Enter` to enter, `Esc` to exit)
  translates `j`/`k` into viewport scroll-offset deltas; we re-resolve/re-paint.
  No `evaluate_script`, no focus juggling, no separate focusable widget exists.
- **Composition:** egui owns the framebuffer end-to-end; Blitz output is just
  another textured rect inside it. The body-pane rect is whatever egui says it
  is each frame --- no `set_bounds` chasing.

### Image gating

Implement `blitz_traits::net::NetProvider`. Its single `fetch` method receives
every sub-resource request with a `url::Url`:

- `cid:foo@bar` → look up the MIME part by `Content-ID` on the
  currently-rendering message, return bytes via `handler.bytes(url, ...)`.
- `data:` → allow (or delegate to `blitz_net::Provider`'s `data:` path).
- `http(s)://` → return an error unless the user has flipped "load images" for
  this message. When they flip it, drop the document and re-resolve so the
  blocked loads retry.
- Anything else (`file:`, custom schemes) → deny.

## Compose → send

Build the MIME with `mail-builder`, then pipe the raw message to `msmtp -t` over
stdin (`-t` makes msmtp read recipients from the To/Cc/Bcc headers). On success,
write a copy into the account's `Sent/cur` with the `S` flag so it appears in
the index on next scan.

## Suggested build order

1. **egui + Blitz compositing skeleton, in the real panel layout.** An eframe
   app on the wgpu backend with the four-region panel structure already in
   place: top account bar, left folder sidebar, message list above the reader in
   the central panel. All four regions are stubbed with placeholder content, but
   they are real `egui` panels so the body-pane rect is the actual reader
   sub-region. Construct a `HtmlDocument` from a hard-coded HTML string, resolve
   it against that rect, paint into a wgpu texture sharing eframe's
   device/queue, and display the texture as an `egui::Image` filling the reader
   region. Wire a stub `NetProvider` that blocks everything except a hard-coded
   `cid:` mapping pointing at a bundled image. Implement panel-focus switching
   (Tab between panels) and the reader sub-mode with `j`/`k` viewport scrolling
   and `Esc` to exit. **This validates the single biggest unknown in the
   project** --- no Blitz example currently does external-host compositing into
   a host-owned wgpu texture, and getting the rect right under egui's panel
   layout is part of that, so it is the part you actually have to figure out
   before anything else is worth building.
2. **Maildir scan → SQLite → threaded list.** Walk maildirs, parse headers with
   `mail-parser`, upsert by msgid, render unified inbox + threads in the list
   pane.
3. **Real body rendering.** Feed parsed `text/html` (or `text/plain` fallback
   wrapped in a minimal stylesheet) from the selected message into the Blitz
   pipeline from step 1. Replace the stub `NetProvider` with the real one:
   `cid:` resolver against the current message's MIME parts, per-message "load
   images" toggle for `http(s)://`.
4. **Flags.** Spec-correct maildir renames on read/flag/delete, mirrored into
   the index, with self-write debouncing on the watcher.
5. **Compose path.** `mail-builder` → `msmtp -t`, plus the `Sent/cur` copy.
6. **`notify`watcher.** Per-folder dirty marking + rescan, full rescan on
   startup as backstop.

## Development environment

The repo is its own dev environment --- no host setup beyond a working
`direnv` + `devenv` install. `direnv allow` once and you have the pinned
Rust toolchain (stable, with `rust-analyzer`/`clippy`/`rustfmt`), the `mold`
linker (via `languages.rust.mold.enable`), `bacon`, `go-task`, `mbsync`,
`msmtp`, and `cargo-insta` on `$PATH`. The graphical runtime deps for
eframe-wgpu and Blitz/Vello (Vulkan loader, libxkbcommon, Wayland/X11 libs,
fontconfig, freetype) are wired into `LD_LIBRARY_PATH` automatically.

### Files

- **`devenv.nix`** --- packages, Rust toolchain, system libs for
  eframe/Blitz, git pre-commit hooks (clippy + rustfmt). Pinned via
  `devenv.lock`.
- **`Taskfile.yml`** --- canonical command surface (see below).
- **`dev/config.toml`** --- self-contained TOML pointing at the fixture
  maildir and the stub msmtp. Selected by `--config dev/config.toml`.
- **`dev/maildir/`** --- in-tree synthetic maildir (maildir++ layout). See
  `dev/maildir/README.md` for the case list we want covered.
- **`dev/msmtp-stub`** --- shell script that writes stdin to
  `/tmp/epost-sent/<timestamp>.eml` instead of actually sending. Wired up
  by `dev/config.toml`'s `[smtp].command`.

### Tasks

| Task                                   | Action                                                       |
| -------------------------------------- | ------------------------------------------------------------ |
| `task dev`                             | `bacon`: auto-rebuild + restart on file change. Main loop.   |
| `task run`                             | One-shot `cargo run` against `dev/config.toml`.              |
| `task build` / `task build:release`    | Plain cargo build.                                           |
| `task fmt` / `task fmt:check`          | rustfmt.                                                     |
| `task lint`                            | `cargo clippy --all-targets -- -D warnings`.                 |
| `task check`                           | `cargo check --all-targets`.                                 |
| `task test`                            | `cargo test`.                                                |
| `task ci`                              | `fmt:check + lint + test` (what CI would run).               |
| `task snap:review` / `task snap:accept`| `cargo insta` for Blitz golden snapshots.                    |
| `task sent:clear`                      | Wipe `/tmp/epost-sent/`.                                     |
| `task index:reset`                     | Delete the sqlite cache to force a full rescan.              |
| `task doctor`                          | Smoke-check the devshell and fixtures.                       |

### CLI surface

The binary accepts `--config <path>` to override the default config
location. `task dev` and `task run` pass `--config dev/config.toml`; real
users on NixOS leave it off and let the binary resolve
`$XDG_CONFIG_HOME/epost/config.toml`. A planned `--open-to <msgid>` flag
will deep-link back into a specific view, useful for getting back where you
were after a bacon-driven restart --- not v1.

### Dev-loop reality

Pure Rust GUI dev has no live preview comparable to web dev. With `mold` +
incremental compilation, small changes rebuild in ~3--10 s and bacon
auto-restarts the window; you lose in-app state on every restart. Dioxus's
`subsecond` hot-patching crate (keep the window across edits, swap function
bodies in place) is an aspirational add-on once the skeleton is stable ---
not blocking on it. **WASM is explicitly out**: it would remove our access
to maildirs / `mbsync` / `msmtp` and would not actually make builds faster.

## Out of scope for v1 (do not build)

IMAP, SMTP, OAuth, notmuch integration, multi-platform support
(Linux/eframe-wgpu only; Blitz itself is portable but we don't promise anything
beyond Linux), encryption/signing, address book. JavaScript execution is
structurally out of scope --- Blitz has no JS engine and that is a property we
depend on.
