//! Centralized semantic-icon set for the TUI.
//!
//! Every glyph that *represents a thing* (issue, PR, merged, approved, comment,
//! lock, bell, search, …) is defined here once, in two variants: the default
//! emoji/Unicode set that renders in virtually any terminal font, and an
//! icon-font set (Font Awesome glyphs) for terminals whose font provides them
//! (`fonts-font-awesome`, or a Nerd Font). The active set is chosen from
//! `TuiSettings::use_icon_font` and held on `App::icons`, so draw code reads
//! `app.icons.*` without re-deciding per frame.
//!
//! Structural/ornamental glyphs (selection cursor ▶, separators ━ ─, bars ▌,
//! expanders ▸) are layout chrome, not semantic icons, and stay hardcoded in
//! `ui.rs`.

use ratatui::style::{Color, Style};

/// The glyph for each semantic icon the TUI renders.
///
/// Public fields are glyphs a call site reads directly. The fields below
/// `mode_badge` are private and reached only through the `item_icon` / `review_*`
/// methods, which map a domain string (item kind+state, review state/decision)
/// to a glyph — keeping that mapping in one place instead of at every call site.
#[derive(Debug, Clone)]
pub struct Icons {
    /// Marker shown before a saved query in the left pane.
    pub query: &'static str,
    pub refresh: &'static str,
    pub new_item: &'static str,
    pub private: &'static str,
    pub pending_reviewer: &'static str,
    pub syncing: &'static str,
    pub bell: &'static str,
    pub clock: &'static str,
    /// Marker shown before a filter stream (a saved filter nested under a query).
    pub filter_stream: &'static str,
    /// Status-bar badge for the active set: `None` in the Unicode set (so it
    /// never renders as tofu on a terminal without the icon font), `Some` in
    /// the icon-font set.
    pub mode_badge: Option<&'static str>,
    merged: &'static str,
    pr: &'static str,
    issue: &'static str,
    /// Plain check mark for a PR's overall review decision (distinct from the
    /// per-reviewer `review_approved` badge).
    check: &'static str,
    review_approved: &'static str,
    review_changes: &'static str,
    review_commented: &'static str,
    review_dismissed: &'static str,
}

impl Icons {
    /// Pick the icon set for the given preference.
    pub fn new(use_icon_font: bool) -> Self {
        if use_icon_font {
            Self::icon_font()
        } else {
            Self::unicode()
        }
    }

    /// Default emoji/Unicode glyphs — render in virtually any terminal font.
    pub fn unicode() -> Self {
        Self {
            query: "🔍",
            refresh: "↻",
            new_item: "●",
            private: "🔒",
            pending_reviewer: "○",
            syncing: "⟳",
            bell: "🔔",
            clock: "🕐",
            filter_stream: "↳",
            mode_badge: None,
            merged: "⬡",
            pr: "⎇",
            issue: "○",
            check: "✓",
            review_approved: "✅",
            review_changes: "✗",
            review_commented: "💬",
            review_dismissed: "↩",
        }
    }

    /// Font Awesome glyphs (Private Use Area), verified against
    /// `fonts-font-awesome` 7.2.0 Solid (`fa-solid-900.ttf`, family name
    /// "Font Awesome 6 Free"); a Nerd Font renders them too. These require an
    /// icon font in the terminal — they render as tofu/blank otherwise.
    /// Comments give the FA glyph name. The lone exception is `query`, a
    /// *brands* glyph (the GitHub logo) that lives in `fa-brands-400.ttf`, not
    /// the Solid font — it needs a Nerd Font (or the brands font) to render.
    pub fn icon_font() -> Self {
        Self {
            query: "\u{f09b}",            // fa github (brands; needs a Nerd Font)
            refresh: "\u{f021}",          // fa arrows-rotate
            new_item: "\u{f111}",         // fa circle
            private: "\u{f023}",          // fa lock
            pending_reviewer: "\u{f192}", // fa circle-dot
            syncing: "\u{f021}",          // fa arrows-rotate
            bell: "\u{f0f3}",             // fa bell
            clock: "\u{f017}",            // fa clock
            filter_stream: "\u{f160}",    // fa arrow-down-wide-short
            mode_badge: Some("\u{f6be}"), // fa cat
            merged: "\u{f387}",           // fa code-merge
            pr: "\u{e13c}",               // fa code-pull-request
            issue: "\u{f192}",            // fa circle-dot
            check: "\u{f00c}",            // fa check
            review_approved: "\u{f058}",  // fa circle-check
            review_changes: "\u{f00d}",   // fa xmark
            review_commented: "\u{f075}", // fa comment
            review_dismissed: "\u{f3e5}", // fa reply
        }
    }

