// Domain / display types shared by every frontend (TUI / GUI).
// framework 非依存（ratatui にも db にも依存しない純粋型）。

#[derive(Clone)]
pub struct QueryEntry {
    pub id: i64,
    /// Display label shown in the left pane (name if set, otherwise query_str).
    pub label: String,
    /// Actual GitHub search query string sent to the API.
    pub query_str: String,
    pub kind: String,
}

#[derive(Clone)]
pub struct FilterStreamEntry {
    pub id: i64,
    pub parent_id: i64,
    pub name: String,
    pub filter: String,
    pub kind: String,
}

/// A single row in the left pane — either a root query or a filter stream.
#[derive(Clone)]
pub enum LeftPaneEntry {
    Query(QueryEntry),
    FilterStream(FilterStreamEntry),
}

impl LeftPaneEntry {
    pub fn id(&self) -> i64 {
        match self {
            Self::Query(q) => q.id,
            Self::FilterStream(fs) => fs.id,
        }
    }

    /// Key for the per-entry unread-count map. Query and filter-stream ids come
    /// from separate tables and can collide as raw i64 (e.g. query #1 and filter
    /// stream #1), so the kind discriminant is included to keep them distinct.
    pub fn unread_key(&self) -> (bool, i64) {
        (self.is_filter_stream(), self.id())
    }

    pub fn kind(&self) -> &str {
        match self {
            Self::Query(q) => &q.kind,
            Self::FilterStream(fs) => &fs.kind,
        }
    }

    /// The root query whose cached items should be loaded.
    pub fn root_query_id(&self) -> i64 {
        match self {
            Self::Query(q) => q.id,
            Self::FilterStream(fs) => fs.parent_id,
        }
    }

    /// The actual GitHub search query string (only meaningful for root queries).
    pub fn root_query_str(&self) -> Option<&str> {
        match self {
            Self::Query(q) => Some(&q.query_str),
            Self::FilterStream(_) => None,
        }
    }

    /// Filter string to apply on top of the inline filter (None for root queries).
    pub fn stream_filter(&self) -> Option<&str> {
        match self {
            Self::Query(_) => None,
            Self::FilterStream(fs) => Some(&fs.filter),
        }
    }

    pub fn is_filter_stream(&self) -> bool {
        matches!(self, Self::FilterStream(_))
    }
}

/// A GitHub user reference: login plus an optional avatar URL. `avatar_url` is
/// `None` for teams (review requests) and may be absent in older cache rows.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct UserRef {
    pub login: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

impl UserRef {
    pub fn new(login: impl Into<String>) -> Self {
        Self {
            login: login.into(),
            avatar_url: None,
        }
    }
}

#[derive(Clone, Default)]
pub struct ItemEntry {
    pub number: i64,
    pub title: String,
    pub repo_owner: String,
    pub repo_name: String,
    /// Whether the repository is private (drives the lock indicator in the item list).
    pub repo_private: bool,
    pub author: Option<UserRef>,
    pub state: String,
    pub updated_at: String,
    pub labels: Vec<String>,
    pub url: String,
    pub comment_count: i64,
    pub kind: String,
    pub requested_reviewers: Vec<UserRef>,
    /// Submitted reviews: user + state (APPROVED / CHANGES_REQUESTED / COMMENTED / DISMISSED)
    pub reviews: Vec<(UserRef, String)>,
    pub body: Option<String>,
    pub assignees: Vec<UserRef>,
    pub is_draft: bool,
    pub created_at_item: Option<String>,
    pub base_ref: Option<String>,
    pub head_ref: Option<String>,
    pub review_decision: Option<String>,
    pub milestone: Option<String>,
    /// The `updated_at` the user had seen when they last read this item, persisted
    /// per cached row. `None` means never read. The item is unread (and highlighted
    /// as new) iff `updated_at` is newer than this — see `logic::is_item_unread`.
    pub last_read_updated_at: Option<String>,
    pub is_new: bool,
}

impl ItemEntry {
    /// The canonical `"owner/name"` repository string used across the UI and
    /// filters. Centralized so the format lives in one place.
    pub fn repo_display(&self) -> String {
        format!("{}/{}", self.repo_owner, self.repo_name)
    }
}

/// A single comment entry fetched from GitHub and displayed in the comments popup.
#[derive(Clone, Debug)]
pub struct CommentEntry {
    pub author: String,
    pub created_at: String,
    pub body: String,
    pub is_minimized: bool,
    pub minimized_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ItemAction {
    OpenBrowser,
    Comment,
    ViewComments,
    ApprovePR,
    MergePR,
    CopyUrl,
    /// Re-fetch just this item from GitHub. Offered by both front-ends.
    RefreshItem,
    /// Launch the external `octorus` (`or`) PR-review TUI for this item. Offered
    /// only by the TUI front-end (see `glauca-tui`'s `item_actions`), never added
    /// to `available_for` so it stays out of the GUI menu.
    ReviewOctorus,
}

impl ItemAction {
    pub fn label(&self) -> &str {
        match self {
            Self::OpenBrowser => "Open in browser",
            Self::Comment => "Comment",
            Self::ViewComments => "View comments",
            Self::ApprovePR => "Approve PR",
            Self::MergePR => "Merge PR",
            Self::CopyUrl => "Copy URL",
            Self::RefreshItem => "Refresh item",
            Self::ReviewOctorus => "Review with octorus",
        }
    }

    pub fn available_for(kind: &str) -> Vec<Self> {
        match kind {
            "pull_request" => vec![
                Self::OpenBrowser,
                Self::CopyUrl,
                Self::RefreshItem,
                Self::ViewComments,
                Self::Comment,
                Self::ApprovePR,
                Self::MergePR,
            ],
            "issue" => vec![
                Self::OpenBrowser,
                Self::CopyUrl,
                Self::RefreshItem,
                Self::ViewComments,
                Self::Comment,
            ],
            _ => vec![],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MergeStrategy {
    Squash,
    Merge,
    Rebase,
}

impl MergeStrategy {
    pub fn all() -> Vec<Self> {
        vec![Self::Squash, Self::Merge, Self::Rebase]
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Squash => "Squash",
            Self::Merge => "Merge",
            Self::Rebase => "Rebase",
        }
    }

    pub fn flag(&self) -> &str {
        match self {
            Self::Squash => "--squash",
            Self::Merge => "--merge",
            Self::Rebase => "--rebase",
        }
    }
}
