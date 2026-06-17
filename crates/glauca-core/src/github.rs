use crate::db::CachedItem;
use anyhow::{Context, Result};
use octocrab::Octocrab;
use serde::Deserialize;

/// Build an authenticated Octocrab instance.
///
/// Authentication priority:
///   1. `GH_TOKEN` env var (set automatically by `gh` for extensions)
///   2. `GITHUB_TOKEN` env var (GitHub Actions / manual PAT)
///   3. Unauthenticated (rate-limited to 60 req/hour)
pub fn build_client() -> Result<Octocrab> {
    let token = std::env::var("GH_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .ok();

    let mut builder = Octocrab::builder();
    if let Some(t) = token {
        builder = builder.personal_token(t);
    }
    builder.build().map_err(Into::into)
}

/// The authenticated user's login, display name, and avatar.
pub struct CurrentUser {
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

/// Fetch the authenticated user (login + display name + avatar).
///
/// Returns `None` if unauthenticated or the API call fails.
pub async fn get_current_user(client: &Octocrab) -> Option<CurrentUser> {
    #[derive(Deserialize)]
    struct UserResponse {
        login: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        avatar_url: Option<String>,
    }
    client
        .get::<UserResponse, _, _>("https://api.github.com/user", None::<&()>)
        .await
        .ok()
        .map(|u| CurrentUser {
            login: u.login,
            name: u.name,
            avatar_url: u.avatar_url,
        })
}

// ── GraphQL query ─────────────────────────────────────────────────────────────

// Issue/PullRequest field selections shared by the list search and the
// single-item fetch, so both produce the same node shape for
// `node_to_cached_item`. Kept as one string to avoid the two queries drifting.
const ITEM_FIELDS: &str = "
      __typename
      ... on Issue {
        number
        title
        state
        url
        createdAt
        updatedAt
        author { __typename login avatarUrl }
        labels(first: 20) { nodes { name } }
        repository { owner { login } name isPrivate }
        comments { totalCount }
        body
        assignees(first: 10) { nodes { login avatarUrl } }
        milestone { title }
      }
      ... on PullRequest {
        number
        title
        state
        url
        createdAt
        updatedAt
        author { __typename login avatarUrl }
        labels(first: 20) { nodes { name } }
        repository { owner { login } name isPrivate }
        comments { totalCount }
        body
        assignees(first: 10) { nodes { login avatarUrl } }
        milestone { title }
        isDraft
        baseRefName
        headRefName
        reviewDecision
        reviews(last: 30) {
          nodes {
            author { __typename login avatarUrl }
            state
          }
        }
        reviewRequests(first: 20) {
          nodes {
            requestedReviewer {
              ... on User { login avatarUrl }
              ... on Team { slug }
            }
          }
        }
      }
";

/// GraphQL search query (list fetch), built once from the shared `ITEM_FIELDS`.
fn search_query() -> String {
    format!(
        "
query SearchItems($q: String!, $after: String) {{
  search(query: $q, type: ISSUE, first: 100, after: $after) {{
    pageInfo {{ hasNextPage endCursor }}
    nodes {{{ITEM_FIELDS}}}
  }}
}}"
    )
}

/// GraphQL single-item query (refresh one PR/Issue by number).
fn item_query() -> String {
    format!(
        "
query FetchItem($owner: String!, $name: String!, $number: Int!) {{
  repository(owner: $owner, name: $name) {{
    issueOrPullRequest(number: $number) {{{ITEM_FIELDS}}}
  }}
}}"
    )
}

// ── GraphQL response types ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct GqlResponse {
    data: GqlData,
}

#[derive(Deserialize)]
struct GqlData {
    search: GqlSearchConnection,
}

#[derive(Deserialize)]
struct GqlSearchConnection {
    #[serde(rename = "pageInfo")]
    page_info: GqlPageInfo,
    nodes: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct GqlPageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

/// One page of search results returned by `search_page()`.
pub struct SearchPageResult {
    pub items: Vec<CachedItem>,
    pub has_next_page: bool,
    pub end_cursor: Option<String>,
}

/// Append `sort:updated-desc` to `query` if no `sort:` qualifier is already present.
///
/// This ensures the fetch order matches the TUI display order (`updated_at DESC`).
/// If the caller already included a `sort:` qualifier their choice is preserved.
pub(crate) fn apply_default_sort(query: &str) -> std::borrow::Cow<'_, str> {
    if query.contains("sort:") {
        std::borrow::Cow::Borrowed(query)
    } else {
        std::borrow::Cow::Owned(format!("{} sort:updated-desc", query))
    }
}

/// Fetch a single page of GitHub search results using GraphQL.
///
/// Pass `after: None` for the first page, then `Some(end_cursor)` for subsequent pages.
/// GraphQL gives us `reviewRequests` in a single round-trip.
pub async fn search_page(
    client: &Octocrab,
    query_id: i64,
    query: &str,
    after: Option<&str>,
) -> Result<SearchPageResult> {
    let effective_query = apply_default_sort(query);

    let payload = serde_json::json!({
        "query": search_query(),
        "variables": {
            "q": effective_query,
            "after": after,
        }
    });
    let resp: GqlResponse = client
        .graphql(&payload)
        .await
        .context("GraphQL search failed")?;

    let conn = resp.data.search;
    let items = conn
        .nodes
        .iter()
        .filter_map(|node| node_to_cached_item(node, query_id))
        .collect();

    Ok(SearchPageResult {
        items,
        has_next_page: conn.page_info.has_next_page,
        end_cursor: conn.page_info.end_cursor,
    })
}

/// Re-fetch a single PR/Issue by repo + number, returning it tagged with
/// `query_id` so it can be upserted into that query's cache. `Ok(None)` means
/// the item no longer exists (e.g. transferred/deleted).
pub async fn fetch_item(
    client: &Octocrab,
    query_id: i64,
    owner: &str,
    name: &str,
    number: i64,
) -> Result<Option<CachedItem>> {
    let payload = serde_json::json!({
        "query": item_query(),
        "variables": {
            "owner": owner,
            "name": name,
            "number": number,
        }
    });
    let resp: serde_json::Value = client
        .graphql(&payload)
        .await
        .context("GraphQL item fetch failed")?;

    let node = &resp["data"]["repository"]["issueOrPullRequest"];
    if node.is_null() {
        return Ok(None);
    }
    Ok(node_to_cached_item(node, query_id))
}

/// Convert a single GraphQL search node (Issue or PullRequest) to a `CachedItem`.
fn node_to_cached_item(node: &serde_json::Value, query_id: i64) -> Option<CachedItem> {
    let typename = node["__typename"].as_str()?;
    let is_pr = typename == "PullRequest";
    let kind = if is_pr { "pull_request" } else { "issue" };

    let state_raw = node["state"].as_str()?.to_lowercase();
    // GraphQL PR states: OPEN → open, CLOSED → closed, MERGED → merged
    // GraphQL Issue states: OPEN → open, CLOSED → closed
    let state = state_raw.as_str();

    let number = node["number"].as_u64()? as i64;
    let title = node["title"].as_str()?.to_string();
    let url = node["url"].as_str()?.to_string();
    let updated_at = node["updatedAt"].as_str()?.to_string();
    let author = node["author"]["login"].as_str().map(|login| {
        // GraphQL returns "Bot" as __typename for GitHub Apps (e.g. renovate).
        // REST API uses "renovate[bot]" convention — replicate that here.
        if node["author"]["__typename"].as_str() == Some("Bot") {
            format!("{}[bot]", login)
        } else {
            login.to_string()
        }
    });
    let author_avatar_url = node["author"]["avatarUrl"]
        .as_str()
        .map(|s| s.to_string());
    let comment_count = node["comments"]["totalCount"].as_u64().unwrap_or(0) as i64;

    let repo_owner = node["repository"]["owner"]["login"].as_str()?.to_string();
    let repo_name = node["repository"]["name"].as_str()?.to_string();
    let repo_private = node["repository"]["isPrivate"].as_bool().unwrap_or(false);

    let labels = node["labels"]["nodes"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l["name"].as_str())
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // Reviewers / assignees are stored as JSON object arrays carrying the avatar
    // URL: [{"login":"alice","avatar_url":"https://…"}]. Teams (review requests)
    // have only a `slug` and no avatar.
    let requested_reviewers: Vec<serde_json::Value> = if is_pr {
        node["reviewRequests"]["nodes"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|n| {
                        let rv = &n["requestedReviewer"];
                        // User has `login`, Team has `slug`
                        let login = rv["login"].as_str().or_else(|| rv["slug"].as_str())?;
                        let avatar_url = rv["avatarUrl"].as_str();
                        Some(serde_json::json!({"login": login, "avatar_url": avatar_url}))
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };

    // Collect submitted reviews, keeping only the latest state per reviewer.
    // reviews() returns nodes in chronological order so the last entry wins.
    let reviews: Vec<serde_json::Value> = if is_pr {
        let mut map: Vec<(String, String, Option<String>)> = Vec::new();
        if let Some(nodes) = node["reviews"]["nodes"].as_array() {
            for r in nodes {
                if let Some(login) = r["author"]["login"].as_str() {
                    let login = if r["author"]["__typename"].as_str() == Some("Bot") {
                        format!("{}[bot]", login)
                    } else {
                        login.to_string()
                    };
                    let avatar_url = r["author"]["avatarUrl"].as_str().map(|s| s.to_string());
                    if let Some(state) = r["state"].as_str() {
                        if let Some(entry) = map.iter_mut().find(|(l, _, _)| *l == login) {
                            entry.1 = state.to_string();
                            entry.2 = avatar_url;
                        } else {
                            map.push((login, state.to_string(), avatar_url));
                        }
                    }
                }
            }
        }
        map.into_iter()
            .map(|(login, state, avatar_url)| {
                serde_json::json!({"login": login, "state": state, "avatar_url": avatar_url})
            })
            .collect()
    } else {
        vec![]
    };

    let body = node["body"].as_str().map(|s| s.to_string());
    let created_at_item = node["createdAt"].as_str().map(|s| s.to_string());
    let assignees: Vec<serde_json::Value> = node["assignees"]["nodes"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let login = a["login"].as_str()?;
                    let avatar_url = a["avatarUrl"].as_str();
                    Some(serde_json::json!({"login": login, "avatar_url": avatar_url}))
                })
                .collect()
        })
        .unwrap_or_default();
    let milestone = node["milestone"]["title"].as_str().map(|s| s.to_string());

    let is_draft = if is_pr {
        node["isDraft"].as_bool().unwrap_or(false)
    } else {
        false
    };
    let base_ref = if is_pr {
        node["baseRefName"].as_str().map(|s| s.to_string())
    } else {
        None
    };
    let head_ref = if is_pr {
        node["headRefName"].as_str().map(|s| s.to_string())
    } else {
        None
    };
    let review_decision = if is_pr {
        node["reviewDecision"].as_str().map(|s| s.to_string())
    } else {
        None
    };

    Some(CachedItem {
        query_id,
        kind: kind.to_string(),
        repo_owner,
        repo_name,
        repo_private,
        number,
        title,
        url,
        author,
        author_avatar_url,
        state: state.to_string(),
        updated_at,
        labels: serde_json::to_string(&labels).unwrap_or_else(|_| "[]".to_string()),
        comment_count,
        requested_reviewers: serde_json::to_string(&requested_reviewers)
            .unwrap_or_else(|_| "[]".to_string()),
        reviews: serde_json::to_string(&reviews).unwrap_or_else(|_| "[]".to_string()),
        body,
        assignees: serde_json::to_string(&assignees).unwrap_or_else(|_| "[]".to_string()),
        is_draft,
        created_at_item,
        base_ref,
        head_ref,
        review_decision,
        milestone,
        cached_at: String::new(),
        read: false,
    })
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_to_cached_item_issue() {
        let node = serde_json::json!({
            "__typename": "Issue",
            "number": 42,
            "title": "Bug report",
            "state": "OPEN",
            "url": "https://github.com/owner/repo/issues/42",
            "updatedAt": "2026-05-23T10:00:00Z",
            "author": { "__typename": "User", "login": "alice" },
            "labels": { "nodes": [{ "name": "bug" }] },
            "repository": { "owner": { "login": "owner" }, "name": "repo" },
            "comments": { "totalCount": 3 }
        });
        let item = node_to_cached_item(&node, 1).unwrap();
        assert_eq!(item.kind, "issue");
        assert_eq!(item.state, "open");
        assert_eq!(item.number, 42);
        assert_eq!(item.author.as_deref(), Some("alice"));
        assert_eq!(item.comment_count, 3);
        let labels: Vec<String> = serde_json::from_str(&item.labels).unwrap();
        assert_eq!(labels, vec!["bug"]);
        let reviewers: Vec<String> = serde_json::from_str(&item.requested_reviewers).unwrap();
        assert!(reviewers.is_empty());
    }

    #[test]
    fn node_to_cached_item_bot_author() {
        let node = serde_json::json!({
            "__typename": "PullRequest",
            "number": 1,
            "title": "Update deps",
            "state": "OPEN",
            "url": "https://github.com/owner/repo/pull/1",
            "updatedAt": "2026-05-24T10:00:00Z",
            "author": { "__typename": "Bot", "login": "renovate" },
            "labels": { "nodes": [] },
            "repository": { "owner": { "login": "owner" }, "name": "repo" },
            "comments": { "totalCount": 0 },
            "reviewRequests": { "nodes": [] }
        });
        let item = node_to_cached_item(&node, 1).unwrap();
        // Bot authors should have "[bot]" suffix to match REST API convention.
        assert_eq!(item.author.as_deref(), Some("renovate[bot]"));
    }

    #[test]
    fn node_to_cached_item_pr_with_reviewers() {
        let node = serde_json::json!({
            "__typename": "PullRequest",
            "number": 7,
            "title": "Add feature",
            "state": "OPEN",
            "url": "https://github.com/owner/repo/pull/7",
            "updatedAt": "2026-05-23T10:00:00Z",
            "author": { "__typename": "User", "login": "bob" },
            "labels": { "nodes": [] },
            "repository": { "owner": { "login": "owner" }, "name": "repo" },
            "comments": { "totalCount": 0 },
            "reviewRequests": {
                "nodes": [
                    { "requestedReviewer": { "login": "carol" } },
                    { "requestedReviewer": { "slug": "my-team" } }
                ]
            }
        });
        let item = node_to_cached_item(&node, 1).unwrap();
        assert_eq!(item.kind, "pull_request");
        assert_eq!(item.state, "open");
        // requested_reviewers is now a JSON object array carrying avatar URLs.
        let reviewers: Vec<String> = crate::logic::decode_users(&item.requested_reviewers)
            .into_iter()
            .map(|u| u.login)
            .collect();
        assert_eq!(reviewers, vec!["carol", "my-team"]);
    }

    #[test]
    fn node_to_cached_item_parses_avatars_and_private() {
        let node = serde_json::json!({
            "__typename": "PullRequest",
            "number": 8,
            "title": "Avatars",
            "state": "OPEN",
            "url": "https://github.com/owner/repo/pull/8",
            "updatedAt": "2026-05-23T10:00:00Z",
            "author": { "__typename": "User", "login": "bob", "avatarUrl": "https://a/bob.png" },
            "labels": { "nodes": [] },
            "repository": { "owner": { "login": "owner" }, "name": "repo", "isPrivate": true },
            "comments": { "totalCount": 0 },
            "assignees": { "nodes": [{ "login": "carol", "avatarUrl": "https://a/carol.png" }] },
            "reviewRequests": { "nodes": [] },
            "reviews": {
                "nodes": [
                    { "author": { "__typename": "User", "login": "dave", "avatarUrl": "https://a/dave.png" }, "state": "APPROVED" }
                ]
            }
        });
        let item = node_to_cached_item(&node, 1).unwrap();
        assert!(item.repo_private);
        assert_eq!(item.author_avatar_url.as_deref(), Some("https://a/bob.png"));

        let assignees = crate::logic::decode_users(&item.assignees);
        assert_eq!(assignees.len(), 1);
        assert_eq!(assignees[0].login, "carol");
        assert_eq!(assignees[0].avatar_url.as_deref(), Some("https://a/carol.png"));

        let reviews = crate::logic::decode_reviews(&item.reviews);
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].0.login, "dave");
        assert_eq!(reviews[0].0.avatar_url.as_deref(), Some("https://a/dave.png"));
        assert_eq!(reviews[0].1, "APPROVED");
    }

    #[test]
    fn node_to_cached_item_defaults_private_false_and_avatar_none() {
        // Issue node without isPrivate / avatarUrl → safe defaults.
        let node = serde_json::json!({
            "__typename": "Issue",
            "number": 9,
            "title": "No avatar",
            "state": "OPEN",
            "url": "https://github.com/owner/repo/issues/9",
            "updatedAt": "2026-05-23T10:00:00Z",
            "author": { "__typename": "User", "login": "alice" },
            "labels": { "nodes": [] },
            "repository": { "owner": { "login": "owner" }, "name": "repo" },
            "comments": { "totalCount": 0 }
        });
        let item = node_to_cached_item(&node, 1).unwrap();
        assert!(!item.repo_private);
        assert_eq!(item.author_avatar_url, None);
    }

    #[test]
    fn node_to_cached_item_merged_pr() {
        let node = serde_json::json!({
            "__typename": "PullRequest",
            "number": 99,
            "title": "Merged PR",
            "state": "MERGED",
            "url": "https://github.com/owner/repo/pull/99",
            "updatedAt": "2026-05-23T10:00:00Z",
            "author": { "__typename": "User", "login": "dave" },
            "labels": { "nodes": [] },
            "repository": { "owner": { "login": "owner" }, "name": "repo" },
            "comments": { "totalCount": 0 },
            "reviewRequests": { "nodes": [] }
        });
        let item = node_to_cached_item(&node, 1).unwrap();
        assert_eq!(item.state, "merged");
    }

    // ── apply_default_sort ───────────────────────────────────────────────────────

    #[test]
    fn apply_default_sort_appends_when_absent() {
        let result = apply_default_sort("is:pr is:open review-requested:@me");
        assert!(result.ends_with("sort:updated-desc"));
        assert!(result.contains("is:pr"));
    }

    #[test]
    fn apply_default_sort_preserves_explicit_sort() {
        let q = "is:pr sort:created-desc";
        let result = apply_default_sort(q);
        assert_eq!(result, q);
        assert!(!result.contains("sort:updated-desc"));
    }

    #[test]
    fn apply_default_sort_preserves_sort_updated_asc() {
        let q = "is:issue sort:updated-asc";
        let result = apply_default_sort(q);
        assert_eq!(result, q);
    }

    // ── node_to_cached_item: None cases ──────────────────────────────────────────

    #[test]
    fn node_to_cached_item_missing_typename_returns_none() {
        // Missing __typename → can't determine kind → None.
        let node = serde_json::json!({
            "number": 1,
            "title": "test",
            "state": "OPEN",
            "url": "https://github.com/o/r/issues/1",
            "updatedAt": "2026-05-24T00:00:00Z",
            "author": { "__typename": "User", "login": "alice" },
            "labels": { "nodes": [] },
            "repository": { "owner": { "login": "o" }, "name": "r" },
            "comments": { "totalCount": 0 }
        });
        assert!(node_to_cached_item(&node, 1).is_none());
    }

    #[test]
    fn node_to_cached_item_missing_state_returns_none() {
        let node = serde_json::json!({
            "__typename": "Issue",
            "number": 1,
            "title": "test",
            // "state" intentionally omitted
            "url": "https://github.com/o/r/issues/1",
            "updatedAt": "2026-05-24T00:00:00Z",
            "author": { "__typename": "User", "login": "alice" },
            "labels": { "nodes": [] },
            "repository": { "owner": { "login": "o" }, "name": "r" },
            "comments": { "totalCount": 0 }
        });
        assert!(node_to_cached_item(&node, 1).is_none());
    }

    #[test]
    fn node_to_cached_item_pr_missing_review_requests_is_empty() {
        // A PR node without the reviewRequests field should yield an empty list.
        let node = serde_json::json!({
            "__typename": "PullRequest",
            "number": 5,
            "title": "PR without reviewRequests field",
            "state": "OPEN",
            "url": "https://github.com/o/r/pull/5",
            "updatedAt": "2026-05-24T00:00:00Z",
            "author": { "__typename": "User", "login": "alice" },
            "labels": { "nodes": [] },
            "repository": { "owner": { "login": "o" }, "name": "r" },
            "comments": { "totalCount": 0 }
            // "reviewRequests" intentionally omitted
        });
        let item = node_to_cached_item(&node, 1).unwrap();
        let reviewers: Vec<String> = serde_json::from_str(&item.requested_reviewers).unwrap();
        assert!(reviewers.is_empty());
    }

    #[test]
    fn node_to_cached_item_null_author_is_none() {
        // GitHub sometimes returns null for author (e.g. deleted accounts).
        let node = serde_json::json!({
            "__typename": "Issue",
            "number": 7,
            "title": "ghost issue",
            "state": "OPEN",
            "url": "https://github.com/o/r/issues/7",
            "updatedAt": "2026-05-24T00:00:00Z",
            "author": null,
            "labels": { "nodes": [] },
            "repository": { "owner": { "login": "o" }, "name": "r" },
            "comments": { "totalCount": 0 }
        });
        let item = node_to_cached_item(&node, 1).unwrap();
        assert!(item.author.is_none());
    }
}
