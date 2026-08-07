//! Markdown → ratatui rendering for the detail pane and the comments popup.
//!
//! The only entry point to tui-markdown, so the two panes cannot drift apart, and so the
//! tests holding the dependency's version floor sit next to the call they constrain.

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
    fn rendered_text(source: &str) -> Vec<String> {
        render_markdown(source)
            .iter()
            .map(Line::to_string)
            .collect()
    }

    /// tui-markdown up to 0.3.8 panicked on a task list item in a *loose* list (items
    /// separated by blank lines): the item's paragraph opened a line with no spans, and the
    /// task marker was inserted at index 1 of it — `insertion index (is 1) should be <= len
    /// (is 0)`. A PR description shaped like this took the whole TUI down.
    ///
    /// joshka/tui-markdown#166 closed that path in 0.3.9 as a side effect of a layout fix;
    /// the `spans.insert(1, ..)` behind the panic is still there and upstream has no test
    /// for it. The version floor in the workspace Cargo.toml is therefore a correctness
    /// constraint, and every `loose_*` case below panics on 0.3.7, so lowering it fails the
    /// suite rather than shipping a crash.
    #[rstest]
    #[case::loose_task_then_plain("- [ ] a\n\n- b\n", &["- [ ] a", "- b"])]
    #[case::loose_all_tasks("- [ ] a\n\n- [x] b\n", &["- [ ] a", "- [x] b"])]
    #[case::loose_ordered("1. [ ] a\n\n2. [x] b\n", &["1. [ ] a", "2. [x] b"])]
    // Nested items are re-indented to 4 columns; the input's 2 are Markdown's minimum.
    #[case::loose_nested("- x\n  - [ ] a\n\n  - b\n", &["- x", "    - [ ] a", "    - b"])]
    // CRLF because that is what the GitHub API actually returns.
    #[case::loose_crlf("- [ ] a\r\n\r\n- b\r\n", &["- [ ] a", "- b"])]
    #[case::loose_link_first(
        "- [ ] [t](http://example.com)\n\n- b\n",
        &["- [ ] t (http://example.com)", "- b"]
    )]
    // A fence inside an item loses the indent that attached it to the item.
    #[case::loose_code_fence_in_item(
        "- [ ] a\n\n  ```sh\n  x\n  ```\n\n- [ ] b\n",
        &["- [ ] a", "", "```sh", "x", "```", "- [ ] b"]
    )]
    // The two below never panicked: they are the baseline the `loose_*` cases
    // differ from, not floor guards.
    #[case::tight("- [ ] a\n- [x] b\n", &["- [ ] a", "- [x] b"])]
    // An item with no text renders as the marker alone — hence the trailing space.
    #[case::empty_item("- [ ]\n\n- b\n", &["- [ ] ", "- b"])]
    fn renders_task_lists(#[case] source: &str, #[case] expected: &[&str]) {
        assert_eq!(rendered_text(source), expected);
    }

    /// TODO(upstream, joshka/tui-markdown): inside a blockquote the checkbox is inserted
    /// right after the `>` prefix, so it lands *before* its bullet. Pinned as-is, not
    /// endorsed: this test failing means upstream fixed it and the expectation should be
    /// corrected.
    #[test]
    fn blockquote_puts_the_checkbox_before_its_bullet() {
        // Note the two spaces: `[ ] ` is inserted whole, ahead of the `- ` bullet.
        assert_eq!(
            rendered_text("> - [ ] a\n>\n> - b\n"),
            vec![">[ ]  - a", "> - b"]
        );
    }
}
