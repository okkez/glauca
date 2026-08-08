//! Terminal ownership: taking the terminal over for the TUI, handing it back,
//! and taking it over again after a child process had it. The three sequences
//! live together because they have to stay each other's inverse.

use super::*;

/// Take over the terminal: raw mode, the alternate screen, mouse reporting.
pub(crate) fn enter_tui(out: &mut impl io::Write) -> anyhow::Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        out,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    Ok(())
}

/// Undo everything [`enter_tui`] set, and show the cursor again — `Terminal::draw` hides it
/// on every frame that sets no cursor position, and ratatui only restores it in `Drop`,
/// which `panic = "abort"` never runs.
///
/// Safe to call when there is nothing to undo: the panic hook calls it for panics outside
/// the TUI's lifetime, including before [`enter_tui`] has run. Keep it that way.
///
/// Takes a writer rather than the `Terminal` because the panic hook cannot borrow it.
pub(crate) fn leave_tui(out: &mut impl io::Write) -> anyhow::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        out,
        crossterm::event::DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    Ok(())
}

/// Take the terminal back after [`leave_tui`] handed it to a child process,
/// repainting from scratch because the child scribbled over the screen and
/// ratatui's diff would otherwise leave that on display.
pub(crate) fn reenter_tui<B: ratatui::backend::Backend + io::Write>(
    terminal: &mut Terminal<B>,
) -> anyhow::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    enter_tui(terminal.backend_mut())?;
    terminal.clear()?;
    Ok(())
}
