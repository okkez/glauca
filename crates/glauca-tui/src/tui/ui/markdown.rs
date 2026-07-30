//! Markdown → ratatui rendering for the detail pane and the comments popup.
//!
//! The only entry point to tui-markdown. A one-line wrapper earns that on the
//! history: a workaround for this pre-1.0 dependency has already lived here once,
//! and the tests that hold its version floor in place need somewhere to sit.

use super::*;

/// Render a Markdown body into styled lines borrowed from `source`.
pub(super) fn render_markdown(source: &str) -> Vec<Line<'_>> {
    tui_markdown::from_str(source).lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// Rendered lines as plain text (`Line`'s `Display` concatenates its spans).
    fn rendered(source: &str) -> Vec<String> {
        render_markdown(source)
            .iter()
            .map(Line::to_string)
            .collect()
    }

    /// tui-markdown up to 0.3.8 panicked on a task list item in a *loose* list
    /// (items separated by blank lines): the item's paragraph opened a line with
    /// no spans, and the task marker was inserted at index 1 of it. A PR
    /// description shaped like this took the whole TUI down.
    ///
    /// That makes the version floor in the workspace Cargo.toml a correctness
    /// constraint, and this the test that holds it: every case below except
    /// `tight` and `empty_item` panics on 0.3.7 (verified), so lowering the floor
    /// fails the suite rather than shipping a crash.
    #[rstest]
    #[case::loose("- [ ] a\n\n- b\n", &["- [ ] a", "- b"])]
    #[case::loose_all_tasks("- [ ] a\n\n- [x] b\n", &["- [ ] a", "- [x] b"])]
    #[case::loose_ordered("1. [ ] a\n\n2. [x] b\n", &["1. [ ] a", "2. [x] b"])]
    #[case::loose_nested("- x\n  - [ ] a\n\n  - b\n", &["- x", "    - [ ] a", "    - b"])]
    // CRLF because that is what the GitHub API actually returns.
    #[case::loose_crlf("- [ ] a\r\n\r\n- b\r\n", &["- [ ] a", "- b"])]
    #[case::loose_link_first(
        "- [ ] [t](http://example.com)\n\n- b\n",
        &["- [ ] t (http://example.com)", "- b"]
    )]
    #[case::loose_code_fence_in_item(
        "- [ ] a\n\n  ```sh\n  x\n  ```\n\n- [ ] b\n",
        &["- [ ] a", "", "```sh", "x", "```", "- [ ] b"]
    )]
    #[case::tight("- [ ] a\n- [x] b\n", &["- [ ] a", "- [x] b"])]
    #[case::empty_item("- [ ]\n\n- b\n", &["- [ ] ", "- b"])]
    fn renders_task_lists(#[case] source: &str, #[case] expected: &[&str]) {
        assert_eq!(rendered(source), expected);
    }

    /// Pinned upstream wart, not desired behaviour: inside a blockquote the
    /// checkbox is inserted right after the `>` prefix, so it lands *before* the
    /// bullet it belongs to. This test failing means upstream fixed it and the
    /// expectation should simply be corrected.
    #[test]
    fn blockquote_puts_the_checkbox_before_its_bullet() {
        assert_eq!(
            rendered("> - [ ] a\n>\n> - b\n"),
            vec![">[ ]  - a", "> - b"]
        );
    }
}
