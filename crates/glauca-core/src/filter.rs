use crate::types::ItemEntry;
#[cfg(test)]
use crate::types::UserRef;

/// Parsed representation of a filter query string.
///
/// Syntax:
///   - Plain token: matches title, author, labels (case-insensitive substring)
///   - `is:pr` / `is:issue` — filter by item kind (matches `item.kind`)
///   - `is:draft` — only draft pull requests
///   - `is:public` / `is:private` — filter by repository visibility
///   - `state:<value>` or `is:<value>` — filter by state (case-insensitive
///     substring; e.g. open/closed/merged — values are not restricted).
///     Note: `is:pr`/`is:issue`/`is:draft`/`is:public`/`is:private` are treated
///     as their own filters, not states.
///   - `author:<login>` — filter by author login
///   - `assignee:<login>` — filter by an assignee login
///   - `label:<name>` — filter by label (substring)
///   - `milestone:<title>` — filter by milestone title (substring; single word
///     only — whitespace-separated values are not supported)
///   - `repo:<owner/name>` — filter by repository (substring)
///   - `base:<branch>` / `head:<branch>` — filter PRs by base/head branch
///   - `review-requested:<login>` — filter by requested reviewer login
///
/// Multiple tokens are ANDed together.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FilterQuery {
    pub text_tokens: Vec<String>,
    pub kinds: Vec<String>,
    pub states: Vec<String>,
    pub authors: Vec<String>,
    pub assignees: Vec<String>,
    pub labels: Vec<String>,
    pub milestones: Vec<String>,
    pub repos: Vec<String>,
    pub base_refs: Vec<String>,
    pub head_refs: Vec<String>,
    pub review_requested: Vec<String>,
    /// `is:draft` → `Some(true)`. Only constrained when set.
    pub is_draft: Option<bool>,
    /// `is:private` → `Some(true)`, `is:public` → `Some(false)`.
    pub is_private: Option<bool>,
}

impl FilterQuery {
    pub fn parse(input: &str) -> Self {
        let mut q = FilterQuery::default();
        for token in input.split_whitespace() {
            let lower = token.to_lowercase();
            if let Some(val) = lower.strip_prefix("state:") {
                q.states.push(val.to_string());
            } else if let Some(val) = lower.strip_prefix("is:") {
                // `is:` is overloaded: kind (pr/issue), draft, repo visibility,
                // else a state value (open/closed/merged/…).
                match val {
                    "pr" | "pull_request" | "pull-request" => q.kinds.push("pull_request".into()),
                    "issue" | "issues" => q.kinds.push("issue".into()),
                    "draft" => q.is_draft = Some(true),
                    "public" => q.is_private = Some(false),
                    "private" => q.is_private = Some(true),
                    _ => q.states.push(val.to_string()),
                }
            } else if let Some(val) = lower.strip_prefix("author:") {
                q.authors.push(val.to_string());
            } else if let Some(val) = lower.strip_prefix("assignee:") {
                q.assignees.push(val.to_string());
            } else if let Some(val) = lower.strip_prefix("label:") {
                q.labels.push(val.to_string());
            } else if let Some(val) = lower.strip_prefix("milestone:") {
                q.milestones.push(val.to_string());
            } else if let Some(val) = lower.strip_prefix("repo:") {
                q.repos.push(val.to_string());
            } else if let Some(val) = lower.strip_prefix("base:") {
                q.base_refs.push(val.to_string());
            } else if let Some(val) = lower.strip_prefix("head:") {
                q.head_refs.push(val.to_string());
            } else if let Some(val) = lower.strip_prefix("review-requested:") {
                q.review_requested.push(val.to_string());
            } else {
                q.text_tokens.push(lower);
            }
        }
        q
    }

    /// `true` when no conditions were parsed — i.e. equal to a fresh, empty query.
    /// Comparing to `default()` keeps this correct as fields are added.
    pub fn is_empty(&self) -> bool {
        *self == FilterQuery::default()
    }

