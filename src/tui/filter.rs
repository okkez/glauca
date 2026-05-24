/// Parsed representation of a filter query string.
///
/// Syntax:
///   - Plain token: matches title, author, labels (case-insensitive substring)
///   - `state:<open|closed|merged>` or `is:<open|closed|merged>` — filter by state
///   - `author:<login>` — filter by author login
///   - `label:<name>` — filter by label (substring)
///   - `repo:<owner/name>` — filter by repository (substring)
///   - `review-requested:<login>` — filter by requested reviewer login
///
/// Multiple tokens are ANDed together.
#[derive(Debug, Default, Clone)]
pub struct FilterQuery {
    pub text_tokens: Vec<String>,
    pub states: Vec<String>,
    pub authors: Vec<String>,
    pub labels: Vec<String>,
    pub repos: Vec<String>,
    pub review_requested: Vec<String>,
}

impl FilterQuery {
    pub fn parse(input: &str) -> Self {
        let mut q = FilterQuery::default();
        for token in input.split_whitespace() {
            let lower = token.to_lowercase();
            if let Some(val) = lower.strip_prefix("state:") {
                q.states.push(val.to_string());
            } else if let Some(val) = lower.strip_prefix("is:") {
                q.states.push(val.to_string());
            } else if let Some(val) = lower.strip_prefix("author:") {
                q.authors.push(val.to_string());
            } else if let Some(val) = lower.strip_prefix("label:") {
                q.labels.push(val.to_string());
            } else if let Some(val) = lower.strip_prefix("repo:") {
                q.repos.push(val.to_string());
            } else if let Some(val) = lower.strip_prefix("review-requested:") {
                q.review_requested.push(val.to_string());
            } else {
                q.text_tokens.push(lower);
            }
        }
        q
    }

    pub fn is_empty(&self) -> bool {
        self.text_tokens.is_empty()
            && self.states.is_empty()
            && self.authors.is_empty()
            && self.labels.is_empty()
            && self.repos.is_empty()
            && self.review_requested.is_empty()
    }

