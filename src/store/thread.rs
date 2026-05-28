use std::collections::{HashMap, HashSet};

use crate::store::index::MessageRow;

#[derive(Debug, Clone)]
pub struct ThreadedRow {
    pub depth: u16,
    pub row: MessageRow,
}

pub fn build_threads(rows: Vec<MessageRow>) -> Vec<ThreadedRow> {
    if rows.is_empty() {
        return vec![];
    }

    // Index rows by msgid for parent lookup. If we see duplicates, keep the
    // first — the upstream caller already deduped by primary key.
    let mut by_msgid: HashMap<String, usize> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        by_msgid.entry(r.msgid.clone()).or_insert(i);
    }

    // Each present message has at most one parent: the last present ancestor
    // in `References`, falling back to `In-Reply-To`. Missing references are
    // ignored — we don't synthesize placeholder containers in v1.
    let mut parent: Vec<Option<usize>> = vec![None; rows.len()];
    for (i, r) in rows.iter().enumerate() {
        let candidates = r.refs.iter().rev().chain(r.in_reply.as_ref());
        for c in candidates {
            if let Some(&p) = by_msgid.get(c)
                && p != i
            {
                parent[i] = Some(p);
                break;
            }
        }
    }

    // Children, plus the latest descendant date per node for root ordering.
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
    for (i, p) in parent.iter().enumerate() {
        if let Some(p) = p {
            children[*p].push(i);
        }
    }
    let mut latest_in_subtree: Vec<i64> = rows.iter().map(|r| r.date).collect();
    // Iterate from "deepest" first by processing in date-descending order; for
    // a small N this is fine and avoids a recursive walk for the bound.
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(rows[i].date));
    for &i in &order {
        if let Some(p) = parent[i]
            && latest_in_subtree[i] > latest_in_subtree[p]
        {
            latest_in_subtree[p] = latest_in_subtree[i];
        }
    }

    // Roots = nodes with no parent (or whose parent isn't in our slice).
    let mut roots: Vec<usize> = (0..rows.len()).filter(|i| parent[*i].is_none()).collect();
    roots.sort_by_key(|&i| std::cmp::Reverse(latest_in_subtree[i]));

    // Sort children by date ascending so the reply chain reads top-down.
    for siblings in children.iter_mut() {
        siblings.sort_by_key(|&i| rows[i].date);
    }

    let mut out: Vec<ThreadedRow> = Vec::with_capacity(rows.len());
    let mut visited: HashSet<usize> = HashSet::with_capacity(rows.len());
    let rows_ref = &rows;
    for r in roots {
        walk(r, 0, &children, rows_ref, &mut visited, &mut out);
    }
    // Defensive: if a parent-pointer cycle slipped through, drop unvisited
    // rows in as a flat tail rather than spinning.
    for (i, r) in rows.into_iter().enumerate() {
        if !visited.contains(&i) {
            out.push(ThreadedRow { depth: 0, row: r });
        }
    }
    out
}

fn walk(
    i: usize,
    depth: u16,
    children: &[Vec<usize>],
    rows: &[MessageRow],
    visited: &mut HashSet<usize>,
    out: &mut Vec<ThreadedRow>,
) {
    if !visited.insert(i) {
        return;
    }
    out.push(ThreadedRow {
        depth,
        row: rows[i].clone(),
    });
    for &c in &children[i] {
        walk(c, depth.saturating_add(1), children, rows, visited, out);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn row(msgid: &str, date: i64, in_reply: Option<&str>, refs: &[&str]) -> MessageRow {
        MessageRow {
            msgid: msgid.into(),
            account: "dev".into(),
            folder: "INBOX".into(),
            path: PathBuf::from("/x"),
            date,
            from_addr: None,
            subject: Some(msgid.into()),
            in_reply: in_reply.map(str::to_owned),
            refs: refs.iter().map(|s| s.to_string()).collect(),
            flags: String::new(),
        }
    }

    #[test]
    fn standalone_root_sorted_by_date_desc() {
        let out = build_threads(vec![row("a", 100, None, &[]), row("b", 200, None, &[])]);
        let msgs: Vec<&str> = out.iter().map(|t| t.row.msgid.as_str()).collect();
        assert_eq!(msgs, vec!["b", "a"]);
        assert!(out.iter().all(|t| t.depth == 0));
    }

    #[test]
    fn reply_indented_under_root() {
        let out = build_threads(vec![
            row("root", 100, None, &[]),
            row("r1", 200, Some("root"), &["root"]),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].row.msgid, "root");
        assert_eq!(out[0].depth, 0);
        assert_eq!(out[1].row.msgid, "r1");
        assert_eq!(out[1].depth, 1);
    }

    #[test]
    fn chain_three_deep_preserves_order() {
        let out = build_threads(vec![
            row("r1", 200, Some("root"), &["root"]),
            row("r2", 300, Some("r1"), &["root", "r1"]),
            row("root", 100, None, &[]),
        ]);
        let pairs: Vec<(u16, &str)> = out
            .iter()
            .map(|t| (t.depth, t.row.msgid.as_str()))
            .collect();
        assert_eq!(pairs, vec![(0, "root"), (1, "r1"), (2, "r2")]);
    }

    #[test]
    fn root_with_newer_reply_sorts_above_older_standalone() {
        let out = build_threads(vec![
            row("old-root", 100, None, &[]),
            row("recent-reply", 500, Some("old-root"), &["old-root"]),
            row("alone", 300, None, &[]),
        ]);
        let firsts: Vec<&str> = out
            .iter()
            .filter(|t| t.depth == 0)
            .map(|t| t.row.msgid.as_str())
            .collect();
        assert_eq!(firsts, vec!["old-root", "alone"]);
    }

    #[test]
    fn orphan_reply_with_missing_parent_treated_as_root() {
        let out = build_threads(vec![row("r1", 200, Some("missing"), &["missing"])]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].depth, 0);
    }
}
