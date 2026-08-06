//! `App` state: construction, accessors, the memoized filter cache, unread-count
//! recomputation, and the two-field modal input helpers. Split out of the tui
//! module so `mod.rs` holds only the central types and the run loop wiring.

use super::*;

/// The two fields of the active two-field modal (name/value order), or `None`
/// outside the input modals. Single source of the input_mode → field pair
/// mapping used by cursor sync and field clearing. Keep [`modal_fields_ref`]
/// (the render-path counterpart) in sync with this mapping.
pub(crate) fn modal_fields(app: &mut App) -> Option<(&mut SingleLineInput, &mut SingleLineInput)> {
    match app.input_mode {
        InputMode::NewQuery => Some((&mut app.new_query_name, &mut app.new_query_input)),
        InputMode::EditQuery => Some((&mut app.edit_input, &mut app.edit_input2)),
        // Filter-stream modals have a variable field count (name + N boxes) and
        // are handled by the filter-stream-specific helpers, not this pair.
        _ => None,
    }
}

/// Immutable counterpart of [`modal_fields`] for the render path (see
/// `ui::draw_*_modal`), so the draw side doesn't re-hand-code the field pairing.
pub(crate) fn modal_fields_ref(app: &App) -> Option<(&SingleLineInput, &SingleLineInput)> {
    match app.input_mode {
        InputMode::NewQuery => Some((&app.new_query_name, &app.new_query_input)),
        InputMode::EditQuery => Some((&app.edit_input, &app.edit_input2)),
        _ => None,
    }
}

/// Whether `mode` is a filter-stream create/edit modal (name + N OR-group
/// boxes). These use `App::filter_stream_name` / `filter_stream_filters` with a
/// variable field count, distinct from the fixed 2-field query modals.
pub(crate) fn is_filter_stream_modal(mode: &InputMode) -> bool {
    matches!(
        mode,
        InputMode::NewFilterStream | InputMode::EditFilterStream
    )
}

/// Show the blinking text cursor only on the active field of a two-field modal
/// (the inactive field's cursor is hidden). No-op outside the input modals.
pub(crate) fn sync_modal_cursors(app: &mut App) {
    let field = app.modal_field;
    if is_filter_stream_modal(&app.input_mode) {
        // field 0 = name; field i>=1 = box i-1.
        app.filter_stream_name.set_active(field == 0);
        for (i, b) in app.filter_stream_filters.iter_mut().enumerate() {
            b.set_active(field == i + 1);
        }
        return;
    }
    let Some((f0, f1)) = modal_fields(app) else {
        return;
    };
    f0.set_active(field == 0);
    f1.set_active(field == 1);
}

/// The active field of a filter-stream modal: `modal_field` 0 = name, `i>=1` =
/// box `i-1`. Single source of the index→field mapping used by the clear and
/// text-input paths (`sync_modal_cursors` touches every box, so it keeps its
/// own loop). Returns `None` for an out-of-range box index.
pub(crate) fn active_filter_stream_field_mut(app: &mut App) -> Option<&mut SingleLineInput> {
    match app.modal_field {
        0 => Some(&mut app.filter_stream_name),
        i => app.filter_stream_filters.get_mut(i - 1),
    }
}

/// Clear the active field of a two-field modal. Keeps Ctrl+U consistent with
/// the filter bar's "C-u:clear" (TextArea's own Ctrl+U is undo). No-op outside
/// the input modals.
pub(crate) fn clear_active_modal_field(app: &mut App) {
    if is_filter_stream_modal(&app.input_mode) {
        if let Some(field) = active_filter_stream_field_mut(app) {
            field.clear();
        }
        return;
    }
    let field = app.modal_field;
    if let Some((f0, f1)) = modal_fields(app) {
        if field == 0 {
            f0.clear();
        } else {
            f1.clear();
        }
    }
}

