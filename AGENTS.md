# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Source of truth: DESIGN.md

`DESIGN.md` is the authoritative architecture document. **Read it before proposing or making non-trivial changes.** It pins the rendering model (TUI via `ratatui` + `crossterm`, with HTML walked through `html5ever` into a Block-IR and drawn as cells; inline images via `ratatui-image`'s kitty/iTerm/sixel/halfblock detection), the storage model (maildir is truth; sqlite is a disposable cache; everything keys on `Message-ID`, never on path), the configuration model (declarative read-only TOML), the concurrency model (`std::thread` + `mpsc` channels, **not** `async`/`await`), and the deployment target (NixOS / Home Manager, fully config-driven). The numbered *Hard requirements / invariants* are real constraints; do not silently relax them. *Suggested build order* is the v1 roadmap.

**Note on history:** this project briefly started as a GUI (eframe + Blitz + wgpu) before pivoting to the TUI design in `DESIGN.md`. The pivot cleanup is done — no GUI code remains. If you see lingering references to wgpu, `NetProvider`, Blitz, eframe, or a "four-panel egui layout" anywhere, that's stale documentation: rip it out, don't build on it.

## Project state

TUI build in progress against `DESIGN.md`'s *Suggested build order*.

- **Step 1 (ratatui four-pane scaffold)** — done. Raw-mode setup with panic-safe terminal restore, four-pane layout with focus borders, ambient-Normal modal keymap with focus-routed keys (`q` / `Tab` / `BackTab` / `l` / `Enter` / `j` / `k` / `Esc` / `Ctrl-C`), placeholder content in every pane. Reader-pane keys (`j`/`k` scroll, `f` link-pick) are dispatched off `app.focus == Reader` rather than a separate sub-mode; only `Command` (`:`) and `LinkPick` (`f` digit capture) carry their own keymaps because they capture text/digit input.
- **Step 2 (maildir scan → SQLite → threaded list)** — done. Scan worker runs on `std::thread` and reports via `mpsc`; UI polls each tick. Maildir++ walk (root `cur`+`new` for INBOX, `.Subfolder/{cur,new}` for the rest), header parse via `mail-parser` (RFC 2047 decoded), upsert into bundled SQLite keyed on `Message-ID`, JWZ-style threading, list pane shows the unified INBOX with `j`/`k` selection. CLI: `--cache <path>` overrides `$XDG_CACHE_HOME/epost/index.sqlite`; `task dev`/`task run` point at `dev/cache/index.sqlite` (gitignored).
- **Step 3 (HTML rendering)** — done. `mail/html.rs` Block-IR walker, `ui/reader.rs` cell layout + link table, Vimium-style link picker (`f` + digits + `<Enter>`), `:open` browser fallback via `[reader].browser`. `cid:` resolved against MIME parts; `data:` decoded inline; no remote fetches. `[reader]` / `[images]` config keys are spec-aligned (`prefer` / `browser` / `protocol` / `max_height_cells`).
- **Step 4 (inline images)** — done. `ratatui-image` wired with capability detection at startup, `[images].protocol` override (`auto` / `kitty` / `iterm` / `sixel` / `halfblocks` / `off`), `max_height_cells` honored. Decoded image cache per msgid (current + previous) avoids re-decoding on back-and-forth navigation.
- **Step 5 (maildir flags)** — done. `mail/flags.rs` owns spec-correct `:2,FLAGS` parsing and the rename primitive (shared with the cross-folder move feature). Auto-`S` fires once per opened body inside `ensure_body_for_selection`; in Normal mode (List focused), `m` toggles Seen, `*` toggles Flagged (`F`, surfaced as a ★ glyph in the list), `d` toggles Trashed (`T`, surfaced as dimmed/strikethrough). Index + in-memory `ThreadedRow` mirrored on every flip via the generic `App::toggle_flag_selected(char)`. `store/watch.rs` exposes a `SelfWrites` registry stub the Step 7 notify watcher will consult. Note: `T` is the maildir spec's mark-for-expunge bit; `d` keeps that semantic in place. Cross-folder Trash routing is the separate `D` / `:trash` binding (see below), not a `T`-flag side-effect.
- **Step 6 (compose path)** — done. `mail/compose.rs` builds MIME via `mail-builder`, pipes to `msmtp -t`, drops a `Sent/cur` copy. `ui/compose.rs` + `ui/embed.rs` host `$EDITOR` under a pty (vt100 capability callbacks, env passthrough, synchronized output). Cmdline: `:compose` / `:reply` / `:reply-all` / `:forward` / `:send` / `:close`.
- **Cross-folder moves** — done. `:archive` / `:spam` / `:trash` route through the per-account `archive_folder` / `spam_folder` / `trash_folder` config keys; `:mv <folder>` takes a literal Maildir++ label. Keys: `a` archives, `D` (uppercase) trashes, `d` still toggles the `T` info-flag. The target `.Folder/{cur,new,tmp}/` is created if missing. Self-writes are recorded on both source and destination paths for the future Step 7 watcher.
- **Step 7 (notify watcher)** — done. `notify`-backed inotify watcher in `store/watch.rs`, per maildir folder (account root non-recursive, plus `{cur,new}` for INBOX and each `.Sub/{cur,new}`). Events route through `SelfWrites::consume` to suppress our own renames, coalesce into per-folder dirty marks, debounce on `[watch].debounce_ms` (default 250 ms), and dispatch through `scan::rescan_folders` — disk I/O restricted to the dirty folders, list + folder stats refreshed from the index. `Index::prune_folder` reflects deletions; `apply_rescan` preserves the selected msgid across rescans (snaps to row 0 when gone). New subfolders auto-register: `:mv NewFolder` calls `Watcher::register_folder` race-free; `Create(Folder)` on an account root triggers the same path for externally-created folders. `[watch]` config: `enabled` (default true), `debounce_ms` (default 250). Watcher start failure (e.g. `fs.inotify.max_user_watches` exhausted) degrades to startup-only scan with a one-shot warning. Flag-flip + cross-folder-move callsites switched to `set_flag_recorded` / `move_to_folder_recorded` so `SelfWrites` is populated *before* the rename, closing the race the live watcher would otherwise see.
- **Multi-account UI** — done. Sidebar renders one `[all]` group (unified across accounts, today's default) plus one `[<account>]` group per configured account, alphabetic, each with that scope's folders. `InboxScreen.current_account: Option<String>` (None = `[all]`) plus the existing `current_folder` form the active scope; `Index::list_folder` / `Index::folder_stats` accept `Option<&str>` to filter. Scan workers emit `Vec<AccountFolderStats>`; in-memory stat patching writes both the `[all]` group and the owning account's group. Folder cycling (`Alt-j`/`Alt-k`, `j`/`k` in Folders pane) walks the flat list skipping non-selectable group headers. Top bar carries an `account · folder` badge. `:account <name>` / `:account all` jumps scope from cmdline. Cross-folder moves continue to route per-row via `MessageRow.account`.
- **`:sync` dispatch** — done. `store/sync.rs` spawns the configured `[sync].command` on a `std::thread`, waits for it, and reports the exit status over `mpsc`. `App.sync_rx` holds the in-flight receiver; `App::poll_sync` drains it each tick into the cmdline status row (`syncing…` → `synced` / `sync failed: …`). The worker pushes `AppEvent::Wake` on completion so the result surfaces without waiting for the idle heartbeat. A second `:sync` while one is in flight errors out instead of queueing. The maildir watcher reconciles whatever the sync wrote — this worker only reports the command itself.

Next-up work tracked in `TODO.md`.

## Commands

All commands go through `task` (go-task); `Taskfile.yml` is the canonical surface. `task --list` enumerates. Highlights:

- `task dev` — main loop. `bacon` auto-rebuilds and restarts against `dev/config.toml`.
- `task run` — one-shot run against the dev config.
- `task ci` — `fmt:check + lint + test`, what CI would run.
- `task doctor` — smoke-check the devshell and fixtures.
- `task snap:review` / `task snap:accept` — `cargo insta` for renderer golden snapshots (golden cell buffers per HTML fixture).

Prefer the tasks over raw `cargo run`: the wrappers pass `--config dev/config.toml` so the app uses the in-repo fixture maildir and the stub msmtp instead of real mail. Single-test invocation is plain `cargo test <name>`.

## Constraints that catch agents

These are easy to flagrantly violate. The full set lives in `DESIGN.md`; this is a triage list to scan before editing:

- **No `async`/`await`, no tokio.** Slow work (mbsync trigger, msmtp send, maildir scans, search, large-batch threading, the browser-fallback spawn for `:open`) runs on `std::thread` workers; results flow back through `mpsc` channels. The main loop is event-driven, not timer-driven: a dedicated input thread and every worker push into one unified `AppEvent` channel (`src/ui/events.rs`), and `main.rs` blocks on `recv_timeout` so it wakes the instant any source has something to say. The timeout is a safety-belt fallback, not the primary trigger. Adding tokio is a redesign — raise it explicitly before doing it.
- **No GUI, no webview, no JavaScript anywhere.** The pivot to TUI means `eframe` / `egui` / `wgpu` / `vello` / `blitz-*` are out. The HTML body is parsed with `html5ever` and rendered into a ratatui cell buffer. The "no HTML engine that can execute code" property is a depended-on security guarantee — `html5ever` is a parser, not an engine.
- **Remote content is never fetched.** The HTML walker resolves `cid:` against MIME parts and decodes `data:` inline; `http(s)://` images render as `[remote image: alt]` placeholders. There is no per-message "load images" toggle. The escape hatch is `:open`, which writes the message HTML to a tempfile (with `cid:` rewrites) and hands it to the user's browser via the configured command.
- **The app never writes its TOML config.** Runtime `:set` is session-only. Persistent state belongs in `$XDG_CACHE_HOME/epost/` (the sqlite index), never in config.
- **Keys on `Message-ID`, never on file path.** mbsync rewrites maildir paths and the info-flag suffix on every sync, so paths go stale constantly.
- **Approximate-fidelity HTML by design.** Cell-grid rendering of CSS will not be pixel-faithful; that is a feature, not a bug. Don't add CSS-engine deps to "improve fidelity." If a class of mail looks too broken, the answer is `:open` and the system browser — not a webview.

## Dev environment / fixtures

`devenv.nix` provides the devshell (Rust toolchain, `mold` linker, `bacon`, `mbsync`, `msmtp`, `cargo-insta`). `direnv allow` activates it.

`dev/` is the in-repo dev harness:

- `dev/config.toml` — the TOML the binary reads under `task dev` / `task run`.
- `dev/maildir/` — in-tree synthetic maildir for the `dev` account. `dev/maildir/README.md` lists the cases covered; outstanding fixture gaps are tracked in `TODO.md`.
- `dev/maildir-work/` — second-account fixture so the multi-account sidebar has real data to render. Mirrored into `dev/scratch/maildir-work/` on first run.
- `dev/msmtp-stub` — shell script wired into `[smtp].command` in dev; captures outbound mail to `/tmp/epost-sent/` instead of sending.
- `dev/fixtures/` — hand-crafted HTML and image fixtures for renderer tests. Step 1's `welcome.html` + `welcome.png` from the GUI era can be reused as the first cell-rendering target.
