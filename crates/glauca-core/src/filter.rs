#[cfg(test)]
use crate::types::UserRef;
use crate::types::{ActorKind, ItemEntry};
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
///   - `review-requested:<login>` / `team-review-requested:<slug>` — filter by a
///     requested reviewer, matching only that actor kind, as on GitHub.
///   - `-<token>` — negate any token above, GitHub-style: the item must NOT
///     match it (`-label:bug`, `-is:draft`, `-wip`). A lone `-` is a plain
///     text token, not a negation.
///
/// Multiple tokens are ANDed together; each negated token independently
/// excludes matching items.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FilterQuery {
    /// Positive conditions — an item must match every one.
    require: Conditions,
    /// Negated (`-` prefixed) conditions — an item must match none.
    exclude: Conditions,
}

/// One direction's worth of parsed conditions (see [`FilterQuery`]'s
/// `require` / `exclude`).
#[derive(Debug, Default, Clone, PartialEq)]
struct Conditions {
    text_tokens: Vec<String>,
    kinds: Vec<String>,
    states: Vec<String>,
    authors: Vec<String>,
    assignees: Vec<String>,
    labels: Vec<String>,
    milestones: Vec<String>,
    repos: Vec<String>,
    base_refs: Vec<String>,
    head_refs: Vec<String>,
    review_requested_users: Vec<String>,
    review_requested_teams: Vec<String>,
    /// `is:draft` → `Some(true)`. Only constrained when set.
    is_draft: Option<bool>,
    /// `is:private` → `Some(true)`, `is:public` → `Some(false)`.
    is_private: Option<bool>,
}

/// Signature shared by every entry in [`QUALIFIER_FIELDS`]. Named to keep the
/// table's type readable (clippy flags the raw fn-pointer type as
/// `type_complexity`).
type QualifierField = fn(&mut Conditions) -> &mut Vec<String>;

/// Value-taking qualifiers: name (no `:`) → the field it appends to. Adding one is a
/// single line; the order carries no meaning, because `Conditions::add_token` splits a
/// token at its first `:` and looks the name up exactly.
///
/// `is:` is absent deliberately — it is overloaded (kind / draft / repo visibility /
/// state) and gets its own arm.
const QUALIFIER_FIELDS: &[(&str, QualifierField)] = &[
    ("state", |c| &mut c.states),
    ("author", |c| &mut c.authors),
    ("assignee", |c| &mut c.assignees),
    ("label", |c| &mut c.labels),
    ("milestone", |c| &mut c.milestones),
    ("repo", |c| &mut c.repos),
    ("base", |c| &mut c.base_refs),
    ("head", |c| &mut c.head_refs),
    // GitHub keeps these apart and so do we: each `requested_reviewers` entry
    // carries an `ActorKind`, so `review-requested:` reaches only users and
    // `team-review-requested:` only teams.
    ("review-requested", |c| &mut c.review_requested_users),
    ("team-review-requested", |c| &mut c.review_requested_teams),
];

impl Conditions {
    /// Record a qualifier's value, unless it is empty.
    ///
    /// A qualifier typed without a value (`label:`) is a half-typed token from the
    /// type-ahead filter, not a constraint, and storing it would do the opposite of
    /// nothing: every qualifier matches with `contains`, and `contains("")` is true, so
    /// an empty value matches every item that has the field at all.
    ///
    /// The test is on the *value*, deliberately, not on the token's shape. A plain word
    /// ending in `:` (`fix:`, `wip:`) strips no known prefix and must still reach
    /// `text_tokens` and filter as text.
    fn push_value(target: &mut Vec<String>, val: &str) {
        if !val.is_empty() {
            target.push(val.to_string());
        }
    }

