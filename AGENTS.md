# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Source of truth: DESIGN.md

`DESIGN.md` is the authoritative architecture document. **Read it before proposing or making non-trivial changes.** It pins the rendering model (Blitz, not `wry`/webview), the storage model (maildir is truth; sqlite is a disposable cache; everything keys on `Message-ID`, never on path), the configuration model (declarative read-only TOML), the concurrency model (`std::thread` + `mpsc` channels, **not** `async`/`await`), and the deployment target (NixOS / Home Manager, fully config-driven). The numbered *Hard requirements / invariants* are real constraints; do not silently relax them. *Suggested build order* is the v1 roadmap.

## Project state

Fresh scaffold. `src/main.rs` is the `cargo init` hello world; the module layout in `DESIGN.md` is the **target**, not the current state — most of those modules do not exist yet. Build-order step 1 (the egui + Blitz compositing skeleton inside the four-panel layout) is the next thing to land.

## Commands

All commands go through `task` (go-task); `Taskfile.yml` is the canonical surface. `task --list` enumerates. Highlights:

- `task dev` — main loop. `bacon` auto-rebuilds and restarts against `dev/config.toml`.
- `task run` — one-shot run against the dev config.
- `task ci` — `fmt:check + lint + test`, what CI would run.
- `task doctor` — smoke-check the devshell and fixtures.
- `task snap:review` / `task snap:accept` — `cargo insta` for Blitz golden snapshots.

Prefer the tasks over raw `cargo run`: the wrappers pass `--config dev/config.toml` so the app uses the in-repo fixture maildir and the stub msmtp instead of real mail. Single-test invocation is plain `cargo test <name>`.

## Constraints that catch agents

These five are easy to flagrantly violate. The full set lives in `DESIGN.md`; this is a triage list to scan before editing:

- **No `async`/`await`, no tokio.** Slow work (mbsync trigger, msmtp send, maildir scans, search, large-batch threading) runs on `std::thread` workers; results flow back through `mpsc` channels. The UI thread polls each frame and calls `Context::request_repaint()` when there's something to show. Adding tokio is a redesign — raise it explicitly before doing it.
- **No `wry`, no webview, no JavaScript anywhere.** The HTML body is Blitz painting into a wgpu texture that egui composites as an `Image`. The "no JS engine" property is a depended-on security guarantee, not an off-by-default flag.
- **The app never writes its TOML config.** Runtime `:set` is session-only. Persistent state belongs in `$XDG_CACHE_HOME/epost/` (the sqlite index), never in config.
- **Blitz is pre-alpha.** Pin the exact version in `Cargo.toml`. Expect API churn between alphas; source-read `blitz-dom/src/{config,document}.rs` and `blitz-traits/src/net.rs` before assuming an API exists — hosted docs are thin.
- **Keys on `Message-ID`, never on file path.** mbsync rewrites maildir paths and the info-flag suffix on every sync, so paths go stale constantly.

## Dev environment / fixtures

`devenv.nix` provides the devshell (Rust toolchain, `mold` linker, `bacon`, `mbsync`, `msmtp`, `cargo-insta`, plus eframe/Blitz runtime libs wired into `LD_LIBRARY_PATH`). `direnv allow` activates it.

`dev/` is the in-repo dev harness:

- `dev/config.toml` — the TOML the binary reads under `task dev` / `task run`.
- `dev/maildir/` — in-tree synthetic maildir. `dev/maildir/README.md` lists the cases to cover; **populating these is open work** as the parser/renderer come up.
- `dev/msmtp-stub` — shell script wired into `[smtp].command` in dev; captures outbound mail to `/tmp/epost-sent/` instead of sending.
