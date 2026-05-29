# Dev fixture maildir (work account)

Companion fixture to `dev/maildir/`. Provides a second account so the
multi-account UI (sidebar `[work]` group, scope-cycling with
`Alt-j`/`Alt-k`, `:account work` jump) has real data to render under
`task dev` / `task run`.

Wired in via `dev/config.toml`'s `[accounts.work]`; mirrored into
`dev/scratch/maildir-work/` on first run (`task _maildir:ensure:work`).
Rebuild from this source with `task maildir:reset`.

## Layout (maildir++)

    dev/maildir-work/
      cur/                   # work INBOX, seen
      new/                   # work INBOX, unseen
      tmp/
      .Sent/
        cur/
        new/
        tmp/

## Cases covered

- `cur/1779200400…work-status-001` — plain-text request from a colleague.
- `cur/1779207600…work-meeting-002` — HTML calendar invite (lists +
  link, exercises the link picker against a non-INBOX-source message).
- `new/1779238800…work-ci-003` — unseen build-failure email so the
  `[work] INBOX` row reads as unread (`bold + (1)` in the sidebar) on
  first launch.
- `.Sent/cur/1779210000…work-status-001-reply` — reply to the first
  message, in-reply-to + references set so threading lines up if you
  open the thread from `[all]` view.
