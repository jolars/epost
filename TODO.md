# TODO

Working list of next-up work. Items roughly ordered within each section by
effort / proximity to merge. Move done items into `AGENTS.md`'s *Project state*
bullets; don't leave stale entries here.

`DESIGN.md`'s *Suggested build order* (Steps 1--7) is exhausted; the items below
are the v1.x finish, not the original v1 spec.

## Near-term

- **`:reload`config re-read.** Re-parse the TOML and hot-swap on `App`. No new
  persistence needed; just `Config::load` + assignment. Useful during iteration
  with two accounts now in play.

## Compose polish

Carry-over from `AGENTS.md` Step 6 --- none of these block v1 but each is a real
usability gap.

- **Draft autosave while editing.** `:postpone`, close-and-save, and `:send`
  persist drafts; periodic autosave would also protect edits made before those
  commands from a process crash.
- **Address completion follow-ups.** v1 lands with prefix-match + Sent harvest
  + mutt `query_command`; remaining work:
    - live re-harvest (today the native cache is startup-only; new Sent mail
      surfaces only after restart);
    - substring / fuzzy matching;
    - frecency-weighted ranking inside the native source (most-recent / most-
      frequent first), or pull through the index instead of walking on each
      startup.

## Reader

- **`To:` / `Cc:` header rows.** Standard mail-client info the reader doesn't
  show today (direct recipient vs cc'd vs list mail). Note this is *not* an
  account indicator --- the `Folder: account · folder` row covers that; on
  BCC'd and list mail the reader's own address isn't in `To:` at all, and an
  alias there doesn't identify the owning account. Plumbing: `parse::Body`
  doesn't carry recipients, so `parse_body` needs to capture `to`/`cc` (the
  extraction already exists in `parse_headers`) and `render_headers` needs to
  read them off `ParsedBody` rather than the index `MessageRow`. Long
  recipient lists want an elide (`a@x, b@y, +3 more`) so the header block
  doesn't push the body offscreen.

## Multi-account follow-ups

- **Account ordering config.** Today's order is alphabetic by `cfg.accounts`
  keys. If users want a custom order, add
  `[ui].account_order = ["personal", "work"]`. Defer unless someone asks.
- **Account-scoped move targets.** `:archive` / `:spam` / `:trash` use the
  owning row's account config --- already correct. But the `[all]` view's "move
  to Sent" path doesn't have a clear meaning if Sent labels differ across
  accounts. Audit and document.

## Dev fixtures

- **HTML table fixture** --- `dev/maildir/README.md` still flags the
  "Outlook-style table layout" case as not-yet-covered.
- **Long subject + long sender name** for list-pane truncation regression
  coverage.

## Out of scope (don't accidentally pick up)

- IMAP / SMTP / OAuth --- `DESIGN.md` *Out of scope permanently*.
- Webview, JavaScript, CSS engine --- security invariant #5.
- `async`/`await` / tokio --- concurrency model is `std::thread` + `mpsc`.
  Adding tokio is a redesign, raise it explicitly.
