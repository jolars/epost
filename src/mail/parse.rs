use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use mail_parser::{MessageParser, MimeHeaders, PartType};

#[derive(Debug, Clone)]
pub struct Headers {
    pub msgid: String,
    pub date: i64,
    pub from: Option<String>,
    /// `Reply-To:`, when present. Reply targeting prefers this over
    /// `From:` (mailing lists / no-reply senders).
    pub reply_to: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    /// `Bcc:` recipients. Almost never present on received mail (Bcc is
    /// stripped en route by the sender's MTA), but a draft we wrote
    /// ourselves into `Drafts/` retains it so resume can restore it.
    pub bcc: Vec<String>,
    pub subject: Option<String>,
    pub in_reply: Option<String>,
    pub refs: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Body {
    pub html: Option<String>,
    pub plain: Option<String>,
    /// Inline parts keyed by `Content-ID` (with surrounding `<>` stripped).
    /// Populated even when step 3 only emits placeholders; step 4 (inline
    /// images) and `:open` (cid rewriting) consume this directly.
    pub cid_parts: HashMap<String, Vec<u8>>,
    /// Non-inline attachment parts in the order `mail-parser` yields them.
    /// Drives the reader's bottom strip and the `:save` / `:open-attachment`
    /// verbs.
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone)]
pub struct Attachment {
    /// Display name. Falls back to `attachment-<n>` when neither
    /// `Content-Disposition: filename` nor `Content-Type: name` is set.
    pub filename: String,
    pub bytes: Vec<u8>,
}

pub fn read_body(path: &Path) -> Result<Body> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(parse_body(&bytes))
}

pub fn parse_body(bytes: &[u8]) -> Body {
    let Some(msg) = MessageParser::default().parse(bytes) else {
        return Body::default();
    };
    // body_html / body_text synthesise the other type on the fly. We only
    // want what was actually present, so check the part type directly.
    let html = msg.html_bodies().find_map(|p| match &p.body {
        PartType::Html(s) => Some(s.as_ref().to_string()),
        _ => None,
    });
    let plain = msg.text_bodies().find_map(|p| match &p.body {
        PartType::Text(s) => Some(s.as_ref().to_string()),
        _ => None,
    });

    let mut cid_parts = HashMap::new();
    for part in msg.parts.iter() {
        let Some(raw_cid) = part.content_id() else {
            continue;
        };
        let cid = strip_cid_brackets(raw_cid).to_string();
        let bytes: Vec<u8> = match &part.body {
            PartType::Binary(b) | PartType::InlineBinary(b) => b.as_ref().to_vec(),
            PartType::Text(s) | PartType::Html(s) => s.as_bytes().to_vec(),
            _ => continue,
        };
        cid_parts.insert(cid, bytes);
    }

    let mut attachments: Vec<Attachment> = Vec::new();
    for (i, part) in msg.attachments().enumerate() {
        // Skip nested message/* parts and any zero-byte oddities — neither
        // is useful for "save to disk" / "open in viewer". Inline image
        // parts (which mail-parser may also surface via attachments())
        // are already exposed through cid_parts; gate them out by
        // Content-Disposition.
        if part.is_message() || part.is_multipart() {
            continue;
        }
        // A `Content-ID` means the HTML can reference the part via `cid:`,
        // so it's already exposed through `cid_parts` (whether or not the
        // part carries an explicit `Content-Disposition: inline`). Skip
        // here so it doesn't double-list in the reader strip.
        if part.content_id().is_some() {
            continue;
        }
        if let Some(cd) = part.content_disposition()
            && cd.is_inline()
        {
            continue;
        }
        let filename = part
            .attachment_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("attachment-{}", i + 1));
        attachments.push(Attachment {
            filename,
            bytes: part.contents().to_vec(),
        });
    }

    Body {
        html,
        plain,
        cid_parts,
        attachments,
    }
}

fn strip_cid_brackets(raw: &str) -> &str {
    let s = raw.trim();
    s.strip_prefix('<')
        .and_then(|r| r.strip_suffix('>'))
        .unwrap_or(s)
}

pub fn read_headers(path: &Path) -> Result<Option<Headers>> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(parse_headers(&bytes))
}

