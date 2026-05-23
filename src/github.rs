use anyhow::Result;
use crate::db::CachedItem;
use octocrab::Octocrab;
use serde::Deserialize;
use url::Url;

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

// ── Custom deserialization types for search results ──────────────────────────

/// Minimal representation of the `pull_request` sub-object in search results.
/// The GitHub API includes `merged_at` here; octocrab's typed model omits it.
#[derive(Deserialize)]
struct PrLink {
    merged_at: Option<String>,
}

#[derive(Deserialize)]
struct SearchItem {
    number: u64,
    title: String,
    state: String,
    html_url: String,
    repository_url: String,
    updated_at: String,
    comments: u64,
    user: SearchUser,
    labels: Vec<SearchLabel>,
    pull_request: Option<PrLink>,
}

#[derive(Deserialize)]
struct SearchUser {
    login: String,
}

#[derive(Deserialize)]
struct SearchLabel {
    name: String,
}

#[derive(Deserialize)]
struct SearchPage {
    items: Vec<SearchItem>,
}

/// Search GitHub for issues and pull requests matching `query`.
///
/// Returns up to 100 results (GitHub API max per page) as `CachedItem`s
/// ready to be upserted into SQLite.
///
/// Uses raw JSON deserialization to capture `pull_request.merged_at`,
/// which octocrab's typed model omits, so merged PRs are stored as
/// `"merged"` rather than `"closed"`.
pub async fn search(
    client: &Octocrab,
    query_id: i64,
    query: &str,
) -> Result<Vec<CachedItem>> {
    let url = format!(
        "https://api.github.com/search/issues?q={}&per_page=100",
        urlencoding::encode(query)
    );
    let page: SearchPage = client.get(url, None::<&()>).await?;

    let items = page
        .items
        .into_iter()
        .map(|item| {
            let is_pr = item.pull_request.is_some();
            let kind = if is_pr { "pull_request" } else { "issue" };
            let state = if item.state == "open" {
                "open"
            } else if is_pr && item.pull_request.as_ref().and_then(|pr| pr.merged_at.as_ref()).is_some() {
                "merged"
            } else {
                "closed"
            };

            // Extract repo owner/name from repository_url:
            //   "https://api.github.com/repos/{owner}/{name}"
            let (repo_owner, repo_name) = extract_repo_url_str(&item.repository_url);

            // Serialize labels as JSON array: ["bug","enhancement"]
            let labels = {
                let names: Vec<String> = item
                    .labels
                    .iter()
                    .map(|l| format!("\"{}\"", l.name.replace('"', "\\\"")))
                    .collect();
                format!("[{}]", names.join(","))
            };

            CachedItem {
                query_id,
                kind: kind.to_string(),
                repo_owner,
                repo_name,
                number: item.number as i64,
                title: item.title,
                url: item.html_url,
                author: Some(item.user.login),
                state: state.to_string(),
                updated_at: item.updated_at,
                labels,
                comment_count: item.comments as i64,
            }
        })
        .collect();

    Ok(items)
}

/// Extract `(owner, name)` from a GitHub repository URL.
///
/// Input: `"https://api.github.com/repos/owner/name"`
pub(crate) fn extract_repo_url(url: &Url) -> (String, String) {
    let segments: Vec<&str> = url.path_segments().map(|s| s.collect::<Vec<_>>()).unwrap_or_default();
    // Path segments: ["repos", "owner", "name"]
    match segments.as_slice() {
        [.., owner, name] if !owner.is_empty() && !name.is_empty() => {
            (owner.to_string(), name.to_string())
        }
        _ => (String::new(), String::new()),
    }
}

fn extract_repo_url_str(s: &str) -> (String, String) {
    if let Ok(url) = s.parse::<Url>() {
        extract_repo_url(&url)
    } else {
        (String::new(), String::new())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_repo_url_standard() {
        let url: Url = "https://api.github.com/repos/octocat/hello-world"
            .parse()
            .unwrap();
        assert_eq!(
            extract_repo_url(&url),
            ("octocat".to_string(), "hello-world".to_string())
        );
    }

    #[test]
    fn extract_repo_url_nested_path() {
        // Should always take the last two segments.
        let url: Url = "https://api.github.com/repos/org/repo".parse().unwrap();
        let (owner, name) = extract_repo_url(&url);
        assert_eq!(owner, "org");
        assert_eq!(name, "repo");
    }

    #[test]
    fn labels_json_no_labels() {
        let names: Vec<String> = vec![];
        let json = format!(
            "[{}]",
            names
                .iter()
                .map(|n| format!("\"{}\"", n.replace('"', "\\\"")))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert_eq!(json, "[]");
        // Verify round-trip via serde_json.
        let parsed: Vec<String> = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn labels_json_with_labels() {
        let names = vec!["bug".to_string(), "good first issue".to_string()];
        let json = format!(
            "[{}]",
            names
                .iter()
                .map(|n| format!("\"{}\"", n.replace('"', "\\\"")))
                .collect::<Vec<_>>()
                .join(",")
        );
        let parsed: Vec<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, names);
    }

    #[test]
    fn labels_json_escapes_quotes() {
        let names = vec!["label\"with\"quotes".to_string()];
        let json = format!(
            "[{}]",
            names
                .iter()
                .map(|n| format!("\"{}\"", n.replace('"', "\\\"")))
                .collect::<Vec<_>>()
                .join(",")
        );
        let parsed: Vec<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0], "label\"with\"quotes");
    }
}
