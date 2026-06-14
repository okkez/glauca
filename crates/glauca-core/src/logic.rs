// framework 非依存のビジネスロジック。TUI / GUI 双方から利用する。

use std::borrow::Cow;

use crate::db::CachedItem;
use crate::filter::FilterQuery;
use crate::types::{ItemEntry, LeftPaneEntry};

/// An item is "new" if it was cached after the entry was last viewed.
pub fn is_item_new_since(cached_at: &str, last_viewed_at: Option<&str>) -> bool {
    last_viewed_at
        .map(|last_viewed_at| cached_at > last_viewed_at)
        .unwrap_or(true)
}

/// Labels / reviewers / assignees are stored as a JSON array string,
/// e.g. '["bug","enhancement"]'.
pub fn serde_labels(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

/// Reviews stored as [{"login":"alice","state":"APPROVED"}].
pub fn serde_reviews(raw: &str) -> Vec<(String, String)> {
    serde_json::from_str::<Vec<serde_json::Value>>(raw)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| {
            let login = v["login"].as_str()?.to_string();
            let state = v["state"].as_str()?.to_string();
            Some((login, state))
        })
        .collect()
}

pub fn cached_item_to_item_entry(c: CachedItem, last_viewed_at: Option<&str>) -> ItemEntry {
    // A read item is neither "new" (no highlight) nor counted as unread.
    let is_new = is_item_new_since(&c.cached_at, last_viewed_at) && !c.read;
    ItemEntry {
        number: c.number,
        title: c.title,
        repo_owner: c.repo_owner,
        repo_name: c.repo_name,
        author: c.author,
        state: c.state,
        updated_at: c.updated_at,
        labels: serde_labels(&c.labels),
        url: c.url,
        comment_count: c.comment_count,
        kind: c.kind,
        requested_reviewers: serde_labels(&c.requested_reviewers),
        reviews: serde_reviews(&c.reviews),
        body: c.body,
        assignees: serde_labels(&c.assignees),
        is_draft: c.is_draft,
        created_at_item: c.created_at_item,
        base_ref: c.base_ref,
        head_ref: c.head_ref,
        review_decision: c.review_decision,
        milestone: c.milestone,
        cached_at: c.cached_at,
        is_new,
        read: c.read,
    }
}

/// Replace `@me` with the authenticated user's login (case-insensitive).
/// Falls back to `@me` unchanged if the user is not known yet.
pub fn expand_me<'a>(current_user: Option<&str>, s: &'a str) -> Cow<'a, str> {
    if let Some(login) = current_user {
        // Only replace the token `@me` (whole word match within tokens).
        if s.contains("@me") {
            return Cow::Owned(
                s.split_whitespace()
                    .map(|tok| {
                        if tok.to_lowercase().ends_with(":@me") {
                            let prefix = &tok[..tok.len() - 3]; // strip "@me"
                            format!("{}{}", prefix, login)
                        } else if tok == "@me" {
                            login.to_string()
                        } else {
                            tok.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
    }
    Cow::Borrowed(s)
}

/// Filter `items` by an optional stream filter ANDed with the inline filter.
/// `@me` tokens are expanded against `current_user`.
pub fn filter_items<'a>(
    items: &'a [ItemEntry],
    stream_filter: Option<&str>,
    inline_filter: &str,
    current_user: Option<&str>,
) -> Vec<&'a ItemEntry> {
    let stream_q = stream_filter.map(|s| FilterQuery::parse(&expand_me(current_user, s)));
    let inline_q = FilterQuery::parse(&expand_me(current_user, inline_filter));

    items
        .iter()
        .filter(|i| {
            stream_q.as_ref().map_or(true, |q| q.matches(i))
                && (inline_q.is_empty() || inline_q.matches(i))
        })
        .collect()
}

/// Compute unread counts for every left-pane entry belonging to `query_id`.
/// Returns `(entry_id, unread_count)` pairs for the caller to store.
pub fn compute_unread_counts(
    entries: &[LeftPaneEntry],
    query_id: i64,
    items: &[ItemEntry],
    current_user: Option<&str>,
) -> Vec<((bool, i64), usize)> {
    let mut out = Vec::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.root_query_id() == query_id)
    {
        let unread = match entry {
            LeftPaneEntry::Query(q) => items
                .iter()
                .filter(|item| {
                    is_item_new_since(&item.cached_at, q.last_viewed_at.as_deref()) && !item.read
                })
                .count(),
            LeftPaneEntry::FilterStream(fs) => {
                let filter = FilterQuery::parse(&expand_me(current_user, &fs.filter));
                items
                    .iter()
                    .filter(|item| {
                        is_item_new_since(&item.cached_at, fs.last_viewed_at.as_deref())
                            && !item.read
                            && filter.matches(item)
                    })
                    .count()
            }
        };
        out.push((entry.unread_key(), unread));
    }
    out
}

pub fn group_range(entries: &[LeftPaneEntry], query_idx: usize) -> std::ops::Range<usize> {
    let end = entries[query_idx + 1..]
        .iter()
        .position(|e| matches!(e, LeftPaneEntry::Query(_)))
        .map(|i| query_idx + 1 + i)
        .unwrap_or(entries.len());
    query_idx..end
}

/// Moves the query group at `query_idx` one position down (past the next query
/// group). Returns the new index of the moved group, or `None` if it was
/// already at the bottom.
pub fn move_group_down(entries: &mut Vec<LeftPaneEntry>, query_idx: usize) -> Option<usize> {
    let range_a = group_range(entries, query_idx);
    let next_query_idx = range_a.end;
    if next_query_idx >= entries.len() {
        return None; // already at the bottom
    }
    let range_b = group_range(entries, next_query_idx);

    // Drain higher indices first to keep lower indices valid.
    let group_b: Vec<_> = entries.drain(range_b.clone()).collect();
    let group_a: Vec<_> = entries.drain(range_a.clone()).collect();
    let insert_at = range_a.start;
    let b_len = group_b.len();
    for (i, item) in group_b.into_iter().chain(group_a).enumerate() {
        entries.insert(insert_at + i, item);
    }
    Some(insert_at + b_len) // new start index of the moved group
}
