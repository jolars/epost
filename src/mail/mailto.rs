//! Compose fields from RFC 6068 mailto URIs. Parsing never sends mail or
//! reads attachments; the resulting fields always go through the composer.

use std::str::FromStr;

use anyhow::{Context, Result, anyhow, ensure};

use super::compose::Draft;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mailto {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
}

pub fn has_scheme(uri: &str) -> bool {
    uri.split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("mailto"))
}

impl FromStr for Mailto {
    type Err = anyhow::Error;

    fn from_str(uri: &str) -> Result<Self> {
        ensure!(has_scheme(uri), "expected a mailto: URI");
        let rest = &uri["mailto:".len()..];
        ensure!(!rest.starts_with("//"), "mailto: URIs do not use //");
        let rest = rest.split_once('#').map_or(rest, |(rest, _)| rest);
        let (to, query) = rest.split_once('?').unwrap_or((rest, ""));
        let mut mailto = Self::default();
        add_recipients(&mut mailto.to, &decode(to, false)?);
        let mut subject_seen = false;
        let mut body_seen = false;
        // Split URI delimiters before decoding so escaped '&' and '=' stay
        // inside their values. Only the fields editable in our form are used.
        for field in query.split('&').filter(|field| !field.is_empty()) {
            let (name, value) = field
                .split_once('=')
                .ok_or_else(|| anyhow!("mailto fields must use name=value"))?;
            let name = decode(name, false)?.to_ascii_lowercase();
            match name.as_str() {
                "to" => add_recipients(&mut mailto.to, &decode(value, false)?),
                "cc" => add_recipients(&mut mailto.cc, &decode(value, false)?),
                "bcc" => add_recipients(&mut mailto.bcc, &decode(value, false)?),
                "subject" => {
                    ensure!(!subject_seen, "duplicate mailto subject");
                    subject_seen = true;
                    mailto.subject = decode(value, false)?;
                }
                "body" => {
                    ensure!(!body_seen, "duplicate mailto body");
                    body_seen = true;
                    mailto.body = decode(value, true)?
                        .replace("\r\n", "\n")
                        .replace('\r', "\n");
                }
                _ => {}
            }
        }
        Ok(mailto)
    }
}

fn add_recipients(recipients: &mut Vec<String>, value: &str) {
    recipients.extend(
        value
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
    );
}

fn decode(value: &str, body: bool) -> Result<String> {
    let mut bytes = value.bytes();
    let mut decoded = Vec::with_capacity(value.len());
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let hi = bytes.next().and_then(|b| char::from(b).to_digit(16));
            let lo = bytes.next().and_then(|b| char::from(b).to_digit(16));
            let (Some(hi), Some(lo)) = (hi, lo) else {
                return Err(anyhow!("invalid percent escape in mailto URI"));
            };
            decoded.push((hi * 16 + lo) as u8);
        } else {
            // Mailto uses URI escaping, not form encoding: '+' is literal.
            decoded.push(byte);
        }
    }
    let decoded = String::from_utf8(decoded).context("mailto value is not UTF-8")?;
    ensure!(
        !decoded
            .chars()
            .any(|c| c.is_control() && !(body && matches!(c, '\r' | '\n' | '\t'))),
        "control characters are not allowed in mailto {}",
        if body { "body text" } else { "headers" },
    );
    Ok(decoded)
}

impl Mailto {
    pub fn apply(self, draft: &mut Draft) {
        draft.to = self.to;
        draft.cc = self.cc;
        draft.bcc = self.bcc;
        draft.subject = self.subject;
        draft.body = self.body;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailto_decodes_recipients_headers_and_body() {
        let mailto: Mailto = "mailto:dev+list@example.com,second@example.com?to=third%40example.com&CC=copy@example.com&cc=another@example.com&bcc=hidden@example.com&%73ubject=H%C3%A9llo%20%26%20welcome&body=First%0D%0ASecond%09line".parse().unwrap();
        assert_eq!(
            mailto.to,
            [
                "dev+list@example.com",
                "second@example.com",
                "third@example.com"
            ]
        );
        assert_eq!(mailto.cc, ["copy@example.com", "another@example.com"]);
        assert_eq!(mailto.bcc, ["hidden@example.com"]);
        assert_eq!(mailto.subject, "Héllo & welcome");
        assert_eq!(mailto.body, "First\nSecond\tline");
    }

    #[test]
    fn mailto_preserves_plus_and_decodes_only_once() {
        let mailto: Mailto =
            "MAILTO:dev%2Blist@example.com?subject=C++%20%2520&body=%26to%3Dother%40example.com"
                .parse()
                .unwrap();
        assert_eq!(mailto.to, ["dev+list@example.com"]);
        assert_eq!(mailto.subject, "C++ %20");
        assert_eq!(mailto.body, "&to=other@example.com");
    }

    #[test]
    fn mailto_supports_empty_and_query_only_uris() {
        assert_eq!("mailto:".parse::<Mailto>().unwrap(), Mailto::default());
        assert_eq!("mailto:?".parse::<Mailto>().unwrap(), Mailto::default());
        let mailto: Mailto = "mailto:?to=dev@example.com&subject=Hello".parse().unwrap();
        assert_eq!(mailto.to, ["dev@example.com"]);
        assert_eq!(mailto.subject, "Hello");
    }

    #[test]
    fn mailto_ignores_fragments_but_keeps_encoded_hashes() {
        let mailto: Mailto = "mailto:dev@example.com?subject=Issue%20%231#ignored"
            .parse()
            .unwrap();
        assert_eq!(mailto.subject, "Issue #1");
    }

    #[test]
    fn mailto_ignores_unsupported_fields_and_keeps_sender() {
        let mailto: Mailto = "mailto:dev@example.com?from=other@example.com&attach=/etc/passwd&attachment=/tmp/file&Content-Type=text/html&subject=Hello".parse().unwrap();
        let mut draft = Draft::new_blank("work", "me@work.example");
        mailto.apply(&mut draft);
        assert_eq!(draft.account, "work");
        assert_eq!(draft.from, "me@work.example");
        assert!(draft.attachments.is_empty());
        assert_eq!(draft.subject, "Hello");
    }

    #[test]
    fn mailto_rejects_bad_encoding_and_control_characters() {
        for uri in [
            "https://example.com",
            "dev@example.com",
            "mailto://dev@example.com",
            "mailto:dev%0A@example.com",
            "mailto:dev@example.com?subject=Hi%0D%0ABcc:other@example.com",
            "mailto:?to=dev%00@example.com",
            "mailto:?cc=dev%0D@example.com",
            "mailto:?bcc=dev%0A@example.com",
            "mailto:?subject=%1B%5B2J",
            "mailto:?body=%00",
            "mailto:?body=%1B",
            "mailto:?subject=%FF",
            "mailto:?subject=%",
            "mailto:?subject=%A",
            "mailto:?body=%GG",
            "mailto:?subject",
            "mailto:?subject=a&subject=b",
            "mailto:?body=a&BODY=b",
        ] {
            assert!(uri.parse::<Mailto>().is_err(), "{uri:?}");
        }
    }
}
