//! The run loop entry point. `run` owns terminal setup/teardown around
//! `run_app`, which starts the engine, builds `App`, and drives the
//! `tokio::select!` loop: draw, dispatch key events to `Action`s, and forward
//! engine messages to `handle_app_message`.

use super::*;

pub async fn run(pool: SqlitePool, gh: Octocrab) -> Result<()> {
    // Install before `enter_tui`, so that a panic during setup still runs the
    // hook; `leave_tui` tolerates a terminal that was never entered.
    install_panic_hook();

    let mut stdout = io::stdout();
    enter_tui(&mut stdout)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, pool, gh).await;

    // Restore terminal unconditionally — best-effort, because a failed escape
    // write must not swallow whatever the run loop was actually reporting.
    let _ = leave_tui(terminal.backend_mut());

    result
}

/// Hand the terminal back (see [`leave_tui`]) before the default hook prints the
/// panic message.
///
/// Without this a panic anywhere in the draw path (tui-markdown has been one such source)
/// leaves the terminal in raw mode on the alternate screen with the cursor hidden, so the
/// shell that comes back echoes nothing. The teardown in `run` cannot cover it, because
/// release builds use `panic = "abort"` and never unwind.
///
/// The hook fires for a panic on any thread, including the engine's background tasks, since
/// under abort one of those takes the process down too.
///
/// TODO(known limitation, debug builds only): there tokio catches a background task's
/// panic, so the TUI keeps drawing into a terminal this has just reset. Accepted rather than
/// gated on the UI thread: the engine treats task failure as unrecoverable anyway, so a
/// visibly broken session beats one that looks fine and has silently stopped syncing.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = leave_tui(&mut io::stdout());
        previous(info);
    }));
}

