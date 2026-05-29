//! `/` (local) and `g/` (global) fuzzy search. While `InboxScreen.search`
//! is `Some`, the list pane renders `SearchState.results` in place of the
//! scan threads. The haystack is cached once at mode entry; each keystroke
//! re-scores in memory via `nucleo-matcher` (fast enough at typical mailbox
//! sizes — ≤50K rows × sub-ms per keystroke).
//!
//! Subject + From only for v1; body search arrives later behind FTS5.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NConfig, Matcher, Utf32String};

use crate::store::index::MessageRow;
use crate::ui::text_input::TextInput;

/// Cap visible results to keep the list pane snappy even on huge
/// haystacks. The user narrows by typing rather than scrolling.
const MAX_RESULTS: usize = 200;

#[derive(Debug)]
pub struct SearchState {
    pub kind: SearchKind,
    pub query: TextInput,
    /// Owned message rows the matcher scores against. Captured once at
    /// mode entry from `Index::list_scope` so keystrokes don't re-query.
    pub haystack: Vec<MessageRow>,
    /// Pre-built haystack strings (`"<subject> <from>"`) in nucleo's
    /// preferred form, indexed in sync with `haystack`.
    haystack_strs: Vec<Utf32String>,
    /// Per-row folder-priority index for global search (lower = higher
    /// rank within a score tier). `u16::MAX` means "not in the priority
    /// list" — those still match but sort after listed folders. Local
    /// search uses all-zeroes.
    folder_priority: Vec<u16>,
    /// `(haystack_idx, score)` sorted highest-score first.
    pub results: Vec<(usize, u32)>,
    /// Msgid the cursor was on before search-mode entry — restored on
    /// `Esc` so cancelling a search doesn't lose your place.
    pub prior_selected_msgid: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SearchKind {
    /// `/` — within `(account, folder)`. All results share the active
    /// scope; the folder column in the list pane is redundant but kept
    /// for visual consistency with global results. `account` and
    /// `folder` are recorded for future scope-aware affordances
    /// (e.g. surfacing the active scope in the status line).
    Local {
        #[allow(dead_code)]
        account: Option<String>,
        #[allow(dead_code)]
        folder: String,
    },
    /// `g/` — across `folders` within `account` (honoring the sidebar's
    /// scope; `account = None` crosses every account). `folders` carries
    /// the priority order from `[search].global_folders`; empty means
    /// "every folder, score-only ranking."
    Global {
        #[allow(dead_code)]
        account: Option<String>,
        folders: Vec<String>,
    },
}

impl SearchKind {
    pub fn is_global(&self) -> bool {
        matches!(self, SearchKind::Global { .. })
    }
}

impl SearchState {
    pub fn new(kind: SearchKind, haystack: Vec<MessageRow>, prior_msgid: Option<String>) -> Self {
        let haystack_strs: Vec<Utf32String> = haystack
            .iter()
            .map(|r| {
                let subj = r.subject.as_deref().unwrap_or("");
                let from = r.from_addr.as_deref().unwrap_or("");
                Utf32String::from(format!("{subj} {from}").as_str())
            })
            .collect();
        let folder_priority: Vec<u16> = match &kind {
            SearchKind::Local { .. } => vec![0; haystack.len()],
            SearchKind::Global { folders, .. } => haystack
                .iter()
                .map(|r| {
                    if folders.is_empty() {
                        0
                    } else {
                        folders
                            .iter()
                            .position(|f| f == &r.folder)
                            .map(|i| i as u16)
                            .unwrap_or(u16::MAX)
                    }
                })
                .collect(),
        };
        let mut state = Self {
            kind,
            query: TextInput::new(),
            haystack,
            haystack_strs,
            folder_priority,
            results: Vec::new(),
            prior_selected_msgid: prior_msgid,
        };
        state.refresh();
        state
    }

    /// Look up the row at `result_idx` in the *results* (not the
    /// haystack). Returns `None` when the index is out of range.
    pub fn row(&self, result_idx: usize) -> Option<&MessageRow> {
        self.results
            .get(result_idx)
            .and_then(|(i, _)| self.haystack.get(*i))
    }

    pub fn selected_row(&self, selected: usize) -> Option<&MessageRow> {
        if self.results.is_empty() {
            return None;
        }
        let i = selected.min(self.results.len() - 1);
        self.row(i)
    }

