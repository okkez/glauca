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

    /// Add (`edit=None`) or edit (`edit=Some(id)`) a filter stream via a dialog
    /// with a name field plus one or more OR-group boxes (each box is one
    /// AND-group; the boxes are ORed — see `glauca_core::filter::StreamFilter`).
    /// The box set lives in `self.filter_stream_form` so add/remove re-renders;
    /// on save the non-blank boxes are joined newline-separated.
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

        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("display name")
                .default_value(init_name)
        });
        // One box per stored OR-group (newline-separated); always at least one.
        let filters: Vec<Entity<InputState>> =
            glauca_core::filter::split_filter_groups(&init_filter)
                .into_iter()
                .map(|g| {
                    let g = g.to_string();
                    cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder("filter (e.g. is:pr is:draft assignee:name)")
                            .default_value(g)
                    })
                })
                .collect();
        self.filter_stream_form = Some(FilterStreamForm {
            edit,
            parent_id,
            kind,
            name,
            filters,
        });

        let this = cx.weak_entity();
        window.open_dialog(cx, move |dlg, _w, _cx| {
            let this = this.clone();
            dlg.title(title)
                .w(px(560.))
                .content(move |content, _w, cx| {
                    let Some(app) = this.upgrade() else {
                        return content;
                    };
                    let Some(form) = app.read(cx).filter_stream_form.clone() else {
                        return content;
                    };

                    let mut col = content
                        .gap_2()
                        .child(div().text_sm().child("Name"))
                        .child(Input::new(&form.name))
                        .child(div().text_sm().child("Filters (item matches ANY box)"));

                    let single = form.filters.len() == 1;
                    for (i, box_input) in form.filters.iter().enumerate() {
                        if i > 0 {
                            col = col.child(div().text_xs().child("OR"));
                        }
                        let mut row = h_flex()
                            .w_full()
                            .gap_2()
                            .child(div().flex_1().child(Input::new(box_input)));
                        // Keep at least one box: no remove button when only one remains.
                        if !single {
                            let this_rm = this.clone();
                            row = row.child(
                                Button::new(("fs-remove", i)).ghost().label("✕").on_click(
                                    move |_, _w, cx| {
                                        if let Some(app) = this_rm.upgrade() {
                                            app.update(cx, |app, cx| {
                                                if let Some(f) = &mut app.filter_stream_form
                                                    && f.filters.len() > 1
                                                {
                                                    f.filters.remove(i);
                                                }
                                                cx.notify();
                                            });
                                        }
                                    },
                                ),
                            );
                        }
                        col = col.child(row);
                    }

                    let this_add = this.clone();
                    col = col.child(
                        Button::new("fs-add")
                            .ghost()
                            .label("+ Add OR box")
                            .on_click(move |_, window, cx| {
                                if let Some(app) = this_add.upgrade() {
                                    app.update(cx, |app, cx| {
                                        let inp = cx.new(|cx| {
                                            InputState::new(window, cx).placeholder(
                                                "filter (e.g. is:pr is:draft assignee:name)",
                                            )
                                        });
                                        if let Some(f) = &mut app.filter_stream_form {
                                            f.filters.push(inp.clone());
                                        }
                                        inp.focus_handle(cx).focus(window, cx);
                                        cx.notify();
                                    });
                                }
                            }),
                    );

                    let this_cancel = this.clone();
                    let this_ok = this.clone();
                    let buttons = h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("fs-cancel").ghost().label("Cancel").on_click(
                            move |_, window, cx| {
                                window.close_dialog(cx);
                                if let Some(app) = this_cancel.upgrade() {
                                    app.update(cx, |app, _| app.filter_stream_form = None);
                                }
                            },
                        ))
                        .child(Button::new("fs-ok").primary().label("OK").on_click(
                            move |_, window, cx| {
                                let Some(app) = this_ok.upgrade() else {
                                    return;
                                };
                                // Read + validate the current form; keep the dialog
                                // open (return None) when name or all boxes are blank.
                                let result = app.update(cx, |app, cx| {
                                    let form = app.filter_stream_form.as_ref()?;
                                    let name = form.name.read(cx).value().trim().to_string();
                                    let filter = glauca_core::filter::join_filter_groups(
                                        form.filters.iter().map(|b| b.read(cx).value().to_string()),
                                    );
                                    if name.is_empty() || filter.is_empty() {
                                        return None;
                                    }
                                    Some((
                                        form.edit,
                                        form.parent_id,
                                        form.kind.clone(),
                                        name,
                                        filter,
                                    ))
                                });
                                if let Some((edit, parent_id, kind, name, filter)) = result {
                                    window.close_dialog(cx);
                                    app.update(cx, |app, _| {
                                        app.filter_stream_form = None;
                                        match edit {
                                            Some(id) => app.send(EngineCommand::EditFilterStream {
                                                id,
                                                name,
                                                filter,
                                            }),
                                            None => app.send(EngineCommand::AddFilterStream {
                                                parent_id,
                                                kind,
                                                name,
                                                filter,
                                            }),
                                        }
                                    });
                                }
                            },
                        ));

                    col.child(buttons)
                })
        });
    }
}