    /// Returns `true` if `item` matches all conditions in this query.
    pub fn matches(&self, item: &ItemEntry) -> bool {
        // kind filter (is:pr / is:issue) — exact match on normalized kind
        for k in &self.kinds {
            if item.kind.to_lowercase() != k.as_str() {
                return false;
            }
        }
        // is:draft — only draft pull requests
        if let Some(want) = self.is_draft
            && item.is_draft != want
        {
            return false;
        }
        // is:public / is:private — repository visibility
        if let Some(want) = self.is_private
            && item.repo_private != want
        {
            return false;
        }
        // state filter
        for s in &self.states {
            if !item.state.to_lowercase().contains(s.as_str()) {
                return false;
            }
        }
        // author filter
        let author_lower = item
            .author
            .as_ref()
            .map(|u| u.login.to_lowercase())
            .unwrap_or_default();
        for a in &self.authors {
            if !author_lower.contains(a.as_str()) {
                return false;
            }
        }
        // assignee filter
        for a in &self.assignees {
            let hit = item
                .assignees
                .iter()
                .any(|u| u.login.to_lowercase().contains(a.as_str()));
            if !hit {
                return false;
            }
        }
        // label filter
        for l in &self.labels {
            let hit = item
                .labels
                .iter()
                .any(|lbl| lbl.to_lowercase().contains(l.as_str()));
            if !hit {
                return false;
            }
        }
        // milestone filter
        for m in &self.milestones {
            let milestone_lower = item.milestone.as_deref().unwrap_or_default().to_lowercase();
            if !milestone_lower.contains(m.as_str()) {
                return false;
            }
        }
        // repo filter
        let repo_lower = format!("{}/{}", item.repo_owner, item.repo_name).to_lowercase();
        for r in &self.repos {
            if !repo_lower.contains(r.as_str()) {
                return false;
            }
        }
        // base/head branch filter (PRs)
        for b in &self.base_refs {
            let base_lower = item.base_ref.as_deref().unwrap_or_default().to_lowercase();
            if !base_lower.contains(b.as_str()) {
                return false;
            }
        }
        for h in &self.head_refs {
            let head_lower = item.head_ref.as_deref().unwrap_or_default().to_lowercase();
            if !head_lower.contains(h.as_str()) {
                return false;
            }
        }
        // review-requested filter
        for rv in &self.review_requested {
            let hit = item
                .requested_reviewers
                .iter()
                .any(|u| u.login.to_lowercase().contains(rv.as_str()));
            if !hit {
                return false;
            }
        }
        // plain text tokens — match title | author | labels
        let title_lower = item.title.to_lowercase();
        for tok in &self.text_tokens {
            let in_title = title_lower.contains(tok.as_str());
            let in_author = author_lower.contains(tok.as_str());
            let in_labels = item
                .labels
                .iter()
                .any(|l| l.to_lowercase().contains(tok.as_str()));
            if !in_title && !in_author && !in_labels {
                return false;
            }
        }
        true
    }

    /// Byte range `(start, end)` in `text` of the earliest plain-text-token match
    /// (case-insensitive), corrected to char boundaries. `None` if there is no
    /// plain text token or no match. Frontends turn this into styled spans.
    pub fn highlight_range(&self, text: &str) -> Option<(usize, usize)> {
        if self.text_tokens.is_empty() {
            return None;
        }

        // Find the earliest matching token position (case-insensitive).
        let lower = text.to_lowercase();
        let mut best: Option<(usize, usize)> = None; // (start_byte, end_byte)
        for tok in &self.text_tokens {
            if let Some(pos) = lower.find(tok.as_str()) {
                let end = pos + tok.len();
                if best.is_none_or(|(start, _)| pos < start) {
                    best = Some((pos, end));
                }
            }
        }

        best.map(|(start, end)| {
            // Ensure byte indices are on char boundaries.
            (
                floor_char_boundary(text, start),
                ceil_char_boundary(text, end),
            )
        })
    }
}

fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, author: &str, state: &str, labels: &[&str], repo: &str) -> ItemEntry {
        let (owner, name) = repo.split_once('/').unwrap_or((repo, ""));
        ItemEntry {
            number: 1,
            title: title.to_string(),
            repo_owner: owner.to_string(),
            repo_name: name.to_string(),
            repo_private: false,
            author: Some(UserRef::new(author)),
            state: state.to_string(),
            updated_at: String::new(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            url: String::new(),
            comment_count: 0,
            kind: "pull_request".into(),
            requested_reviewers: vec![],
            reviews: vec![],
            body: None,
            assignees: vec![],
            is_draft: false,
            created_at_item: None,
            base_ref: None,
            head_ref: None,
            review_decision: None,
            milestone: None,
            last_read_updated_at: None,
            is_new: false,
        }
    }

    #[test]
    fn plain_text_matches_title() {
        let q = FilterQuery::parse("fix");
        assert!(q.matches(&item("Fix the bug", "alice", "open", &[], "owner/repo")));
        assert!(!q.matches(&item("Add feature", "alice", "open", &[], "owner/repo")));
    }

    #[test]
    fn state_filter() {
        let q = FilterQuery::parse("state:open");
        assert!(q.matches(&item("PR", "a", "open", &[], "o/r")));
        assert!(!q.matches(&item("PR", "a", "closed", &[], "o/r")));
    }

    #[test]
    fn is_prefix_alias() {
        let q = FilterQuery::parse("is:merged");
        assert!(q.matches(&item("PR", "a", "merged", &[], "o/r")));
        assert!(!q.matches(&item("PR", "a", "open", &[], "o/r")));
    }

    #[test]
    fn author_filter() {
        let q = FilterQuery::parse("author:bob");
        assert!(q.matches(&item("PR", "bob", "open", &[], "o/r")));
        assert!(!q.matches(&item("PR", "alice", "open", &[], "o/r")));
    }

    #[test]
    fn label_filter() {
        let q = FilterQuery::parse("label:bug");
        assert!(q.matches(&item("PR", "a", "open", &["bug", "wontfix"], "o/r")));
        assert!(!q.matches(&item("PR", "a", "open", &["enhancement"], "o/r")));
    }

    #[test]
    fn repo_filter() {
        let q = FilterQuery::parse("repo:owner/myrepo");
        assert!(q.matches(&item("PR", "a", "open", &[], "owner/myrepo")));
        assert!(!q.matches(&item("PR", "a", "open", &[], "other/repo")));
    }

    #[test]
    fn combined_filter() {
        let q = FilterQuery::parse("fix state:open label:bug");
        assert!(q.matches(&item("Fix crash", "a", "open", &["bug"], "o/r")));
        assert!(!q.matches(&item("Fix crash", "a", "closed", &["bug"], "o/r")));
        assert!(!q.matches(&item("Fix crash", "a", "open", &["enhancement"], "o/r")));
    }

    fn issue(title: &str, author: &str, state: &str) -> ItemEntry {
        let mut i = item(title, author, state, &[], "o/r");
        i.kind = "issue".into();
        i
    }

    #[test]
    fn is_pr_matches_pull_request_only() {
        let q = FilterQuery::parse("is:pr");
        assert!(q.matches(&item("PR", "a", "open", &[], "o/r")));
        assert!(!q.matches(&issue("Issue", "a", "open")));
    }

    #[test]
    fn is_issue_matches_issue_only() {
        let q = FilterQuery::parse("is:issue");
        assert!(q.matches(&issue("Issue", "a", "open")));
        assert!(!q.matches(&item("PR", "a", "open", &[], "o/r")));
    }

    #[test]
    fn is_pr_combined_with_state_and_author() {
        // Reproduces the reported case: a child-query filter that previously
        // matched nothing because `is:pr` was checked against item.state.
        // The `[bot]` brackets must be treated as literal characters.
        let q = FilterQuery::parse("is:pr is:open author:repro-atlantis[bot]");
        assert!(q.matches(&item("PR", "repro-atlantis[bot]", "open", &[], "o/r")));
        // Wrong author, wrong state, and issue kind must all fail.
        assert!(!q.matches(&item("PR", "someone-else", "open", &[], "o/r")));
        assert!(!q.matches(&item("PR", "repro-atlantis[bot]", "closed", &[], "o/r")));
        assert!(!q.matches(&issue("Issue", "repro-atlantis[bot]", "open")));
    }

    #[test]
    fn is_pr_differs_from_state_pr() {
        // `is:pr` is a kind filter; `state:pr` is a (never-matching) state filter.
        let pr = item("PR", "a", "open", &[], "o/r");
        assert!(FilterQuery::parse("is:pr").matches(&pr));
        assert!(!FilterQuery::parse("state:pr").matches(&pr));
    }

    #[test]
    fn is_draft_matches_only_draft_prs() {
        let q = FilterQuery::parse("is:draft");
        let mut draft = item("PR", "a", "open", &[], "o/r");
        draft.is_draft = true;
        assert!(q.matches(&draft));
        assert!(!q.matches(&item("PR", "a", "open", &[], "o/r")));
    }

    #[test]
    fn is_private_and_public_split_on_repo_visibility() {
        let mut private = item("PR", "a", "open", &[], "o/r");
        private.repo_private = true;
        let public = item("PR", "a", "open", &[], "o/r"); // repo_private defaults to false
        assert!(FilterQuery::parse("is:private").matches(&private));
        assert!(!FilterQuery::parse("is:private").matches(&public));
        assert!(FilterQuery::parse("is:public").matches(&public));
        assert!(!FilterQuery::parse("is:public").matches(&private));
    }

    fn with_assignees(assignees: &[&str]) -> ItemEntry {
        let mut pr = item("PR", "alice", "open", &[], "o/r");
        pr.assignees = assignees.iter().map(|a| UserRef::new(*a)).collect();
        pr
    }

    #[test]
    fn assignee_filter() {
        assert!(FilterQuery::parse("assignee:bob").matches(&with_assignees(&["bob", "carol"])));
        assert!(!FilterQuery::parse("assignee:dave").matches(&with_assignees(&["bob", "carol"])));
        assert!(!FilterQuery::parse("assignee:bob").matches(&with_assignees(&[])));
    }

    #[test]
    fn milestone_filter() {
        let mut pr = item("PR", "a", "open", &[], "o/r");
        pr.milestone = Some("v2.0".to_string());
        assert!(FilterQuery::parse("milestone:v2.0").matches(&pr));
        assert!(!FilterQuery::parse("milestone:v3.0").matches(&pr));
        // No milestone set → no match.
        assert!(!FilterQuery::parse("milestone:v2.0").matches(&item(
            "PR",
            "a",
            "open",
            &[],
            "o/r"
        )));
    }

    #[test]
    fn base_and_head_filter() {
        let mut pr = item("PR", "a", "open", &[], "o/r");
        pr.base_ref = Some("main".to_string());
        pr.head_ref = Some("feature/x".to_string());
        assert!(FilterQuery::parse("base:main").matches(&pr));
        assert!(!FilterQuery::parse("base:develop").matches(&pr));
        assert!(FilterQuery::parse("head:feature/x").matches(&pr));
        assert!(!FilterQuery::parse("head:feature/y").matches(&pr));
        // No refs set → no match.
        assert!(!FilterQuery::parse("base:main").matches(&item("PR", "a", "open", &[], "o/r")));
    }

    #[test]
    fn combined_is_draft_and_assignee_are_anded() {
        let q = FilterQuery::parse("is:pr is:draft assignee:bob");
        let mut pr = with_assignees(&["bob"]);
        pr.is_draft = true;
        assert!(q.matches(&pr));
        // Same item but not a draft must fail.
        let mut non_draft = with_assignees(&["bob"]);
        non_draft.is_draft = false;
        assert!(!q.matches(&non_draft));
    }

    fn pr_with_reviewers(reviewers: &[&str]) -> ItemEntry {
        let mut pr = item("PR", "alice", "open", &[], "o/r");
        pr.requested_reviewers = reviewers.iter().map(|r| UserRef::new(*r)).collect();
        pr
    }

    #[test]
    fn review_requested_matches_a_requested_reviewer() {
        let pr = pr_with_reviewers(&["bob", "carol"]);
        assert!(FilterQuery::parse("review-requested:bob").matches(&pr));
        assert!(FilterQuery::parse("review-requested:carol").matches(&pr));
    }

    #[test]
    fn review_requested_no_match_for_unrequested_login() {
        let pr = pr_with_reviewers(&["bob", "carol"]);
        assert!(!FilterQuery::parse("review-requested:dave").matches(&pr));
    }

    #[test]
    fn review_requested_no_match_when_no_reviewers() {
        let pr = pr_with_reviewers(&[]);
        assert!(!FilterQuery::parse("review-requested:bob").matches(&pr));
    }

    #[test]
    fn empty_filter_matches_all() {
        let q = FilterQuery::parse("");
        assert!(q.is_empty());
        assert!(q.matches(&item("anything", "a", "open", &[], "o/r")));
    }

    // ── highlight_range ─────────────────────────────────────────────────────────

    #[test]
    fn highlight_range_no_match_returns_none() {
        let q = FilterQuery::parse("xyz");
        assert_eq!(q.highlight_range("Fix the bug"), None);
    }

    #[test]
    fn highlight_range_match_in_middle() {
        let q = FilterQuery::parse("bug");
        // "Fix the " = 8 bytes, "bug" = 3 bytes.
        assert_eq!(q.highlight_range("Fix the bug here"), Some((8, 11)));
    }

    #[test]
    fn highlight_range_match_at_start() {
        let q = FilterQuery::parse("fix");
        assert_eq!(q.highlight_range("Fix the bug"), Some((0, 3)));
    }

    #[test]
    fn highlight_range_match_at_end() {
        let q = FilterQuery::parse("bug");
        assert_eq!(q.highlight_range("Fix the bug"), Some((8, 11)));
    }

    #[test]
    fn highlight_range_empty_query_none() {
        let q = FilterQuery::parse("");
        assert_eq!(q.highlight_range("Fix the bug"), None);
    }

    #[test]
    fn highlight_range_structured_token_none() {
        // state:open is a structured token, not a plain text token → no highlight.
        let q = FilterQuery::parse("state:open");
        assert_eq!(q.highlight_range("open issue title"), None);
    }

    // ── parse edge cases ────────────────────────────────────────────────────────

    #[test]
    fn parse_case_insensitive_structured_token() {
        let q = FilterQuery::parse("State:Open");
        assert_eq!(q.states, vec!["open"]);
        assert!(q.text_tokens.is_empty());
    }

    #[test]
    fn parse_repeated_state_tokens() {
        let q = FilterQuery::parse("state:open state:closed");
        assert_eq!(q.states.len(), 2);
        assert!(q.states.contains(&"open".to_string()));
        assert!(q.states.contains(&"closed".to_string()));
    }

    #[test]
    fn parse_multiple_structured_types() {
        let q = FilterQuery::parse("author:alice label:bug state:open");
        assert_eq!(q.authors, vec!["alice"]);
        assert_eq!(q.labels, vec!["bug"]);
        assert_eq!(q.states, vec!["open"]);
        assert!(q.text_tokens.is_empty());
    }

    // ── matches edge cases ───────────────────────────────────────────────────────

    #[test]
    fn plain_text_matches_author() {
        let q = FilterQuery::parse("alice");
        assert!(q.matches(&item("some PR", "alice", "open", &[], "o/r")));
        assert!(!q.matches(&item("some PR", "bob", "open", &[], "o/r")));
    }

    #[test]
    fn plain_text_matches_label() {
        let q = FilterQuery::parse("bug");
        assert!(q.matches(&item("some PR", "a", "open", &["bug"], "o/r")));
        assert!(!q.matches(&item("some PR", "a", "open", &["enhancement"], "o/r")));
    }

    #[test]
    fn author_filter_none_author_does_not_match() {
        let q = FilterQuery::parse("author:alice");
        let mut i = item("PR", "placeholder", "open", &[], "o/r");
        i.author = None;
        assert!(!q.matches(&i));
    }

    #[test]
    fn state_filter_case_insensitive() {
        let q = FilterQuery::parse("state:open");
        let mut i = item("PR", "a", "open", &[], "o/r");
        i.state = "Open".to_string();
        assert!(q.matches(&i));
    }

    #[test]
    fn is_and_state_are_equivalent() {
        let q_is = FilterQuery::parse("is:merged");
        let q_state = FilterQuery::parse("state:merged");
        let merged = item("PR", "a", "merged", &[], "o/r");
        let open = item("PR", "a", "open", &[], "o/r");
        assert_eq!(q_is.matches(&merged), q_state.matches(&merged));
        assert_eq!(q_is.matches(&open), q_state.matches(&open));
    }

    #[test]
    fn author_filter_case_insensitive() {
        let q = FilterQuery::parse("author:Alice");
        assert!(q.matches(&item("PR", "alice", "open", &[], "o/r")));
    }

    #[test]
    fn highlight_range_case_insensitive_match() {
        let q = FilterQuery::parse("fix");
        // "Fix" should still match even though the token is lowercase "fix".
        assert_eq!(q.highlight_range("Fix the bug"), Some((0, 3)));
    }

    #[test]
    fn highlight_range_multibyte_text_no_panic() {
        // Ensure we don't panic on multibyte (non-ASCII) text.
        let q = FilterQuery::parse("bug");
        let _ = q.highlight_range("バグ修正 bug fix");
        let _ = q.highlight_range("日本語テスト");
    }
}