impl App {
    pub fn new(queries: Vec<QueryEntry>) -> Self {
        let entries = queries.into_iter().map(LeftPaneEntry::Query).collect();
        Self {
            focus: Focus::QueryList,
            input_mode: InputMode::Normal,
            entries,
            entry_cursor: 0,
            items: Vec::new(),
            items_version: 0,
            filtered_cache: RefCell::new(FilteredCache::default()),
            item_cursor: 0,
            unread_counts: HashMap::new(),
            pending_items: None,
            pending_changes: ChangeCounts::default(),
            filter: SingleLineInput::new(),
            stream_filter: None,
            new_query_input: SingleLineInput::new(),
            new_query_name: SingleLineInput::new(),
            filter_stream_name: SingleLineInput::new(),
            filter_stream_filters: vec![SingleLineInput::new()],
            edit_input: SingleLineInput::new(),
            edit_input2: SingleLineInput::new(),
            modal_field: 0,
            action_cursor: 0,
            merge_strategy_cursor: 0,
            review_event_cursor: 0,
            review_body: None,
            comments: Vec::new(),
            comments_loading: false,
            comments_scroll: 0,
            comments_show_hidden: false,
            comments_sort_desc: false,
            status: None,
            syncing: false,
            bg_sync_pending: 0,
            detail_scroll: 0,
            current_user: None,
            notifications_enabled: false,
            notif_tracker: ItemTracker::new(),
            icons: Icons::default(),
            custom_actions: CustomActions::default(),
            custom_action_cursor: 0,
            body_refresh_requested: HashSet::new(),
            mouse_regions: RefCell::new(MouseRegions::default()),
            last_mouse_click: None,
        }
    }

    /// Custom actions applicable to the currently selected item, in definition
    /// order. Empty when nothing is selected or none match the item's kind.
    pub fn custom_actions_for_selected(&self) -> Vec<&CustomAction> {
        match self.selected_item() {
            Some(item) => self.custom_actions.for_kind(&item.kind),
            None => Vec::new(),
        }
    }

    /// Whether any custom action applies to the selected item. Cheaper than
    /// `custom_actions_for_selected` for the common "is the list non-empty?"
    /// check (per-frame status hint, `x` guard) — it allocates nothing.
    pub fn has_custom_actions_for_selected(&self) -> bool {
        match self.selected_item() {
            Some(item) => self.custom_actions.has_for_kind(&item.kind),
            None => false,
        }
    }

    pub fn parsed_filter(&self) -> FilterQuery {
        FilterQuery::parse(&self.expand_me(self.filter.value()))
    }

    /// Replace `@me` with the authenticated user's login (case-insensitive).
    /// Falls back to `@me` unchanged if the user is not known yet.
    fn expand_me<'a>(&'a self, filter: &'a str) -> std::borrow::Cow<'a, str> {
        glauca_core::logic::expand_me(self.current_user.as_deref(), filter)
    }

    pub fn filtered_items(&self) -> Vec<&ItemEntry> {
        {
            let mut cache = self.filtered_cache.borrow_mut();
            // Compare inputs against the cached key by reference first — this runs
            // several times per render, so we only allocate an owned key on an
            // actual miss (filter/stream/user changed or items were replaced).
            let stale = match &cache.key {
                Some((version, stream, inline, user)) => {
                    *version != self.items_version
                        || stream.as_deref() != self.stream_filter.as_deref()
                        || inline.as_str() != self.filter.value()
                        || user.as_deref() != self.current_user.as_deref()
                }
                None => true,
            };
            if stale {
                cache.indices = glauca_core::logic::filter_item_indices(
                    &self.items,
                    self.stream_filter.as_deref(),
                    self.filter.value(),
                    self.current_user.as_deref(),
                );
                cache.key = Some((
                    self.items_version,
                    self.stream_filter.clone(),
                    self.filter.value().to_string(),
                    self.current_user.clone(),
                ));
            }
        }
        self.filtered_cache
            .borrow()
            .indices
            .iter()
            .map(|&i| &self.items[i])
            .collect()
    }

