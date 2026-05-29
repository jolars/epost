# TODO

Working list of next-up work. Items roughly ordered within each section
by effort / proximity to merge. Move done items into `AGENTS.md`'s
*Project state* bullets; don't leave stale entries here.

`DESIGN.md`'s *Suggested build order* (Steps 1–7) is exhausted; the
items below are the v1.x finish, not the original v1 spec.

## Near-term

- **`:sync` command dispatch.** Config stub `[sync].command` already
  parsed in `src/config.rs`. Wire `:sync` in `src/ui/cmdline.rs`
  through a worker thread (mirror the browser-fallback shell-out from
  Step 3). Surface result in the cmdline status row. Bindable to a
  key once dispatch exists. ~30 LoC.
- **OSC 8 hyperlinks in the reader.** Wrap link runs with
  `ESC ] 8 ; ; URL ESC \` in `src/ui/reader.rs` so capable terminals
  (kitty, wezterm, foot) get native click/copy. Picker (`f`) keeps
  working unchanged for the rest. Small.
- **`:reload` config re-read.** Re-parse the TOML and hot-swap on
  `App`. No new persistence needed; just `Config::load` + assignment.
  Useful during iteration with two accounts now in play.

## Search

- **`/` mode + Index-side filter.** Add `Mode::Search`, a search
  buffer in `App`, and `Index::search(&query)` that does `LIKE`
  against `subject` / `from_addr`. Body search comes later via FTS5
  (sqlite ships with it, but the schema migration is its own task).
  Decide: scope-aware (search within current `(account, folder)`) or
  global? Recommend scope-aware to match the new sidebar mental model,
  with a leading `g/` for global.

## Compose polish

Carry-over from `AGENTS.md` Step 6 — none of these block v1 but each
is a real usability gap.

- **Attachments (multipart/mixed).** Add `attachments: Vec<PathBuf>`
  to `Draft` in `src/mail/compose.rs`; wrap body in `multipart/mixed`
  when non-empty via `mail-builder`'s `MultipartBuilder`. Surface
  through a `:attach <path>` cmdline command (no UI field yet).
- **Drafts/cur persistence across restart.** Serialize `Draft` into
  `Drafts/cur/<unique>:2,` on editor exit; restore on `:compose`. Wipe
  on successful `:send`.
- **Address (recipient) completion.** Walk `Sent/cur` at startup to
  build a `HashSet<String>` of past recipients; popup completion on
  the To/Cc/Bcc fields. Naive prefix match is fine for v1.

## Multi-account follow-ups

- **Account ordering config.** Today's order is alphabetic by
  `cfg.accounts` keys. If users want a custom order, add
  `[ui].account_order = ["personal", "work"]`. Defer unless someone
  asks.
- **Account-scoped move targets.** `:archive` / `:spam` / `:trash`
  use the owning row's account config — already correct. But the
  `[all]` view's "move to Sent" path doesn't have a clear meaning if
  Sent labels differ across accounts. Audit and document.

## Dev fixtures

- **HTML table fixture** — `dev/maildir/README.md` still flags the
  "Outlook-style table layout" case as not-yet-covered.
- **Long subject + long sender name** for list-pane truncation
  regression coverage.

## Out of scope (don't accidentally pick up)

- IMAP / SMTP / OAuth — `DESIGN.md` *Out of scope for v1*.
- Webview, JavaScript, CSS engine — security invariant #5.
- `async`/`await` / tokio — concurrency model is `std::thread` +
  `mpsc`. Adding tokio is a redesign, raise it explicitly.
- Mouse text selection in the reader — terminal selection works
  through the user's terminal; we don't intercept.