async fn run_app<B: ratatui::backend::Backend + io::Write>(
    terminal: &mut Terminal<B>,
    pool: SqlitePool,
    gh: Octocrab,
) -> Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    // Start the async engine: left-pane entries, current user, background tasks.
    let tui_settings = TuiSettings::load();
    let (mut engine, init) = Engine::start(
        pool,
        gh,
        glauca_core::engine::SyncConfig::effective(
            tui_settings.sync_interval_secs,
            tui_settings.full_fetch_interval_secs,
        ),
        glauca_core::engine::MaintenanceConfig::effective(
            tui_settings.retention_days,
            tui_settings.max_items_per_query,
        ),
    )
    .await?;

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

    // Default to repainting after every handled event; the one exception is a mouse event
    // we don't act on (motion/drag, which crossterm emits in bursts), which opts out below.
    // Defaulting on means a new event arm can't silently freeze the UI.
    let mut needs_redraw = true;

    loop {
        if needs_redraw {
            terminal.draw(|f| ui::draw(f, &app))?;
        }
        needs_redraw = true;

        tokio::select! {
            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event {
                    // Any keystroke breaks a pending double-click chain, so a
                    // click that happens to follow (e.g. after closing a modal
                    // opened with Enter) isn't mistaken for a double-click.
                    app.last_mouse_click = None;
                    // Ignore key-release events: terminals with the keyboard-enhancement
                    // protocol (or Windows) emit them, and acting on both press and
                    // release would double-fire 'd'/'J'/'K'. Repeat events are kept so
                    // held-key repeat still works if enhancement flags are enabled.
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    // 'd' in query list → delete the selected entry; the UI updates on the
                    // engine's *Deleted message once the DB write succeeds.
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

                    // 'a' in query list → mark the selected entry read: a query marks its
                    // whole root query, a filter stream only its matching items, with the
                    // filter expanded here since the engine does not know `@me`.
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

                    // J/K: move the selected entry within its group. The entries vec is
                    // replaced wholesale when the engine's EntriesReloaded arrives.
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
                        Action::LoadEntry => load_selected_entry(&mut app, &engine, true).await,
                        Action::LoadEntryCached => {
                            load_selected_entry(&mut app, &engine, false).await
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
                            let name = app.filter_stream_name.value().trim().to_string();
                            // Join the OR-group boxes into the stored newline-separated
                            // string (blank boxes dropped); see StreamFilter.
                            let filter = glauca_core::filter::join_filter_groups(
                                app.filter_stream_filters.iter().map(|b| b.value()),
                            );
                            app.input_mode = InputMode::Normal;
                            keys::reset_filter_stream_modal(&mut app);

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
                            let name = app.filter_stream_name.value().trim().to_string();
                            let filter = glauca_core::filter::join_filter_groups(
                                app.filter_stream_filters.iter().map(|b| b.value()),
                            );
                            app.input_mode = InputMode::Normal;
                            keys::reset_filter_stream_modal(&mut app);

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
                                            leave_tui(terminal.backend_mut())?;
                                            let editor_result = run_editor("");
                                            reenter_tui(terminal)?;

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
                                            leave_tui(terminal.backend_mut())?;
                                            let editor_result = run_editor(
                                                "# Review comment (required for Comment / Request changes; optional for Approve)\n# Lines starting with '#' are ignored.\n",
                                            );
                                            reenter_tui(terminal)?;

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
                        Action::OpenBrowser => open_selected_in_browser(&app, &engine).await,
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
                    // Viewing an item marks it read (and lazily fetches its body).
                    if app.input_mode == InputMode::Normal {
                        refresh_focused_item(&mut app, &engine).await;
                    }
                } else if let Event::Mouse(mouse) = event {
                    // Mouse is only handled in Normal mode. `handle_mouse` returns None
                    // for events we don't act on (motion/drag/etc.).
                    if app.input_mode == InputMode::Normal
                        && let Some(action) = handle_mouse(&mut app, mouse)
                    {
                        match action {
                            Action::LoadEntry => {
                                load_selected_entry(&mut app, &engine, true).await
                            }
                            Action::LoadEntryCached => {
                                load_selected_entry(&mut app, &engine, false).await
                            }
                            Action::OpenBrowser => {
                                open_selected_in_browser(&app, &engine).await
                            }
                            _ => {}
                        }
                        // Mirror the post-key handling for the newly-selected item.
                        refresh_focused_item(&mut app, &engine).await;
                    } else {
                        // Motion/drag, or a click while a modal is open: nothing
                        // changed, so skip the repaint (motion arrives in bursts).
                        needs_redraw = false;
                    }
                }
                // Other events (resize/focus/paste) keep the default repaint.
            }
            Some(msg) = engine.recv() => {
                handle_app_message(&mut app, &engine, msg).await;
            }
        }
    }

    Ok(())
}

/// Load the currently selected left-pane entry into the item list, resetting the filter and
/// cursors. `always_sync` forces a GitHub fetch (a deliberate select); when false, only
/// stale caches are synced (wheel scrolling).
async fn load_selected_entry(app: &mut App, engine: &Engine, always_sync: bool) {
    app.filter = SingleLineInput::new();
    app.item_cursor = 0;
    app.detail_scroll = 0;
    app.clear_items();
    select_current_entry(app, engine, always_sync).await;
}

/// After a selection change, mark the focused item read and lazily fetch its body if
/// missing — a no-op unless an item pane is focused. Shared by the key and mouse paths.
async fn refresh_focused_item(app: &mut App, engine: &Engine) {
    if matches!(app.focus, Focus::ItemList | Focus::ItemDetail) {
        mark_selected_item_read(app, engine).await;
        refetch_selected_body_if_missing(app, engine).await;
    }
}

/// Open the currently selected item in the browser. Shared by the `OpenBrowser`
/// key action and mouse double-clicks on an item.
async fn open_selected_in_browser(app: &App, engine: &Engine) {
    if let Some(item) = app.selected_item().cloned() {
        engine
            .send(EngineCommand::OpenBrowser {
                item: Box::new(item),
            })
            .await;
    }
}