    pub fn selected_item(&self) -> Option<&ItemEntry> {
        let filtered = self.filtered_items();
        filtered.get(self.item_cursor).copied()
    }

    pub fn selected_entry(&self) -> Option<&LeftPaneEntry> {
        self.entries.get(self.entry_cursor)
    }

    /// Returns the root query id for the currently selected entry.
    pub fn selected_root_query_id(&self) -> Option<i64> {
        self.selected_entry().map(|e| e.root_query_id())
    }

    pub(crate) fn clamp_item_cursor(&mut self) {
        let max = self.filtered_items().len().saturating_sub(1);
        if self.item_cursor > max {
            self.item_cursor = max;
        }
    }

    /// Install `items` as the visible list, clamping the cursor. `is_new` (unread)
    /// is already set per item by `cached_item_to_item_entry` when the engine builds
    /// them, so there is nothing to recompute here.
    pub(crate) fn apply_items_to_view(&mut self, items: Vec<ItemEntry>) {
        self.items = items;
        self.items_version = self.items_version.wrapping_add(1);
        self.clamp_item_cursor();
    }

    /// Empty the visible list, invalidating the memoized filter cache. Use this
    /// instead of `self.items.clear()` so `filtered_cache` never maps stale
    /// indices into the now-empty list.
    pub(crate) fn clear_items(&mut self) {
        self.items.clear();
        self.items_version = self.items_version.wrapping_add(1);
    }

    /// Drop any held-back background-sync results / banner.
    pub(crate) fn clear_pending(&mut self) {
        self.pending_items = None;
        self.pending_changes = ChangeCounts::default();
    }

    /// Apply the stashed background-sync results to the visible list (the `u`
    /// key). No-op when nothing is pending.
    pub fn apply_pending_items(&mut self) {
        let Some(items) = self.pending_items.take() else {
            return;
        };
        self.pending_changes = ChangeCounts::default();
        if let Some(qid) = self.selected_root_query_id() {
            self.recompute_unread_counts_for_query(qid, &items);
        }
        self.apply_items_to_view(items);
    }

    /// Whether the filters shaping the current view lean on `@me` while the login
    /// is unknown — i.e. the list is empty for a reason the list itself can't show.
    /// The status bar turns this into a warning; see
    /// [`glauca_core::logic::has_unexpanded_me`].
    pub fn me_unexpanded(&self) -> bool {
        let unexpanded =
            |f: &str| glauca_core::logic::has_unexpanded_me(self.current_user.as_deref(), f);
        unexpanded(self.stream_filter.as_deref().unwrap_or_default())
            || unexpanded(self.filter.value())
    }

    /// Adopt a login the engine resolved after startup (see
    /// `AppMessage::CurrentUserResolved`) and redo the work that was wrong without
    /// it: every `@me` filter had been matching nobody.
    ///
    /// The visible list needs no nudge — `filtered_items`'s cache keys on the login,
    /// so the next frame recomputes it. Unread badges do: they are only recomputed
    /// when a query's items load. Refreshing the selected query's badges here fixes
    /// what the user is looking at; the rest correct themselves on their next
    /// background sync, a minute or so later.
    pub(crate) fn adopt_current_user(&mut self, login: String) {
        self.status = Some(format!("signed in as {login}"));
        self.current_user = Some(login);
        if let Some(query_id) = self.selected_root_query_id() {
            let items = std::mem::take(&mut self.items);
            self.recompute_unread_counts_for_query(query_id, &items);
            self.items = items;
        }
        // A resolved login can *shrink* the list as well as grow it — `-author:@me`
        // matched everything while `@me` was literal — so the cursor can be left
        // past the end, which empties the detail pane and makes `j` do nothing.
        self.clamp_item_cursor();
    }

