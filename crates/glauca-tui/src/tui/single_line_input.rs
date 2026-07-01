//! A single-line text input backed by [`ratatui_textarea::TextArea`].
//!
//! `TextArea` is a multi-line editor, but every input field in the TUI (the
//! inline filter bar and the two-field edit/new modals) is conceptually a
//! single line. Rather than repeating "keep it one line" across free functions
//! and hoping every call site honours it, this newtype *owns* the invariant:
//! newline/tab-inserting keys are dropped in [`SingleLineInput::input`], stray
//! `\n`/`\r` in seed text is stripped in [`SingleLineInput::from_text`], and the
//! active line is never underlined. Callers only ever see the first line.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;
use ratatui_textarea::{CursorMove, TextArea};

/// A one-line text input. Wraps a [`TextArea`] and guarantees it stays single
/// line: the stored buffer always has exactly one line, so [`value`] borrows it
/// directly.
///
/// [`value`]: SingleLineInput::value
#[derive(Debug, Clone)]
pub struct SingleLineInput {
    area: TextArea<'static>,
}

impl SingleLineInput {
    /// An empty input.
    pub fn new() -> Self {
        Self::from_text("")
    }

    /// An input pre-filled with `text`, cursor at the end (so typing continues
    /// after the existing value). Unlike `TextArea::default()`, the active line
    /// is not underlined (that highlight reads as noise here).
    ///
    /// Any newline in `text` is stripped: stored values should never contain
    /// one, but a stray `\n` (e.g. an out-of-band DB edit) must not build a
    /// multi-line buffer that [`value`] would silently truncate on the next
    /// save.
    ///
    /// [`value`]: SingleLineInput::value
    pub fn from_text(text: impl Into<String>) -> Self {
        let line = text.into().replace(['\n', '\r'], "");
        let mut area = TextArea::new(vec![line]);
        area.set_cursor_line_style(Style::default());
        area.move_cursor(CursorMove::End);
        Self { area }
    }

    /// The current text. A `TextArea` always holds at least one line, and this
    /// type keeps it to exactly one, so this borrows that line's contents.
    pub fn value(&self) -> &str {
        self.area.lines().first().map(String::as_str).unwrap_or("")
    }

    /// Whether the field is empty.
    pub fn is_empty(&self) -> bool {
        self.area.is_empty()
    }

    /// Reset the field to empty.
    pub fn clear(&mut self) {
        *self = Self::new();
    }

    /// Forward `key` to the underlying `TextArea`, dropping any key that would
    /// break the one-line invariant by inserting a newline or a tab.
    /// `TextArea::input` treats Enter, Ctrl+M, a literal `\n`/`\r` Char, Tab and
    /// BackTab (Shift+Tab) as newline/tab insertion, and Ctrl+Y as paste (which
    /// splices multi-line content via `insert_str`), so guarding only
    /// `KeyCode::Enter`/`Tab` at the call site is not enough — [`value`] would
    /// then silently drop everything past the first line. Returns whether the
    /// text buffer actually changed (false for a dropped key or a pure cursor
    /// move), so callers can react only to edits.
    ///
    /// Ctrl+U is *not* handled here: `TextArea`'s own Ctrl+U is undo, so callers
    /// intercept it to mean "clear" (via [`clear`]) before forwarding.
    ///
    /// [`value`]: SingleLineInput::value
    /// [`clear`]: SingleLineInput::clear
    pub fn input(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab => return false,
            KeyCode::Char('\n' | '\r') => return false,
            // Ctrl+M inserts a newline; Ctrl+Y pastes (multi-line via insert_str).
            KeyCode::Char('m' | 'y') if ctrl => return false,
            _ => {}
        }
        self.area.input(key)
    }

    /// Show the blinking text cursor when `active`, hide it otherwise. Used to
    /// keep the cursor only on the focused field of a two-field modal.
    pub fn set_active(&mut self, active: bool) {
        let style = if active {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        self.area.set_cursor_style(style);
    }

    /// Whether this field currently shows its cursor (see [`set_active`]).
    /// Test-only: production code sets active state via [`set_active`] but never
    /// reads it back — the cursor style is consumed only by rendering.
    ///
    /// [`set_active`]: SingleLineInput::set_active
    #[cfg(test)]
    pub fn is_active(&self) -> bool {
        self.area
            .cursor_style()
            .add_modifier
            .contains(Modifier::REVERSED)
    }
}

impl Default for SingleLineInput {
    fn default() -> Self {
        Self::new()
    }
}

impl Widget for &SingleLineInput {
    fn render(self, area: Rect, buf: &mut Buffer) {
        (&self.area).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn new_is_empty() {
        let input = SingleLineInput::new();
        assert!(input.is_empty());
        assert_eq!(input.value(), "");
    }

    #[test]
    fn from_text_strips_newlines() {
        let input = SingleLineInput::from_text("a\nb\r\nc");
        assert_eq!(input.value(), "abc");
        // The internal buffer must stay single line.
        assert_eq!(input.area.lines().len(), 1);
    }

    #[test]
    fn typing_appends_after_seed() {
        // from_text puts the cursor at the end, so typing continues the value.
        let mut input = SingleLineInput::from_text("ab");
        assert!(input.input(key(KeyCode::Char('c'))));
        assert_eq!(input.value(), "abc");
    }

    #[test]
    fn input_never_adds_a_line() {
        let mut input = SingleLineInput::from_text("x");
        for k in [
            key(KeyCode::Enter),
            key(KeyCode::Tab),
            key(KeyCode::BackTab),
            key(KeyCode::Char('\n')),
            key(KeyCode::Char('\r')),
            ctrl('m'),
            ctrl('y'),
        ] {
            // Newline/tab/paste keys are dropped: no edit, still one line.
            assert!(!input.input(k), "{k:?} should be dropped");
            assert_eq!(input.area.lines().len(), 1, "{k:?} split the buffer");
        }
        assert_eq!(input.value(), "x");
    }

    #[test]
    fn cursor_move_reports_no_edit() {
        let mut input = SingleLineInput::from_text("abc");
        assert!(!input.input(key(KeyCode::Left)));
        assert!(!input.input(key(KeyCode::Home)));
        assert_eq!(input.value(), "abc");
    }

    #[test]
    fn insert_in_the_middle() {
        let mut input = SingleLineInput::from_text("ac");
        input.input(key(KeyCode::Left)); // between a and c
        assert!(input.input(key(KeyCode::Char('b'))));
        assert_eq!(input.value(), "abc");
    }

    #[test]
    fn clear_empties_the_field() {
        let mut input = SingleLineInput::from_text("something");
        input.clear();
        assert!(input.is_empty());
        assert_eq!(input.value(), "");
    }

    #[test]
    fn set_active_toggles_is_active() {
        let mut input = SingleLineInput::new();
        input.set_active(true);
        assert!(input.is_active());
        input.set_active(false);
        assert!(!input.is_active());
    }

    #[test]
    fn multibyte_left_then_insert() {
        // Cursor moves by whole characters, not bytes.
        let mut input = SingleLineInput::from_text("あい");
        input.input(key(KeyCode::Left)); // between あ and い
        assert!(input.input(key(KeyCode::Char('う'))));
        assert_eq!(input.value(), "あうい");
        assert_eq!(input.area.lines().len(), 1);
    }
}