    /// Normalise a qualifier's value before the empty-value gate sees it.
    ///
    /// Only `team-review-requested:` needs it. GitHub spells the value `org/team-slug`,
    /// but only the bare slug is cached, so keep the last path segment — without this
    /// the *canonical* form a user copies out of their saved query would match nothing,
    /// exactly the silent failure the qualifier was added to fix. A trailing slash
    /// (`my-org/`) leaves that segment empty; fall back to the raw value, which still
    /// contains a `/` and so matches nothing — for a half-typed *value* that is the
    /// honest answer, where a missing value constrains nothing at all.
    fn normalize_value<'a>(qualifier: &str, val: &'a str) -> &'a str {
        if qualifier != "team-review-requested" {
            return val;
        }
        match val.rsplit_once('/') {
            Some((_org, slug)) if !slug.is_empty() => slug,
            _ => val,
        }
    }

    /// Parse one token (already lowercased, `-` stripped) into this set.
    ///
    /// Dispatch is on the qualifier *name*, taken as everything before the first `:`,
    /// so `QUALIFIER_FIELDS` needs no ordering discipline and every value passes
    /// through one normalisation and one empty-value gate.
    fn add_token(&mut self, lower: &str) {
        let Some((qualifier, val)) = lower.split_once(':') else {
            Self::push_value(&mut self.text_tokens, lower);
            return;
        };
        if qualifier == "is" {
            // `is:` is overloaded: kind (pr/issue), draft, repo visibility,
            // else a state value (open/closed/merged/…).
            match val {
                "pr" | "pull_request" | "pull-request" => self.kinds.push("pull_request".into()),
                "issue" | "issues" => self.kinds.push("issue".into()),
                "draft" => self.is_draft = Some(true),
                "public" => self.is_private = Some(false),
                "private" => self.is_private = Some(true),
                _ => Self::push_value(&mut self.states, val),
            }
            return;
        }
        let Some((_, field)) = QUALIFIER_FIELDS.iter().find(|(name, _)| *name == qualifier) else {
            // A plain word that merely ends in `:` (`fix:`, `wip:`) names no qualifier
            // and must still reach `text_tokens` — with its colon.
            Self::push_value(&mut self.text_tokens, lower);
            return;
        };
        let val = Self::normalize_value(qualifier, val);
        Self::push_value(field(self), val);
    }

    /// `true` when every condition in this set evaluates to `want` against
    /// `item` — `want == true` checks a require set (all must hit),
    /// `want == false` an exclude set (none may hit).
    fn all_eval_to(&self, item: &ItemEntry, want: bool) -> bool {
        // kind filter (is:pr / is:issue) — exact match on normalized kind
        for k in &self.kinds {
            if (item.kind.to_lowercase() == k.as_str()) != want {
                return false;
            }
        }
        // is:draft — draft pull requests
        if let Some(v) = self.is_draft
            && (item.is_draft == v) != want
        {
            return false;
        }
        // is:public / is:private — repository visibility
        if let Some(v) = self.is_private
            && (item.repo_private == v) != want
        {
            return false;
        }
        // state filter
        for s in &self.states {
            if item.state.to_lowercase().contains(s.as_str()) != want {
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
            if author_lower.contains(a.as_str()) != want {
                return false;
            }
        }
        // assignee filter
        for a in &self.assignees {
            let hit = item
                .assignees
                .iter()
                .any(|u| u.login.to_lowercase().contains(a.as_str()));
            if hit != want {
                return false;
            }
        }
        // label filter
        for l in &self.labels {
            let hit = item
                .labels
                .iter()
                .any(|lbl| lbl.to_lowercase().contains(l.as_str()));
            if hit != want {
                return false;
            }
        }
        // milestone filter
        for m in &self.milestones {
            let milestone_lower = item.milestone.as_deref().unwrap_or_default().to_lowercase();
            if milestone_lower.contains(m.as_str()) != want {
                return false;
            }
        }
        // repo filter
        let repo_lower = item.repo_display().to_lowercase();
        for r in &self.repos {
            if repo_lower.contains(r.as_str()) != want {
                return false;
            }
        }
        // base/head branch filter (PRs)
        for b in &self.base_refs {
            let base_lower = item.base_ref.as_deref().unwrap_or_default().to_lowercase();
            if base_lower.contains(b.as_str()) != want {
                return false;
            }
        }
        for h in &self.head_refs {
            let head_lower = item.head_ref.as_deref().unwrap_or_default().to_lowercase();
            if head_lower.contains(h.as_str()) != want {
                return false;
            }
        }
        // review-requested filter — the qualifier picks the actor kind, so a team
        // slug and a user login of the same name do not collide.
        for (values, kind) in [
            (&self.review_requested_users, ActorKind::User),
            (&self.review_requested_teams, ActorKind::Team),
        ] {
            for rv in values {
                let hit = item
                    .requested_reviewers
                    .iter()
                    .filter(|u| u.kind == kind)
                    .any(|u| u.login.to_lowercase().contains(rv.as_str()));
                if hit != want {
                    return false;
                }
            }
        }
        // plain text tokens — fuzzy-match title | author | repo | labels
        if !self.text_tokens.is_empty() {
            let author_login = item.author.as_ref().map(|u| u.login.as_str()).unwrap_or("");
            let repo = item.repo_display();
            let mut fields: Vec<&str> = vec![item.title.as_str(), author_login, repo.as_str()];
            fields.extend(item.labels.iter().map(|l| l.as_str()));
            for tok in &self.text_tokens {
                if fuzzy_hit(tok, &fields) != want {
                    return false;
                }
            }
        }
        true
    }
}

