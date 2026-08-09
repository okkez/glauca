// framework 非依存のビジネスロジック。TUI / GUI 双方から利用する。

use std::borrow::Cow;

use crate::db::CachedItem;
use crate::filter::{FilterQuery, StreamFilter};
use crate::types::{ActorKind, EntryKey, ItemEntry, LeftPaneEntry, UserRef};

/// An item is unread iff its current `updated_at` is newer than the `updated_at` the user
/// had seen when they last read it. Never-read items (`None`) are always unread.
///
/// String comparison is valid because every `updated_at` is RFC3339 UTC (`…Z`), so
/// lexicographic order equals chronological order — the same assumption behind the
/// `ORDER BY updated_at DESC` in `db::fetch_items`.
pub fn is_item_unread(updated_at: &str, last_read_updated_at: Option<&str>) -> bool {
    last_read_updated_at
        .map(|seen| updated_at > seen)
        .unwrap_or(true)
}

/// What a freshly synced list changed relative to the list currently on screen, keyed by
/// (repo_owner, repo_name, number). Drives the deferred-refresh banner.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChangeCounts {
    /// Items new to the list, or whose `updated_at` advanced.
    pub updated: usize,
    /// Items on screen but absent from the fresh list — pruned by
    /// `db::prune_missing_items`, or by the cache-size sweep.
    ///
    /// Counting these is what makes a removal-only background sync visible. A front-end
    /// looking at `updated` alone would see zero changes, discard the pruned list, and
    /// keep displaying the removed item forever.
    pub removed: usize,
}

impl ChangeCounts {
    /// Every change, of either kind.
    pub fn total(&self) -> usize {
        self.updated + self.removed
    }

    /// Nothing changed, so no banner is shown and the held-back list can be dropped.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// Banner text: "3 updated", "2 no longer match", or "3 updated, 2 no longer match".
    /// Empty when nothing changed.
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
/// Deliberately asymmetric with [`crate::notify::ItemTracker`]: desktop notifications
/// count only the `updated` side, so a removal never fires one.
///
/// TODO(known limitation): the diff key is (owner, repo, number) plus `updated_at`, so a
/// field that changes *without* `updated_at` advancing is not counted and the held-back
/// list is discarded. The cached row is already correct by then. Applying silently when
/// `is_empty()` is deliberately not done — `MarkItemRead` is fire-and-forget, so an
/// in-flight fresh list could resurrect an item the user just read as unread.
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
/// '[{"login":"alice","avatar_url":"https://…","kind":"user"}]'. Rows written before `kind`
/// existed omit it and decode as users (`UserRef::kind` is `#[serde(default)]`); older rows
/// still hold a plain string array ('["alice"]') and fall back to that, rendering without
/// avatars until the next re-sync.
///
/// A row can also carry a `kind` this binary does not recognise (an older binary reading
/// what a newer one wrote). `ActorKind` has no catch-all variant, so such an element fails
/// to deserialize; salvage the array element-by-element rather than losing every reviewer
/// for one unrecognised entry.
pub fn decode_users(raw: &str) -> Vec<UserRef> {
    if let Ok(users) = serde_json::from_str::<Vec<UserRef>>(raw) {
        return users;
    }
    // Checked before the per-element salvage below, which would otherwise "succeed" on a
    // string array but decode every bare string to nothing.
    if let Ok(logins) = serde_json::from_str::<Vec<String>>(raw) {
        return logins.into_iter().map(UserRef::new).collect();
    }
    serde_json::from_str::<Vec<serde_json::Value>>(raw)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| serde_json::from_value::<UserRef>(v).ok())
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

/// Reviewers to show on an item, as (user, state): everyone who submitted a review, plus
/// requested reviewers who have not yet reviewed (as `Pending`).
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

/// The stand-in for the authenticated user's login in a filter or saved query.
const ME_TOKEN: &str = "@me";

/// Replace `@me` with the authenticated user's login (case-insensitive).
/// Falls back to `@me` unchanged if the user is not known yet.
pub fn expand_me<'a>(current_user: Option<&str>, s: &'a str) -> Cow<'a, str> {
    match current_user {
        // Tokenised rather than a substring test, so the gate accepts exactly what the
        // expansion below rewrites — including `AUTHOR:@ME`.
        Some(login) if has_me_token(s) => Cow::Owned(
            // Expand per line so newline group separators survive: a filter stream's
            // `filter` holds one OR-group per line, and collapsing `\n` into a space
            // would merge the groups into a single AND-group.
            s.split('\n')
                .map(|line| expand_me_line(line, login))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => Cow::Borrowed(s),
    }
}

