use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use mail_parser::{MessageParser, MimeHeaders, PartType};

#[derive(Debug, Clone)]
pub struct Headers {
    pub msgid: String,
    pub date: i64,
    pub from: Option<String>,
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

    Body {
        html,
        plain,
        cid_parts,
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

pub fn parse_headers(bytes: &[u8]) -> Option<Headers> {
    let msg = MessageParser::default().parse_headers(bytes)?;

    let msgid = msg.message_id()?.to_string();

    let date = msg
        .date()
        .map(|d| d.to_timestamp())
        .filter(|t| *t > 0)
        .unwrap_or(0);

    let from = msg.from().and_then(|addrs| {
        let a = addrs.first()?;
        let name = a.name().map(|s| s.to_string());
        let addr = a.address().map(|s| s.to_string());
        match (name, addr) {
            (Some(n), Some(a)) => Some(format!("{n} <{a}>")),
            (None, Some(a)) => Some(a),
            (Some(n), None) => Some(n),
            (None, None) => None,
        }
    });

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
        subject,
        in_reply,
        refs,
    })
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
}
