//! Shared `#[cfg(test)]` fixtures for the tui module's split test modules
//! (`state`, `keys`, `process`, …). Kept in one place so the extracted test
//! modules don't each re-declare the same item / key / action builders.

use super::*;

/// A minimal open PR item; tests override only the fields they exercise via
/// `ItemEntry { field: …, ..make_item(n, "title") }`.
pub(crate) fn make_item(number: i64, title: &str) -> ItemEntry {
    ItemEntry {
        number,
        title: title.to_string(),
        repo_owner: "owner".into(),
        repo_name: "repo".into(),
        author: Some(glauca_core::types::UserRef::new("alice")),
        state: "open".into(),
        kind: "pull_request".into(),
        ..Default::default()
    }
}

pub(crate) fn make_app_with_items(titles: &[&str]) -> App {
    let mut app = App::new(vec![QueryEntry {
        id: 1,
        label: "test query".into(),
        query_str: "test query".into(),
        kind: "pull_request".into(),
    }]);
    app.items = titles
        .iter()
        .enumerate()
        .map(|(i, t)| make_item(i as i64 + 1, t))
        .collect();
    app
}

pub(crate) fn make_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(crate) fn make_ctrl_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

/// Build a single-line input field pre-filled with `s`, cursor at the end
/// (mimics text the user has just typed).
pub(crate) fn ta(s: &str) -> SingleLineInput {
    SingleLineInput::from_text(s)
}

pub(crate) fn make_custom_action(name: &str, kinds: &[&str]) -> CustomAction {
    CustomAction {
        name: name.into(),
        label: None,
        command: vec!["true".into()],
        kinds: kinds.iter().map(|s| s.to_string()).collect(),
        env: Default::default(),
    }
}
