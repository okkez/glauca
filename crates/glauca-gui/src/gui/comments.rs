//! Comments overlay: open/close and the single-key controls (scroll, jump,
//! sort/hidden toggles) that are active while the overlay is focused. `on_quit`
//! lives here too since it flushes and exits from the same key surface.

use gpui::*;

use glauca_core::types::*;

use super::*;

impl GlaucaApp {
    /// `c` — open the comments overlay for the selected item.
    pub(crate) fn on_open_comments(
        &mut self,
        _: &OpenComments,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.focus == Focus::QueryList {
            return;
        }
        if let Some(item) = self.selected_item() {
            self.open_comments(item, window, cx);
        }
    }

    /// Open the comments overlay for `item` and request its comments. Clearing
    /// `comments` + setting `comments_loading` first means a quick reopen never
    /// shows the previous item's comments.
    pub(crate) fn open_comments(
        &mut self,
        item: ItemEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.comments.clear();
        self.comments_loading = true;
        self.comments_open = true;
        self.comments_sort_desc = false;
        self.comments_show_hidden = false;
        self.comments_scroll.set_offset(point(px(0.), px(0.)));
        self.comments_title =
            SharedString::from(format!("Comments — #{} {}", item.number, item.title));
        self.send(EngineCommand::LoadComments {
            owner: item.repo_owner.clone(),
            repo: item.repo_name.clone(),
            number: item.number as u64,
        });
        self.comments_focus_handle.focus(window, cx);
        cx.notify();
    }

    /// Close the comments overlay and return focus to the root so nav keys work.
    pub(crate) fn close_comments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.comments_open = false;
        self.comments_loading = false;
        self.comments.clear();
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn on_comments_scroll_down(
        &mut self,
        _: &CommentsScrollDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        scroll_vertically(&self.comments_scroll, DETAIL_SCROLL_STEP);
        cx.notify();
    }

    pub(crate) fn on_comments_scroll_up(
        &mut self,
        _: &CommentsScrollUp,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        scroll_vertically(&self.comments_scroll, -DETAIL_SCROLL_STEP);
        cx.notify();
    }

    pub(crate) fn on_comments_top(
        &mut self,
        _: &CommentsTop,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.comments_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    pub(crate) fn on_comments_bottom(
        &mut self,
        _: &CommentsBottom,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.comments_scroll.scroll_to_bottom();
        cx.notify();
    }

    pub(crate) fn on_comments_toggle_sort(
        &mut self,
        _: &CommentsToggleSort,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.comments_sort_desc = !self.comments_sort_desc;
        self.comments_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    pub(crate) fn on_comments_toggle_hidden(
        &mut self,
        _: &CommentsToggleHidden,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.comments_show_hidden = !self.comments_show_hidden;
        self.comments_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    pub(crate) fn on_comments_close(
        &mut self,
        _: &CommentsClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_comments(window, cx);
    }

    pub(crate) fn on_quit(&mut self, _: &Quit, _window: &mut Window, cx: &mut Context<Self>) {
        // The `on_app_quit` hook (registered in `new`) flushes any pending settings
        // synchronously during shutdown, so quitting via the `q`/menu action needs
        // nothing special here beyond triggering the quit.
        cx.quit();
    }
}
