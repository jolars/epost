# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Source of truth: DESIGN.md

`DESIGN.md` is the authoritative architecture document. **Read it before proposing or making non-trivial changes.** It pins the rendering model (TUI via `ratatui` + `crossterm`, with HTML walked through `html5ever` into a Block-IR and drawn as cells; inline images via `ratatui-image`'s kitty/iTerm/sixel/halfblock detection), the storage model (maildir is truth; sqlite is a disposable cache; everything keys on `Message-ID`, never on path), the configuration model (declarative read-only TOML), the concurrency model (`std::thread` + `mpsc` channels, **not** `async`/`await`), and the deployment target (NixOS / Home Manager, fully config-driven). The numbered *Hard requirements / invariants* are real constraints; do not silently relax them. *Suggested build order* is the v1 roadmap.

**Note on history:** this project briefly started as a GUI (eframe + Blitz + wgpu) before pivoting to the TUI design in `DESIGN.md`. The pivot cleanup is done — no GUI code remains. If you see lingering references to wgpu, `NetProvider`, Blitz, eframe, or a "four-panel egui layout" anywhere, that's stale documentation: rip it out, don't build on it.

## Project state

TUI build in progress against `DESIGN.md`'s *Suggested build order*.

- **Step 1 (ratatui four-pane scaffold)** — done. Raw-mode setup with panic-safe terminal restore, four-pane layout with focus borders, Normal/Reader modal keymap (`q` / `Tab` / `BackTab` / `l` / `Enter` / `j` / `k` / `Esc` / `Ctrl-C`), placeholder content in every pane.
- **Step 2 (maildir scan → SQLite → threaded list)** — done. Scan worker runs on `std::thread` and reports via `mpsc`; UI polls each tick. Maildir++ walk (root `cur`+`new` for INBOX, `.Subfolder/{cur,new}` for the rest), header parse via `mail-parser` (RFC 2047 decoded), upsert into bundled SQLite keyed on `Message-ID`, JWZ-style threading, list pane shows the unified INBOX with `j`/`k` selection. CLI: `--cache <path>` overrides `$XDG_CACHE_HOME/epost/index.sqlite`; `task dev`/`task run` point at `dev/cache/index.sqlite` (gitignored).
- **Step 3 (HTML rendering)** — done. `mail/html.rs` Block-IR walker, `ui/reader.rs` cell layout + link table, Vimium-style link picker (`f` + digits + `<Enter>`), `:open` browser fallback via `[reader].browser`. `cid:` resolved against MIME parts; `data:` decoded inline; no remote fetches. `[reader]` / `[images]` config keys are spec-aligned (`prefer` / `browser` / `protocol` / `max_height_cells`).
- **Step 4 (inline images)** — done. `ratatui-image` wired with capability detection at startup, `[images].protocol` override (`auto` / `kitty` / `iterm` / `sixel` / `halfblocks` / `off`), `max_height_cells` honored. Decoded image cache per msgid (current + previous) avoids re-decoding on back-and-forth navigation.
- **Step 5 (maildir flags)** — done. `mail/flags.rs` owns spec-correct `:2,FLAGS` parsing and the rename primitive (shared with the future folder-move feature). Auto-`S` fires once per opened body inside `ensure_body_for_selection`; in Normal mode (List focused), `m` toggles Seen, `*` toggles Flagged (`F`, surfaced as a ★ glyph in the list), `d` toggles Trashed (`T`, surfaced as dimmed/strikethrough). Index + in-memory `ThreadedRow` mirrored on every flip via the generic `App::toggle_flag_selected(char)`. `store/watch.rs` exposes a `SelfWrites` registry stub the Step 7 notify watcher will consult. Note: `T` is the maildir spec's mark-for-expunge bit — actual expunge / cross-folder Trash routing is a separate piece, not Step 5.
- Steps 6–7 — compose path, notify watcher. See `DESIGN.md` for details.

**Forward-looking: cross-folder moves.** Eventually we want `:archive` / `:spam` / `:trash` (and `:mv <folder>`) from the unified inbox, routing to the correct account's Archive/Spam/Trash. The `mail/flags.rs::move_to_folder` primitive is in place; what's still missing is `[accounts.*].archive_folder` / `spam_folder` / `trash_folder` config keys and the cmdline wiring. Not in scope for Step 5.

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

- **No `async`/`await`, no tokio.** Slow work (mbsync trigger, msmtp send, maildir scans, search, large-batch threading, the browser-fallback spawn for `:open`) runs on `std::thread` workers; results flow back through `mpsc` channels. The UI thread polls each tick. Adding tokio is a redesign — raise it explicitly before doing it.
- **No GUI, no webview, no JavaScript anywhere.** The pivot to TUI means `eframe` / `egui` / `wgpu` / `vello` / `blitz-*` are out. The HTML body is parsed with `html5ever` and rendered into a ratatui cell buffer. The "no HTML engine that can execute code" property is a depended-on security guarantee — `html5ever` is a parser, not an engine.
- **Remote content is never fetched.** The HTML walker resolves `cid:` against MIME parts and decodes `data:` inline; `http(s)://` images render as `[remote image: alt]` placeholders. There is no per-message "load images" toggle. The escape hatch is `:open`, which writes the message HTML to a tempfile (with `cid:` rewrites) and hands it to the user's browser via the configured command.
- **The app never writes its TOML config.** Runtime `:set` is session-only. Persistent state belongs in `$XDG_CACHE_HOME/epost/` (the sqlite index), never in config.
- **Keys on `Message-ID`, never on file path.** mbsync rewrites maildir paths and the info-flag suffix on every sync, so paths go stale constantly.
- **Approximate-fidelity HTML by design.** Cell-grid rendering of CSS will not be pixel-faithful; that is a feature, not a bug. Don't add CSS-engine deps to "improve fidelity." If a class of mail looks too broken, the answer is `:open` and the system browser — not a webview.

## Dev environment / fixtures

`devenv.nix` provides the devshell (Rust toolchain, `mold` linker, `bacon`, `mbsync`, `msmtp`, `cargo-insta`). `direnv allow` activates it.

`dev/` is the in-repo dev harness:

- `dev/config.toml` — the TOML the binary reads under `task dev` / `task run`.
- `dev/maildir/` — in-tree synthetic maildir. `dev/maildir/README.md` lists the cases to cover; **populating these is open work** as the parser/renderer come up.
- `dev/msmtp-stub` — shell script wired into `[smtp].command` in dev; captures outbound mail to `/tmp/epost-sent/` instead of sending.
- `dev/fixtures/` — hand-crafted HTML and image fixtures for renderer tests. Step 1's `welcome.html` + `welcome.png` from the GUI era can be reused as the first cell-rendering target.
