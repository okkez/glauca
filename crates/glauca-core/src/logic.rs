// framework 非依存のビジネスロジック。TUI / GUI 双方から利用する。

use std::borrow::Cow;

use crate::db::CachedItem;
use crate::filter::{FilterQuery, StreamFilter};
use crate::types::{ActorKind, ItemEntry, LeftPaneEntry, UserRef};

/// An item is unread iff its current `updated_at` is newer than the `updated_at`
/// the user had seen when they last read it. Never-read items (`None`) are always
/// unread. Unread items are highlighted as "new" and counted in the unread badge.
///
/// String comparison is valid because every `updated_at` is RFC3339 UTC (`…Z`), so
/// lexicographic order equals chronological order (the same assumption behind the
/// `ORDER BY updated_at DESC` in `db::fetch_items`).
pub fn is_item_unread(updated_at: &str, last_read_updated_at: Option<&str>) -> bool {
    last_read_updated_at
        .map(|seen| updated_at > seen)
        .unwrap_or(true)
}

/// What a freshly synced list changed relative to the list currently on screen.
/// Items are keyed by (repo_owner, repo_name, number). Drives the deferred-refresh
/// banner shown when a background sync's results are held back from the view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChangeCounts {
    /// Items new to the list, or whose `updated_at` advanced.
    pub updated: usize,
    /// Items on screen but absent from the fresh list. Usually they stopped matching
    /// the query and were pruned (`db::prune_missing_items`), though the cache-size
    /// sweep (`db::prune_query_overflow`) can remove rows too.
    ///
    /// Counting these is what makes a removal-only background sync visible. A
    /// front-end that looked at `updated` alone would see zero changes, discard the
    /// pruned list, and keep displaying the removed item forever.
    pub removed: usize,
}

impl ChangeCounts {
    /// Every change, of either kind.
    pub fn total(&self) -> usize {
        self.updated + self.removed
    }

    /// Nothing changed, so no banner should be shown and the held-back list can be
    /// dropped. This is the check front-ends gate on.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// Banner text for the deferred-refresh affordance: "3 updated",
    /// "2 no longer match", or "3 updated, 2 no longer match". Empty when nothing
    /// changed — callers show no banner then.
    pub fn banner_label(&self) -> String {
        match (self.updated, self.removed) {
            (0, 0) => String::new(),
            (u, 0) => format!("{u} updated"),
            (0, r) => format!("{r} no longer match"),
            (u, r) => format!("{u} updated, {r} no longer match"),
        }
    }
}

/// Diff `fresh` against `current`, counting new/updated items and removals.
///
/// Note the deliberate asymmetry with [`crate::notify::ItemTracker`]: desktop
/// notifications count only the `updated` side, so a removal never fires one.
///
/// TODO(known limitation): the diff key is (owner, repo, number) plus `updated_at`, so
/// a field that changes *without* `updated_at` advancing is not counted and the
/// held-back list is discarded. The cached row is already correct by then; the view
/// catches up on the next foreground load. Applying silently when `is_empty()` is
/// deliberately not done — `MarkItemRead` is fire-and-forget, so an in-flight fresh
/// list could resurrect an item the user just read as unread.
pub fn count_changes(current: &[ItemEntry], fresh: &[ItemEntry]) -> ChangeCounts {
    type Key<'a> = (&'a str, &'a str, i64);
    fn key(it: &ItemEntry) -> Key<'_> {
        (it.repo_owner.as_str(), it.repo_name.as_str(), it.number)
    }

    let seen: std::collections::HashMap<Key<'_>, &str> = current
        .iter()
        .map(|it| (key(it), it.updated_at.as_str()))
        .collect();
    let updated = fresh
        .iter()
        .filter(|it| match seen.get(&key(it)) {
            None => true,                                         // newly appeared
            Some(prev_updated) => *prev_updated != it.updated_at, // changed
        })
        .count();

    let fresh_keys: std::collections::HashSet<Key<'_>> = fresh.iter().map(key).collect();
    let removed = current
        .iter()
        .filter(|it| !fresh_keys.contains(&key(it)))
        .count();

    ChangeCounts { updated, removed }
}