/// The part of `tok` that survives `@me` substitution, or `None` when the token
/// carries no `@me` to substitute.
///
/// The two accepted spellings are a bare `@me` (prefix `""`) and a qualifier's value,
/// `author:@me` (prefix `author:`, case-insensitive). A token that merely *contains* the
/// letters — a plain search for `@mentions` — is not one of them.
///
/// Both expansion and [`has_unexpanded_me`] route through here, so they cannot disagree
/// about what counts as an `@me`.
fn me_token_prefix(tok: &str) -> Option<&str> {
    if tok.eq_ignore_ascii_case(ME_TOKEN) {
        return Some("");
    }
    // `split_at_checked` rather than slicing: a token can be multibyte, and a cut
    // landing inside a character must yield "not an `@me`", not a panic.
    let me_starts_at = tok.len().checked_sub(ME_TOKEN.len())?;
    let (prefix, suffix) = tok.split_at_checked(me_starts_at)?;
    (prefix.ends_with(':') && suffix.eq_ignore_ascii_case(ME_TOKEN)).then_some(prefix)
}

/// `true` when any token in `s` is an `@me` (see [`me_token_prefix`]).
///
/// Public for the one caller that has no login to compare against: code handed a string
/// [`expand_me`] has *already* run over, where a surviving `@me` means the login was
/// unknown at expansion time. See `engine`'s `MarkAllRead`.
pub fn has_me_token(s: &str) -> bool {
    s.split_whitespace()
        .any(|tok| me_token_prefix(tok).is_some())
}