    /// Returns `true` if `item` matches all conditions in this query.
    pub fn matches(&self, item: &crate::tui::ItemEntry) -> bool {
        // state filter
        for s in &self.states {
            if !item.state.to_lowercase().contains(s.as_str()) {
                return false;
            }
        }
        // author filter
        let author_lower = item
            .author
            .as_deref()
            .unwrap_or("")
            .to_lowercase();
        for a in &self.authors {
            if !author_lower.contains(a.as_str()) {
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
        // repo filter
        let repo_lower = format!("{}/{}", item.repo_owner, item.repo_name).to_lowercase();
        for r in &self.repos {
            if !repo_lower.contains(r.as_str()) {
                return false;
            }
        }
        // review-requested filter
        for rv in &self.review_requested {
            let hit = item
                .requested_reviewers
                .iter()
                .any(|login| login.to_lowercase().contains(rv.as_str()));
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

    /// Returns spans for `text`, highlighting occurrences of plain text tokens.
    /// Used to highlight matching parts in the item list.
    pub fn highlight_spans<'a>(
        &self,
        text: &'a str,
        normal: ratatui::style::Style,
        highlight: ratatui::style::Style,
    ) -> Vec<ratatui::text::Span<'a>> {
        if self.text_tokens.is_empty() {
            return vec![ratatui::text::Span::styled(text, normal)];
        }

        // Find the earliest matching token position (case-insensitive).
        let lower = text.to_lowercase();
        let mut best: Option<(usize, usize)> = None; // (start_byte, end_byte)
        for tok in &self.text_tokens {
            if let Some(pos) = lower.find(tok.as_str()) {
                let end = pos + tok.len();
                if best.is_none() || pos < best.unwrap().0 {
                    best = Some((pos, end));
                }
            }
        }

        match best {
            None => vec![ratatui::text::Span::styled(text, normal)],
            Some((start, end)) => {
                // Ensure byte indices are on char boundaries
                let start = floor_char_boundary(text, start);
                let end = ceil_char_boundary(text, end);
                let mut spans = Vec::new();
                if start > 0 {
                    spans.push(ratatui::text::Span::styled(&text[..start], normal));
                }
                spans.push(ratatui::text::Span::styled(&text[start..end], highlight));
                if end < text.len() {
                    spans.push(ratatui::text::Span::styled(&text[end..], normal));
                }
                spans
            }
        }
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
    use crate::tui::ItemEntry;

    fn item(title: &str, author: &str, state: &str, labels: &[&str], repo: &str) -> ItemEntry {
        let (owner, name) = repo.split_once('/').unwrap_or((repo, ""));
        ItemEntry {
            number: 1,
            title: title.to_string(),
            repo_owner: owner.to_string(),
            repo_name: name.to_string(),
            author: Some(author.to_string()),
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

    #[test]
    fn review_requested_filter() {
        let mut pr = item("PR", "alice", "open", &[], "o/r");
        pr.requested_reviewers = vec!["bob".into(), "carol".into()];

        let q = FilterQuery::parse("review-requested:bob");
        assert!(q.matches(&pr));

        let q = FilterQuery::parse("review-requested:carol");
        assert!(q.matches(&pr));

        let q = FilterQuery::parse("review-requested:dave");
        assert!(!q.matches(&pr));

        // item with no reviewers
        let no_reviewers = item("PR", "alice", "open", &[], "o/r");
        let q = FilterQuery::parse("review-requested:bob");
        assert!(!q.matches(&no_reviewers));
    }

    #[test]
    fn empty_filter_matches_all() {
        let q = FilterQuery::parse("");
        assert!(q.is_empty());
        assert!(q.matches(&item("anything", "a", "open", &[], "o/r")));
    }

    #[test]
    fn highlight_spans_no_match_returns_single_span() {
        let q = FilterQuery::parse("xyz");
        let normal = ratatui::style::Style::default();
        let highlight = ratatui::style::Style::default().fg(ratatui::style::Color::Yellow);
        let spans = q.highlight_spans("Fix the bug", normal, highlight);
        // No match → single unstyled span with full text.
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "Fix the bug");
        assert_eq!(spans[0].style, normal);
    }

    #[test]
    fn highlight_spans_match_in_middle() {
        let q = FilterQuery::parse("bug");
        let normal = ratatui::style::Style::default();
        let highlight = ratatui::style::Style::default().fg(ratatui::style::Color::Yellow);
        let spans = q.highlight_spans("Fix the bug here", normal, highlight);
        // Three spans: before, match, after.
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "Fix the ");
        assert_eq!(spans[1].content, "bug");
        assert_eq!(spans[1].style, highlight);
        assert_eq!(spans[2].content, " here");
    }

    #[test]
    fn highlight_spans_match_at_start() {
        let q = FilterQuery::parse("fix");
        let normal = ratatui::style::Style::default();
        let highlight = ratatui::style::Style::default().fg(ratatui::style::Color::Yellow);
        let spans = q.highlight_spans("Fix the bug", normal, highlight);
        // Two spans: match, after (no prefix span).
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Fix");
        assert_eq!(spans[0].style, highlight);
        assert_eq!(spans[1].content, " the bug");
    }

    #[test]
    fn highlight_spans_match_at_end() {
        let q = FilterQuery::parse("bug");
        let normal = ratatui::style::Style::default();
        let highlight = ratatui::style::Style::default().fg(ratatui::style::Color::Yellow);
        let spans = q.highlight_spans("Fix the bug", normal, highlight);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Fix the ");
        assert_eq!(spans[1].content, "bug");
        assert_eq!(spans[1].style, highlight);
    }

    #[test]
    fn highlight_spans_empty_query_no_highlight() {
        let q = FilterQuery::parse("");
        let normal = ratatui::style::Style::default();
        let highlight = ratatui::style::Style::default().fg(ratatui::style::Color::Yellow);
        let spans = q.highlight_spans("Fix the bug", normal, highlight);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style, normal);
    }

    #[test]
    fn highlight_spans_structured_token_no_highlight() {
        // state:open is a structured token, not a plain text token → no highlight.
        let q = FilterQuery::parse("state:open");
        let normal = ratatui::style::Style::default();
        let highlight = ratatui::style::Style::default().fg(ratatui::style::Color::Yellow);
        let spans = q.highlight_spans("open issue title", normal, highlight);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style, normal);
    }

    // ── parse edge cases ────────────────────────────────────────────────────────

    #[test]
    fn parse_case_insensitive_structured_token() {
        // Tokens are lowercased before parsing, so State:Open should work.
        let q = FilterQuery::parse("State:Open");
        assert_eq!(q.states, vec!["open"]);
        assert!(q.text_tokens.is_empty());
    }

    #[test]
    fn parse_repeated_state_tokens() {
        // Both values are collected; matches() ANDs them, so both must hold.
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
        // Plain text should also match against author login.
        assert!(q.matches(&item("some PR", "alice", "open", &[], "o/r")));
        assert!(!q.matches(&item("some PR", "bob", "open", &[], "o/r")));
    }

    #[test]
    fn plain_text_matches_label() {
        let q = FilterQuery::parse("bug");
        // Plain text should also match against labels.
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
        // Item state is compared case-insensitively.
        let q = FilterQuery::parse("state:open");
        let mut i = item("PR", "a", "open", &[], "o/r");
        i.state = "Open".to_string(); // stored with capital O
        assert!(q.matches(&i));
    }

    #[test]
    fn is_and_state_are_equivalent() {
        // `is:merged` and `state:merged` should behave the same way.
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
        // parse lowercases the token; item author compared via to_lowercase()
        assert!(q.matches(&item("PR", "alice", "open", &[], "o/r")));
    }

    // ── highlight_spans edge cases ───────────────────────────────────────────────

    #[test]
    fn highlight_spans_case_insensitive_match() {
        let q = FilterQuery::parse("fix");
        let normal = ratatui::style::Style::default();
        let highlight = ratatui::style::Style::default().fg(ratatui::style::Color::Yellow);
        // "Fix" should still be highlighted even though the token is lowercase "fix".
        let spans = q.highlight_spans("Fix the bug", normal, highlight);
        assert!(spans.iter().any(|s| s.style == highlight));
    }

    #[test]
    fn highlight_spans_multibyte_text_no_panic() {
        // Ensure we don't panic on multibyte (non-ASCII) text.
        let q = FilterQuery::parse("bug");
        let normal = ratatui::style::Style::default();
        let highlight = ratatui::style::Style::default().fg(ratatui::style::Color::Yellow);
        // Should not panic regardless of whether there's a match.
        let _ = q.highlight_spans("バグ修正 bug fix", normal, highlight);
        let _ = q.highlight_spans("日本語テスト", normal, highlight);
    }
}
