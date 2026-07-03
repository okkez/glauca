use crate::types::ItemEntry;
#[cfg(test)]
use crate::types::UserRef;
use frizbee::{Config, Matcher};
use std::cell::RefCell;
use std::collections::HashMap;

/// Shared frizbee config for plain-text token matching.
///
/// `max_typos: 0` keeps matching to a strict subsequence (fzf-style, no typo
/// tolerance). frizbee matches case-insensitively by default (case only
/// influences scoring, which we ignore). `sort: false` — we test single items,
/// so result order is unused.
fn fuzzy_config() -> Config {
    Config {
        max_typos: Some(0),
        sort: false,
        ..Config::default()
    }
}

thread_local! {
    /// Per-thread cache of compiled matchers, keyed by needle. Building a
    /// `Matcher` allocates a prefilter + Smith-Waterman state; filtering runs
    /// over every item on each keystroke, so rebuilding one per item (via the
    /// free `frizbee::match_list`) measured ~8x slower than reusing a single
    /// matcher per token. Cleared past a cap to bound memory over a session.
    static MATCHERS: RefCell<HashMap<String, Matcher>> = RefCell::new(HashMap::new());
}

/// Upper bound on cached matchers before the cache is dropped wholesale.
const MATCHER_CACHE_CAP: usize = 256;

/// Run `f` with the cached matcher for `needle`, building it once per distinct
/// needle. `Matcher::match_*` take `&mut self` (they reuse internal buffers),
/// hence the `&mut` handle.
fn with_matcher<R>(needle: &str, f: impl FnOnce(&mut Matcher) -> R) -> R {
    MATCHERS.with(|cell| {
        let mut cache = cell.borrow_mut();
        if !cache.contains_key(needle) {
            if cache.len() >= MATCHER_CACHE_CAP {
                cache.clear();
            }
            cache.insert(needle.to_string(), Matcher::new(needle, &fuzzy_config()));
        }
        f(cache.get_mut(needle).expect("just inserted"))
    })
}

/// `true` when `needle` fuzzy-matches any of `haystacks`.
fn fuzzy_hit(needle: &str, haystacks: &[&str]) -> bool {
    with_matcher(needle, |m| !m.match_list(haystacks).is_empty())
}

