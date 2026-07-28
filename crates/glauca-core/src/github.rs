use crate::db::CachedItem;
use anyhow::{Context, Result};
use octocrab::Octocrab;
use serde::Deserialize;
use tracing::{info, warn};

/// Build an authenticated Octocrab instance.
///
/// Authentication priority (mirrors go-gh's `auth.TokenForHost`):
///   1. `GH_TOKEN` env var (GitHub Actions / manual PAT)
///   2. `GITHUB_TOKEN` env var (GitHub Actions / manual PAT)
///   3. `gh auth token` — covers gh's config file and system keyring.
///      `gh` does NOT inject `GH_TOKEN` into an extension's environment, so this
///      is what makes `gh glauca` authenticated (otherwise it falls back to the
///      unauthenticated 60 req/hour-per-IP pool and rate-limits almost instantly).
///   4. Unauthenticated (rate-limited to 60 req/hour)
pub fn build_client() -> Result<Octocrab> {
    let (token, auth_source) = resolve_token(|k| std::env::var(k).ok(), gh_auth_token);
    info!(auth = auth_source, "building GitHub client");
    if token.is_none() {
        warn!(
            "no GitHub token found; running unauthenticated (60 req/hour). Run `gh auth login` to authenticate."
        );
    }

    let mut builder = Octocrab::builder();
    if let Some(t) = token {
        builder = builder.personal_token(t);
    }
    builder.build().map_err(Into::into)
}

/// Resolve the auth token and a label for its source, given an env-var lookup and
/// a `gh auth token` fallback. Split out from [`build_client`] so the precedence
/// can be unit-tested without touching the real environment or spawning `gh`.
fn resolve_token(
    env: impl Fn(&str) -> Option<String>,
    fetch_gh_token: impl Fn() -> Option<String>,
) -> (Option<String>, &'static str) {
    if let Some(t) = env("GH_TOKEN") {
        return (Some(t), "GH_TOKEN");
    }
    if let Some(t) = env("GITHUB_TOKEN") {
        return (Some(t), "GITHUB_TOKEN");
    }
    if let Some(t) = fetch_gh_token() {
        return (Some(t), "gh auth token");
    }
    (None, "unauthenticated")
}

/// Retrieve the token via `gh auth token`. Returns `None` if `gh` is missing,
/// the user is not logged in, or the output is empty.
fn gh_auth_token() -> Option<String> {
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!token.is_empty()).then_some(token)
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
    match client
        .get::<UserResponse, _, _>("https://api.github.com/user", None::<&()>)
        .await
    {
        Ok(u) => Some(CurrentUser {
            login: u.login,
            name: u.name,
            avatar_url: u.avatar_url,
        }),
        Err(e) => {
            warn!(error = %e, "get_current_user failed (unauthenticated or API error)");
            None
        }
    }
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
    // Optional: a rate-limited GraphQL response is HTTP 200 with `data: null`
    // and a top-level `errors` array, so `data` must tolerate being absent.
    data: Option<GqlData>,
    #[serde(default)]
    errors: Vec<GqlError>,
}

#[derive(Deserialize)]
struct GqlError {
    #[serde(rename = "type")]
    err_type: Option<String>,
    message: String,
}