/// Labels are stored as a JSON array string, e.g. '["bug","enhancement"]'.
pub fn decode_labels(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

/// Reviewers / assignees are stored as a JSON array of objects, e.g.
/// '[{"login":"alice","avatar_url":"https://…","kind":"user"}]'. Rows written
/// before `kind` existed omit it and decode as users (`ActorKind`'s
/// `#[serde(default)]`). Older cache rows hold a plain string array
/// ('["alice"]'); fall back to that for backward compat (those rows render
/// without avatars until the next re-sync).
pub fn decode_users(raw: &str) -> Vec<UserRef> {
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
pub fn decode_reviews(raw: &str) -> Vec<(UserRef, String)> {
    serde_json::from_str::<Vec<serde_json::Value>>(raw)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| {
            let login = v["login"].as_str()?.to_string();
            let state = v["state"].as_str()?.to_string();
            let avatar_url = v["avatar_url"].as_str().map(|s| s.to_string());
            Some((
                UserRef {
                    login,
                    avatar_url,
                    kind: ActorKind::User,
                },
                state,
            ))
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

pub fn cached_item_to_item_entry(c: CachedItem) -> ItemEntry {
    // Unread (highlighted as new) iff updated since the user last read it.
    let is_new = is_item_unread(&c.updated_at, c.last_read_updated_at.as_deref());
    ItemEntry {
        number: c.number,
        title: c.title,
        repo_owner: c.repo_owner,
        repo_name: c.repo_name,
        repo_private: c.repo_private,
        author: c.author.map(|login| UserRef {
            login,
            avatar_url: c.author_avatar_url,
            kind: ActorKind::User,
        }),
        state: c.state,
        updated_at: c.updated_at,
        labels: decode_labels(&c.labels),
        url: c.url,
        comment_count: c.comment_count,
        kind: c.kind,
        requested_reviewers: decode_users(&c.requested_reviewers),
        reviews: decode_reviews(&c.reviews),
        body: c.body,
        assignees: decode_users(&c.assignees),
        is_draft: c.is_draft,
        created_at_item: c.created_at_item,
        base_ref: c.base_ref,
        head_ref: c.head_ref,
        review_decision: c.review_decision,
        milestone: c.milestone,
        last_read_updated_at: c.last_read_updated_at,
        is_new,
    }
}

/// Replace `@me` with the authenticated user's login (case-insensitive).
/// Falls back to `@me` unchanged if the user is not known yet.
pub fn expand_me<'a>(current_user: Option<&str>, s: &'a str) -> Cow<'a, str> {
    match current_user {
        // Only rewrite when `@me` actually appears; otherwise borrow unchanged.
        Some(login) if s.contains("@me") => Cow::Owned(
            // Expand per line so newline group separators survive: a filter
            // stream's `filter` holds one OR-group per line (see
            // `filter::StreamFilter`), and collapsing `\n` into a space here
            // would merge the groups into a single AND-group.
            s.split('\n')
                .map(|line| expand_me_line(line, login))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => Cow::Borrowed(s),
    }
}

/// Replace `@me` / `:@me` tokens on a single (newline-free) line with `login`.
fn expand_me_line(line: &str, login: &str) -> String {
    line.split_whitespace()
        .map(|tok| {
            if tok.to_lowercase().ends_with(":@me") {
                let prefix = &tok[..tok.len() - 3]; // strip "@me"
                format!("{prefix}{login}")
            } else if tok == "@me" {
                login.to_string()
            } else {
                tok.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Filter `items` by an optional stream filter ANDed with the inline filter.
/// `@me` tokens are expanded against `current_user`.
pub fn filter_items<'a>(
    items: &'a [ItemEntry],
    stream_filter: Option<&str>,
    inline_filter: &str,
    current_user: Option<&str>,
) -> Vec<&'a ItemEntry> {
    filter_item_indices(items, stream_filter, inline_filter, current_user)
        .into_iter()
        .map(|i| &items[i])
        .collect()
}

/// Like [`filter_items`], but returns the indices of the matching items instead
/// of borrowing them. Callers that memoize the filter result store these
/// (indices don't borrow `items`, so they can be cached) and map back on demand.
pub fn filter_item_indices(
    items: &[ItemEntry],
    stream_filter: Option<&str>,
    inline_filter: &str,
    current_user: Option<&str>,
) -> Vec<usize> {
    let stream_q = stream_filter.map(|s| StreamFilter::parse(s, current_user));
    let inline_q = FilterQuery::parse(&expand_me(current_user, inline_filter));

    items
        .iter()
        .enumerate()
        .filter(|(_, i)| {
            stream_q.as_ref().is_none_or(|q| q.matches(i))
                && (inline_q.is_empty() || inline_q.matches(i))
        })
        .map(|(idx, _)| idx)
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
            LeftPaneEntry::Query(_) => items
                .iter()
                .filter(|item| {
                    is_item_unread(&item.updated_at, item.last_read_updated_at.as_deref())
                })
                .count(),
            LeftPaneEntry::FilterStream(fs) => {
                let filter = StreamFilter::parse(&fs.filter, current_user);
                items
                    .iter()
                    .filter(|item| {
                        is_item_unread(&item.updated_at, item.last_read_updated_at.as_deref())
                            && filter.matches(item)
                    })
                    .count()
            }
        };
        out.push((entry.unread_key(), unread));
    }
    out
}

/// Display label of the root query with `query_id`, or `None` if no such root
/// query is in `entries`.
pub fn query_label(entries: &[LeftPaneEntry], query_id: i64) -> Option<String> {
    entries.iter().find_map(|e| match e {
        LeftPaneEntry::Query(q) if q.id == query_id => Some(q.label.clone()),
        _ => None,
    })
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
    use rstest::rstest;

    #[test]
    fn expand_me_preserves_newline_group_separators() {
        // Filter-stream OR-groups are newline-separated; expanding `@me` must
        // keep them as separate lines, not merge them into one space-joined line.
        assert_eq!(
            expand_me(Some("alice"), "author:@me\nstate:closed"),
            "author:alice\nstate:closed"
        );
        // Multiple lines, mixed tokens.
        assert_eq!(
            expand_me(Some("bob"), "assignee:@me label:bug\n@me"),
            "assignee:bob label:bug\nbob"
        );
    }

    #[test]
    fn decode_users_parses_new_object_array() {
        let users = decode_users(r#"[{"login":"alice","avatar_url":"https://a/x.png"}]"#);
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].login, "alice");
        assert_eq!(users[0].avatar_url.as_deref(), Some("https://a/x.png"));
    }

    #[test]
    fn decode_users_reads_the_actor_kind() {
        let users = decode_users(
            r#"[{"login":"alice","avatar_url":"https://a/x.png","kind":"user"},
                {"login":"my-team","avatar_url":null,"kind":"team"}]"#,
        );
        let kinds: Vec<ActorKind> = users.iter().map(|u| u.kind).collect();
        assert_eq!(kinds, vec![ActorKind::User, ActorKind::Team]);
    }

    #[test]
    fn decode_users_treats_a_missing_kind_as_a_user() {
        // Rows cached before `kind` existed carry no discriminator. Reading them as
        // users is what bounds the upgrade window to "teams look like users until the
        // next full fetch" instead of the reverse, which would mis-render real users.
        let users = decode_users(r#"[{"login":"my-team","avatar_url":null}]"#);
        assert_eq!(users[0].kind, ActorKind::User);
    }

    #[test]
    fn decode_users_falls_back_to_legacy_string_array() {
        // Old cache rows stored a plain string array; they should still parse
        // (with no avatar) until the next re-sync.
        let users = decode_users(r#"["bob","carol"]"#);
        let logins: Vec<&str> = users.iter().map(|u| u.login.as_str()).collect();
        assert_eq!(logins, vec!["bob", "carol"]);
        assert!(users.iter().all(|u| u.avatar_url.is_none()));
        assert!(users.iter().all(|u| u.kind == ActorKind::User));
    }

    #[test]
    fn decode_reviews_extracts_user_state_and_avatar() {
        let reviews =
            decode_reviews(r#"[{"login":"alice","state":"APPROVED","avatar_url":"https://a"}]"#);
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].0.login, "alice");
        assert_eq!(reviews[0].0.avatar_url.as_deref(), Some("https://a"));
        assert_eq!(reviews[0].1, "APPROVED");
    }

    #[rstest]
    #[case::approved("APPROVED", ReviewState::Approved)]
    #[case::changes_requested("CHANGES_REQUESTED", ReviewState::ChangesRequested)]
    #[case::commented("COMMENTED", ReviewState::Commented)]
    #[case::dismissed("DISMISSED", ReviewState::Dismissed)]
    #[case::pending("PENDING", ReviewState::Pending)]
    // Unknown / unexpected values fall back to Commented.
    #[case::unknown_falls_back("WHATEVER", ReviewState::Commented)]
    fn review_state_from_state_maps_all_variants(
        #[case] input: &str,
        #[case] expected: ReviewState,
    ) {
        assert_eq!(ReviewState::from_state(input), expected);
    }

    #[test]
    fn reviewer_overlays_unions_reviews_and_pending_requests() {
        let item = ItemEntry {
            requested_reviewers: vec![UserRef::new("carol"), UserRef::new("alice")],
            reviews: vec![(UserRef::new("alice"), "APPROVED".into())],
            ..Default::default()
        };
        let overlays = reviewer_overlays(&item);
        // alice (reviewed) keeps her state; carol (requested only) is pending.
        assert_eq!(overlays.len(), 2);
        assert_eq!(overlays[0].0.login, "alice");
        assert_eq!(overlays[0].1, ReviewState::Approved);
        assert_eq!(overlays[1].0.login, "carol");
        assert_eq!(overlays[1].1, ReviewState::Pending);
    }

    #[test]
    fn reviewer_overlays_empty_when_no_reviews_or_requests() {
        assert!(reviewer_overlays(&ItemEntry::default()).is_empty());
    }

    fn item_at(number: i64, updated_at: &str) -> ItemEntry {
        ItemEntry {
            repo_owner: "okkez".into(),
            repo_name: "glauca".into(),
            number,
            updated_at: updated_at.into(),
            ..Default::default()
        }
    }

    /// `(number, updated_at)` pairs → items, for the table below.
    fn items(spec: &[(i64, &str)]) -> Vec<ItemEntry> {
        spec.iter().map(|(n, at)| item_at(*n, at)).collect()
    }

    #[rstest]
    #[case::identical(&[(1, "old")], &[(1, "old")], 0, 0)]
    #[case::new_and_updated(
        &[(1, "old"), (2, "old")],
        // 1 unchanged, 2 updated, 3 new.
        &[(1, "old"), (2, "new"), (3, "new")],
        2, 0
    )]
    // Regression test: a sync whose only change is a removal must still be counted,
    // or the front-ends discard the pruned list and keep showing the ghost.
    #[case::removal_only(&[(1, "old"), (2, "old")], &[(1, "old")], 0, 1)]
    #[case::update_and_removal(&[(1, "old"), (2, "old")], &[(1, "new")], 1, 1)]
    #[case::empty_current(&[], &[(1, "old"), (2, "old")], 2, 0)]
    #[case::empty_fresh(&[(1, "old"), (2, "old")], &[], 0, 2)]
    fn count_changes_cases(
        #[case] current: &[(i64, &str)],
        #[case] fresh: &[(i64, &str)],
        #[case] want_updated: usize,
        #[case] want_removed: usize,
    ) {
        assert_eq!(
            count_changes(&items(current), &items(fresh)),
            ChangeCounts {
                updated: want_updated,
                removed: want_removed,
            }
        );
    }

    #[rstest]
    #[case::updated_only(3, 0, "3 updated")]
    #[case::removed_only(0, 2, "2 no longer match")]
    #[case::both(3, 2, "3 updated, 2 no longer match")]
    #[case::nothing(0, 0, "")]
    fn banner_label_cases(#[case] updated: usize, #[case] removed: usize, #[case] want: &str) {
        let counts = ChangeCounts { updated, removed };
        assert_eq!(counts.banner_label(), want);
        assert_eq!(counts.is_empty(), want.is_empty());
    }

    #[test]
    fn is_item_unread_never_read_is_unread() {
        assert!(is_item_unread("2026-06-01T00:00:00Z", None));
    }

    #[test]
    fn is_item_unread_updated_after_read_resurfaces() {
        // Read at the older updated_at; a newer update makes it unread again.
        assert!(is_item_unread(
            "2026-06-02T00:00:00Z",
            Some("2026-06-01T00:00:00Z")
        ));
    }

    #[test]
    fn is_item_unread_same_updated_at_is_read() {
        // No change since it was last read → stays read.
        assert!(!is_item_unread(
            "2026-06-01T00:00:00Z",
            Some("2026-06-01T00:00:00Z")
        ));
    }
}