    pub(crate) fn recompute_unread_counts_for_query(&mut self, query_id: i64, items: &[ItemEntry]) {
        for (key, unread) in glauca_core::logic::compute_unread_counts(
            &self.entries,
            query_id,
            items,
            self.current_user.as_deref(),
        ) {
            self.unread_counts.insert(key, unread);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::*;
    use glauca_core::types::FilterStreamEntry;
    use rstest::rstest;

    #[test]
    fn clamp_item_cursor_when_filter_reduces_list() {
        let mut app = make_app_with_items(&["Fix alpha", "Fix beta", "Add gamma"]);
        app.item_cursor = 2; // points to "Add gamma"

        // Apply filter that matches only 2 items.
        app.filter = ta("fix");
        app.clamp_item_cursor();

        // Cursor should clamp to 1 (last index in the 2-item filtered list).
        assert_eq!(app.item_cursor, 1);
    }

    /// Applying a background sync that *removed* items must not leave the cursor
    /// past the end, and must invalidate the memoized filter cache. Removals only
    /// reach this path now that `count_changes` counts them, so this locks in that
    /// the shrinking case is handled.
    #[test]
    fn apply_pending_items_clamps_cursor_after_removal() {
        let mut app = make_app_with_items(&["Alpha", "Beta", "Gamma"]);
        assert_eq!(app.filtered_items().len(), 3); // populates the filter cache
        app.item_cursor = 2; // points to "Gamma", which is about to disappear

        app.pending_items = Some(vec![make_item(1, "Alpha"), make_item(2, "Beta")]);
        app.pending_changes = ChangeCounts {
            updated: 0,
            removed: 1,
        };
        app.apply_pending_items();

        assert_eq!(app.items.len(), 2);
        assert_eq!(app.item_cursor, 1);
        assert!(app.pending_changes.is_empty());
        assert!(app.pending_items.is_none());
        assert_eq!(app.filtered_items().len(), 2);
    }

    #[test]
    fn filtered_items_returns_all_when_empty_filter() {
        let app = make_app_with_items(&["Alpha", "Beta", "Gamma"]);
        assert_eq!(app.filtered_items().len(), 3);
    }

    #[test]
    fn filtered_cache_invalidates_on_items_change() {
        // The memoized filter cache keys on items_version; clearing or replacing
        // items must invalidate it so stale indices are never mapped into a
        // changed list (which would return wrong results or panic).
        let mut app = make_app_with_items(&["Fix a", "Fix b", "Add c"]);
        app.filter = ta("fix");
        assert_eq!(app.filtered_items().len(), 2); // populates the cache

        app.clear_items();
        assert_eq!(app.filtered_items().len(), 0); // must reflect the clear, not panic

        app.apply_items_to_view(vec![make_item(1, "Fix again"), make_item(2, "Nope")]);
        assert_eq!(app.filtered_items().len(), 1); // recomputed against new items
    }

    #[test]
    fn filtered_items_plain_text() {
        let mut app = make_app_with_items(&["Fix the bug", "Add feature", "Fix crash"]);
        app.filter = ta("fix");
        let filtered = app.filtered_items();
        assert_eq!(filtered.len(), 2);
        assert!(
            filtered
                .iter()
                .all(|i| i.title.to_lowercase().contains("fix"))
        );
    }

    #[test]
    fn selected_item_follows_cursor() {
        let mut app = make_app_with_items(&["First", "Second", "Third"]);
        app.item_cursor = 1;
        assert_eq!(
            app.selected_item().map(|i| i.title.as_str()),
            Some("Second")
        );
    }

    #[test]
    fn selected_item_respects_filter() {
        let mut app = make_app_with_items(&["Fix alpha", "Add beta", "Fix gamma"]);
        app.filter = ta("fix");
        app.item_cursor = 1;
        // filtered = ["Fix alpha", "Fix gamma"], cursor=1 → "Fix gamma"
        assert_eq!(
            app.selected_item().map(|i| i.title.as_str()),
            Some("Fix gamma")
        );
    }

    #[test]
    fn selected_item_none_when_list_empty() {
        let app = make_app_with_items(&[]);
        assert!(app.selected_item().is_none());
    }

    #[test]
    fn stream_filter_applied_before_inline_filter() {
        let mut app = make_app_with_items(&["Fix bug", "Add feature", "Fix crash closed"]);
        // Simulate a filter stream that shows only open items
        app.stream_filter = Some("state:open".into());
        // All items have state "open" so all 3 pass stream filter
        assert_eq!(app.filtered_items().len(), 3);

        // Now add inline filter
        app.filter = ta("fix");
        // Only "Fix bug" and "Fix crash closed" match "fix", and all pass stream filter
        assert_eq!(app.filtered_items().len(), 2);
    }

    // An `@me` filter with no login matches nothing and says nothing about why, so
    // the status bar has to. The warning follows whichever filter is in play.
    #[rstest]
    // Stream filter or search box — either one is enough to empty the list.
    #[case::stream_filter(None, Some("author:@me"), "", true)]
    #[case::inline_filter(None, None, "author:@me", true)]
    #[case::both(None, Some("author:@me"), "assignee:@me", true)]
    // Nothing to expand → nothing to warn about.
    #[case::no_me_token(None, Some("state:open"), "fix", false)]
    #[case::no_filters_at_all(None, None, "", false)]
    // Login known → `@me` works, so warning here would be noise.
    #[case::login_known(Some("alice"), Some("author:@me"), "", false)]
    fn me_unexpanded(
        #[case] current_user: Option<&str>,
        #[case] stream_filter: Option<&str>,
        #[case] inline_filter: &str,
        #[case] expected: bool,
    ) {
        let mut app = App::new(vec![]);
        app.current_user = current_user.map(Into::into);
        app.stream_filter = stream_filter.map(Into::into);
        app.filter = ta(inline_filter);
        assert_eq!(app.me_unexpanded(), expected);
    }

    /// The warning must clear itself once the login lands — it is a live property of
    /// the filters, not a flag someone has to remember to reset.
    #[test]
    fn me_unexpanded_clears_when_the_login_resolves() {
        let mut app = make_app_with_items(&["alice's PR"]);
        app.stream_filter = Some("author:@me".into());
        assert!(app.me_unexpanded());

        app.adopt_current_user("alice".into());

        assert!(!app.me_unexpanded());
    }

    /// The bug this whole path exists for: the app started before the network was
    /// up, so the login never resolved and `author:@me` matched nobody all session.
    /// Adopting a late-resolved login must un-break the list without a restart.
    #[test]
    fn adopting_a_late_login_revives_an_at_me_filter() {
        let mut app = make_app_with_items(&["alice's PR"]);
        app.stream_filter = Some("author:@me".into());
        // Unresolved login: `@me` stays literal, so the stream shows nothing.
        assert!(app.current_user.is_none());
        assert!(app.filtered_items().is_empty());

        app.adopt_current_user("alice".into());

        // Same items, same filter — only the login changed.
        assert_eq!(app.filtered_items().len(), 1);
    }

    /// A negated `@me` filter runs the other way: it matched everything while the
    /// login was literal, and shrinks once it resolves. The cursor must come back
    /// inside the list, or the detail pane goes blank and `j` stops responding.
    #[test]
    fn adopting_a_late_login_pulls_the_cursor_back_into_a_shrunken_list() {
        let mut app = make_app_with_items(&["alice's PR", "alice's other PR"]);
        app.stream_filter = Some("-author:@me".into());
        assert_eq!(app.filtered_items().len(), 2);
        app.item_cursor = 1;

        app.adopt_current_user("alice".into());

        assert!(app.filtered_items().is_empty());
        assert_eq!(app.item_cursor, 0, "cursor left past the end of the list");
    }

    /// Unread badges are computed against the same filters, so they were wrong for
    /// the same reason and must be refreshed too — not just the visible list.
    #[test]
    fn adopting_a_late_login_refreshes_unread_counts() {
        let mut app = App::new(vec![]);
        app.entries = vec![
            LeftPaneEntry::Query(QueryEntry {
                id: 1,
                label: "Open PRs".into(),
                query_str: "is:pr is:open".into(),
                kind: "pull_request".into(),
            }),
            LeftPaneEntry::FilterStream(FilterStreamEntry {
                id: 2,
                parent_id: 1,
                name: "Mine".into(),
                filter: "author:@me".into(),
                kind: "pull_request".into(),
            }),
        ];
        app.items = vec![ItemEntry {
            updated_at: "2026-05-24T10:00:00Z".into(),
            ..make_item(1, "alice's unread PR")
        }];
        let stream_key = app.entries[1].unread_key();

        app.recompute_unread_counts_for_query(1, &app.items.clone());
        assert_eq!(app.unread_counts.get(&stream_key), Some(&0));

        app.adopt_current_user("alice".into());

        assert_eq!(
            app.unread_counts.get(&stream_key),
            Some(&1),
            "the stream's badge still counts nothing after the login resolved"
        );
    }

    #[test]
    fn stream_filter_restricts_items() {
        let mut app = App::new(vec![QueryEntry {
            id: 1,
            label: "test".into(),
            query_str: "test".into(),
            kind: "pull_request".into(),
        }]);
        app.items = vec![
            make_item(1, "Open PR"),
            ItemEntry {
                state: "closed".into(),
                ..make_item(2, "Closed PR")
            },
        ];
        app.stream_filter = Some("state:open".into());
        let filtered = app.filtered_items();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].title, "Open PR");
    }