    /// Re-score `haystack` against the current query and refresh
    /// `results`. Empty query: show every row sorted by folder priority
    /// (global) or in haystack order (local; date-DESC from list_scope).
    pub fn refresh(&mut self) {
        let q = self.query.as_str();
        if q.is_empty() {
            self.results = (0..self.haystack.len()).map(|i| (i, 0u32)).collect();
            if matches!(self.kind, SearchKind::Global { .. }) {
                let prio = &self.folder_priority;
                self.results.sort_by(|a, b| prio[a.0].cmp(&prio[b.0]));
            }
            self.results.truncate(MAX_RESULTS);
            return;
        }
        let mut matcher = Matcher::new(NConfig::DEFAULT);
        let pattern = Pattern::parse(q, CaseMatching::Smart, Normalization::Smart);
        let mut scored: Vec<(usize, u32)> = Vec::new();
        for (i, hs) in self.haystack_strs.iter().enumerate() {
            if let Some(score) = pattern.score(hs.slice(..), &mut matcher) {
                scored.push((i, score));
            }
        }
        let prio = &self.folder_priority;
        // Score desc → folder priority asc → haystack idx asc (date DESC
        // from list_scope already, so lower idx = newer).
        scored.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| prio[a.0].cmp(&prio[b.0]))
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(MAX_RESULTS);
        self.results = scored;
    }

    /// Drop a row from the haystack and re-run the matcher. Used after
    /// a cross-folder move on a search result so the moved row stops
    /// showing in `results`.
    pub fn drop_msgid(&mut self, msgid: &str) {
        let Some(idx) = self.haystack.iter().position(|r| r.msgid == msgid) else {
            return;
        };
        self.haystack.remove(idx);
        self.haystack_strs.remove(idx);
        self.folder_priority.remove(idx);
        self.refresh();
    }

    /// Patch `path` / `flags` on a row in the haystack so the bold-unread
    /// list rendering stays accurate after a flag toggle on a search
    /// result. No-op when the msgid isn't in the haystack.
    pub fn patch_row(&mut self, msgid: &str, new_path: &std::path::Path, new_flags: &str) {
        if let Some(r) = self.haystack.iter_mut().find(|r| r.msgid == msgid) {
            r.path = new_path.to_path_buf();
            r.flags = new_flags.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn row(msgid: &str, subject: &str, from: &str, folder: &str) -> MessageRow {
        MessageRow {
            msgid: msgid.into(),
            account: "dev".into(),
            folder: folder.into(),
            path: PathBuf::from("/x"),
            date: 0,
            from_addr: Some(from.into()),
            subject: Some(subject.into()),
            in_reply: None,
            refs: vec![],
            flags: "S".into(),
        }
    }

    #[test]
    fn empty_query_shows_everything() {
        let hs = vec![
            row("<a>", "alpha", "", "INBOX"),
            row("<b>", "beta", "", "INBOX"),
        ];
        let s = SearchState::new(
            SearchKind::Local {
                account: None,
                folder: "INBOX".into(),
            },
            hs,
            None,
        );
        assert_eq!(s.results.len(), 2);
    }

    #[test]
    fn fuzzy_ranks_subject_match_first() {
        let hs = vec![
            row("<a>", "Welcome to epost", "alice@x", "INBOX"),
            row("<b>", "Meeting notes", "bob@x", "INBOX"),
            row("<c>", "Hello there", "carol@x", "INBOX"),
        ];
        let mut s = SearchState::new(
            SearchKind::Local {
                account: None,
                folder: "INBOX".into(),
            },
            hs,
            None,
        );
        for c in "welcome".chars() {
            s.query.insert_char(c);
        }
        s.refresh();
        let top = s.row(0).unwrap();
        assert_eq!(top.msgid, "<a>");
    }

    #[test]
    fn global_priority_orders_within_score_tier() {
        // Two rows tied on score; the one in the higher-priority folder
        // (INBOX < Archive) wins.
        let hs = vec![
            row("<arch>", "report draft", "x@x", "Archive"),
            row("<inb>", "report draft", "y@y", "INBOX"),
        ];
        let mut s = SearchState::new(
            SearchKind::Global {
                account: None,
                folders: vec!["INBOX".into(), "Archive".into()],
            },
            hs,
            None,
        );
        for c in "report".chars() {
            s.query.insert_char(c);
        }
        s.refresh();
        let top = s.row(0).unwrap();
        assert_eq!(
            top.msgid, "<inb>",
            "INBOX outranks Archive in priority tier"
        );
    }

    #[test]
    fn drop_msgid_removes_from_results() {
        let hs = vec![
            row("<a>", "alpha", "", "INBOX"),
            row("<b>", "beta", "", "INBOX"),
        ];
        let mut s = SearchState::new(
            SearchKind::Local {
                account: None,
                folder: "INBOX".into(),
            },
            hs,
            None,
        );
        s.drop_msgid("<a>");
        assert_eq!(s.results.len(), 1);
        assert_eq!(s.row(0).unwrap().msgid, "<b>");
    }

    #[test]
    fn patch_row_updates_flags() {
        let hs = vec![row("<a>", "alpha", "", "INBOX")];
        let mut s = SearchState::new(
            SearchKind::Local {
                account: None,
                folder: "INBOX".into(),
            },
            hs,
            None,
        );
        s.patch_row("<a>", std::path::Path::new("/new"), "ST");
        let r = s.row(0).unwrap();
        assert_eq!(r.flags, "ST");
        assert_eq!(r.path, std::path::PathBuf::from("/new"));
    }
}