impl FilterQuery {
    pub fn parse(input: &str) -> Self {
        let mut q = FilterQuery::default();
        for token in input.split_whitespace() {
            let lower = token.to_lowercase();
            match lower.strip_prefix('-') {
                // `-token` negates; a lone `-` stays a plain text token.
                Some(rest) if !rest.is_empty() => q.exclude.add_token(rest),
                _ => q.require.add_token(&lower),
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
        self.require.all_eval_to(item, true) && self.exclude.all_eval_to(item, false)
    }

    /// Byte ranges `(start, end)` in `text` covering every plain-text-token
    /// fuzzy match (case-insensitive subsequence), snapped to char boundaries
    /// and merged into ascending, non-overlapping runs. Empty when there is no
    /// plain text token or no match. Frontends turn these into styled spans.
    ///
    /// Because fuzzy matches are non-contiguous, a single token can produce
    /// several ranges. Negated (`-`) text tokens never highlight.
    pub fn highlight_ranges(&self, text: &str) -> Vec<(usize, usize)> {
        if self.require.text_tokens.is_empty() {
            return Vec::new();
        }

        // frizbee 0.9.x reports matched *byte* offsets (in reverse order); gather
        // them across every token. NOTE: frizbee's upstream `main` switched
        // `MatchIndices.indices` to *char* indices — the type stays `Vec<usize>`,
        // so a version bump would compile cleanly but silently corrupt multibyte
        // highlights. `frizbee_reports_byte_offsets` guards this contract; if it
        // fails after upgrading frizbee, revisit the byte→char handling here.
        let mut byte_hits: Vec<usize> = Vec::new();
        for tok in &self.require.text_tokens {
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
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut run_start = byte_hits[0];
        let mut run_end = byte_hits[0]; // last byte in the current run (inclusive)
        let flush = |start: usize, last: usize, ranges: &mut Vec<(usize, usize)>| {
            let snapped_start = text.floor_char_boundary(start);
            let snapped_end = text.ceil_char_boundary(last + 1);
            match ranges.last_mut() {
                Some(prev) if snapped_start <= prev.1 => prev.1 = prev.1.max(snapped_end),
                _ => ranges.push((snapped_start, snapped_end)),
            }
        };
        for &b in &byte_hits[1..] {
            if b == run_end + 1 {
                run_end = b;
            } else {
                flush(run_start, run_end, &mut ranges);
                run_start = b;
                run_end = b;
            }
        }
        flush(run_start, run_end, &mut ranges);
        ranges
    }
}

/// Separator between OR-groups within a single stored filter-stream string.
///
/// A filter stream's `filter` is one AND-group per line; the groups are ORed
/// (see [`StreamFilter`]). A newline can never appear inside a single group —
/// every frontend's filter input is single-line — so legacy single-group
/// filters (no newline) read back as exactly one group with no migration.
pub const FILTER_GROUP_SEP: char = '\n';

/// Split a stored filter-stream string into its raw OR-group segments,
/// preserving empty segments. The form layer uses this to map the stored
/// string back onto one input box per group (an empty string yields one empty
/// box). Matching drops empty groups; see [`StreamFilter::parse`].
pub fn split_filter_groups(s: &str) -> Vec<&str> {
    s.split(FILTER_GROUP_SEP).collect()
}

/// Join box values into a stored filter-stream string, dropping blank groups
/// and trimming each. The inverse of [`split_filter_groups`] for the round trip
/// through the create/edit form.
pub fn join_filter_groups<I, S>(groups: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = String::new();
    for group in groups {
        let group = group.as_ref().trim();
        if group.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(FILTER_GROUP_SEP);
        }
        out.push_str(group);
    }
    out
}

/// A filter-stream filter: an OR of AND-groups ([`FilterQuery`]).
///
/// The stored string holds one group per line (see [`FILTER_GROUP_SEP`]); an
/// item matches when it matches **any** group. Blank groups are dropped, and a
/// filter with no non-blank group matches everything (same as an empty
/// [`FilterQuery`] on the stream side previously).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct StreamFilter {
    groups: Vec<FilterQuery>,
}

impl StreamFilter {
    /// Parse a stored filter into its OR-groups, expanding `@me` (against
    /// `current_user`) within each group. Splitting into groups first keeps each
    /// group an independent AND-group; blank groups are dropped by `from_groups`.
    pub fn parse(input: &str, current_user: Option<&str>) -> Self {
        Self::from_groups(
            split_filter_groups(input)
                .into_iter()
                .map(|g| FilterQuery::parse(&crate::logic::expand_me(current_user, g))),
        )
    }

    /// Like [`StreamFilter::parse`], but for a string whose `@me` tokens are
    /// already expanded (e.g. the persisted filter carried on a mark-read
    /// request), so it must not be expanded a second time.
    pub fn parse_expanded(input: &str) -> Self {
        // With no `current_user`, `parse` expands nothing (`expand_me(None, _)`
        // is a no-op), so this is exactly "split and parse each group".
        Self::parse(input, None)
    }

    fn from_groups(groups: impl Iterator<Item = FilterQuery>) -> Self {
        StreamFilter {
            groups: groups.filter(|q| !q.is_empty()).collect(),
        }
    }