    #[test]
    fn recompute_unread_counts_excludes_read_and_applies_filter() {
        let mut app = App::new(vec![]);
        app.entries = vec![
            LeftPaneEntry::Query(QueryEntry {
                id: 1,
                label: "Open PRs".into(),
                query_str: "is:pr is:open".into(),
                kind: "pull_request".into(),
            }),
            LeftPaneEntry::FilterStream(FilterStreamEntry {
                id: 2,
                parent_id: 1,
                name: "Open only".into(),
                filter: "state:open".into(),
                kind: "pull_request".into(),
            }),
        ];
        let items = vec![
            // Unread (never read).
            ItemEntry {
                updated_at: "2026-05-24T10:00:00Z".into(),
                ..make_item(1, "Open unread")
            },
            // Read: updated_at not newer than last_read_updated_at.
            ItemEntry {
                updated_at: "2026-05-24T10:00:00Z".into(),
                last_read_updated_at: Some("2026-05-24T10:00:00Z".into()),
                ..make_item(2, "Open read")
            },
            // Unread but closed → excluded by the stream's state:open filter.
            ItemEntry {
                state: "closed".into(),
                updated_at: "2026-05-24T10:00:00Z".into(),
                ..make_item(3, "Closed unread")
            },
        ];

        app.recompute_unread_counts_for_query(1, &items);

        // Query #1 (no filter) → items 1 and 3 are unread → 2.
        // Filter stream #2 (state:open) → only item 1 (item 2 read, item 3 closed) → 1.
        assert_eq!(app.unread_counts.get(&(false, 1)), Some(&2));
        assert_eq!(app.unread_counts.get(&(true, 2)), Some(&1));
    }

