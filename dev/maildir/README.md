# Dev fixture maildir

In-tree synthetic maildir used by `task dev` / `task run` so the app has
something to render without needing a real `mbsync` setup. Selected by
`dev/config.toml`'s `accounts.dev.maildir`. The companion
`dev/maildir-work/` fixture backs `accounts.work` so the multi-account
UI has a second tree to render — see its README for the cases there.

## Layout (maildir++)

    dev/maildir/
      cur/          # the dev account's INBOX, seen
      new/          # INBOX, unseen
      tmp/          # in-flight (mbsync would use this; we won't)
      .Sent/
        cur/
        new/
        tmp/

Each message is a single file with name `<unix-ts>.<unique>.<host>:2,<flags>`
per the maildir spec. Flags we care about: `S` (seen), `R` (replied),
`T` (trashed), `F` (flagged), `D` (draft).

## Synthetic cases to cover

Filled in as the corresponding code lands. Each case is one or more `.eml`
files placed under `cur/` or `.Sent/cur/`.

- [x] Plain text only — `cur/1779181200…welcome…`
- [x] Plain + HTML multipart alternative — `cur/1779105600…multipart…`
- [x] HTML with `cid:` inline image (resolved against the message's MIME parts)
      — `cur/1779192000…cid-image…`
- [x] HTML with `http(s)://` tracking pixel (must render as a placeholder)
      — `cur/1779195600…remote-image…`
- [x] Threaded reply chain, 3 messages, real `In-Reply-To` / `References` —
      `cur/177901020*…thread-root…` + `…thread-reply1…` + `…thread-reply2…`
- [x] Broken / malformed HTML — `cur/1779199200…broken…`
- [ ] Outlook-style table layout
- [ ] Long subject + long sender name (list-pane truncation)
- [x] Non-ASCII subject + body (UTF-8 round-trip) — `new/1778850000…utf8…`
- [x] `multipart/mixed` with two `Content-Disposition: attachment` parts
      (exercises the reader's bottom strip + `:save` / `:open-attachment`)
      — `cur/1779264000…attach-fixture…`
