// framework 非依存のビジネスロジック。TUI / GUI 双方から利用する。

use std::borrow::Cow;

use crate::db::CachedItem;
use crate::filter::FilterQuery;
use crate::types::{ItemEntry, LeftPaneEntry, UserRef};

/// An item is "new" if it was cached after the entry was last viewed.
pub fn is_item_new_since(cached_at: &str, last_viewed_at: Option<&str>) -> bool {
    last_viewed_at
        .map(|last_viewed_at| cached_at > last_viewed_at)
        .unwrap_or(true)
}

/// Labels are stored as a JSON array string, e.g. '["bug","enhancement"]'.
pub fn serde_labels(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

/// Reviewers / assignees are stored as a JSON array of objects, e.g.
/// '[{"login":"alice","avatar_url":"https://…"}]'. Older cache rows hold a
/// plain string array ('["alice"]'); fall back to that for backward compat
/// (those rows render without avatars until the next re-sync).
pub fn serde_users(raw: &str) -> Vec<UserRef> {
    if let Ok(users) = serde_json::from_str::<Vec<UserRef>>(raw) {
        return users;
    }
    serde_json::from_str::<Vec<String>>(raw)
        .unwrap_or_default()
        .into_iter()
        .map(UserRef::new)
        .collect()
}

/// Reviews stored as [{"login":"alice","state":"APPROVED","avatar_url":"…"}].
/// Returns (user, state) pairs; `avatar_url` is optional.
pub fn serde_reviews(raw: &str) -> Vec<(UserRef, String)> {
    serde_json::from_str::<Vec<serde_json::Value>>(raw)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| {
            let login = v["login"].as_str()?.to_string();
            let state = v["state"].as_str()?.to_string();
            let avatar_url = v["avatar_url"].as_str().map(|s| s.to_string());
            Some((UserRef { login, avatar_url }, state))
        })
        .collect()
}

/// Review state of a reviewer, for the item-list overlay badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    /// Requested as a reviewer but has not submitted a review yet.
    Pending,
}

impl ReviewState {
    /// Map a GitHub review `state` string to a [`ReviewState`]. Unknown values
    /// fall back to `Commented`.
    pub fn from_state(state: &str) -> Self {
        match state {
            "APPROVED" => ReviewState::Approved,
            "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
            "DISMISSED" => ReviewState::Dismissed,
            "PENDING" => ReviewState::Pending,
            _ => ReviewState::Commented,
        }
    }
}

/// Reviewers to show on an item, as (user, state): everyone who submitted a
/// review (with their state) plus requested reviewers who have not yet reviewed
/// (as `Pending`). Mirrors the TUI detail-pane reviewer logic.
pub fn reviewer_overlays(item: &ItemEntry) -> Vec<(UserRef, ReviewState)> {
    let mut out: Vec<(UserRef, ReviewState)> = item
        .reviews
        .iter()
        .map(|(u, state)| (u.clone(), ReviewState::from_state(state)))
        .collect();
    for u in &item.requested_reviewers {
        if !out.iter().any(|(ru, _)| ru.login == u.login) {
            out.push((u.clone(), ReviewState::Pending));
        }
    }
    out
}

pub fn cached_item_to_item_entry(c: CachedItem, last_viewed_at: Option<&str>) -> ItemEntry {
    // A read item is neither "new" (no highlight) nor counted as unread.
    let is_new = is_item_new_since(&c.cached_at, last_viewed_at) && !c.read;
    ItemEntry {
        number: c.number,
        title: c.title,
        repo_owner: c.repo_owner,
        repo_name: c.repo_name,
        repo_private: c.repo_private,
        author: c.author.map(|login| UserRef {
            login,
            avatar_url: c.author_avatar_url,
        }),
        state: c.state,
        updated_at: c.updated_at,
        labels: serde_labels(&c.labels),
        url: c.url,
        comment_count: c.comment_count,
        kind: c.kind,
        requested_reviewers: serde_users(&c.requested_reviewers),
        reviews: serde_reviews(&c.reviews),
        body: c.body,
        assignees: serde_users(&c.assignees),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_users_parses_new_object_array() {
        let users = serde_users(r#"[{"login":"alice","avatar_url":"https://a/x.png"}]"#);
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].login, "alice");
        assert_eq!(users[0].avatar_url.as_deref(), Some("https://a/x.png"));
    }

    #[test]
    fn serde_users_falls_back_to_legacy_string_array() {
        // Old cache rows stored a plain string array; they should still parse
        // (with no avatar) until the next re-sync.
        let users = serde_users(r#"["bob","carol"]"#);
        let logins: Vec<&str> = users.iter().map(|u| u.login.as_str()).collect();
        assert_eq!(logins, vec!["bob", "carol"]);
        assert!(users.iter().all(|u| u.avatar_url.is_none()));
    }

    #[test]
    fn serde_reviews_extracts_user_state_and_avatar() {
        let reviews =
            serde_reviews(r#"[{"login":"alice","state":"APPROVED","avatar_url":"https://a"}]"#);
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].0.login, "alice");
        assert_eq!(reviews[0].0.avatar_url.as_deref(), Some("https://a"));
        assert_eq!(reviews[0].1, "APPROVED");
    }

    #[test]
    fn review_state_from_state_maps_all_variants() {
        assert_eq!(ReviewState::from_state("APPROVED"), ReviewState::Approved);
        assert_eq!(
            ReviewState::from_state("CHANGES_REQUESTED"),
            ReviewState::ChangesRequested
        );
        assert_eq!(ReviewState::from_state("COMMENTED"), ReviewState::Commented);
        assert_eq!(ReviewState::from_state("DISMISSED"), ReviewState::Dismissed);
        assert_eq!(ReviewState::from_state("PENDING"), ReviewState::Pending);
        // Unknown / unexpected values fall back to Commented.
        assert_eq!(ReviewState::from_state("WHATEVER"), ReviewState::Commented);
    }

    #[test]
    fn reviewer_overlays_unions_reviews_and_pending_requests() {
        let mut item = ItemEntry {
            number: 1,
            title: String::new(),
            repo_owner: "o".into(),
            repo_name: "r".into(),
            repo_private: false,
            author: None,
            state: "open".into(),
            updated_at: String::new(),
            labels: vec![],
            url: String::new(),
            comment_count: 0,
            kind: "pull_request".into(),
            requested_reviewers: vec![UserRef::new("carol"), UserRef::new("alice")],
            reviews: vec![(UserRef::new("alice"), "APPROVED".into())],
            body: None,
            assignees: vec![],
            is_draft: false,
            created_at_item: None,
            base_ref: None,
            head_ref: None,
            review_decision: None,
            milestone: None,
            cached_at: String::new(),
            is_new: false,
            read: false,
        };
        let overlays = reviewer_overlays(&item);
        // alice (reviewed) keeps her state; carol (requested only) is pending.
        assert_eq!(overlays.len(), 2);
        assert_eq!(overlays[0].0.login, "alice");
        assert_eq!(overlays[0].1, ReviewState::Approved);
        assert_eq!(overlays[1].0.login, "carol");
        assert_eq!(overlays[1].1, ReviewState::Pending);

        // No requested reviewers, no reviews → empty.
        item.requested_reviewers.clear();
        item.reviews.clear();
        assert!(reviewer_overlays(&item).is_empty());
    }
}