/// Replace `@me` / `:@me` tokens on a single (newline-free) line with `login`.
fn expand_me_line(line: &str, login: &str) -> String {
    line.split_whitespace()
        .map(|tok| match me_token_prefix(tok) {
            Some(prefix) => format!("{prefix}{login}"),
            None => tok.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `true` when the filters shaping a view — the filter stream's (`None` for a root
/// query) ANDed with the inline search box — lean on `@me` while the login is
/// unknown, so [`expand_me`] leaves the token literal.
///
/// A literal `@me` doesn't fail in one direction: `author:@me` matches nobody, while
/// `-author:@me` excludes only the login literally named "@me" and so matches *everybody*.
/// Either way the list on screen is not the one asked for and nothing says why — front-ends
/// show [`ME_UNEXPANDED_WARNING`] next to the result.
///
/// It takes both filters rather than one because all three front-ends ask about exactly
/// this pair; three copies of "either of them" would drift.
///
/// Tokenised exactly as expansion is (see [`me_token_prefix`]) rather than by substring:
/// `@mentions` is a search term, and warning about it would teach the user to ignore the
/// warning.
pub fn has_unexpanded_me(
    current_user: Option<&str>,
    stream_filter: Option<&str>,
    inline_filter: &str,
) -> bool {
    current_user.is_none()
        && (stream_filter.is_some_and(has_me_token) || has_me_token(inline_filter))
}

/// What to tell the user when [`has_unexpanded_me`] holds. Lives next to the predicate so
/// the three front-ends explain the same empty list the same way.
///
/// It is tempting to add "retrying…", but the retry gives up on a refusal
/// (`engine::current_user_retry_task`), and a promise of recovery that may never come is
/// worse than the bare cause.
pub const ME_UNEXPANDED_WARNING: &str = "@me not expanded: GitHub login unknown";

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

/// Like [`filter_items`], but returns indices instead of borrowing the items — callers that
/// memoize the filter result can cache these, where borrows of `items` would not.
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
/// Returns `(key, unread_count)` pairs for the caller to store.
pub fn compute_unread_counts(
    entries: &[LeftPaneEntry],
    query_id: i64,
    items: &[ItemEntry],
    current_user: Option<&str>,
) -> Vec<(EntryKey, usize)> {
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
        out.push((entry.key(), unread));
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

/// Resolve the left-pane cursor after `AppMessage::EntriesReloaded`: the position of
/// `active` in the freshly-loaded `entries`, or `fallback_cursor` (clamped) when `active`
/// is no longer present. That absence means the moved row was deleted by another instance
/// between the keypress and this reload — one of the two ways a reorder can be rejected.
///
/// Also reports whether the resolved position holds a *different* entry than `previous`,
/// compared by identity (`EntryKey`) rather than index — true whenever the reload changes
/// which entry is selected, for any reason (the active row was deleted, `active` names a
/// row other than the previously selected one, or the selection moved out from under this
/// reload). The one case this is guaranteed `false` for is the one that matters for
/// avoiding needless work: a successful reorder of the row that was already selected keeps
/// `active` equal to `previous`, so the front-end can skip reloading the item pane.
pub fn resolve_reloaded_selection(
    entries: &[LeftPaneEntry],
    active: EntryKey,
    previous: Option<EntryKey>,
    fallback_cursor: usize,
) -> (usize, bool) {
    let cursor = entries
        .iter()
        .position(|e| e.key() == active)
        .unwrap_or_else(|| fallback_cursor.min(entries.len().saturating_sub(1)));
    let changed = entries.get(cursor).map(|e| e.key()) != previous;
    (cursor, changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::QueryEntry;
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

    /// Case-insensitive in every arm: a token left literal would match nobody.
    #[rstest]
    #[case::uppercase_qualifier("AUTHOR:@ME", "AUTHOR:alice")]
    #[case::mixed_case_bare("@Me", "alice")]
    fn expand_me_rewrites_me_tokens_whatever_their_case(
        #[case] input: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(expand_me(Some("alice"), input), expected);
    }

    /// A token that only *contains* the letters is a search term, not an `@me`.
    #[rstest]
    #[case::substring_lookalike("@mentions")]
    // Multibyte tokens must also not panic on the byte-offset split.
    #[case::multibyte("バグ修正")]
    fn expand_me_leaves_tokens_that_only_look_like_me(#[case] input: &str) {
        assert_eq!(expand_me(Some("alice"), input), input);
    }

    // Either filter shaping the view can be the inert one, and the warning speaks for
    // both. (*Which* strings count as an `@me` is pinned by the agreement test below.)
    #[rstest]
    #[case::stream_filter(Some("author:@me"), "", true)]
    #[case::inline_filter(None, "author:@me", true)]
    #[case::both(Some("author:@me"), "assignee:@me", true)]
    #[case::one_of_several_or_groups(Some("state:open\nauthor:@me"), "", true)]
    #[case::neither(Some("state:open"), "fix", false)]
    #[case::no_filters(None, "", false)]
    fn warning_fires_when_either_filter_needs_a_login(
        #[case] stream_filter: Option<&str>,
        #[case] inline_filter: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(
            has_unexpanded_me(None, stream_filter, inline_filter),
            expected
        );
    }

    #[test]
    fn a_known_login_silences_the_warning() {
        assert!(!has_unexpanded_me(
            Some("alice"),
            Some("author:@me"),
            "assignee:@me"
        ));
    }

    /// The warning must fire on exactly the filters a login would have changed. If the two
    /// disagree, either a working filter is flagged or a broken one stays silent.
    #[rstest]
    #[case::qualifier("author:@me")]
    #[case::bare_token("@me")]
    #[case::uppercase_qualifier("AUTHOR:@ME")]
    #[case::one_of_several_or_groups("state:open\nauthor:@me")]
    #[case::no_me_at_all("author:bob label:bug")]
    // A search term that merely contains the letters is not an `@me`: expansion
    // leaves it alone, so the warning must too.
    #[case::substring_lookalike("@mentions")]
    #[case::substring_in_qualifier("label:@mention")]
    fn warning_fires_exactly_when_a_login_would_change_the_filter(#[case] filter: &str) {
        let a_login_changes_it = expand_me(Some("alice"), filter) != filter;
        assert_eq!(
            has_unexpanded_me(None, Some(filter), ""),
            a_login_changes_it,
            "warning and expansion disagree about {filter:?}"
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
        // Rows cached before `kind` existed carry no discriminator. Reading them as users
        // bounds the upgrade window to "teams look like users until the next full fetch"
        // instead of the reverse, which would mis-render real users.
        let users = decode_users(r#"[{"login":"my-team","avatar_url":null}]"#);
        assert_eq!(users[0].kind, ActorKind::User);
    }

    #[test]
    fn decode_users_falls_back_to_legacy_string_array() {
        // Old cache rows stored a plain string array; they must still parse, without avatars.
        let users = decode_users(r#"["bob","carol"]"#);
        let logins: Vec<&str> = users.iter().map(|u| u.login.as_str()).collect();
        assert_eq!(logins, vec!["bob", "carol"]);
        assert!(users.iter().all(|u| u.avatar_url.is_none()));
        assert!(users.iter().all(|u| u.kind == ActorKind::User));
    }

    #[test]
    fn decode_users_salvages_well_formed_entries_around_an_unrecognised_kind() {
        // An older binary must not lose every reviewer because a newer one wrote a `kind`
        // it doesn't know (e.g. a future `"bot"`): the well-formed entries still come
        // through rather than the whole row decoding to nothing.
        let users = decode_users(
            r#"[{"login":"alice","avatar_url":null,"kind":"user"},
                {"login":"some-bot","avatar_url":null,"kind":"bot"}]"#,
        );
        let logins: Vec<&str> = users.iter().map(|u| u.login.as_str()).collect();
        assert_eq!(logins, vec!["alice"]);
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
        assert!(is_item_unread(
            "2026-06-02T00:00:00Z",
            Some("2026-06-01T00:00:00Z")
        ));
    }

    #[test]
    fn is_item_unread_same_updated_at_is_read() {
        assert!(!is_item_unread(
            "2026-06-01T00:00:00Z",
            Some("2026-06-01T00:00:00Z")
        ));
    }

    fn query(id: i64) -> LeftPaneEntry {
        LeftPaneEntry::Query(QueryEntry {
            id,
            label: format!("q{id}"),
            query_str: "is:open".into(),
            kind: "pull_request".into(),
        })
    }

    /// A successful reorder: `active` is still present at its (possibly new) position,
    /// which is the same entry the caller says was already selected. Regression guard for
    /// the "reload items on every reorder" property called out in the fix — this must
    /// report `changed = false`.
    #[test]
    fn resolve_reloaded_selection_no_change_on_successful_reorder() {
        // [B, A, C] after swapping A and B; A (id 1) is the row that was moved and was
        // already selected before the swap.
        let entries = vec![query(2), query(1), query(3)];
        let active = EntryKey {
            is_filter_stream: false,
            id: 1,
        };
        let previous = Some(active);
        let (cursor, changed) =
            resolve_reloaded_selection(&entries, active, previous, /* fallback */ 0);
        assert_eq!(cursor, 1);
        assert!(!changed);
    }

    /// A rejected reorder where the active row was deleted by another instance in the
    /// meantime: `active` is absent from `entries`, the cursor falls back, and — because
    /// the fallback lands on a different entry than the one previously selected — the
    /// caller must be told to re-select.
    #[test]
    fn resolve_reloaded_selection_reports_change_when_active_row_is_gone() {
        let entries = vec![query(2), query(3)];
        let deleted = EntryKey {
            is_filter_stream: false,
            id: 1,
        };
        // The user had entry id 1 selected (now gone); fallback_cursor mirrors the old
        // cursor position, which now points at a surviving entry (id 2).
        let previous = Some(deleted);
        let (cursor, changed) = resolve_reloaded_selection(&entries, deleted, previous, 0);
        assert_eq!(cursor, 0);
        assert!(changed);
        assert_eq!(entries[cursor].key().id, 2);
    }

    /// A rejected reorder where the active row survives (the pair merely drifted apart,
    /// e.g. a second reorder keypress racing the first's reload): `active` is still found,
    /// so nothing changed even though the reorder itself failed.
    #[test]
    fn resolve_reloaded_selection_no_change_when_active_survives_a_rejected_reorder() {
        let entries = vec![query(1), query(2), query(3)];
        let active = EntryKey {
            is_filter_stream: false,
            id: 1,
        };
        let previous = Some(active);
        let (cursor, changed) = resolve_reloaded_selection(&entries, active, previous, 5);
        assert_eq!(cursor, 0);
        assert!(!changed);
    }
}
