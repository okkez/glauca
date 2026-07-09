//! The run loop entry point. `run` owns terminal setup/teardown around
//! `run_app`, which starts the engine, builds `App`, and drives the
//! `tokio::select!` loop: draw, dispatch key events to `Action`s, and forward
//! engine messages to `handle_app_message`.

use super::*;

pub async fn run(pool: SqlitePool, gh: Octocrab) -> Result<()> {
    // Set up terminal
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, pool, gh).await;

    // Restore terminal unconditionally
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    )?;

    result
}

async fn run_app<B: ratatui::backend::Backend + io::Write>(
    terminal: &mut Terminal<B>,
    pool: SqlitePool,
    gh: Octocrab,
) -> Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    // Start the async engine: builds the left-pane entries, resolves the current
    // user, and spawns the background worker / refresh timer / command loop.
    let tui_settings = TuiSettings::load();
    let (mut engine, init) = Engine::start(pool, gh, tui_settings.sync_interval_secs).await?;

    // Build App from the engine's initial entries (filter streams interleaved).
    let queries: Vec<QueryEntry> = init
        .entries
        .iter()
        .filter_map(|e| match e {
            LeftPaneEntry::Query(q) => Some(q.clone()),
            LeftPaneEntry::FilterStream(_) => None,
        })
        .collect();
    let mut app = App::new(queries);
    app.entries = init.entries;
    app.current_user = init.current_user;
    app.notifications_enabled = tui_settings.notifications_enabled;
    app.icons = Icons::new(tui_settings.use_icon_font);
    app.custom_actions = CustomActions::load();

    // Prime unread counts for every root query via a cached load (no sync).
    let root_query_ids: Vec<i64> = app
        .entries
        .iter()
        .filter_map(|entry| match entry {
            LeftPaneEntry::Query(q) => Some(q.id),
            LeftPaneEntry::FilterStream(_) => None,
        })
        .collect();
    for query_id in &root_query_ids {
        engine
            .send(EngineCommand::LoadCached {
                query_id: *query_id,
            })
            .await;
    }

    // Load items for the initially selected entry; sync only if the cache is stale.
    let initially_synced_id = select_current_entry(&mut app, &engine, false).await;

    // Enqueue all other stale queries for immediate background refresh.
    engine
        .send(EngineCommand::EnqueueStale {
            skip_query_id: initially_synced_id,
        })
        .await;

    let mut events = EventStream::new();

    loop {
        terminal.draw(|f| ui::draw(f, &app))?;

        tokio::select! {
            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event {
                    // Ignore key-release events. Terminals with the keyboard-
                    // enhancement protocol (or Windows) emit them, and acting on
                    // both press and release would double-fire actions like
                    // 'd'/'J'/'K'. Repeat events are kept so held-key repeat still
                    // works if enhancement flags are ever enabled.
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    // 'd' in query list → delete selected entry (UI updates on the
                    // QueryDeleted / FilterStreamDeleted message once the DB op succeeds).
                    if key.code == KeyCode::Char('d')
                        && app.focus == Focus::QueryList
                        && app.input_mode == InputMode::Normal
                    {
                        let cmd = match app.entries.get(app.entry_cursor) {
                            Some(LeftPaneEntry::Query(q)) => {
                                Some(EngineCommand::DeleteQuery { query_id: q.id })
                            }
                            Some(LeftPaneEntry::FilterStream(fs)) => {
                                Some(EngineCommand::DeleteFilterStream { id: fs.id })
                            }
                            None => None,
                        };
                        if let Some(cmd) = cmd {
                            engine.send(cmd).await;
                        }
                        continue;
                    }

                    // 'a' in query list → mark all items of the selected entry read.
                    // A query marks its whole root query; a filter stream marks only
                    // its matching items (filter expanded with the current user here,
                    // since the engine does not know `@me`). The engine persists and
                    // reloads the query, which refreshes unread counts via ItemsLoaded.
                    if key.code == KeyCode::Char('a')
                        && app.focus == Focus::QueryList
                        && app.input_mode == InputMode::Normal
                    {
                        let cmd = app.entries.get(app.entry_cursor).map(|entry| {
                            EngineCommand::MarkAllRead {
                                query_id: entry.root_query_id(),
                                filter: entry.stream_filter().map(|f| {
                                    glauca_core::logic::expand_me(app.current_user.as_deref(), f)
                                        .into_owned()
                                }),
                            }
                        });
                        if let Some(cmd) = cmd {
                            engine.send(cmd).await;
                        }
                        continue;
                    }

                    // J/K: move selected entry up/down within its group. The DB swap
                    // runs through the engine; the entries vec is reordered on the
                    // QueriesSwapped / FilterStreamsSwapped confirmation message.
                    if (key.code == KeyCode::Char('J') || key.code == KeyCode::Char('K'))
                        && app.focus == Focus::QueryList
                        && app.input_mode == InputMode::Normal
                    {
                        let down = key.code == KeyCode::Char('J');
                        if let Some(cmd) = reorder_command(&app.entries, app.entry_cursor, down) {
                            engine.send(cmd).await;
                        }
                        continue;
                    }

                    let action = handle_key(&mut app, key);
                    // Keep the visible text cursor on the active modal field.
                    sync_modal_cursors(&mut app);
                    match action {
                        Action::Quit => break,
                        Action::LoadEntry => {
                            app.filter = SingleLineInput::new();
                            app.item_cursor = 0;
                            app.detail_scroll = 0;
                            app.clear_items();
                            select_current_entry(&mut app, &engine, true).await;
                        }
                        Action::SaveNewQuery => {
                            let query_str = app.new_query_input.value().trim().to_string();
                            let name_str = app.new_query_name.value().trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.modal_field = 0;
                            app.new_query_input = SingleLineInput::new();
                            app.new_query_name = SingleLineInput::new();
                            let name = if name_str.is_empty() {
                                None
                            } else {
                                Some(name_str)
                            };
                            engine
                                .send(EngineCommand::AddQuery {
                                    name,
                                    query: query_str,
                                })
                                .await;
                        }
                        Action::SaveNewFilterStream => {
                            let name = app.new_filter_stream_name.value().trim().to_string();
                            let filter =
                                app.new_filter_stream_filter.value().trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.new_filter_stream_name = SingleLineInput::new();
                            app.new_filter_stream_filter = SingleLineInput::new();

                            // Determine parent: root_query_id of the currently selected entry
                            if let Some(entry) = app.entries.get(app.entry_cursor) {
                                let parent_id = entry.root_query_id();
                                let kind = entry.kind().to_string();
                                engine
                                    .send(EngineCommand::AddFilterStream {
                                        parent_id,
                                        kind,
                                        name,
                                        filter,
                                    })
                                    .await;
                            }
                        }
                        Action::SaveEditFilterStream => {
                            let name = app.edit_input.value().trim().to_string();
                            let filter = app.edit_input2.value().trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.edit_input = SingleLineInput::new();
                            app.edit_input2 = SingleLineInput::new();

                            if let Some(LeftPaneEntry::FilterStream(fs)) =
                                app.entries.get(app.entry_cursor)
                            {
                                engine
                                    .send(EngineCommand::EditFilterStream {
                                        id: fs.id,
                                        name,
                                        filter,
                                    })
                                    .await;
                            }
                        }
                        Action::SaveEditQuery => {
                            let name_input = app.edit_input.value().trim().to_string();
                            let new_query = app.edit_input2.value().trim().to_string();
                            app.input_mode = InputMode::Normal;
                            app.edit_input = SingleLineInput::new();
                            app.edit_input2 = SingleLineInput::new();

                            if let Some(LeftPaneEntry::Query(q)) =
                                app.entries.get(app.entry_cursor)
                            {
                                // Empty name means "use query string as label"
                                let new_name: Option<String> = if name_input.is_empty() {
                                    None
                                } else {
                                    Some(name_input)
                                };
                                engine
                                    .send(EngineCommand::EditQuery {
                                        id: q.id,
                                        name: new_name,
                                        query: new_query,
                                    })
                                    .await;
                            }
                        }
                        Action::Confirm => {
                            if let Some(item) = app.selected_item().cloned() {
                                let actions = item_actions(&item.kind);
                                if let Some(action) = actions.get(app.action_cursor).cloned() {
                                    match action {
                                        ItemAction::OpenBrowser => {
                                            app.input_mode = InputMode::Normal;
                                            engine
                                                .send(EngineCommand::OpenBrowser {
                                                    item: Box::new(item.clone()),
                                                })
                                                .await;
                                        }
                                        ItemAction::Comment => {
                                            app.input_mode = InputMode::Normal;
                                            suspend_tui(terminal)?;
                                            let editor_result = run_editor("");
                                            restore_tui(terminal)?;

                                            match editor_result {
                                                Ok(Some(body)) => {
                                                    engine
                                                        .send(EngineCommand::Comment {
                                                            url: item.url.clone(),
                                                            kind: item.kind.clone(),
                                                            body,
                                                        })
                                                        .await;
                                                }
                                                Ok(None) => {
                                                    app.status = Some("Comment cancelled".into());
                                                }
                                                Err(e) => {
                                                    app.status = Some(format!("Editor error: {e}"));
                                                }
                                            }
                                        }
                                        ItemAction::ViewComments => {
                                            // Open in-TUI comments popup: fetch via API in background
                                            app.input_mode = InputMode::CommentsPopup;
                                            app.comments.clear();
                                            app.comments_loading = true;
                                            app.comments_scroll = 0;
                                            engine
                                                .send(EngineCommand::LoadComments {
                                                    owner: item.repo_owner.clone(),
                                                    repo: item.repo_name.clone(),
                                                    number: item.number as u64,
                                                })
                                                .await;
                                        }
                                        ItemAction::ApprovePR => {
                                            app.input_mode = InputMode::Normal;
                                            suspend_tui(terminal)?;
                                            let editor_result = run_editor(
                                                "# Review comment (required for Comment / Request changes; optional for Approve)\n# Lines starting with '#' are ignored.\n",
                                            );
                                            restore_tui(terminal)?;

                                            match editor_result {
                                                Ok(body_opt) => {
                                                    // Strip comment lines; empty → no body. Then
                                                    // confirm the review event before submitting.
                                                    app.review_body = body_opt.and_then(|body| {
                                                        let stripped = body
                                                            .lines()
                                                            .filter(|line| !line.starts_with('#'))
                                                            .collect::<Vec<_>>()
                                                            .join("\n");
                                                        let stripped = stripped.trim().to_string();
                                                        (!stripped.is_empty()).then_some(stripped)
                                                    });
                                                    app.review_event_cursor = 0;
                                                    app.input_mode = InputMode::ReviewMenu;
                                                }
                                                Err(e) => {
                                                    app.status = Some(format!("Editor error: {e}"));
                                                }
                                            }
                                        }
                                        ItemAction::MergePR => {
                                            app.input_mode = InputMode::MergeMenu;
                                            app.merge_strategy_cursor = 0;
                                        }
                                        ItemAction::CopyUrl => {
                                            app.input_mode = InputMode::Normal;
                                            app.status = Some(match copy_to_clipboard_osc52(&item.url) {
                                                Ok(()) => "Copied URL to clipboard".into(),
                                                Err(e) => format!("Copy failed: {e}"),
                                            });
                                        }
                                        ItemAction::ReviewOctorus => {
                                            app.input_mode = InputMode::Normal;
                                            app.status = Some(run_octorus_review(terminal, &item)?);
                                        }
                                        ItemAction::RefreshItem => {
                                            app.input_mode = InputMode::Normal;
                                            refresh_selected_item(&mut app, &engine).await;
                                        }
                                    }
                                }
                            }
                        }
                        Action::ConfirmMergeStrategy => {
                            if let Some(item) = app.selected_item().cloned() {
                                let strategies = MergeStrategy::all();
                                if let Some(strategy) = strategies.get(app.merge_strategy_cursor).cloned() {
                                    app.input_mode = InputMode::Normal;
                                    engine
                                        .send(EngineCommand::Merge {
                                            url: item.url.clone(),
                                            strategy,
                                        })
                                        .await;
                                }
                            }
                        }
                        Action::ConfirmReviewEvent => {
                            if let Some(item) = app.selected_item().cloned()
                                && let Some(event) =
                                    ReviewEvent::all().get(app.review_event_cursor).copied()
                            {
                                // gh requires a body for comment / request-changes.
                                if event.requires_body() && app.review_body.is_none() {
                                    app.status = Some(
                                        "Review comment required for Comment / Request changes"
                                            .into(),
                                    );
                                } else {
                                    app.input_mode = InputMode::Normal;
                                    let body = app.review_body.take();
                                    engine
                                        .send(EngineCommand::SubmitReview {
                                            url: item.url.clone(),
                                            event,
                                            body,
                                        })
                                        .await;
                                }
                            }
                        }
                        Action::OpenBrowser => {
                            if let Some(item) = app.selected_item().cloned() {
                                engine
                                    .send(EngineCommand::OpenBrowser {
                                        item: Box::new(item),
                                    })
                                    .await;
                            }
                        }
                        Action::CopyUrl => {
                            if let Some(item) = app.selected_item().cloned() {
                                app.status = Some(match copy_to_clipboard_osc52(&item.url) {
                                    Ok(()) => "Copied URL to clipboard".into(),
                                    Err(e) => format!("Copy failed: {e}"),
                                });
                            }
                        }
                        Action::ConfirmCustom => {
                            let action = app
                                .custom_actions_for_selected()
                                .get(app.custom_action_cursor)
                                .map(|&a| a.clone());
                            if let (Some(action), Some(item)) =
                                (action, app.selected_item().cloned())
                            {
                                app.input_mode = InputMode::Normal;
                                engine
                                    .send(EngineCommand::RunCustomAction {
                                        action: Box::new(action),
                                        item: Box::new(item),
                                    })
                                    .await;
                            }
                        }
                        Action::ReviewOctorus => {
                            if let Some(item) = app.selected_item().cloned() {
                                app.status = Some(run_octorus_review(terminal, &item)?);
                            }
                        }
                        Action::RefreshList => {
                            refresh_selected_list(&mut app, &engine).await;
                        }
                        Action::RefreshItem => {
                            refresh_selected_item(&mut app, &engine).await;
                        }
                        Action::FullResync => {
                            full_resync_selected(&mut app, &engine).await;
                        }
                        Action::ApplyPending => {
                            app.apply_pending_items();
                        }
                        Action::None => {}
                    }
                    // Viewing an item (cursor on the item list or its detail pane)
                    // marks it read and decrements the unread badge. Idempotent —
                    // a no-op once the item is already read.
                    if app.input_mode == InputMode::Normal
                        && matches!(app.focus, Focus::ItemList | Focus::ItemDetail)
                    {
                        mark_selected_item_read(&mut app, &engine).await;
                    }
                }
            }
            Some(msg) = engine.recv() => {
                handle_app_message(&mut app, &engine, msg).await;
            }
        }
    }

    Ok(())
}