    /// (badge, style) for a reviewer's submitted review state. The colour is
    /// independent of the icon set; only the glyph switches.
    pub fn review_state_badge(&self, state: &str) -> (&'static str, Style) {
        match state {
            "APPROVED" => (self.review_approved, Style::default().fg(Color::Green)),
            "CHANGES_REQUESTED" => (self.review_changes, Style::default().fg(Color::Red)),
            "COMMENTED" => (self.review_commented, Style::default().fg(Color::Blue)),
            "DISMISSED" => (self.review_dismissed, Style::default().fg(Color::Cyan)),
            _ => ("?", Style::default()),
        }
    }

    /// (glyph, style) for a PR's overall review decision. Colour matches the
    /// decision; the glyph follows the active set. Returns an empty glyph for
    /// unknown values, so the caller shows the raw decision text alone.
    pub fn review_decision_badge(&self, decision: &str) -> (&'static str, Style) {
        match decision {
            "APPROVED" => (self.check, Style::default().fg(Color::Green)),
            "CHANGES_REQUESTED" => (self.review_changes, Style::default().fg(Color::Red)),
            "REVIEW_REQUIRED" => (self.pending_reviewer, Style::default().fg(Color::Yellow)),
            _ => ("", Style::default()),
        }
    }

    /// Combined kind+state glyph for the item list: the glyph shows the kind
    /// (issue vs PR, with a merged PR shown as a merge), and the caller colours
    /// it by state (see `ui::state_style`). Mirrors GitHub's single stateful
    /// item icon, replacing a separate state dot + kind icon shown side by side.
    pub fn item_icon(&self, kind: &str, state: &str) -> &'static str {
        match (kind, state) {
            ("pull_request", "merged") => self.merged,
            ("pull_request", _) => self.pr,
            _ => self.issue,
        }
    }
}

impl Default for Icons {
    fn default() -> Self {
        Self::unicode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_selects_set() {
        // The two sets differ, and `new` picks between them by the flag.
        assert_eq!(Icons::new(false).pr, Icons::unicode().pr);
        assert_eq!(Icons::new(true).pr, Icons::icon_font().pr);
        assert_ne!(Icons::unicode().pr, Icons::icon_font().pr);
    }

    #[test]
    fn review_state_badge_maps() {
        let i = Icons::unicode();
        assert_eq!(i.review_state_badge("APPROVED").0, "✅");
        assert_eq!(i.review_state_badge("unknown").0, "?");
    }

    #[test]
    fn item_icon_combines_kind_and_state() {
        let i = Icons::unicode();
        // PR: kind glyph, but a merged PR swaps to the merge glyph.
        assert_eq!(i.item_icon("pull_request", "open"), i.pr);
        assert_eq!(i.item_icon("pull_request", "closed"), i.pr);
        assert_eq!(i.item_icon("pull_request", "merged"), i.merged);
        // Issue: kind glyph regardless of state (state is conveyed by colour).
        assert_eq!(i.item_icon("issue", "open"), i.issue);
        assert_eq!(i.item_icon("issue", "closed"), i.issue);
    }
}