/// Join GraphQL error messages into one human-readable line, for the error text and
/// the partial-response warning alike so both report failures the same way.
fn error_detail(errors: &[GqlError]) -> String {
    errors
        .iter()
        .map(|e| e.message.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

/// True when a GraphQL response carries a primary rate-limit error
/// (`{"errors":[{"type":"RATE_LIMITED",...}]}`).
fn is_rate_limited(resp: &GqlResponse) -> bool {
    resp.errors
        .iter()
        .any(|e| e.err_type.as_deref() == Some("RATE_LIMITED"))
}

#[derive(Deserialize)]
struct GqlData {
    search: GqlSearchConnection,
}

/// Why a `search_page` call failed. `RateLimited` is treated specially by the
/// engine (back off) rather than surfaced as a hard error.
pub enum SearchError {
    RateLimited,
    Other(anyhow::Error),
}

/// Classify an octocrab error: HTTP 429, or 403 whose message names a rate/abuse
/// limit, is a rate limit; everything else is a generic failure.
fn classify_octocrab_error(e: octocrab::Error) -> SearchError {
    if let octocrab::Error::GitHub { source, .. } = &e {
        let status = source.status_code.as_u16();
        let msg = source.message.to_lowercase();
        let rate_limited = status == 429
            || (status == 403
                && (msg.contains("rate limit")
                    || msg.contains("abuse")
                    || msg.contains("secondary")));
        if rate_limited {
            return SearchError::RateLimited;
        }
    }
    SearchError::Other(anyhow::Error::new(e).context("GraphQL search failed"))
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
    /// Whether this page is a faithful view of what GitHub returned: every node
    /// parsed into an item, and the response carried no errors alongside its data.
    ///
    /// A page can be lossy without failing: `nodes` elements are nullable, so a
    /// per-node error yields a `null` (plus a top-level `errors` entry) in an
    /// otherwise-200 response, and `node_to_cached_item` also drops any node
    /// missing a required field. Dropping an item is harmless for upserting — it
    /// just isn't refreshed this cycle — but it is *not* harmless for pruning: the
    /// missing key would look like an item that left the query. `sync_task` must
    /// therefore refuse to prune against a non-faithful page.
    pub faithful: bool,
    /// Raw `nodes` length, before parse failures were dropped. `sync_task` compares
    /// the total against `SEARCH_RESULT_CAP`, which must count what GitHub actually
    /// returned — using the parsed count would let dropped nodes pull a truncated
    /// result set under the cap and enable a prune that deletes live rows.
    pub node_count: usize,
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

/// Whether `query` already constrains `updated:` itself. Such a query is never
/// narrowed further by `apply_updated_since` (the user's choice is preserved), so
/// the engine must treat its fetch as a *full* one — otherwise it would skip the
/// prune for a result set it actually fetched in full. See `engine::resolve_since`.
pub(crate) fn constrains_updated(query: &str) -> bool {
    query.contains("updated:")
}

/// Append `updated:>=<since>` to `query` for an incremental fetch, unless the
/// query already constrains `updated:` (then the user's choice is preserved) or
/// `since` is `None` (a full fetch).
pub(crate) fn apply_updated_since<'a>(
    query: &'a str,
    since: Option<&str>,
) -> std::borrow::Cow<'a, str> {
    match since {
        Some(since) if !constrains_updated(query) => {
            std::borrow::Cow::Owned(format!("{query} updated:>={since}"))
        }
        _ => std::borrow::Cow::Borrowed(query),
    }
}

/// Fetch a single page of GitHub search results using GraphQL.
///
/// Pass `after: None` for the first page, then `Some(end_cursor)` for subsequent pages.
/// `since` (RFC3339 UTC) narrows the fetch to items updated at/after that time for
/// incremental syncs; `None` fetches the full result set.
/// GraphQL gives us `reviewRequests` in a single round-trip.
pub async fn search_page(
    client: &Octocrab,
    query_id: i64,
    query: &str,
    since: Option<&str>,
    after: Option<&str>,
) -> std::result::Result<SearchPageResult, SearchError> {
    let with_since = apply_updated_since(query, since);
    let effective_query = apply_default_sort(&with_since);

    let payload = serde_json::json!({
        "query": search_query(),
        "variables": {
            "q": effective_query,
            "after": after,
        }
    });
    let resp: GqlResponse = match client.graphql(&payload).await {
        Ok(resp) => resp,
        Err(e) => return Err(classify_octocrab_error(e)),
    };

    if is_rate_limited(&resp) {
        return Err(SearchError::RateLimited);
    }
    let Some(data) = resp.data else {
        let detail = error_detail(&resp.errors);
        return Err(SearchError::Other(anyhow::anyhow!(
            "GraphQL search returned no data: {detail}"
        )));
    };

    let conn = data.search;
    let (items, faithful) = parse_nodes(&conn.nodes, query_id, &resp.errors);

    Ok(SearchPageResult {
        items,
        has_next_page: conn.page_info.has_next_page,
        end_cursor: conn.page_info.end_cursor,
        faithful,
        node_count: conn.nodes.len(),
    })
}

/// Parse a page's `nodes` into items, and report whether the page is a *faithful*
/// view of what GitHub returned (see [`SearchPageResult::faithful`]).
///
/// Split out from `search_page` so the faithfulness accounting is testable without
/// an HTTP mock. A partial failure is HTTP 200 with `data` *and* `errors` —
/// typically a nullable `nodes` element the resolver couldn't produce — and an
/// unparseable node is dropped just as quietly. Neither is fatal for upserting, but
/// both are logged: silently serving a short page is how a transient upstream hiccup
/// would otherwise turn into deleted rows.
fn parse_nodes(
    nodes: &[serde_json::Value],
    query_id: i64,
    errors: &[GqlError],
) -> (Vec<CachedItem>, bool) {
    let items: Vec<CachedItem> = nodes
        .iter()
        .filter_map(|node| node_to_cached_item(node, query_id))
        .collect();

    let dropped = nodes.len() - items.len();
    let faithful = dropped == 0 && errors.is_empty();
    if !faithful {
        let detail = error_detail(errors);
        warn!(
            dropped,
            node_count = nodes.len(),
            errors = %detail,
            "incomplete search page; will not prune against it"
        );
    }
    (items, faithful)
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
    let author_avatar_url = node["author"]["avatarUrl"].as_str().map(|s| s.to_string());
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
        // A freshly fetched item carries no read marker; upsert preserves the
        // existing row's `last_read_updated_at` via its DO UPDATE clause.
        last_read_updated_at: None,
    })
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    // Env lookup over a fixed map, for resolve_token tests.
    fn env_from<'a>(pairs: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        }
    }

    // Precedence: GH_TOKEN > GITHUB_TOKEN > `gh auth token` > unauthenticated.
    #[rstest]
    #[case::prefers_gh_token(&[("GH_TOKEN", "a"), ("GITHUB_TOKEN", "b")], Some("gh"), Some("a"), "GH_TOKEN")]
    #[case::falls_back_to_github_token(&[("GITHUB_TOKEN", "b")], Some("gh"), Some("b"), "GITHUB_TOKEN")]
    #[case::falls_back_to_gh_auth_token(&[], Some("gh"), Some("gh"), "gh auth token")]
    #[case::unauthenticated(&[], None, None, "unauthenticated")]
    fn resolve_token_precedence(
        #[case] env: &[(&str, &str)],
        #[case] gh_auth: Option<&str>,
        #[case] want_token: Option<&str>,
        #[case] want_source: &str,
    ) {
        let (tok, src) = resolve_token(env_from(env), || gh_auth.map(Into::into));
        assert_eq!(tok.as_deref(), want_token);
        assert_eq!(src, want_source);
    }

    #[test]
    fn detects_graphql_rate_limit_error() {
        let limited: GqlResponse = serde_json::from_value(serde_json::json!({
            "data": null,
            "errors": [{ "type": "RATE_LIMITED", "message": "API rate limit exceeded" }]
        }))
        .unwrap();
        assert!(is_rate_limited(&limited));
    }

    #[test]
    fn non_rate_limit_errors_are_not_flagged() {
        let other: GqlResponse = serde_json::from_value(serde_json::json!({
            "data": null,
            "errors": [{ "type": "NOT_FOUND", "message": "Could not resolve" }]
        }))
        .unwrap();
        assert!(!is_rate_limited(&other));

        let ok: GqlResponse = serde_json::from_value(serde_json::json!({
            "data": { "search": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] } }
        }))
        .unwrap();
        assert!(!is_rate_limited(&ok));
    }

    // ── parse_nodes: faithfulness accounting ─────────────────────────────────────

    /// A minimal well-formed search node.
    fn ok_node(number: i64) -> serde_json::Value {
        serde_json::json!({
            "__typename": "Issue",
            "number": number,
            "title": "Bug report",
            "state": "OPEN",
            "url": format!("https://github.com/owner/repo/issues/{number}"),
            "updatedAt": "2026-05-23T10:00:00Z",
            "author": { "__typename": "User", "login": "alice" },
            "labels": { "nodes": [] },
            "repository": { "owner": { "login": "owner" }, "name": "repo" },
            "comments": { "totalCount": 0 }
        })
    }

    fn gql_error(message: &str) -> GqlError {
        GqlError {
            err_type: None,
            message: message.to_string(),
        }
    }

    /// Every node parsed and no errors → the page is a faithful view of the query's
    /// results, so `sync_task` may prune against it.
    #[test]
    fn parse_nodes_faithful_when_all_parse_and_no_errors() {
        let nodes = vec![ok_node(1), ok_node(2)];
        let (items, faithful) = parse_nodes(&nodes, 7, &[]);
        assert_eq!(items.len(), 2);
        assert!(faithful);
    }

    /// A `null` node — GitHub's shape for a per-node resolver failure — is dropped
    /// silently. That must mark the page unfaithful: treating the missing key as "the
    /// item left the query" would delete a live row and its read marker.
    #[test]
    fn parse_nodes_unfaithful_when_a_node_is_null() {
        let nodes = vec![ok_node(1), serde_json::Value::Null, ok_node(3)];
        let (items, faithful) = parse_nodes(&nodes, 7, &[gql_error("Something went wrong")]);
        assert_eq!(items.len(), 2, "the null node is dropped");
        assert!(!faithful);
    }

    /// A node missing a required field is dropped by `node_to_cached_item` even
    /// without any top-level error, so the drop count alone must flip the flag.
    #[test]
    fn parse_nodes_unfaithful_when_a_node_is_malformed() {
        let mut bad = ok_node(2);
        bad.as_object_mut().unwrap().remove("updatedAt");
        let nodes = vec![ok_node(1), bad];
        let (items, faithful) = parse_nodes(&nodes, 7, &[]);
        assert_eq!(items.len(), 1);
        assert!(!faithful);
    }

    /// Errors alongside a full set of parsed nodes still mean a partial response;
    /// don't prune against it.
    #[test]
    fn parse_nodes_unfaithful_when_errors_present_despite_all_nodes_parsing() {
        let nodes = vec![ok_node(1)];
        let (items, faithful) = parse_nodes(&nodes, 7, &[gql_error("upstream hiccup")]);
        assert_eq!(items.len(), 1);
        assert!(!faithful);
    }

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
        assert_eq!(
            assignees[0].avatar_url.as_deref(),
            Some("https://a/carol.png")
        );

        let reviews = crate::logic::decode_reviews(&item.reviews);
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].0.login, "dave");
        assert_eq!(
            reviews[0].0.avatar_url.as_deref(),
            Some("https://a/dave.png")
        );
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

    // An explicit sort: qualifier is left untouched (append test kept separate
    // above because it verifies suffix/substring, not full-string equality).
    #[rstest]
    #[case::explicit_sort("is:pr sort:created-desc")]
    #[case::sort_updated_asc("is:issue sort:updated-asc")]
    fn apply_default_sort_preserves_explicit_sort(#[case] q: &str) {
        let result = apply_default_sort(q);
        assert_eq!(result, q);
        assert!(!result.contains("sort:updated-desc"));
    }

    #[rstest]
    #[case::appends_when_set(
        "is:pr is:open",
        Some("2026-06-19T00:00:00Z"),
        "is:pr is:open updated:>=2026-06-19T00:00:00Z"
    )]
    #[case::none_is_full_fetch("is:pr is:open", None, "is:pr is:open")]
    // User already constrains `updated:` → leave their query untouched.
    #[case::respects_user_qualifier(
        "is:pr updated:>2026-01-01",
        Some("2026-06-19T00:00:00Z"),
        "is:pr updated:>2026-01-01"
    )]
    fn apply_updated_since_cases(
        #[case] q: &str,
        #[case] since: Option<&str>,
        #[case] expected: &str,
    ) {
        assert_eq!(apply_updated_since(q, since), expected);
    }

    #[rstest]
    #[case::explicit_lower_bound("is:pr updated:>2026-01-01", true)]
    #[case::explicit_range("is:pr updated:2026-01-01..2026-02-01", true)]
    #[case::no_updated_qualifier("is:pr is:open", false)]
    #[case::similar_but_different("is:pr team-review-requested:o/t", false)]
    fn constrains_updated_cases(#[case] q: &str, #[case] expected: bool) {
        assert_eq!(constrains_updated(q), expected);
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
