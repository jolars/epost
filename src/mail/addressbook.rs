//! Address completion for the compose tab. Two sources feed the popup:
//! a native walk of each account's Sent folder (this module) and an
//! external mutt-style `query_command` (see `addressbook_external`).
//!
//! The native source is a one-shot startup worker. It iterates every
//! configured Sent folder, parses headers via the existing
//! `mail::parse` plumbing, harvests addresses from `From / To / Cc /
//! Bcc`, dedupes on lowercase email, and returns one
//! `AddressBookResult` over an `mpsc` channel. The UI swaps it into
//! `App.address_book` and queries it inline as the user types.
//!
//! Live re-harvest (re-scanning after every sync or send) is out of
//! scope for v1 — restart picks up new addresses. The Sent folder is
//! small enough on every personal account I've measured that the
//! startup cost is a single-digit millisecond hit, so no incremental
//! reporting either.
//!
//! Matching is naive case-insensitive prefix on the email and on each
//! alphanumeric word inside the display name. Substring / fuzzy is
//! follow-up work, not v1.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};

use crate::config::FolderRole;
use crate::mail::parse;
use crate::store::AccountSpec;

/// Which source surfaced a contact. Drives ranking in the popup
/// (external first, then native), and lets the UI badge entries if it
/// ever wants to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    External,
    Native,
}

/// One queryable contact. `email_lc` is the dedup key and the
/// prefix-match target so callers don't relowercase on every keystroke.
/// `email` retains display case so accepting an entry doesn't mangle a
/// camelCased local-part.
#[derive(Debug, Clone)]
pub struct Contact {
    pub name: Option<String>,
    pub email: String,
    pub email_lc: String,
    pub source: Source,
}

impl Contact {
    /// Build a Contact from a parsed `Name <addr>` or bare-`addr` form.
    /// Returns `None` when there is no `@` to split on — we don't index
    /// non-email tokens (`undisclosed-recipients:;`, mailing-list
    /// placeholders, etc.).
    pub fn from_raw(raw: &str, source: Source) -> Option<Contact> {
        let (name, email) = parse_addr(raw)?;
        let email_lc = email.to_ascii_lowercase();
        Some(Contact {
            name,
            email,
            email_lc,
            source,
        })
    }

    /// Render as the RFC 5322 form the compose field expects: `Name
    /// <email>` when a name is known, bare `email` otherwise.
    pub fn render_address(&self) -> String {
        match self.name.as_ref() {
            Some(name) if !name.is_empty() => format!("{} <{}>", name, self.email),
            _ => self.email.clone(),
        }
    }

    /// Case-insensitive prefix match on the email or on any
    /// alphanumeric word inside the display name. Matches what most TUI
    /// mail clients do — "ali" hits both `alice@…` and
    /// `Bob <ali@example.com>` (where the name is "ali"), but not
    /// `…@alibaba.com` mid-domain.
    pub fn matches_prefix(&self, prefix_lc: &str) -> bool {
        if prefix_lc.is_empty() {
            return true;
        }
        if self.email_lc.starts_with(prefix_lc) {
            return true;
        }
        if let Some(name) = self.name.as_ref() {
            for word in name.split(|c: char| !c.is_alphanumeric()) {
                if word.is_empty() {
                    continue;
                }
                if word.to_ascii_lowercase().starts_with(prefix_lc) {
                    return true;
                }
            }
        }
        false
    }
}

/// Native address book — populated once at startup by
/// `start_addressbook_worker`. The UI holds one of these on `App` and
/// queries it on every keystroke that meets `min_chars`.
#[derive(Debug, Default)]
pub struct AddressBook {
    pub native: Vec<Contact>,
}

impl AddressBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the native cache. Called when the worker's result lands.
    pub fn set_native(&mut self, contacts: Vec<Contact>) {
        self.native = contacts;
    }

    /// Native contacts matching `prefix_lc`, capped at `limit`. Order
    /// is insertion order from the scan (which is roughly recency-
    /// adjacent since the scan walks `cur/` then `new/` per folder).
    pub fn query_native(&self, prefix_lc: &str, limit: usize) -> Vec<Contact> {
        self.native
            .iter()
            .filter(|c| c.matches_prefix(prefix_lc))
            .take(limit)
            .cloned()
            .collect()
    }
}

/// Worker output: a single batch of harvested contacts.
pub struct AddressBookResult {
    pub contacts: Vec<Contact>,
}

/// Spawn the startup walk. One thread, one channel. Returns
/// immediately; the UI drains the receiver on subsequent ticks.
pub fn start_addressbook_worker(specs: Vec<AccountSpec>) -> Receiver<AddressBookResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let contacts = collect_from_specs(&specs);
        let _ = tx.send(AddressBookResult { contacts });
    });
    rx
}