    /// `true` when there is no constraining group (matches everything).
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// `true` when `item` matches any group (OR), or there is no group.
    pub fn matches(&self, item: &ItemEntry) -> bool {
        self.groups.is_empty() || self.groups.iter().any(|g| g.matches(item))
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

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

    // Single-qualifier / plain-text matching against the `item()` builder.
    // Each case carries `(query, item, expected)`; positive and negative rows
    // are listed adjacently so a failing qualifier is obvious from its name.
    #[rstest]
    // plain text matches the title …
    #[case::text_title_hit("fix", item("Fix the bug", "alice", "open", &[], "owner/repo"), true)]
    #[case::text_title_miss("fix", item("Add feature", "alice", "open", &[], "owner/repo"), false)]
    // … the author …
    #[case::text_author_hit("alice", item("some PR", "alice", "open", &[], "o/r"), true)]
    #[case::text_author_miss("alice", item("some PR", "bob", "open", &[], "o/r"), false)]
    // … a label …
    #[case::text_label_hit("bug", item("some PR", "a", "open", &["bug"], "o/r"), true)]
    #[case::text_label_miss("bug", item("some PR", "a", "open", &["enhancement"], "o/r"), false)]
    // … and the "owner/name" repo string.
    #[case::text_repo_hit("myrepo", item("PR", "a", "open", &[], "owner/myrepo"), true)]
    #[case::text_repo_miss("myrepo", item("PR", "a", "open", &[], "owner/other"), false)]
    // state:
    #[case::state_open("state:open", item("PR", "a", "open", &[], "o/r"), true)]
    #[case::state_closed("state:open", item("PR", "a", "closed", &[], "o/r"), false)]
    // is: is an alias for state:
    #[case::is_merged_hit("is:merged", item("PR", "a", "merged", &[], "o/r"), true)]
    #[case::is_merged_miss("is:merged", item("PR", "a", "open", &[], "o/r"), false)]
    // author:
    #[case::author_hit("author:bob", item("PR", "bob", "open", &[], "o/r"), true)]
    #[case::author_miss("author:bob", item("PR", "alice", "open", &[], "o/r"), false)]
    // label:
    #[case::label_hit("label:bug", item("PR", "a", "open", &["bug", "wontfix"], "o/r"), true)]
    #[case::label_miss("label:bug", item("PR", "a", "open", &["enhancement"], "o/r"), false)]
    // repo:
    #[case::repo_hit("repo:owner/myrepo", item("PR", "a", "open", &[], "owner/myrepo"), true)]
    #[case::repo_miss("repo:owner/myrepo", item("PR", "a", "open", &[], "other/repo"), false)]
    fn matches(#[case] q: &str, #[case] it: ItemEntry, #[case] expected: bool) {
        assert_eq!(FilterQuery::parse(q).matches(&it), expected);
    }

    #[test]
    fn combined_filter() {
        let q = FilterQuery::parse("fix state:open label:bug");
        assert!(q.matches(&item("Fix crash", "a", "open", &["bug"], "o/r")));
        assert!(!q.matches(&item("Fix crash", "a", "closed", &["bug"], "o/r")));
        assert!(!q.matches(&item("Fix crash", "a", "open", &["enhancement"], "o/r")));
    }

    #[test]
    fn stream_filter_single_group_is_legacy_and() {
        // A filter with no newline is one AND-group — same as FilterQuery.
        let sf = StreamFilter::parse("state:open label:bug", None);
        assert!(sf.matches(&item("PR", "a", "open", &["bug"], "o/r")));
        assert!(!sf.matches(&item("PR", "a", "open", &["enhancement"], "o/r")));
        assert!(!sf.matches(&item("PR", "a", "closed", &["bug"], "o/r")));
    }

    #[test]
    fn stream_filter_groups_are_ored() {
        // Each line is AND-internal; lines are ORed.
        let sf = StreamFilter::parse("label:bug\nstate:closed", None);
        assert!(sf.matches(&item("PR", "a", "open", &["bug"], "o/r"))); // 1st group
        assert!(sf.matches(&item("PR", "a", "closed", &["enhancement"], "o/r"))); // 2nd group
        assert!(!sf.matches(&item("PR", "a", "open", &["enhancement"], "o/r"))); // neither
    }

    #[test]
    fn stream_filter_drops_blank_groups() {
        let sf = StreamFilter::parse("label:bug\n\n   \n", None);
        assert!(sf.matches(&item("PR", "a", "open", &["bug"], "o/r")));
        assert!(!sf.matches(&item("PR", "a", "open", &["enhancement"], "o/r")));
    }

    #[test]
    fn stream_filter_all_blank_matches_everything() {
        let sf = StreamFilter::parse("  \n\n", None);
        assert!(sf.is_empty());
        assert!(sf.matches(&item("anything", "a", "open", &[], "o/r")));
    }

    #[test]
    fn stream_filter_expands_me_per_group() {
        let sf = StreamFilter::parse("author:@me\nstate:closed", Some("alice"));
        assert!(sf.matches(&item("PR", "alice", "open", &[], "o/r"))); // @me → alice
        assert!(!sf.matches(&item("PR", "bob", "open", &[], "o/r")));
        assert!(sf.matches(&item("PR", "bob", "closed", &[], "o/r"))); // 2nd group
    }

    #[test]
    fn stream_filter_parse_expanded_skips_me_expansion() {
        // parse_expanded must leave `@me` literal (the string is already expanded
        // upstream), so it matches an author whose login is literally "@me" and
        // NOT some other user.
        let sf = StreamFilter::parse_expanded("author:@me");
        assert!(sf.matches(&item("PR", "@me", "open", &[], "o/r")));
        assert!(!sf.matches(&item("PR", "alice", "open", &[], "o/r")));
    }

    #[test]
    fn mark_read_pipeline_preserves_or_groups_with_at_me() {
        // Regression guard for the mark-all-read pipeline: the callers (TUI
        // run.rs, GUI entries.rs, Tauri commands.rs) expand `@me` on the WHOLE
        // stored filter string before sending MarkAllRead, and the engine parses
        // it with `parse_expanded`. `expand_me` USED TO collapse whitespace
        // (including the '\n' group separators), which merged a multi-group
        // `@me` filter into one AND-group so mark-all-read marked the
        // intersection. It now preserves the separators; assert the marked set
        // equals the union the list shows.
        let raw = "author:@me\nstate:closed";
        let user = Some("alice");

        // What the item list / unread counts use: OR of per-group parses.
        let shown = StreamFilter::parse(raw, user);
        // What mark-all-read uses: expand_me on the whole string, then parse_expanded.
        let expanded = crate::logic::expand_me(user, raw).into_owned();
        let marked = StreamFilter::parse_expanded(&expanded);

        let alice_open = item("PR", "alice", "open", &[], "o/r");
        let bob_closed = item("PR", "bob", "closed", &[], "o/r");

        // The list shows both (union of the two OR-groups).
        assert!(shown.matches(&alice_open));
        assert!(shown.matches(&bob_closed));

        // Mark-all-read must mark the same set the list shows.
        assert!(
            marked.matches(&alice_open),
            "alice's open PR is shown but would not be marked read"
        );
        assert!(
            marked.matches(&bob_closed),
            "bob's closed PR is shown but would not be marked read"
        );
    }

    #[test]
    fn split_filter_groups_keeps_empty_boxes() {
        assert_eq!(split_filter_groups("a\nb"), vec!["a", "b"]);
        assert_eq!(split_filter_groups(""), vec![""]); // one empty box
    }

    #[test]
    fn join_filter_groups_drops_blank_and_trims() {
        assert_eq!(join_filter_groups(["a", "", "  ", "b"]), "a\nb");
        assert_eq!(join_filter_groups([" x ", "y"]), "x\ny");
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

    #[rstest]
    #[case::assignee_hit("assignee:bob", &["bob", "carol"], true)]
    #[case::assignee_miss("assignee:dave", &["bob", "carol"], false)]
    #[case::assignee_none("assignee:bob", &[], false)]
    fn matches_assignee(#[case] q: &str, #[case] assignees: &[&str], #[case] expected: bool) {
        assert_eq!(
            FilterQuery::parse(q).matches(&with_assignees(assignees)),
            expected
        );
    }

    #[rstest]
    #[case::milestone_hit("milestone:v2.0", Some("v2.0"), true)]
    #[case::milestone_miss("milestone:v3.0", Some("v2.0"), false)]
    // No milestone set → no match.
    #[case::milestone_none("milestone:v2.0", None, false)]
    fn matches_milestone(#[case] q: &str, #[case] milestone: Option<&str>, #[case] expected: bool) {
        let mut pr = item("PR", "a", "open", &[], "o/r");
        pr.milestone = milestone.map(str::to_string);
        assert_eq!(FilterQuery::parse(q).matches(&pr), expected);
    }

    // `with_refs` items carry base_ref = "main", head_ref = "feature/x".
    #[rstest]
    #[case::base_hit("base:main", true, true)]
    #[case::base_miss("base:develop", true, false)]
    #[case::head_hit("head:feature/x", true, true)]
    #[case::head_miss("head:feature/y", true, false)]
    // No refs set → no match.
    #[case::base_none("base:main", false, false)]
    fn matches_base_head(#[case] q: &str, #[case] with_refs: bool, #[case] expected: bool) {
        let mut pr = item("PR", "a", "open", &[], "o/r");
        if with_refs {
            pr.base_ref = Some("main".to_string());
            pr.head_ref = Some("feature/x".to_string());
        }
        assert_eq!(FilterQuery::parse(q).matches(&pr), expected);
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

    /// Build a PR with the given requested reviewers. A `team:`-prefixed entry
    /// becomes a team reviewer; anything else a user. An entry with no prefix also
    /// stands in for a row cached before `kind` existed, which decodes as a user.
    fn pr_with_reviewers(reviewers: &[&str]) -> ItemEntry {
        let mut pr = item("PR", "alice", "open", &[], "o/r");
        pr.requested_reviewers = reviewers
            .iter()
            .map(|r| match r.strip_prefix("team:") {
                Some(slug) => UserRef {
                    login: slug.to_string(),
                    avatar_url: None,
                    kind: ActorKind::Team,
                },
                None => UserRef::new(*r),
            })
            .collect();
        pr
    }

    #[rstest]
    #[case::requested_bob("review-requested:bob", &["bob", "carol"], true)]
    #[case::requested_carol("review-requested:carol", &["bob", "carol"], true)]
    #[case::unrequested_login("review-requested:dave", &["bob", "carol"], false)]
    #[case::no_reviewers("review-requested:bob", &[], false)]
    // Each qualifier matches only its own actor kind, as on GitHub. Before
    // `ActorKind` the two were interchangeable, because a team slug was stored in
    // the same shape as a user login.
    #[case::team_requested("team-review-requested:my-team", &["bob", "team:my-team"], true)]
    #[case::team_not_requested("team-review-requested:other-team", &["bob", "team:my-team"], false)]
    #[case::team_negated("-team-review-requested:my-team", &["bob", "team:my-team"], false)]
    #[case::team_qualifier_skips_users("team-review-requested:bob", &["bob"], false)]
    #[case::user_qualifier_skips_teams("review-requested:my-team", &["team:my-team"], false)]
    // A reviewer cached before `kind` existed decodes as a user, so the *user*
    // qualifier is the one that reaches it until the next full fetch rewrites the row.
    #[case::kindless_row_is_a_user("review-requested:my-team", &["my-team"], true)]
    // GitHub's canonical `org/team` spelling must work: only the bare slug is cached,
    // so the org prefix is dropped rather than silently matching nothing.
    #[case::team_org_qualified("team-review-requested:my-org/my-team", &["team:my-team"], true)]
    #[case::team_org_qualified_no_match("team-review-requested:my-org/other", &["team:my-team"], false)]
    // A trailing slash must not degenerate into `contains("")`, which would match
    // every item that has any requested reviewer.
    #[case::team_trailing_slash_matches_nothing("team-review-requested:my-org/", &["team:my-team"], false)]
    #[case::team_bare_slash_matches_nothing("team-review-requested:/", &["team:my-team"], false)]
    // A value-less qualifier is dropped, so it constrains nothing rather than acting
    // as a hidden "has any requested reviewer" filter.
    #[case::team_no_value_is_not_a_constraint("team-review-requested:", &[], true)]
    #[case::review_requested_no_value_is_not_a_constraint("review-requested:", &[], true)]
    fn matches_review_requested(
        #[case] q: &str,
        #[case] reviewers: &[&str],
        #[case] expected: bool,
    ) {
        assert_eq!(
            FilterQuery::parse(q).matches(&pr_with_reviewers(reviewers)),
            expected
        );
    }

    /// Dropping a value-less qualifier must not drop the tokens typed alongside it —
    /// the half-typed one stops constraining, the rest keep working.
    #[test]
    fn value_less_qualifier_leaves_sibling_conditions_intact() {
        let q = FilterQuery::parse("is:pr label:");
        let pr = item("Some PR", "alice", "open", &[], "o/r");
        let mut issue = item("Some issue", "alice", "open", &[], "o/r");
        issue.kind = "issue".into();

        assert!(q.matches(&pr), "is:pr must still apply");
        assert!(!q.matches(&issue), "is:pr must still exclude issues");
    }

    /// `label:` alone would otherwise become `contains("")` — true for any item that
    /// has at least one label — quietly turning a partial filter into a wrong one.
    #[test]
    fn value_less_label_does_not_filter_by_having_labels() {
        let q = FilterQuery::parse("label:");
        let unlabelled = item("No labels", "alice", "open", &[], "o/r");
        let labelled = item("Labelled", "alice", "open", &["bug"], "o/r");

        assert!(q.matches(&unlabelled));
        assert!(q.matches(&labelled));
    }

    /// A plain word that happens to end in `:` is not a qualifier — it has no known
    /// prefix, so it must reach `text_tokens` and keep filtering. Dropping it because
    /// of the token's *shape* would silently unfilter the list.
    #[test]
    fn text_token_ending_in_colon_still_filters() {
        let q = FilterQuery::parse("fix:");
        let hit = item("fix: crash on start", "alice", "open", &[], "o/r");
        let miss = item("unrelated title", "alice", "open", &[], "o/r");

        assert!(q.matches(&hit));
        assert!(!q.matches(&miss));
    }

    #[test]
    fn empty_filter_matches_all() {
        let q = FilterQuery::parse("");
        assert!(q.is_empty());
        assert!(q.matches(&item("anything", "a", "open", &[], "o/r")));
    }

    // ── highlight_ranges ────────────────────────────────────────────────────────

    #[rstest]
    // No 'x','y','z' subsequence in the text → no ranges.
    #[case::no_match("xyz", "Fix the bug", vec![])]
    // "Fix the " = 8 bytes, "bug" = 3 bytes.
    #[case::match_in_middle("bug", "Fix the bug here", vec![(8, 11)])]
    #[case::match_at_start("fix", "Fix the bug", vec![(0, 3)])]
    #[case::match_at_end("bug", "Fix the bug", vec![(8, 11)])]
    #[case::empty_query("", "Fix the bug", vec![])]
    // state:open is a structured token, not a plain text token → no highlight.
    #[case::structured_token("state:open", "open issue title", vec![])]
    // "fb" is a non-contiguous subsequence of "Foo Bar": 'f' at 0, 'b' at 4.
    #[case::fuzzy_multiple_runs("fb", "Foo Bar", vec![(0, 1), (4, 5)])]
    // A negated text token never highlights.
    #[case::negated_token("-bug", "Fix the bug", vec![])]
    // "Fix" matches even though the token is lowercase "fix".
    #[case::case_insensitive("fix", "Fix the bug", vec![(0, 3)])]
    fn highlight_ranges(
        #[case] q: &str,
        #[case] text: &str,
        #[case] expected: Vec<(usize, usize)>,
    ) {
        assert_eq!(FilterQuery::parse(q).highlight_ranges(text), expected);
    }

    // ── parse edge cases ────────────────────────────────────────────────────────

    #[test]
    fn parse_case_insensitive_structured_token() {
        let q = FilterQuery::parse("State:Open");
        assert_eq!(q.require.states, vec!["open"]);
        assert!(q.require.text_tokens.is_empty());
    }

    #[test]
    fn parse_repeated_state_tokens() {
        let q = FilterQuery::parse("state:open state:closed");
        assert_eq!(q.require.states.len(), 2);
        assert!(q.require.states.contains(&"open".to_string()));
        assert!(q.require.states.contains(&"closed".to_string()));
    }

    #[test]
    fn parse_multiple_structured_types() {
        let q = FilterQuery::parse("author:alice label:bug state:open");
        assert_eq!(q.require.authors, vec!["alice"]);
        assert_eq!(q.require.labels, vec!["bug"]);
        assert_eq!(q.require.states, vec!["open"]);
        assert!(q.require.text_tokens.is_empty());
    }

    #[test]
    fn parse_negated_tokens_go_to_exclude() {
        let q = FilterQuery::parse("-label:bug -wip state:open");
        assert_eq!(q.exclude.labels, vec!["bug"]);
        assert_eq!(q.exclude.text_tokens, vec!["wip"]);
        assert_eq!(q.require.states, vec!["open"]);
        assert!(q.require.labels.is_empty());
    }

    /// A word that merely ends in `:` names no qualifier, so it filters as text —
    /// colon included. The lookup is by qualifier *name*, so this must not depend on
    /// which prefixes happen to be in the table.
    #[test]
    fn unknown_qualifier_keeps_its_colon_and_filters_as_text() {
        let q = FilterQuery::parse("fix: wip:");
        assert_eq!(
            q.require.text_tokens,
            vec!["fix:".to_string(), "wip:".to_string()]
        );
        assert!(q.require.states.is_empty());
    }

    /// `review-requested` is a *suffix* of `team-review-requested` — neither is a prefix
    /// of the other, and they no longer share a field. Each token is dispatched on its
    /// full qualifier name, so neither can shadow the other however the table is ordered.
    #[test]
    fn overlapping_qualifier_names_are_dispatched_in_full() {
        let q = FilterQuery::parse("team-review-requested:my-org/my-team review-requested:alice");
        assert_eq!(
            q.require.review_requested_teams,
            vec!["my-team".to_string()]
        );
        assert_eq!(q.require.review_requested_users, vec!["alice".to_string()]);
        assert!(q.require.text_tokens.is_empty());
    }

    // ── negation (`-` prefix) ─────────────────────────────────────────────────

    #[test]
    fn negated_label_excludes_matching_items() {
        let q = FilterQuery::parse("-label:bug");
        assert!(!q.matches(&item("PR", "a", "open", &["bug"], "o/r")));
        assert!(q.matches(&item("PR", "a", "open", &["enhancement"], "o/r")));
        assert!(q.matches(&item("PR", "a", "open", &[], "o/r")));
    }

    #[test]
    fn negated_author_repo_and_state() {
        assert!(!FilterQuery::parse("-author:bob").matches(&item("PR", "bob", "open", &[], "o/r")));
        assert!(FilterQuery::parse("-author:bob").matches(&item(
            "PR",
            "alice",
            "open",
            &[],
            "o/r"
        )));
        assert!(!FilterQuery::parse("-repo:owner/myrepo").matches(&item(
            "PR",
            "a",
            "open",
            &[],
            "owner/myrepo"
        )));
        assert!(FilterQuery::parse("-repo:owner/myrepo").matches(&item(
            "PR",
            "a",
            "open",
            &[],
            "other/repo"
        )));
        assert!(!FilterQuery::parse("-state:closed").matches(&item(
            "PR",
            "a",
            "closed",
            &[],
            "o/r"
        )));
        assert!(FilterQuery::parse("-is:closed").matches(&item("PR", "a", "open", &[], "o/r")));
    }

    #[test]
    fn negated_is_draft_excludes_drafts() {
        let q = FilterQuery::parse("-is:draft");
        let mut draft = item("PR", "a", "open", &[], "o/r");
        draft.is_draft = true;
        assert!(!q.matches(&draft));
        assert!(q.matches(&item("PR", "a", "open", &[], "o/r")));
    }

    #[test]
    fn negated_is_pr_keeps_issues_only() {
        let q = FilterQuery::parse("-is:pr");
        assert!(q.matches(&issue("Issue", "a", "open")));
        assert!(!q.matches(&item("PR", "a", "open", &[], "o/r")));
    }

    #[test]
    fn negated_is_public_keeps_private_only() {
        let mut private = item("PR", "a", "open", &[], "o/r");
        private.repo_private = true;
        let public = item("PR", "a", "open", &[], "o/r");
        let q = FilterQuery::parse("-is:public");
        assert!(q.matches(&private));
        assert!(!q.matches(&public));
    }

    #[test]
    fn negated_plain_text_excludes_fuzzy_hits() {
        let q = FilterQuery::parse("-wip");
        assert!(!q.matches(&item("WIP: fix crash", "a", "open", &[], "o/r")));
        // Fuzzy, like positive tokens: "wip" is a subsequence of this title.
        assert!(!q.matches(&item("Work in progress", "a", "open", &[], "o/r")));
        assert!(q.matches(&item("Fix crash", "a", "open", &[], "o/r")));
    }

    #[test]
    fn positive_and_negative_tokens_combine() {
        let q = FilterQuery::parse("is:pr -label:wontfix state:open");
        assert!(q.matches(&item("PR", "a", "open", &["bug"], "o/r")));
        assert!(!q.matches(&item("PR", "a", "open", &["bug", "wontfix"], "o/r")));
        assert!(!q.matches(&item("PR", "a", "closed", &["bug"], "o/r")));
        assert!(!q.matches(&issue("Issue", "a", "open")));
    }

    #[test]
    fn lone_hyphen_is_a_plain_text_token() {
        let q = FilterQuery::parse("-");
        assert!(!q.is_empty());
        assert!(q.matches(&item("re-add feature", "a", "open", &[], "o/r")));
        assert!(!q.matches(&item("add feature", "a", "open", &[], "o/r")));
    }

    #[test]
    fn negated_query_is_not_empty() {
        assert!(!FilterQuery::parse("-is:draft").is_empty());
    }

    #[test]
    fn negation_of_missing_field_passes() {
        // No milestone set → `-milestone:v2` keeps the item (same as GitHub).
        let q = FilterQuery::parse("-milestone:v2");
        assert!(q.matches(&item("PR", "a", "open", &[], "o/r")));
        let mut with_ms = item("PR", "a", "open", &[], "o/r");
        with_ms.milestone = Some("v2.0".into());
        assert!(!q.matches(&with_ms));
    }

    // ── matches edge cases ───────────────────────────────────────────────────────

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
    fn frizbee_reports_byte_offsets() {
        // Pins the load-bearing assumption in `highlight_ranges`: frizbee 0.9.x
        // returns BYTE offsets, not char indices. "あ" is 3 bytes, so the 'b' in
        // "あb" is at byte 3 but char index 1. If a future frizbee returns char
        // indices (as its upstream `main` does), this fails loudly instead of
        // silently mis-highlighting multibyte titles.
        let hits: Vec<usize> = frizbee::match_list_indices("b", &["あb"], &fuzzy_config())
            .into_iter()
            .flat_map(|m| m.indices)
            .collect();
        assert!(
            hits.contains(&3),
            "expected byte offset 3 for 'b' in \"あb\", got {hits:?} — frizbee may have switched to char indices"
        );
        assert!(
            !hits.contains(&1),
            "index 1 would mean char indices, not bytes"
        );
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
}