    // ── App::new defaults ────────────────────────────────────────────────────────

    #[test]
    fn app_new_default_state() {
        let app = App::new(vec![]);
        assert_eq!(app.focus, Focus::QueryList);
        assert!(matches!(app.input_mode, InputMode::Normal));
        assert!(app.entries.is_empty());
        assert!(app.items.is_empty());
        assert_eq!(app.item_cursor, 0);
        assert_eq!(app.entry_cursor, 0);
        assert!(app.filter.is_empty());
        assert!(app.stream_filter.is_none());
        assert!(!app.syncing);
        assert!(app.current_user.is_none());
    }

    #[test]
    fn app_new_creates_one_entry_per_query() {
        let queries = vec![
            QueryEntry {
                id: 1,
                label: "Open PRs".into(),
                query_str: "is:pr is:open".into(),
                kind: "pull_request".into(),
            },
            QueryEntry {
                id: 2,
                label: "Open issues".into(),
                query_str: "is:issue is:open".into(),
                kind: "issue".into(),
            },
        ];

        let app = App::new(queries);

        assert!(app.items.is_empty());
        assert_eq!(app.entry_cursor, 0);
        assert_eq!(app.item_cursor, 0);
        assert_eq!(app.entries.len(), 2);
        match &app.entries[0] {
            LeftPaneEntry::Query(query) => {
                assert_eq!(query.id, 1);
                assert_eq!(query.label, "Open PRs");
                assert_eq!(query.query_str, "is:pr is:open");
            }
            LeftPaneEntry::FilterStream(_) => panic!("expected query entry"),
        }
        match &app.entries[1] {
            LeftPaneEntry::Query(query) => {
                assert_eq!(query.id, 2);
                assert_eq!(query.label, "Open issues");
                assert_eq!(query.query_str, "is:issue is:open");
            }
            LeftPaneEntry::FilterStream(_) => panic!("expected query entry"),
        }
    }

