# epost

A terminal email client for Linux maildirs, written in Rust.

`epost` reads mail that `mbsync` (or any other maildir writer) put on disk and
sends by piping to `msmtp`. It has no IMAP or SMTP implementation of its own.
HTML mail is a first-class citizen: bodies are parsed with `html5ever` and
rendered into the terminal cell grid, with a Vimium-style link picker, inline
images via the terminal's image protocol, and an `:open` escape hatch to the
system browser for the messages a cell grid can't do justice to.

**Status:** pre-1.0 and under active development. Usable, but expect rough
edges and changing defaults.

## Highlights

- Vim-style modal input throughout: reader, message list, and composer.
- Four-pane layout (folders, message list, reader, cmdline) with a unified
  inbox across accounts.
- Maildir is the source of truth; the SQLite index is a disposable cache that
  can be deleted and rebuilt at any time.
- Live `inotify` watching, so external syncs show up without a restart.
- Threaded message list, flags, cross-folder moves with undo, search.
- Compose with attachments, address completion, and a native vim-style body
  editor (or your `$EDITOR` under a pty).
- Remote content is never fetched, and nothing in the HTML path can execute
  code.

## Requirements

- Linux, a recent Rust toolchain.
- `mbsync` (or equivalent) to fetch mail, and `msmtp` to send it.
- A terminal supporting the kitty graphics protocol, iTerm2 inline images, or
  sixel for inline images; anything else falls back to half-blocks or
  placeholders.

## Install

With Nix:

```sh
nix run github:jolars/epost
```

From source:

```sh
cargo install --path .
```

## Configuration

`epost` reads `~/.config/epost/config.toml` (strictly: unknown keys are an
error) and never writes it. A minimal config:

```toml
[accounts.personal]
maildir = "~/Mail/personal"
from = "Jane Doe <jane@example.com>"
sent = "Sent"
archive = "Archive"
trash = "Trash"

[smtp]
command = ["msmtp", "-t"]

[sync]
# Optional: what `:sync` runs. Omit if you drive mbsync from a timer.
command = ["mbsync", "-a"]
```

See the *Configuration* section of [`DESIGN.md`](DESIGN.md) for the full
schema.

CLI flags: `--config <path>` and `--cache <path>` override the config and index
locations.

## Development

The devshell (Rust toolchain, `bacon`, `mbsync`, `msmtp`, `cargo-insta`) comes
from `devenv.nix`; run `direnv allow` to activate it. All commands go through
[go-task](https://taskfile.dev):

```sh
task dev    # main loop, auto-rebuilding against dev/config.toml
task run    # one-shot run against the dev config
task ci     # fmt:check + lint + test
task --list # everything else
```

`dev/` holds an in-repo fixture maildir and stub `msmtp`/browser scripts, so
development never touches real mail.

[`DESIGN.md`](DESIGN.md) is the authoritative architecture document,
[`AGENTS.md`](AGENTS.md) tracks implementation state, and [`TODO.md`](TODO.md)
lists what's next.
