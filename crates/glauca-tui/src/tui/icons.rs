//! Centralized semantic-icon set for the TUI.
//!
//! Every glyph that *represents a thing* (issue, PR, merged, approved, comment,
//! lock, bell, search, …) is defined here once, in two variants: the default
//! emoji/Unicode set that renders in virtually any terminal font, and a Nerd
//! Font (octicon/Font-Awesome) set that requires a Nerd Font in the terminal.
//! The active set is chosen from `TuiSettings::use_nerd_font_icons` and held on
//! `App::icons`, so draw code reads `app.icons.*` without re-deciding per frame.
//!
//! Structural/ornamental glyphs (selection cursor ▶, separators ━ ─, bars ▌,
//! expanders ▸) are layout chrome, not semantic icons, and stay hardcoded in
//! `ui.rs`.

use ratatui::style::{Color, Style};

/// The glyph for each semantic icon the TUI renders.
///
/// Public fields are glyphs a call site reads directly. The fields below
/// `mode_badge` are private and reached only through the `*_badge` / `kind_icon`
/// methods, which map a domain string (item state, kind, review state/decision)
/// to a glyph — keeping that mapping in one place instead of at every call site.
#[derive(Debug, Clone)]
pub struct Icons {
    pub search: &'static str,
    pub refresh: &'static str,
    pub new_item: &'static str,
    pub private: &'static str,
    pub pending_reviewer: &'static str,
    pub syncing: &'static str,
    pub bell: &'static str,
    pub clock: &'static str,
    /// Status-bar badge for the active set: `None` in the Unicode set (so it
    /// never renders as tofu on a non-Nerd-Font terminal), `Some` in the Nerd
    /// Font set.
    pub mode_badge: Option<&'static str>,
    open: &'static str,
    merged: &'static str,
    closed: &'static str,
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
    pub fn new(use_nerd_font: bool) -> Self {
        if use_nerd_font {
            Self::nerd()
        } else {
            Self::unicode()
        }
    }

    /// Default emoji/Unicode glyphs — render in virtually any terminal font.
    pub fn unicode() -> Self {
        Self {
            search: "🔍",
            refresh: "↻",
            new_item: "●",
            private: "🔒",
            pending_reviewer: "○",
            syncing: "⟳",
            bell: "🔔",
            clock: "🕐",
            mode_badge: None,
            open: "●",
            merged: "⬡",
            closed: "✕",
            pr: "⎇",
            issue: "○",
            check: "✓",
            review_approved: "✅",
            review_changes: "✗",
            review_commented: "💬",
            review_dismissed: "↩",
        }
    }

    /// Nerd Font glyphs (Private Use Area). Git-specific icons use octicons;
    /// the rest use Font Awesome. Requires a Nerd Font in the terminal — these
    /// codepoints render as tofu/blank otherwise. Comments give the glyph name;
    /// verify the rendering visually if a glyph looks wrong.
    pub fn nerd() -> Self {
        Self {
            search: "\u{f002}",            // nf-fa-search
            refresh: "\u{f021}",           // nf-fa-refresh
            new_item: "\u{f111}",          // nf-fa-circle (filled dot)
            private: "\u{f023}",           // nf-fa-lock
            pending_reviewer: "\u{f10c}",  // nf-fa-circle_o (hollow dot)
            syncing: "\u{f021}",           // nf-fa-refresh
            bell: "\u{f0f3}",              // nf-fa-bell
            clock: "\u{f017}",             // nf-fa-clock_o
            mode_badge: Some("\u{f011b}"), // nf-md-cat
            open: "\u{f111}",              // nf-fa-circle (filled dot)
            merged: "\u{f419}",            // nf-oct-git_merge
            closed: "\u{f057}",            // nf-fa-times_circle
            pr: "\u{f407}",                // nf-oct-git_pull_request
            issue: "\u{f41b}",             // nf-oct-issue_opened
            check: "\u{f00c}",             // nf-fa-check
            review_approved: "\u{f00c}",   // nf-fa-check
            review_changes: "\u{f00d}",    // nf-fa-times
            review_commented: "\u{f075}",  // nf-fa-comment
            review_dismissed: "\u{f112}",  // nf-fa-reply
        }
    }

    /// Open/merged/closed state badge for an item.
    pub fn state_badge(&self, state: &str) -> &'static str {
        match state {
            "open" => self.open,
            "merged" => self.merged,
            "closed" => self.closed,
            _ => "?",
        }
    }

    /// PR vs issue kind icon.
    pub fn kind_icon(&self, kind: &str) -> &'static str {
        match kind {
            "pull_request" => self.pr,
            _ => self.issue,
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
        assert_eq!(Icons::new(true).pr, Icons::nerd().pr);
        assert_ne!(Icons::unicode().pr, Icons::nerd().pr);
    }

    #[test]
    fn badges_map_states() {
        let i = Icons::unicode();
        assert_eq!(i.state_badge("open"), "●");
        assert_eq!(i.state_badge("merged"), "⬡");
        assert_eq!(i.state_badge("closed"), "✕");
        assert_eq!(i.state_badge("???"), "?");
        assert_eq!(i.kind_icon("pull_request"), "⎇");
        assert_eq!(i.kind_icon("issue"), "○");
        assert_eq!(i.review_state_badge("APPROVED").0, "✅");
        assert_eq!(i.review_state_badge("unknown").0, "?");
    }
}