    #[test]
    fn app_new_initializes_action_state() {
        let app = App::new(vec![]);
        assert_eq!(app.action_cursor, 0);
        assert_eq!(app.merge_strategy_cursor, 0);
    }

    // ── expand_me ────────────────────────────────────────────────────────────────

    #[rstest]
    #[case::author_at_me(Some("octocat"), "author:@me", "author:octocat")]
    #[case::review_requested_at_me(
        Some("octocat"),
        "review-requested:@me",
        "review-requested:octocat"
    )]
    #[case::standalone_at_me(Some("octocat"), "@me", "octocat")]
    #[case::multiple_tokens(
        Some("octocat"),
        "author:@me review-requested:@me",
        "author:octocat review-requested:octocat"
    )]
    // current_user is None → @me is preserved.
    #[case::no_current_user(None, "author:@me", "author:@me")]
    // No @me token → query is returned unchanged.
    #[case::no_at_me(Some("octocat"), "is:pr is:open label:bug", "is:pr is:open label:bug")]
    fn expand_me(#[case] user: Option<&str>, #[case] input: &str, #[case] expected: &str) {
        let mut app = App::new(vec![]);
        app.current_user = user.map(Into::into);
        assert_eq!(app.expand_me(input), expected);
    }

    #[test]
    fn sync_modal_cursors_shows_only_active_field() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::EditQuery;
        app.modal_field = 1;
        sync_modal_cursors(&mut app);
        assert!(!app.edit_input.is_active());
        assert!(app.edit_input2.is_active());

        app.modal_field = 0;
        sync_modal_cursors(&mut app);
        assert!(app.edit_input.is_active());
        assert!(!app.edit_input2.is_active());
    }

    #[test]
    fn sync_modal_cursors_tracks_active_filter_stream_box() {
        let mut app = App::new(vec![]);
        app.input_mode = InputMode::NewFilterStream;
        app.filter_stream_filters = vec![SingleLineInput::new(), SingleLineInput::new()];

        // field 0 = name
        app.modal_field = 0;
        sync_modal_cursors(&mut app);
        assert!(app.filter_stream_name.is_active());
        assert!(!app.filter_stream_filters[0].is_active());

        // field 2 = second box
        app.modal_field = 2;
        sync_modal_cursors(&mut app);
        assert!(!app.filter_stream_name.is_active());
        assert!(!app.filter_stream_filters[0].is_active());
        assert!(app.filter_stream_filters[1].is_active());
    }

    #[test]
    fn custom_actions_for_selected_filters_by_kind() {
        let mut app = make_app_with_items(&["First"]); // PR
        app.custom_actions = CustomActions {
            actions: vec![
                make_custom_action("pr", &["pull_request"]),
                make_custom_action("issue", &["issue"]),
                make_custom_action("any", &[]),
            ],
        };
        let names: Vec<&str> = app
            .custom_actions_for_selected()
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(names, vec!["pr", "any"]);
    }
}