/// Walk every configured Sent folder, harvest addresses, dedupe.
/// Public for unit testing — the worker only wraps this in a thread.
pub fn collect_from_specs(specs: &[AccountSpec]) -> Vec<Contact> {
    let mut by_email: HashMap<String, Contact> = HashMap::new();
    for spec in specs {
        let Some(binding) = spec.binding_by_role(FolderRole::Sent) else {
            continue;
        };
        if !binding.path.is_dir() {
            continue;
        }
        for sub in ["cur", "new"] {
            let dir = binding.path.join(sub);
            if !dir.is_dir() {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                let Some(headers) = parse::parse_headers(&bytes) else {
                    continue;
                };
                let raws = headers
                    .from
                    .iter()
                    .chain(headers.reply_to.iter())
                    .map(|s| s.as_str())
                    .chain(headers.to.iter().map(String::as_str))
                    .chain(headers.cc.iter().map(String::as_str))
                    .chain(headers.bcc.iter().map(String::as_str));
                for raw in raws {
                    let Some(c) = Contact::from_raw(raw, Source::Native) else {
                        continue;
                    };
                    by_email
                        .entry(c.email_lc.clone())
                        .and_modify(|existing| {
                            if existing.name.is_none() && c.name.is_some() {
                                existing.name = c.name.clone();
                            }
                        })
                        .or_insert(c);
                }
            }
        }
    }
    by_email.into_values().collect()
}

