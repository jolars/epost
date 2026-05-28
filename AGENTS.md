# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Source of truth: DESIGN.md

`DESIGN.md` is the authoritative architecture document. **Read it before proposing or making non-trivial changes.** It pins the rendering model (TUI via `ratatui` + `crossterm`, with HTML walked through `html5ever` into a Block-IR and drawn as cells; inline images via `ratatui-image`'s kitty/iTerm/sixel/halfblock detection), the storage model (maildir is truth; sqlite is a disposable cache; everything keys on `Message-ID`, never on path), the configuration model (declarative read-only TOML), the concurrency model (`std::thread` + `mpsc` channels, **not** `async`/`await`), and the deployment target (NixOS / Home Manager, fully config-driven). The numbered *Hard requirements / invariants* are real constraints; do not silently relax them. *Suggested build order* is the v1 roadmap.

**Note on history:** this project pivoted from a GUI (eframe + Blitz + wgpu) to a TUI design after step 1 of the GUI plan was finished. The GUI code (eframe `App`, Blitz/Vello compositing in `ui/body.rs`, `mail/net.rs` `NetProvider` stub, `wgpu` / `eframe` / `blitz-*` / `vello` / `anyrender_*` deps) is on its way out; don't extend it. If you see lingering references to wgpu textures, `NetProvider`, Blitz, or a "four-panel egui layout," those are residue from the pre-pivot design and should be ripped out, not built on.

## Project state

Mid-pivot. The repo contains a working GUI step-1 (eframe + Blitz compositing skeleton) that's being torn down in favour of a TUI redesign. `DESIGN.md` describes the new target. The new build-order step 1 (ratatui scaffold in the four-pane layout) is the next thing to land; expect to delete `src/ui/body.rs`, `src/mail/net.rs`, and the GUI deps in `Cargo.toml` as part of it.

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

`devenv.nix` provides the devshell (Rust toolchain, `mold` linker, `bacon`, `mbsync`, `msmtp`, `cargo-insta`). `direnv allow` activates it. With the GUI pivot the Vulkan / Wayland / X11 / fontconfig / freetype runtime libs and pkg-config build deps are no longer needed and should be trimmed when the GUI code is removed.

`dev/` is the in-repo dev harness:

- `dev/config.toml` — the TOML the binary reads under `task dev` / `task run`.
- `dev/maildir/` — in-tree synthetic maildir. `dev/maildir/README.md` lists the cases to cover; **populating these is open work** as the parser/renderer come up.
- `dev/msmtp-stub` — shell script wired into `[smtp].command` in dev; captures outbound mail to `/tmp/epost-sent/` instead of sending.
- `dev/fixtures/` — hand-crafted HTML and image fixtures for renderer tests. Step 1's `welcome.html` + `welcome.png` from the GUI era can be reused as the first cell-rendering target.
