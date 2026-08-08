//! Context/action menu: building the anchored `PopupMenu` for items and
//! left-pane entries, and dispatching the chosen `ItemAction`.

use gpui::*;
use gpui_component::menu::PopupMenu;

use glauca_core::actions::CustomAction;
use glauca_core::types::ItemEntry;

use super::*;

impl GlaucaApp {
    /// Open a `PopupMenu` anchored at `pos`. A self-managed overlay: it focuses itself and
    /// emits `DismissEvent` on selection/Esc/outside-click, which clears `self.menu`.
    pub(crate) fn open_menu(
        &mut self,
        pos: Point<Pixels>,
        kind: MenuKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Custom actions for the item menu, computed here (not in `populate_menu`,
        // which runs while the app entity is leased). See `actions_for_item`.
        let custom_actions = match &kind {
            MenuKind::Item(item) => self.actions_for_item(item),
            _ => Vec::new(),
        };
        let app = cx.entity();
        let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
            populate_menu(menu, &app, kind, custom_actions, window, cx)
        });
        cx.subscribe(&menu, |this, _menu, _e: &DismissEvent, cx| {
            this.menu = None;
            cx.notify();
        })
        .detach();
        menu.focus_handle(cx).focus(window, cx);
        self.menu = Some(menu);
        self.menu_pos = pos;
        cx.notify();
    }

    pub(crate) fn dispatch_action(
        &mut self,
        action: ItemAction,
        item: ItemEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            ItemAction::OpenBrowser => self.send(EngineCommand::OpenBrowser {
                item: Box::new(item),
            }),
            ItemAction::Comment => self.open_comment_dialog(item, window, cx),
            ItemAction::ApprovePR => self.open_review_dialog(item, window, cx),
            ItemAction::MergePR => self.open_merge_dialog(item, window, cx),
            ItemAction::ViewComments => self.open_comments(item, window, cx),
            ItemAction::CopyUrl => cx.write_to_clipboard(ClipboardItem::new_string(item.url)),
            ItemAction::RefreshItem => {
                let number = item.number;
                if let Some(query_id) = self.selected_root_query_id() {
                    self.send(EngineCommand::RefreshItem {
                        query_id,
                        repo_owner: item.repo_owner,
                        repo_name: item.repo_name,
                        number,
                    });
                    self.status = Some(format!("Refreshing #{number}…"));
                }
            }
            // octorus is a terminal TUI launched only from the CLI front-end; it
            // is never offered in the GUI menu, so this arm is unreachable.
            ItemAction::ReviewOctorus => {}
        }
    }
}

/// Add a menu item whose click runs `f` against the `GlaucaApp` entity. Keeps the
/// click closures `'static` while still calling back into the view.
pub(crate) fn app_menu_item<F>(
    menu: PopupMenu,
    app: &Entity<GlaucaApp>,
    label: impl Into<SharedString>,
    f: F,
) -> PopupMenu
where
    F: Fn(&mut GlaucaApp, &mut Window, &mut Context<GlaucaApp>) + 'static,
{
    let app = app.clone();
    menu.item(
        PopupMenuItem::new(label.into()).on_click(move |_ev, window, cx| {
            app.update(cx, |this, cx| f(this, window, cx));
        }),
    )
}

/// Build the action menu for a given `MenuKind`. Shared by right-click and Enter.
pub(crate) fn populate_menu(
    mut menu: PopupMenu,
    app: &Entity<GlaucaApp>,
    kind: MenuKind,
    custom_actions: Vec<CustomAction>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    match kind {
        MenuKind::Item(item) => {
            let item = *item;
            for action in ItemAction::available_for(&item.kind) {
                let item = item.clone();
                let label = action.label().to_string();
                menu = app_menu_item(menu, app, label, move |this, window, cx| {
                    this.dispatch_action(action.clone(), item.clone(), window, cx);
                });
            }
            // Cascading submenu of user-defined custom actions applicable to this
            // item's kind (precomputed by the caller). Hidden when none apply.
            if !custom_actions.is_empty() {
                menu = menu.separator();
                let app = app.clone();
                menu = menu.submenu("Custom actions", window, cx, move |submenu, _w, _cx| {
                    add_custom_action_items(submenu, &app, &item, &custom_actions)
                });
            }
        }
        MenuKind::Entry { index, is_query } => {
            menu = app_menu_item(menu, app, "Edit", move |this, window, cx| {
                this.edit_entry_at(index, window, cx);
            });
            menu = app_menu_item(menu, app, "Delete", move |this, _w, _cx| {
                this.delete_entry_at(index);
            });
            menu = app_menu_item(menu, app, "Move up", move |this, _w, _cx| {
                this.reorder_entry(index, false);
            });
            menu = app_menu_item(menu, app, "Move down", move |this, _w, _cx| {
                this.reorder_entry(index, true);
            });
            menu = menu.separator();
            menu = app_menu_item(menu, app, "Mark all as read", move |this, _w, _cx| {
                this.mark_all_read_at(index);
            });
            menu = menu.separator();
            if is_query {
                menu = app_menu_item(menu, app, "New filter stream", move |this, window, cx| {
                    this.new_filter_stream_under(index, window, cx);
                });
            }
            menu = app_menu_item(menu, app, "New query", |this, window, cx| {
                this.new_query(window, cx);
            });
        }
        MenuKind::NewQueryOnly => {
            menu = app_menu_item(menu, app, "New query", |this, window, cx| {
                this.new_query(window, cx);
            });
        }
        MenuKind::CustomActions { item, actions } => {
            menu = add_custom_action_items(menu, app, &item, &actions);
        }
    }
    menu
}

/// Populate `menu` with one item per custom `action`, each sending `RunCustomAction` on
/// click. Shared by the item menu's submenu and the `x` picker.
pub(crate) fn add_custom_action_items(
    mut menu: PopupMenu,
    app: &Entity<GlaucaApp>,
    item: &ItemEntry,
    actions: &[CustomAction],
) -> PopupMenu {
    for action in actions {
        let item = item.clone();
        let action = action.clone();
        let label = action.display_label().to_string();
        menu = app_menu_item(menu, app, label, move |this, _w, _cx| {
            this.send(EngineCommand::RunCustomAction {
                action: Box::new(action.clone()),
                item: Box::new(item.clone()),
            });
        });
    }
    menu
}
