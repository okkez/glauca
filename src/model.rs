use std::fmt;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ItemKind {
    PullRequest,
    Issue,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[allow(dead_code)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum IssueState {
    Open,
    Closed,
}

/// Wraps the state of a PR or issue.
/// The variant must match the `ItemKind` of the containing item.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ItemState {
    Pr(PrState),
    Issue(IssueState),
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RepositoryRef {
    pub owner: String,
    pub name: String,
}

impl RepositoryRef {
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            name: name.into(),
        }
    }
}

impl fmt::Display for RepositoryRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ItemId {
    pub kind: ItemKind,
    pub repository: RepositoryRef,
    pub number: u64,
}

impl ItemId {
    pub fn new(kind: ItemKind, repository: RepositoryRef, number: u64) -> Self {
        Self {
            kind,
            repository,
            number,
        }
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            ItemKind::PullRequest => "pr",
            ItemKind::Issue => "issue",
        };
        write!(f, "{}:{}#{}", kind, self.repository, self.number)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SearchResultItem {
    pub id: ItemId,
    pub title: String,
    pub url: String,
    pub author: Option<String>,
    pub state: ItemState,
    pub updated_at: String,
    pub labels: Vec<String>,
    pub comment_count: u32,
}

impl SearchResultItem {
    pub fn new(
        kind: ItemKind,
        repository: RepositoryRef,
        number: u64,
        title: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        let default_state = match kind {
            ItemKind::PullRequest => ItemState::Pr(PrState::Open),
            ItemKind::Issue => ItemState::Issue(IssueState::Open),
        };
        Self {
            id: ItemId::new(kind, repository, number),
            title: title.into(),
            url: url.into(),
            author: None,
            state: default_state,
            updated_at: String::new(),
            labels: Vec::new(),
            comment_count: 0,
        }
    }

    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }

    pub fn with_state(mut self, state: ItemState) -> Self {
        self.state = state;
        self
    }

    pub fn with_updated_at(mut self, updated_at: impl Into<String>) -> Self {
        self.updated_at = updated_at.into();
        self
    }

    pub fn with_labels<I, S>(mut self, labels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.labels = labels.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_comment_count(mut self, comment_count: u32) -> Self {
        self.comment_count = comment_count;
        self
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NormalizedItem {
    pub id: ItemId,
    pub title: String,
    pub url: String,
    pub author: Option<String>,
    pub state: ItemState,
    pub updated_at: String,
    pub labels: Vec<String>,
    pub comment_count: u32,
}

impl From<SearchResultItem> for NormalizedItem {
    fn from(value: SearchResultItem) -> Self {
        Self {
            id: value.id,
            title: value.title,
            url: value.url,
            author: value.author,
            state: value.state,
            updated_at: value.updated_at,
            labels: value.labels,
            comment_count: value.comment_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_pull_request_results() {
        let repository = RepositoryRef::new("octo-org", "octo-repo");
        let item = SearchResultItem::new(
            ItemKind::PullRequest,
            repository.clone(),
            42,
            "Add cache-aware search",
            "https://example.com/pr/42",
        )
        .with_author("octocat")
        .with_state(ItemState::Pr(PrState::Merged))
        .with_updated_at("2026-05-22T00:00:00Z")
        .with_labels(["enhancement", "cache"])
        .with_comment_count(3);

        let normalized = NormalizedItem::from(item);

        assert_eq!(normalized.id.to_string(), "pr:octo-org/octo-repo#42");
        assert_eq!(normalized.title, "Add cache-aware search");
        assert_eq!(normalized.url, "https://example.com/pr/42");
        assert_eq!(normalized.author.as_deref(), Some("octocat"));
        assert_eq!(normalized.state, ItemState::Pr(PrState::Merged));
        assert_eq!(normalized.updated_at, "2026-05-22T00:00:00Z");
        assert_eq!(normalized.labels, vec!["enhancement", "cache"]);
        assert_eq!(normalized.comment_count, 3);
    }

    #[test]
    fn normalizes_issue_results() {
        let repository = RepositoryRef::new("octo-org", "octo-repo");
        let item = SearchResultItem::new(
            ItemKind::Issue,
            repository,
            7,
            "Add CLI filtering",
            "https://example.com/issues/7",
        )
        .with_state(ItemState::Issue(IssueState::Closed))
        .with_updated_at("2026-05-21T00:00:00Z");

        let normalized = NormalizedItem::from(item);

        assert_eq!(normalized.id.to_string(), "issue:octo-org/octo-repo#7");
        assert_eq!(normalized.state, ItemState::Issue(IssueState::Closed));
        assert_eq!(normalized.labels, Vec::<String>::new());
    }
}