/// Count `multipart/*` attachment parts on the message at `path`.
/// Used by the draft-resume path to warn that attachments are being
/// dropped (v1 doesn't reconstruct them on the composer side).
/// Returns 0 on a parse failure or an unparseable file rather than
/// erroring — a missing warning is preferable to refusing to resume.
pub fn count_attachments(path: &Path) -> usize {
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    let Some(msg) = MessageParser::default().parse(&bytes) else {
        return 0;
    };
    msg.attachment_count()
}

pub fn parse_headers(bytes: &[u8]) -> Option<Headers> {
    let msg = MessageParser::default().parse_headers(bytes)?;

    let msgid = msg.message_id()?.to_string();

    let date = msg
        .date()
        .map(|d| d.to_timestamp())
        .filter(|t| *t > 0)
        .unwrap_or(0);

    let from = msg.from().and_then(|addrs| format_addr(addrs.first()?));
    let reply_to = msg.reply_to().and_then(|addrs| format_addr(addrs.first()?));
    let to = collect_addrs(msg.to());
    let cc = collect_addrs(msg.cc());
    let bcc = collect_addrs(msg.bcc());

    let subject = msg.subject().map(|s| s.to_string());

    let in_reply = msg
        .in_reply_to()
        .as_text_list()
        .and_then(|v| v.first().map(|s| s.to_string()));

    let refs = msg
        .references()
        .as_text_list()
        .map(|v| v.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();

    Some(Headers {
        msgid,
        date,
        from,
        reply_to,
        to,
        cc,
        bcc,
        subject,
        in_reply,
        refs,
    })
}

fn format_addr(a: &mail_parser::Addr) -> Option<String> {
    let name = a.name().map(|s| s.to_string());
    let addr = a.address().map(|s| s.to_string());
    match (name, addr) {
        (Some(n), Some(a)) => Some(format!("{n} <{a}>")),
        (None, Some(a)) => Some(a),
        (Some(n), None) => Some(n),
        (None, None) => None,
    }
}

fn collect_addrs(field: Option<&mail_parser::Address<'_>>) -> Vec<String> {
    let Some(addrs) = field else {
        return Vec::new();
    };
    addrs.iter().filter_map(format_addr).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: &[u8] = b"\
Message-ID: <a@example.com>\r\n\
From: Jane Doe <jane@example.com>\r\n\
Subject: Hi\r\n\
Date: Tue, 26 May 2026 09:00:00 +0000\r\n\
\r\n\
body\r\n";

    const REPLY: &[u8] = b"\
Message-ID: <b@example.com>\r\n\
From: Bob <bob@example.net>\r\n\
Subject: Re: Hi\r\n\
Date: Wed, 27 May 2026 10:00:00 +0000\r\n\
In-Reply-To: <a@example.com>\r\n\
References: <root@example.net> <a@example.com>\r\n\
\r\n\
body\r\n";

    const UTF8: &[u8] = b"\
Message-ID: <c@example.jp>\r\n\
From: =?UTF-8?B?44Om44Kt?= <yuki@example.jp>\r\n\
Subject: =?UTF-8?B?5pel5pys6Kqe44Gu5Lu25ZCN44OG44K544OI?=\r\n\
Date: Fri, 22 May 2026 11:00:00 +0000\r\n\
\r\n\
body\r\n";

    #[test]
    fn plain_extracts_msgid_from_subject_date() {
        let h = parse_headers(PLAIN).unwrap();
        assert_eq!(h.msgid, "a@example.com");
        assert_eq!(h.from.as_deref(), Some("Jane Doe <jane@example.com>"));
        assert_eq!(h.subject.as_deref(), Some("Hi"));
        assert!(h.date > 0);
        assert!(h.in_reply.is_none());
        assert!(h.refs.is_empty());
    }

    #[test]
    fn reply_extracts_in_reply_to_and_references() {
        let h = parse_headers(REPLY).unwrap();
        assert_eq!(h.in_reply.as_deref(), Some("a@example.com"));
        assert_eq!(h.refs, vec!["root@example.net", "a@example.com"]);
    }

    #[test]
    fn rfc_2047_subject_decodes_to_utf8() {
        let h = parse_headers(UTF8).unwrap();
        assert_eq!(h.subject.as_deref(), Some("日本語の件名テスト"));
    }

    const MULTI_RECIPIENT: &[u8] = b"\
Message-ID: <r@example.com>\r\n\
From: sender@example.com\r\n\
To: First <first@example.com>, second@example.com\r\n\
Cc: Third <third@example.com>\r\n\
Subject: many\r\n\
Date: Tue, 26 May 2026 09:00:00 +0000\r\n\
\r\n\
body\r\n";

    #[test]
    fn parses_multi_recipient_to_and_cc() {
        let h = parse_headers(MULTI_RECIPIENT).unwrap();
        assert_eq!(
            h.to,
            vec![
                "First <first@example.com>".to_string(),
                "second@example.com".to_string(),
            ]
        );
        assert_eq!(h.cc, vec!["Third <third@example.com>".to_string()]);
    }

    const WITH_REPLY_TO: &[u8] = b"\
Message-ID: <rt@example.com>\r\n\
From: List Bot <bot@list.example.com>\r\n\
Reply-To: List <list@list.example.com>\r\n\
To: subscriber@example.com\r\n\
Subject: announce\r\n\
Date: Tue, 26 May 2026 09:00:00 +0000\r\n\
\r\n\
body\r\n";

    #[test]
    fn captures_reply_to_when_present() {
        let h = parse_headers(WITH_REPLY_TO).unwrap();
        assert_eq!(h.reply_to.as_deref(), Some("List <list@list.example.com>"));
        assert_eq!(h.from.as_deref(), Some("List Bot <bot@list.example.com>"));
    }

    #[test]
    fn reply_to_absent_when_unset() {
        let h = parse_headers(PLAIN).unwrap();
        assert!(h.reply_to.is_none());
        assert!(h.to.is_empty());
        assert!(h.cc.is_empty());
    }

    const MULTIPART_WITH_CID: &[u8] = b"\
Message-ID: <m@example.com>\r\n\
From: a@example.com\r\n\
Subject: t\r\n\
Date: Tue, 26 May 2026 09:00:00 +0000\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/related; boundary=\"bnd\"\r\n\
\r\n\
--bnd\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<html><body><p>see <img src=\"cid:logo@x\"></p></body></html>\r\n\
--bnd\r\n\
Content-Type: image/png\r\n\
Content-Transfer-Encoding: base64\r\n\
Content-ID: <logo@x>\r\n\
\r\n\
aGVsbG8=\r\n\
--bnd--\r\n";

    #[test]
    fn parses_html_body_and_cid_part() {
        let body = parse_body(MULTIPART_WITH_CID);
        assert!(body.html.as_deref().unwrap_or("").contains("cid:logo@x"));
        let part = body.cid_parts.get("logo@x").expect("cid part");
        assert_eq!(part.as_slice(), b"hello"); // base64-decoded
    }

    const PLAIN_ONLY: &[u8] = b"\
Message-ID: <p@example.com>\r\n\
From: a@example.com\r\n\
Subject: hi\r\n\
Date: Tue, 26 May 2026 09:00:00 +0000\r\n\
\r\n\
just text\r\n";

    #[test]
    fn plain_only_yields_no_html() {
        let body = parse_body(PLAIN_ONLY);
        assert!(body.html.is_none());
        assert_eq!(body.plain.as_deref().unwrap_or("").trim(), "just text");
    }

    const WITH_ATTACHMENT: &[u8] = b"\
Message-ID: <atch@example.com>\r\n\
From: a@example.com\r\n\
Subject: with attachment\r\n\
Date: Tue, 26 May 2026 09:00:00 +0000\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"bnd\"\r\n\
\r\n\
--bnd\r\n\
Content-Type: text/plain\r\n\
\r\n\
body\r\n\
--bnd\r\n\
Content-Type: application/pdf\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
aGVsbG8=\r\n\
--bnd--\r\n";

    #[test]
    fn extracts_attachment_filename_type_and_bytes() {
        let body = parse_body(WITH_ATTACHMENT);
        assert_eq!(body.attachments.len(), 1);
        let a = &body.attachments[0];
        assert_eq!(a.filename, "report.pdf");
        assert_eq!(a.bytes.as_slice(), b"hello");
    }

    #[test]
    fn cid_inline_part_is_not_in_attachments() {
        // The MULTIPART_WITH_CID fixture's inline image must flow only
        // through cid_parts; an inline part double-listed under
        // attachments would surface as a fake attachment in the strip.
        let body = parse_body(MULTIPART_WITH_CID);
        assert!(body.cid_parts.contains_key("logo@x"));
        assert!(body.attachments.is_empty(), "{:?}", body.attachments);
    }
}