/// Merge external + native query results. External wins on duplicates
/// (external has the curated display name), and the result is capped at
/// `limit`. Order: external in input order, then native in input order,
/// minus anything already shown by the external block.
pub fn merge(external: Vec<Contact>, native: Vec<Contact>, limit: usize) -> Vec<Contact> {
    let mut out: Vec<Contact> = Vec::with_capacity(external.len() + native.len());
    let mut seen: HashMap<String, ()> = HashMap::new();
    for c in external {
        if seen.contains_key(&c.email_lc) {
            continue;
        }
        seen.insert(c.email_lc.clone(), ());
        out.push(c);
        if out.len() >= limit {
            return out;
        }
    }
    for c in native {
        if seen.contains_key(&c.email_lc) {
            continue;
        }
        seen.insert(c.email_lc.clone(), ());
        out.push(c);
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Pull `(name, email)` out of an RFC 5322-ish address string. Accepts
/// `Name <email>`, `"Quoted Name" <email>`, and bare `email`. Returns
/// `None` for anything without an `@` we can lock onto.
fn parse_addr(raw: &str) -> Option<(Option<String>, String)> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(start) = s.find('<')
        && let Some(end) = s.rfind('>')
        && end > start
    {
        let email = s[start + 1..end].trim();
        if email.is_empty() || !email.contains('@') {
            return None;
        }
        let raw_name = s[..start].trim().trim_matches('"').trim();
        let name = if raw_name.is_empty() {
            None
        } else {
            Some(decode_quoted_name(raw_name))
        };
        return Some((name, email.to_string()));
    }
    if !s.contains('@') {
        return None;
    }
    Some((None, s.to_string()))
}

/// Best-effort dequoting of display names. `mail-parser` already RFC
/// 2047-decodes the headers it returns, so all we strip here is
/// extra surrounding double quotes (`"Alice"` → `Alice`) and
/// escaped backslash-quote pairs.
fn decode_quoted_name(raw: &str) -> String {
    let trimmed = raw.trim_matches('"');
    trimmed.replace("\\\"", "\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_and_email() {
        let (name, email) = parse_addr("Alice <alice@example.com>").unwrap();
        assert_eq!(name.as_deref(), Some("Alice"));
        assert_eq!(email, "alice@example.com");
    }

    #[test]
    fn parses_quoted_name() {
        let (name, email) = parse_addr(r#""Alice Q. Smith" <alice@example.com>"#).unwrap();
        assert_eq!(name.as_deref(), Some("Alice Q. Smith"));
        assert_eq!(email, "alice@example.com");
    }

    #[test]
    fn parses_bare_email() {
        let (name, email) = parse_addr("alice@example.com").unwrap();
        assert!(name.is_none());
        assert_eq!(email, "alice@example.com");
    }

    #[test]
    fn rejects_non_email_token() {
        assert!(parse_addr("undisclosed-recipients:;").is_none());
        assert!(parse_addr("").is_none());
        assert!(parse_addr("Bob <not-an-email>").is_none());
    }

    #[test]
    fn contact_renders_with_or_without_name() {
        let c = Contact::from_raw("Alice <alice@example.com>", Source::Native).unwrap();
        assert_eq!(c.render_address(), "Alice <alice@example.com>");
        let c = Contact::from_raw("alice@example.com", Source::Native).unwrap();
        assert_eq!(c.render_address(), "alice@example.com");
    }

    #[test]
    fn matches_email_prefix() {
        let c = Contact::from_raw("Alice <alice@example.com>", Source::Native).unwrap();
        assert!(c.matches_prefix("al"));
        assert!(c.matches_prefix("alice@"));
        assert!(!c.matches_prefix("xample"));
    }

    #[test]
    fn matches_name_word_prefix() {
        let c = Contact::from_raw("Alice Q Smith <a@b.invalid>", Source::Native).unwrap();
        // First word.
        assert!(c.matches_prefix("ali"));
        // Mid-name word.
        assert!(c.matches_prefix("smi"));
        // Not a substring.
        assert!(!c.matches_prefix("ice"));
    }

    #[test]
    fn matches_is_case_insensitive() {
        let c = Contact::from_raw("Alice <Alice@Example.COM>", Source::Native).unwrap();
        assert!(c.matches_prefix("alice@"));
        assert_eq!(c.email_lc, "alice@example.com");
        // Display case preserved.
        assert_eq!(c.email, "Alice@Example.COM");
    }

    #[test]
    fn empty_prefix_matches_everything() {
        let c = Contact::from_raw("Alice <alice@example.com>", Source::Native).unwrap();
        assert!(c.matches_prefix(""));
    }

    #[test]
    fn merge_dedups_external_winning() {
        let ext = vec![Contact {
            name: Some("Alice (khard)".into()),
            email: "alice@example.com".into(),
            email_lc: "alice@example.com".into(),
            source: Source::External,
        }];
        let native = vec![
            Contact {
                name: Some("Alice".into()),
                email: "alice@example.com".into(),
                email_lc: "alice@example.com".into(),
                source: Source::Native,
            },
            Contact {
                name: None,
                email: "bob@example.com".into(),
                email_lc: "bob@example.com".into(),
                source: Source::Native,
            },
        ];
        let merged = merge(ext, native, 10);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].source, Source::External);
        assert_eq!(merged[0].name.as_deref(), Some("Alice (khard)"));
        assert_eq!(merged[1].email_lc, "bob@example.com");
    }

    #[test]
    fn merge_caps_at_limit() {
        let make = |i: u32| Contact {
            name: None,
            email: format!("u{i}@x"),
            email_lc: format!("u{i}@x"),
            source: Source::Native,
        };
        let native: Vec<Contact> = (0..10).map(make).collect();
        let merged = merge(Vec::new(), native, 3);
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn collect_walks_sent_and_dedups_addresses() {
        use crate::config::Account;
        use crate::mail::layout::Layout;
        use std::fs;
        use std::io::Write;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("dev");
        // Maildir++ INBOX + .Sent.
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(root.join(".Sent").join(sub)).unwrap();
        }
        let write_eml = |p: &std::path::Path, mid: &str, to: &str, cc: &str| {
            let mut f = fs::File::create(p).unwrap();
            writeln!(f, "Message-ID: <{mid}>").unwrap();
            writeln!(f, "Date: Thu, 1 Jan 1970 00:00:00 +0000").unwrap();
            writeln!(f, "From: Me <me@example.invalid>").unwrap();
            writeln!(f, "To: {to}").unwrap();
            if !cc.is_empty() {
                writeln!(f, "Cc: {cc}").unwrap();
            }
            writeln!(f, "Subject: t").unwrap();
            writeln!(f).unwrap();
            writeln!(f, "body").unwrap();
        };
        write_eml(
            &root.join(".Sent").join("cur").join("1:2,S"),
            "a",
            "Alice <alice@example.com>",
            "Bob <bob@example.com>",
        );
        // Second message: duplicate Alice (different case), new Carol.
        write_eml(
            &root.join(".Sent").join("cur").join("2:2,S"),
            "b",
            "ALICE@example.com",
            "Carol <carol@example.com>",
        );

        let acc = Account {
            maildir: root.clone(),
            from: "Me <me@example.invalid>".into(),
            layout: Layout::Maildirpp,
            inbox: None,
            archive: None,
            sent: Some("Sent".into()),
            spam: None,
            trash: None,
            drafts: None,
            extra_folders: Vec::new(),
            smtp: None,
            primary: false,
        };
        let spec = AccountSpec::from_account("dev", &acc);
        let contacts = collect_from_specs(&[spec]);
        let emails: std::collections::HashSet<String> =
            contacts.iter().map(|c| c.email_lc.clone()).collect();
        assert!(emails.contains("alice@example.com"));
        assert!(emails.contains("bob@example.com"));
        assert!(emails.contains("carol@example.com"));
        assert!(emails.contains("me@example.invalid"));
        // Dedup: ALICE@example.com collapsed with Alice@example.com.
        let alice = contacts
            .iter()
            .filter(|c| c.email_lc == "alice@example.com")
            .count();
        assert_eq!(alice, 1, "Alice should dedupe across messages");
        // First-seen non-empty name wins.
        let alice = contacts
            .iter()
            .find(|c| c.email_lc == "alice@example.com")
            .unwrap();
        assert_eq!(alice.name.as_deref(), Some("Alice"));
    }
}
