# Project Bootstrap: `epost` --- a Linux maildir email reader/composer

## What we are building

A **TUI** email client, Linux-only, written in Rust. It lives in the terminal,
treats HTML mail as a first-class citizen (not "pipe to w3m and good luck
finding the links"), and falls back to the system browser for the small set of
messages where a cell-grid rendering would lose important information.

- **UI shell:** `ratatui` on the `crossterm` backend. Our process owns no
  graphics pipeline of its own --- we write cells (and image-protocol
  payloads) to the terminal, which handles the actual pixel pushing.
  Modern terminals (kitty, ghostty, WezTerm, Alacritty, foot) GPU-composite
  those cells and can upload kitty-graphics image data straight to GPU
  textures; that acceleration is real, it's just owned by the terminal, not
  us. Fully keyboard-driven, vim-style modal input.
- **Window layout:** four-region pane structure --- top account/status bar,
  left folder sidebar, message list above the reader on the right, bottom
  status/cmdline. Side/list/reader panes are toggleable. See *Window layout*
  below.
- **HTML body:** parsed with `html5ever`, walked into a Block-IR
  (paragraphs, headings, lists, tables, blockquotes, inline runs, links,
  images), and rendered into a ratatui cell buffer + a numbered **link
  table** + a list of **inline image draws**. The rendering is
  *approximate-fidelity by design* --- CSS layout collapses to a cell grid,
  fonts are whatever the terminal uses, but no important information is
  silently lost: links surface in a Vimium-style picker (`f`), inline images
  render via the terminal's image protocol, and an `:open` /
  `<leader>o` escape hatch pipes the message HTML to `xdg-open` (or any
  configured command) for the messages where the cell rendering isn't good
  enough.
- **Inline images:** `ratatui-image` handles capability detection and
  emission (kitty graphics protocol, iTerm2 inline images, sixel,
  half-block fallback). `cid:` parts come from the parsed MIME tree;
  remote (`http(s)://`) images are never fetched.
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

1. **Single TUI render pipeline.** ratatui owns the terminal back-buffer
   end-to-end. The HTML body is rendered into a cell buffer with optional
   inline-image draws sequenced into the same frame. There is no overlay
   compositing, no separate focus target, no embedded child widget. Keyboard
   input is read by `crossterm` and routed by our modal keymap.
2. **Everything keys on `Message-ID`, never on file path.** `mbsync` rewrites
   maildir paths and the `:2,<flags>` info suffix on every sync, so paths go
   stale constantly. Row identity is `(Message-ID, account, folder)` --- *not*
   `Message-ID` alone, because one Message-ID legitimately exists in several
   places at once (Gmail copies a message into both `Inbox` and `[Gmail]/All
   Mail`; the same list mail is delivered to two accounts). Keying on
   `Message-ID` alone made a scan of one folder re-key or clobber the row in
   another, and made a move resolve to the wrong copy. The index upserts by that
   triple and updates `path`/`flags` on rescan; a cross-folder move is
   delete-then-insert (the destination is a distinct row).
3. **The SQLite index is a cache.** Maildir is truth. A corrupt or deleted index
   is never a data-loss event --- full rescan on startup is the backstop.
4. **Remote content is never fetched.** The HTML walker only resolves `cid:`
   parts against the current message's MIME tree and `data:` URIs inline.
   `http(s)://` images render as a `[remote image: alt]` placeholder; we do
   not provide a "load images" toggle. The escape hatch for messages where
   you really want the rendered remote content is `:open` --- the entire
   message HTML (with `cid:` references rewritten to disk-backed temp files)
   is handed to the system browser via the configured command.
5. **No HTML engine that can execute code.** `html5ever` is a parser. The Block-IR
   walker emits styled cells and image draws --- nothing runs scripts, fetches
   stylesheets, evaluates expressions, or follows `<meta http-equiv="refresh">`.
   This is structural, not a flag to turn off.
6. **When we change a flag ourselves** (e.g. mark read → add `S` to the info
   suffix), we rename the file per the maildir spec, then update the row
   directly. Debounce the file watcher so we don't rescan on our own writes.
7. **Configuration is declarative and read-only.** A single TOML file is the
   source of truth for accounts, pane defaults, keybinds, browser-fallback
   command, and image-protocol overrides. **The app never writes it.** Runtime
   `:set ...` commands are strictly ephemeral --- session-only --- because the
   workflow we're designing for is a NixOS / Home Manager module that generates
   the config declaratively, and a config-as-output app cannot be
   config-driven. `:reload` re-reads the file. Parsing is strict: unknown keys
   fail the load with a pointing error rather than being silently ignored.
8. **UI never blocks.** Maildir scans, compose sends, search, large-batch
   threading, and the browser-fallback spawn all run on worker threads; the
   UI thread polls `mpsc` channels each tick. We use `std::thread` + channels,
   **not** `async`/`await` --- the workload is a small number of long-running
   jobs, not many concurrent I/O operations, so threads are simpler and
   incur no runtime cost.

## Crates

- `ratatui` --- TUI widgets and layout
- `crossterm` --- terminal backend (raw mode, events, alt-screen, resize)
- `ratatui-image` --- inline image rendering with capability detection
  (kitty graphics, iTerm2, sixel, half-block fallback). It picks the right
  protocol at startup from `$TERM` + env probing; we expose an override in
  config for users who want to pin a specific protocol.
- `image` --- decode `cid:` PNG/JPEG/GIF parts ahead of feeding them to
  ratatui-image
- `html5ever` + `markup5ever_rcdom` --- HTML parsing into a DOM we can walk
- `rusqlite` (bundled feature) --- index
- `mail-parser` --- MIME parsing (headers, multipart, text/plain + text/html
  alternatives, embedded parts for `cid:` resolution)
- `mail-builder` --- building outgoing MIME
- `maildirpp` (or `maildir`) --- maildir cur/new/tmp + info flags. Supports
  both subfolder conventions, picked per account via `[accounts.<name>].layout`:
  `"verbatim"` (the default — real nested subdirectories: `Sent/`,
  `Sent/2024/`; matches mbsync's `SubFolders Verbatim`) and `"maildir++"`
  (dot-prefixed flat siblings: `.Sent`, `.Sent.2024`). The walker lives
  in `mail/layout.rs`; the index stores per-layout folder labels as
  opaque strings (no cross-layout normalization).
- `notify` --- inotify file watching to mark folders dirty
- `anyhow` + `thiserror` --- errors
- `serde` + `toml` --- config (strict-parsed)
- `directories` --- XDG path resolution (`$XDG_CONFIG_HOME`, `$XDG_CACHE_HOME`)
- `clap` --- CLI (`--config <path>`)

Explicitly **not** depended on (ripped out during the GUI → TUI pivot):
`eframe`, `egui`, `egui-wgpu`, `wgpu`, `vello`, `anyrender`,
`anyrender_vello`, `blitz-dom`, `blitz-html`, `blitz-paint`, `blitz-traits`.
The pre-alpha pin warning that used to live here is gone with them.

## Module layout

```
src/
  main.rs          // crossterm raw-mode setup, event loop, terminal restore
  config.rs        // accounts -> maildir paths, keybinds, msmtp cmd,
                   // browser-fallback cmd, image-protocol override (TOML)
  store/
    scan.rs        // walk maildirs, parse headers, upsert by msgid
    index.rs       // rusqlite schema + queries (unified inbox, thread fetch)
    thread.rs      // jwz threading over references / in-reply-to
    watch.rs       // notify (inotify) -> mark folders dirty, debounced
  mail/
    parse.rs       // mail-parser wrappers: headers, body alternatives, cid parts
    flags.rs       // maildir info flags <-> internal state, spec-correct renames
    compose.rs     // build MIME via mail-builder, pipe to `msmtp -t` stdin
    html.rs        // html5ever parse + walk into Block-IR
                   // (BlockIR = tree of Paragraph/Heading/List/Table/
                   //  Quote/InlineRun/Link/Image nodes)
  ui/
    app.rs         // top-level state: pane layout/focus/visibility, mode, selection
    keys.rs        // modal keymap: Normal / Command / Search / Insert / Reader
    accounts.rs    // top bar: account selector / sync state / global status
    folders.rs     // left sidebar: folder tree
    list.rs        // message/thread list pane
    reader.rs      // Block-IR -> cells; link picker; inline image draws;
                   // scroll; :open browser fallback
    images.rs      // thin wrapper over ratatui-image for cid:/data: parts
    browser.rs     // xdg-open fallback: write cid-rewritten HTML to tempdir,
                   // spawn user-configured command on a worker thread
    cmdline.rs     // ":" command parser, "/" search, status line
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
sidebar = true       # folder pane visible at startup
list    = true       # message list visible
reader  = true       # reader visible

[smtp]
command = ["msmtp", "-t"]   # default; per-account overrides take precedence

[sync]
# Optional. If set, `:sync` (and any keybind that invokes it) runs this
# command on a worker thread. If omitted, `:sync` is a no-op --- assume
# the user drives mbsync externally (systemd-user timer, goimapnotify, ...).
# command = ["mbsync", "-a"]
# command = ["systemctl", "--user", "start", "mbsync.service"]

[reader]
prefer = "html"               # "html" | "plain"
# Browser fallback for ":open". Receives the rewritten HTML file path as
# the final argument. Override to e.g. firefox / qutebrowser / lynx.
browser = ["xdg-open"]
# Reader yanks (`Y`/`yy` line, `yip`/`yap` paragraph, `yie`/`yae` whole
# body, `yl` link) emit OSC 52 to the host terminal by
# default. Set `clipboard` to a command vec to pipe the selected text
# to that command's stdin instead — for tmux setups or terminals where
# OSC 52 is disabled. E.g. `["wl-copy"]` or `["xclip", "-selection",
# "clipboard"]`. OSC 52 is suppressed when this is set; the two paths
# are exclusive to avoid double-paste.
# clipboard = ["wl-copy"]
# Mouse-drag selection in the reader pane. Default true: press anchors
# visual-char mode, drag extends, release yanks (OSC 52 or fallback);
# scroll-wheel scrolls the reader. Disable to keep the terminal's
# native drag-select and middle-click paste over the app's panes.
# mouse = false

[images]
# Auto-detect by default. Override with one of:
#   "kitty" | "iterm" | "sixel" | "halfblocks" | "off"
# "off" disables inline images entirely; everything renders as
# `[image: alt]` placeholders.
protocol = "auto"
max_height_cells = 24   # cap image height; preserves aspect ratio

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
"<Tab>"   = "pane-next"
"j"       = "select-next"
"k"       = "select-prev"
"<Enter>" = "focus-reader"

[keys.reader]
"j"        = "scroll-down 1"
"k"        = "scroll-up 1"
"<C-d>"    = "scroll-down 10"
"<C-u>"    = "scroll-up 10"
"f"        = "link-pick"          # Vimium-style: numbers each link, type to follow
"<Enter>"  = "link-follow"        # follow the currently-hovered link
"o"        = "open-browser"       # pipe message to [reader].browser
"Y"        = "yank-line"          # current line (vim `Y` == `yy`)
"yy"       = "yank-line"          # same as Y
"yip"      = "yank-inner-paragraph" # top-level block at the reader cursor
"yap"      = "yank-a-paragraph"     # same block, plus a trailing newline
"yie"      = "yank-entire"        # whole body (vim-textobj-entire: inner entire)
"yae"      = "yank-entire"        # whole body, plus a trailing newline
"yl"       = "yank-link"          # first link at or after the reader cursor
"w"        = "word-forward"       # next word start ("W" = WORD, whitespace-delimited)
"b"        = "word-back"          # prev word start ("B" = WORD)
"e"        = "word-end"           # next word end   ("E" = WORD)
"v"        = "visual-char"        # enter char-wise visual selection
"V"        = "visual-line"        # enter line-wise visual selection
"<C-v>"    = "visual-block"       # enter block-wise (rectangular) visual selection
"<Esc>"    = "pane-focus list"

[keys.visual]
"j"        = "extend-down 1"      # cursor follows; scroll follows cursor
"k"        = "extend-up 1"
"h"        = "extend-left 1"      # char-wise only; line-wise highlights whole lines
"l"        = "extend-right 1"
"gg"       = "extend-to-top"
"G"        = "extend-to-bottom"
"0"        = "extend-to-line-start"
"$"        = "extend-to-line-end"
"w"        = "extend-word-forward" # b / e and the W/B/E WORD variants too
"y"        = "yank-selection"     # copy then exit to Normal
"v"        = "exit-or-swap-char"  # same kind exits; different kind swaps
"V"        = "exit-or-swap-line"
"<C-v>"    = "exit-or-swap-block" # same kind exits; different kind swaps to block
"<Esc>"    = "exit-visual"

[keys.mouse]
# Reader-pane only. Press anchors at the cell, drag promotes the gesture
# to a visual-char selection and extends, release yanks and exits to
# Normal. A plain click (Down/Up at the same cell) leaves the cursor at
# the click and stays in Normal — no yank, no visual flash. Wheel events
# over the reader scroll the body by 3 lines per notch. Disable with
# `[reader].mouse = false` (default true) to keep the terminal's native
# drag-select and middle-click-paste.
"<LeftPress>"   = "mouse-anchor"
"<LeftDrag>"    = "mouse-extend"
"<LeftRelease>" = "mouse-yank"
"<ScrollUp>"    = "scroll-up 3"
"<ScrollDown>"  = "scroll-down 3"
```

Schema lives in `src/config.rs` as plain `serde::Deserialize` structs with
`#[serde(deny_unknown_fields)]`; defaults via `#[serde(default)]` + `Default`
impls per struct. `~` expansion on path fields is applied at parse time.

## Index schema (minimal)

```sql
CREATE TABLE IF NOT EXISTS msg (
  msgid     TEXT NOT NULL,
  account   TEXT NOT NULL,
  folder    TEXT NOT NULL,
  path      TEXT NOT NULL,      -- current maildir path; mutable, never a key
  date      INTEGER NOT NULL,   -- unix seconds
  from_addr TEXT,
  subject   TEXT,
  in_reply  TEXT,               -- In-Reply-To msgid
  refs      TEXT,               -- space-joined References msgids
  flags     TEXT,               -- maildir info flags, mirrored from suffix
  -- One Message-ID can live in several folders/accounts at once (Gmail
  -- Inbox + All Mail; the same list mail in two accounts), so identity
  -- is the triple, not msgid alone.
  PRIMARY KEY (msgid, account, folder)
);
CREATE INDEX IF NOT EXISTS idx_folder_date ON msg(folder, date);
CREATE INDEX IF NOT EXISTS idx_account ON msg(account);
```

- **Unified inbox:** `SELECT * FROM msg WHERE folder='inbox' ORDER BY date DESC`
- **Threading:** pull candidate set, run jwz over `in_reply`/`refs`, render as a
  tree.

## Window layout

ratatui composes regions with its `Layout` constraint system. Four primary
regions plus a single-row status/cmdline at the bottom:

```
+--------------------------------------------------+
| account · folder · sync state          mode/keys |   top bar (1 row)
+---------+----------------------------------------+
|         | message list                           |
|         |                                        |
| folders +----------------------------------------+
| (tree)  | reader  (html cells + inline images)   |
|         |                                        |
|         |                                        |
+---------+----------------------------------------+
| :command / search / status                       |   cmdline (1 row)
+--------------------------------------------------+
```

- **Top bar:** current account, current folder, sync indicator, current mode
  (`NORMAL` / `READER` / `COMMAND` / etc.). Always visible.
- **Folder sidebar:** the folder tree under the currently-selected account
  (or "all" for the unified inbox view). Resizable, toggleable.
- **Message list:** threaded view; current selection drives the reader.
  Toggleable.
- **Reader:** Block-IR rendered into cells; inline images draw on top via
  ratatui-image's stateful protocol (kitty/iTerm/sixel/halfblocks). Toggleable
  --- hiding the list expands the reader vertically.
- **Cmdline / status:** single row. `:` enters Command mode, `/` enters Search.
  Otherwise displays transient status (sync result, error from compose,
  "loading inline image", etc.).

### Pane focus

Normal mode tracks which pane "has" navigation focus: `Folders`, `List`, or
`Reader`. `j`/`k` navigate within the focused pane:

- `Folders` focused → move folder selection.
- `List` focused → move message/thread selection.
- `Reader` focused → enter reader sub-mode (`l`/`Enter` to enter); `j`/`k`
  scroll the rendered cells; `Esc` returns focus to the list. `f` enters the
  link picker (Vimium-style): every link gets a number, type the number to
  follow.

Pane-switch keys: `Tab`/`Shift-Tab` for ring rotation, plus vim-window-style
`Ctrl-w h/j/k/l` for direct moves. Hidden panes reclaim their space; the next
reader render picks up the new pane size.

## The HTML rendering model

Email HTML is rendered to terminal cells. The fidelity goal is **readable, not
faithful**: layout collapses to one column at the pane's width, fonts are
whatever the terminal uses, link/image markers are explicit (not silently
hidden), but the structural intent of the message --- paragraphs, headings,
lists, tables, blockquotes, emphasis, links, images --- is preserved.

### Pipeline

1. **Parse.** `mail-parser` gives us the message's `text/html` part (or
   `text/plain` if no HTML alternative exists) plus a list of MIME parts
   keyed by `Content-ID` for `cid:` resolution.
2. **HTML to Block-IR.** `mail/html.rs` parses the HTML with `html5ever` and
   walks the DOM into a Block-IR:

   ```rust
   pub enum Block {
       Paragraph(Vec<Inline>),
       Heading { level: u8, text: Vec<Inline> },
       List { ordered: bool, items: Vec<Vec<Block>> },
       Quote(Vec<Block>),
       Table { rows: Vec<Vec<Vec<Inline>>> },
       Pre(String),
       HRule,
       Image { cid: Option<String>, src: Option<String>, alt: String },
   }
   pub enum Inline {
       Text { content: String, style: Style },    // bold/italic/underline/code
       Link { href: String, runs: Vec<Inline> },
       LineBreak,
   }
   ```

   Unknown / unsupported tags collapse to their children. Scripts, styles,
   metadata, hidden tracking blocks are dropped.

3. **Layout.** `ui/reader.rs` walks Block-IR for the current pane width and
   emits ratatui `Line`s + a **link table** (`Vec<(LinkId, Rect, String)>` ---
   id, on-screen rect, href) + a list of **image draws** (`Vec<(Rect,
   ImageRef)>`). The walker handles wrapping, list indentation, table column
   sizing (truncate-with-ellipsis on overflow), and quote-prefix gutters
   (`> `).
4. **Render.** ratatui draws the cells; then for each pending image draw,
   ratatui-image renders the inline image via the detected protocol into
   the image's rect. Stateful image protocols (kitty/iTerm) preserve their
   placement across scroll redraws.
5. **Interact.** Link picker: keypress `f` overlays each link's id (e.g. `12`)
   as a tiny inverse-video tag; typed digits select. `<Enter>` on a selected
   link opens it via `[reader].browser` (a `<leader>` for `mailto:` links
   that calls the compose flow with To: pre-filled --- TBD). `:open` writes
   the message's HTML to a temp file (with `cid:` references rewritten to
   point at extracted parts), spawns the browser command on a worker
   thread, doesn't wait.

### Privacy stance

- `cid:` → resolved against the message's MIME parts.
- `data:` → decoded inline (small attachments only; oversized `data:` images
  collapse to a placeholder).
- `http(s)://` → never fetched. Always rendered as
  `[remote image: <alt or url>]`.
- `file:` / anything else → placeholder.

The "load images" toggle from the GUI design is gone. If a user really wants
the live remote content, they hit `:open` and view the message in their
browser, which is honest about the privacy trade-off (their browser fetches
it; the mail client never does).

### Inline images

`ratatui-image` does the heavy lifting:

- At startup we construct a `Picker` that probes for protocol support
  (kitty graphics, iTerm2, sixel, halfblocks). The `[images]` config overrides
  the detected choice.
- For each renderable image (cid- or data-resolved bytes), we decode with
  `image`, hand it to `Picker` to produce a `StatefulProtocol`, and store it
  alongside the Block::Image.
- During render, ratatui-image's stateful widget paints the image into the
  reader pane at the cell rect we hand it. Subsequent frames re-use the
  same `StatefulProtocol` so kitty/iTerm don't redraw the image on every
  scroll tick (their protocols are placement-aware).

## Compose → send

Build the MIME with `mail-builder`, then pipe the raw message to `msmtp -t` over
stdin (`-t` makes msmtp read recipients from the To/Cc/Bcc headers). On success,
write a copy into the account's `Sent/cur` with the `S` flag so it appears in
the index on next scan. The compose UI itself shells out to `$EDITOR` (or
`[compose].editor` in config) for the body --- consistent with how aerc /
neomutt work and keeps us from re-implementing a text editor.

## Suggested build order

1. **ratatui scaffold in the four-pane layout.** Raw-mode setup,
   alt-screen, terminal-restore on panic. Top bar / folder sidebar / list /
   reader / cmdline regions all drawn with placeholder content. Modal
   keymap stub (Normal + Reader; Command/Search/Insert come later). `Tab` /
   `Shift-Tab` cycles pane focus; `l`/`Enter` enters reader sub-mode;
   `j`/`k` scroll a static blob of cells in the reader; `:q` quits. **This
   replaces the egui+Blitz step 1 from the GUI design** --- the load-bearing
   render-into-host-owned-wgpu-texture risk is gone with the pivot, and
   this step is mostly mechanical.
2. **Maildir scan → SQLite → threaded list.** Walk maildirs, parse headers
   with `mail-parser`, upsert by msgid, render unified inbox + threads in the
   list pane.
3. **HTML rendering.** `mail/html.rs` Block-IR walker, `ui/reader.rs` layout
   into cells + link table, Vimium-style link picker, `:open` browser
   fallback. `cid:` resolved against MIME parts; `data:` decoded inline; no
   remote fetches.
4. **Inline images.** Wire `ratatui-image` into the renderer for resolved
   image bytes. Capability detection at startup; `[images].protocol`
   override. Honor `max_height_cells`.
5. **Flags.** Spec-correct maildir renames on read/flag/delete, mirrored into
   the index, with self-write debouncing on the watcher.
6. **Compose path.** `mail-builder` → `msmtp -t`, plus the `Sent/cur` copy.
   `$EDITOR` shell-out for the body.
7. **`notify` watcher.** Per-folder dirty marking + rescan, full rescan on
   startup as backstop.

## Development environment

The repo is its own dev environment --- no host setup beyond a working
`direnv` + `devenv` install. `direnv allow` once and you have the pinned
Rust toolchain (stable, with `rust-analyzer`/`clippy`/`rustfmt`), the `mold`
linker (via `languages.rust.mold.enable`), `bacon`, `go-task`, `mbsync`,
`msmtp`, and `cargo-insta` on `$PATH`. With the pivot away from GPU
rendering, the Vulkan / Wayland / X11 / fontconfig / freetype runtime libs
and the pkg-config build deps from the GUI era have been dropped.

### Files

- **`devenv.nix`** --- packages, Rust toolchain, git pre-commit hooks
  (clippy + rustfmt). Pinned via `devenv.lock`.
- **`Taskfile.yml`** --- canonical command surface (see below).
- **`dev/config.toml`** --- self-contained TOML pointing at the fixture
  maildir and the stub msmtp. Selected by `--config dev/config.toml`.
- **`dev/maildir/`** --- in-tree synthetic maildir (maildir++ layout). See
  `dev/maildir/README.md` for the case list we want covered.
- **`dev/msmtp-stub`** --- shell script that writes stdin to
  `/tmp/epost-sent/<timestamp>.eml` instead of actually sending. Wired up
  by `dev/config.toml`'s `[smtp].command`.
- **`dev/fixtures/`** --- hand-crafted HTML and image fixtures for the
  rendering tests in step 1 and onward.

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
| `task snap:review` / `task snap:accept`| `cargo insta` for renderer golden snapshots.                 |
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

TUI iteration is much faster than the GUI iteration the original design
assumed. With `mold` + incremental compilation, small changes rebuild in
~2-5 s; bacon restarts the binary into a fresh raw-mode session. There's no
window state to lose. The snapshot-test harness (`cargo insta`) is the
right tool for renderer regressions --- a golden cell buffer per HTML
fixture lets us catch layout drift without needing a window open.

## Terminal capability assumptions

- **Cursor / styling:** SGR + 256-color / truecolor as detected by ratatui.
- **Inline images:** kitty graphics protocol, iTerm2 inline images, sixel,
  or halfblocks (256-color block characters) --- ratatui-image picks one
  automatically; user can override or disable.
- **Hyperlinks:** links render underlined/blue and are opened via the
  Vimium-style picker (`f`) or `:open`. Terminal-native OSC 8 hyperlinks are
  also emitted (opt-in `[reader].osc8_links`, default on) by folding each
  anchor into the link's first cell; the cursor's own line is left
  un-anchored so motions stay clean. The fold works around ratatui counting
  the embedded URL toward the cell's display width — see
  `emit_osc8_hyperlinks`.
- **Resize:** crossterm `Resize` events trigger a re-layout of the reader
  cells and re-emission of inline image draws at the new geometry.

## Out of scope for v1 (do not build)

IMAP, SMTP, OAuth, notmuch integration, GUI mode (we pivoted away from
eframe/Blitz/wgpu and are not going back without a clear reason), encryption
/ signing, address book. JavaScript execution is structurally out of scope
--- no HTML engine in the dep tree can execute code. Text selection inside
the reader (terminal selection still works through the user's terminal,
mouse-driven; we don't intercept).
