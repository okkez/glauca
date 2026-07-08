//! Add/edit dialogs for saved queries and filter streams: the shared
//! two-field form and the query/filter-stream specific entry points.

use gpui::*;

use glauca_core::engine::EngineCommand;
use glauca_core::types::*;

use super::*;

impl GlaucaApp {
    pub(crate) fn on_new_query(
        &mut self,
        _: &NewQuery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_query(window, cx);
    }

    /// Open the "add query" form.
    pub(crate) fn new_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_query_form(None, String::new(), String::new(), window, cx);
    }

    pub(crate) fn on_new_filter_stream(
        &mut self,
        _: &NewFilterStream,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_filter_stream_under(self.entry_cursor, window, cx);
    }

    /// Open the "add filter stream" form parented to the entry at `index`'s root query.
    pub(crate) fn new_filter_stream_under(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let parent_id = entry.root_query_id();
        let kind = entry.kind().to_string();
        self.open_filter_stream_form(
            FilterStreamFormParams {
                edit: None,
                parent_id,
                kind,
                init_name: String::new(),
                init_filter: String::new(),
            },
            window,
            cx,
        );
    }

    pub(crate) fn on_edit_entry(
        &mut self,
        _: &EditEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit_entry_at(self.entry_cursor, window, cx);
    }

    /// Open the edit form for the entry at `index` (query or filter stream).
    pub(crate) fn edit_entry_at(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(LeftPaneEntry::Query(q)) = self.entries.get(index) {
            let (id, name, query) = (q.id, q.label.clone(), q.query_str.clone());
            self.open_query_form(Some(id), name, query, window, cx);
        } else if let Some(LeftPaneEntry::FilterStream(fs)) = self.entries.get(index) {
            let (id, parent, kind, name, filter) = (
                fs.id,
                fs.parent_id,
                fs.kind.clone(),
                fs.name.clone(),
                fs.filter.clone(),
            );
            self.open_filter_stream_form(
                FilterStreamFormParams {
                    edit: Some(id),
                    parent_id: parent,
                    kind,
                    init_name: name,
                    init_filter: filter,
                },
                window,
                cx,
            );
        }
    }

    /// Open a two-`Input` dialog (the shared shell of the query / filter-stream
    /// forms) and hand the trimmed values to `on_submit` on OK. Field
    /// requirements stay with the caller: `on_submit` ignores invalid input,
    /// matching the previous behavior where OK always closes the dialog.
    pub(crate) fn open_two_field_form(
        &mut self,
        title: &'static str,
        first: (&'static str, String),
        second: (&'static str, String),
        on_submit: impl Fn(&mut Self, String, String) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let first_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(first.0)
                .default_value(first.1)
        });
        let second_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(second.0)
                .default_value(second.1)
        });
        let this = cx.weak_entity();
        let on_submit = std::rc::Rc::new(on_submit);
        window.open_dialog(cx, move |dlg, _w, _cx| {
            let (first_c, second_c) = (first_input.clone(), second_input.clone());
            let (first_ok, second_ok) = (first_input.clone(), second_input.clone());
            let this = this.clone();
            let on_submit = on_submit.clone();
            dlg.title(title)
                .w(px(520.))
                .content(move |content, _w, _cx| {
                    content
                        .gap_3()
                        .child(Input::new(&first_c))
                        .child(Input::new(&second_c))
                })
                .on_ok(move |_, _w, cx| {
                    let a = first_ok.read(cx).value().trim().to_string();
                    let b = second_ok.read(cx).value().trim().to_string();
                    if let Some(app) = this.upgrade() {
                        let on_submit = on_submit.clone();
                        app.update(cx, move |app, _| on_submit(app, a, b));
                    }
                    true
                })
        });
    }

    /// Add (`edit=None`) or edit (`edit=Some(id)`) a root query via a 2-field dialog.
    pub(crate) fn open_query_form(
        &mut self,
        edit: Option<i64>,
        init_name: String,
        init_query: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = if edit.is_some() {
            "Edit query"
        } else {
            "Add query"
        };
        self.open_two_field_form(
            title,
            ("display name (optional)", init_name),
            (
                "GitHub search query (e.g. repo:owner/name is:pr is:open)",
                init_query,
            ),
            move |app, n, q| {
                if q.is_empty() {
                    return; // the query string is required
                }
                let name = if n.is_empty() { None } else { Some(n) };
                match edit {
                    Some(id) => app.send(EngineCommand::EditQuery { id, name, query: q }),
                    None => app.send(EngineCommand::AddQuery { name, query: q }),
                }
            },
            window,
            cx,
        );
    }

    /// Add (`edit=None`) or edit (`edit=Some(id)`) a filter stream via a 2-field dialog.
    pub(crate) fn open_filter_stream_form(
        &mut self,
        params: FilterStreamFormParams,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let FilterStreamFormParams {
            edit,
            parent_id,
            kind,
            init_name,
            init_filter,
        } = params;
        let title = if edit.is_some() {
            "Edit filter stream"
        } else {
            "Add filter stream"
        };
        self.open_two_field_form(
            title,
            ("display name", init_name),
            ("filter (e.g. is:pr is:draft assignee:name)", init_filter),
            move |app, n, f| {
                if n.is_empty() || f.is_empty() {
                    return; // both fields are required
                }
                match edit {
                    Some(id) => app.send(EngineCommand::EditFilterStream {
                        id,
                        name: n,
                        filter: f,
                    }),
                    None => app.send(EngineCommand::AddFilterStream {
                        parent_id,
                        kind: kind.clone(),
                        name: n,
                        filter: f,
                    }),
                }
            },
            window,
            cx,
        );
    }
}
