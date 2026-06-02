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

- **Drafts/cur persistence across restart.** Serialize `Draft` into
  `Drafts/cur/<unique>:2,` on editor exit; restore on `:compose`. Wipe on
  successful `:send`.
- **Address completion follow-ups.** v1 lands with prefix-match + Sent harvest
  + mutt `query_command`; remaining work:
    - live re-harvest (today the native cache is startup-only; new Sent mail
      surfaces only after restart);
    - substring / fuzzy matching;
    - frecency-weighted ranking inside the native source (most-recent / most-
      frequent first), or pull through the index instead of walking on each
      startup.

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