/// Parsed representation of a filter query string.
///
/// Syntax:
///   - Plain token: fuzzy-matches title, author, repo, labels (case-insensitive
///     subsequence, fzf-style — `fltr` matches `filter`)
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
        // plain text tokens — fuzzy-match title | author | repo | labels
        if !self.text_tokens.is_empty() {
            let author_login = item.author.as_ref().map(|u| u.login.as_str()).unwrap_or("");
            let repo = format!("{}/{}", item.repo_owner, item.repo_name);
            let mut fields: Vec<&str> = vec![item.title.as_str(), author_login, repo.as_str()];
            fields.extend(item.labels.iter().map(|l| l.as_str()));
            for tok in &self.text_tokens {
                if !fuzzy_hit(tok, &fields) {
                    return false;
                }
            }
        }
        true
    }

    /// Byte ranges `(start, end)` in `text` covering every plain-text-token
    /// fuzzy match (case-insensitive subsequence), snapped to char boundaries
    /// and merged into ascending, non-overlapping runs. Empty when there is no
    /// plain text token or no match. Frontends turn these into styled spans.
    ///
    /// Because fuzzy matches are non-contiguous, a single token can produce
    /// several ranges.
    pub fn highlight_ranges(&self, text: &str) -> Vec<(usize, usize)> {
        if self.text_tokens.is_empty() {
            return Vec::new();
        }

        // frizbee reports matched *byte* offsets (in reverse order); gather them
        // across every token.
        let mut byte_hits: Vec<usize> = Vec::new();
        for tok in &self.text_tokens {
            for mi in with_matcher(tok, |m| m.match_list_indices(&[text])) {
                byte_hits.extend(mi.indices);
            }
        }
        byte_hits.retain(|&b| b < text.len());
        if byte_hits.is_empty() {
            return Vec::new();
        }
        byte_hits.sort_unstable();
        byte_hits.dedup();

        // Coalesce consecutive byte offsets into `(start, end)` runs (end
        // exclusive), snap each end to char boundaries, then merge runs that
        // snapping pushed into (or adjacent to) one another — this keeps whole
        // multibyte chars intact when a matched byte lands mid-character.
        let mut out: Vec<(usize, usize)> = Vec::new();
        let mut run_start = byte_hits[0];
        let mut prev = byte_hits[0];
        let flush = |start: usize, last: usize, out: &mut Vec<(usize, usize)>| {
            let s = text.floor_char_boundary(start);
            let e = text.ceil_char_boundary(last + 1);
            match out.last_mut() {
                Some(prev) if s <= prev.1 => prev.1 = prev.1.max(e),
                _ => out.push((s, e)),
            }
        };
        for &b in &byte_hits[1..] {
            if b == prev + 1 {
                prev = b;
            } else {
                flush(run_start, prev, &mut out);
                run_start = b;
                prev = b;
            }
        }
        flush(run_start, prev, &mut out);
        out
    }
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

    // ── highlight_ranges ────────────────────────────────────────────────────────

    #[test]
    fn highlight_ranges_no_match_returns_empty() {
        // No 'x','y','z' subsequence in the text → no ranges.
        let q = FilterQuery::parse("xyz");
        assert_eq!(q.highlight_ranges("Fix the bug"), vec![]);
    }

    #[test]
    fn highlight_ranges_match_in_middle() {
        let q = FilterQuery::parse("bug");
        // "Fix the " = 8 bytes, "bug" = 3 bytes.
        assert_eq!(q.highlight_ranges("Fix the bug here"), vec![(8, 11)]);
    }

    #[test]
    fn highlight_ranges_match_at_start() {
        let q = FilterQuery::parse("fix");
        assert_eq!(q.highlight_ranges("Fix the bug"), vec![(0, 3)]);
    }

    #[test]
    fn highlight_ranges_match_at_end() {
        let q = FilterQuery::parse("bug");
        assert_eq!(q.highlight_ranges("Fix the bug"), vec![(8, 11)]);
    }

    #[test]
    fn highlight_ranges_empty_query_empty() {
        let q = FilterQuery::parse("");
        assert_eq!(q.highlight_ranges("Fix the bug"), vec![]);
    }

    #[test]
    fn highlight_ranges_structured_token_empty() {
        // state:open is a structured token, not a plain text token → no highlight.
        let q = FilterQuery::parse("state:open");
        assert_eq!(q.highlight_ranges("open issue title"), vec![]);
    }

    #[test]
    fn highlight_ranges_fuzzy_produces_multiple_runs() {
        // "fb" is a non-contiguous subsequence of "Foo Bar": 'f' at 0, 'b' at 4.
        let q = FilterQuery::parse("fb");
        assert_eq!(q.highlight_ranges("Foo Bar"), vec![(0, 1), (4, 5)]);
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
    fn highlight_ranges_case_insensitive_match() {
        let q = FilterQuery::parse("fix");
        // "Fix" should still match even though the token is lowercase "fix".
        assert_eq!(q.highlight_ranges("Fix the bug"), vec![(0, 3)]);
    }

    #[test]
    fn highlight_ranges_multibyte_text_no_panic() {
        // Ensure we don't panic and never split a multibyte char.
        let q = FilterQuery::parse("bug");
        for text in ["バグ修正 bug fix", "日本語テスト"] {
            for (s, e) in q.highlight_ranges(text) {
                assert!(text.is_char_boundary(s) && text.is_char_boundary(e));
            }
        }
    }

    #[test]
    fn highlight_ranges_multibyte_highlights_correct_chars() {
        // "バグ" = 6 bytes (2 chars × 3), " " = 1 byte, then "bug" at bytes 7..10.
        // Verifies frizbee indices are treated as char indices: the highlighted
        // slice must be exactly "bug", not garbage from byte/char confusion.
        let q = FilterQuery::parse("bug");
        let text = "バグ bug";
        let ranges = q.highlight_ranges(text);
        assert_eq!(ranges, vec![(7, 10)]);
        assert_eq!(&text[7..10], "bug");
    }

    #[test]
    fn plain_text_fuzzy_subsequence_matches() {
        // "fltr" is a subsequence of "Filter", not a contiguous substring.
        let q = FilterQuery::parse("fltr");
        assert!(q.matches(&item("Filter the list", "a", "open", &[], "o/r")));
        // A char not present in order must not match.
        assert!(!FilterQuery::parse("zzz").matches(&item("Filter", "a", "open", &[], "o/r")));
    }

    #[test]
    fn plain_text_matches_repo() {
        // Plain tokens now also match the "owner/name" repo string.
        let q = FilterQuery::parse("myrepo");
        assert!(q.matches(&item("PR", "a", "open", &[], "owner/myrepo")));
        assert!(!q.matches(&item("PR", "a", "open", &[], "owner/other")));
    }
}
