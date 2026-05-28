# Dev fixture maildir

In-tree synthetic maildir used by `task dev` / `task run` so the app has
something to render without needing a real `mbsync` setup. Selected by
`dev/config.toml`'s `accounts.dev.maildir`.

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

- [ ] Plain text only
- [ ] Plain + HTML multipart alternative
- [ ] HTML with `cid:` inline image (must resolve via `NetProvider`)
- [ ] HTML with `http(s)://` tracking pixel (must be blocked by default)
- [ ] Threaded reply chain, 3--4 messages, real `In-Reply-To` / `References`
- [ ] Broken / malformed HTML
- [ ] Outlook-style table layout (Blitz stress case)
- [ ] Long subject + long sender name (list-pane truncation)
- [ ] Non-ASCII subject + body (UTF-8 round-trip)
