//! TUI-only external-process and terminal helpers: the OSC 52 clipboard copy,
//! the $EDITOR round-trip (with TUI suspend/restore), the octorus PR-review
//! launcher, and `item_actions` (the TUI's item action menu source of truth).

use super::*;

/// Copy `text` to the system clipboard via the OSC 52 terminal escape sequence.
/// This works without a clipboard tool (xclip/pbcopy) or X11/Wayland, and over
/// SSH, as long as the terminal emulator supports OSC 52. The sequence is just
/// an escape code, so writing it mid-session does not disturb the alternate
/// screen.
pub(crate) fn copy_to_clipboard_osc52(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let seq = osc52_sequence(text);
    let mut out = io::stdout();
    out.write_all(seq.as_bytes())?;
    out.flush()
}

/// Build the OSC 52 clipboard escape sequence for `text` (pure; the side effect
/// of writing to the terminal lives in [`copy_to_clipboard_osc52`]).
fn osc52_sequence(text: &str) -> String {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text.as_bytes()))
}

// ── Editor / terminal helpers (TUI-only) ─────────────────────────────────────

/// Does NOT suspend/restore the TUI — the caller must do that around this call.
pub(crate) fn run_editor(initial_content: &str) -> anyhow::Result<Option<String>> {
    let cwd = std::env::current_dir()?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = cwd.join(format!(".glauca-editor-{}-{nonce}.md", std::process::id()));

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::fs::write(&path, initial_content)?;

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    let status = std::process::Command::new(program)
        .args(parts)
        .arg(&path)
        .status()?;

    let result = if status.success() {
        let content = std::fs::read_to_string(&path)?;
        let trimmed = content.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    } else {
        None
    };

    let _ = std::fs::remove_file(&path);
    Ok(result)
}

pub(crate) fn suspend_tui<B: ratatui::backend::Backend + io::Write>(
    terminal: &mut Terminal<B>,
) -> anyhow::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen
    )?;
    Ok(())
}

pub(crate) fn restore_tui<B: ratatui::backend::Backend + io::Write>(
    terminal: &mut Terminal<B>,
) -> anyhow::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    terminal.clear()?;
    Ok(())
}

/// Actions offered for an item in the TUI action menu. This is the TUI's source
/// of truth (used by the menu render, cursor bounds, and confirm handler): the
/// shared `ItemAction::available_for` plus the TUI-only `ReviewOctorus` for PRs
/// (kept out of `available_for` so it never appears in the GUI menu).
pub(crate) fn item_actions(kind: &str) -> Vec<ItemAction> {
    let mut actions = ItemAction::available_for(kind);
    if kind == "pull_request" {
        actions.push(ItemAction::ReviewOctorus);
    }
    actions
}

/// Launch the external `octorus` (`or`) PR-review TUI for `item`, releasing the
/// terminal while it runs and restoring it afterwards. Returns a status-line
/// message. Requires `or` on PATH (`cargo install octorus`) and an authenticated
/// `gh`.
///
/// Resolves the repo's local checkout via ghq's root rules and passes it as
/// `--working-dir` so octorus operates on the target repo rather than glauca's
/// own CWD. If no local checkout is found, returns without launching.
pub(crate) fn run_octorus_review<B: ratatui::backend::Backend + io::Write>(
    terminal: &mut Terminal<B>,
    item: &ItemEntry,
) -> anyhow::Result<String>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let Some(workdir) = glauca_core::ghq::resolve_local_checkout(&item.repo_owner, &item.repo_name)
    else {
        // ローカルに checkout が無ければ、誤ったディレクトリで octorus を動かさないよう
        // TUI を中断せずにそのまま戻す。
        return Ok(format!(
            "Local checkout for {}/{} not found (searched ghq roots)",
            item.repo_owner, item.repo_name
        ));
    };

    suspend_tui(terminal)?;
    // `--working-dir` は OsStr のまま渡す（非 UTF-8 パスを壊さないため）。
    let result = std::process::Command::new("or")
        .arg("--repo")
        .arg(item.repo_display())
        .arg("--pr")
        .arg(item.number.to_string())
        .arg("--working-dir")
        .arg(&workdir)
        .status();
    restore_tui(terminal)?;

    Ok(match result {
        Ok(status) if status.success() => "Returned from octorus".into(),
        Ok(status) => format!("octorus exited with {status}"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            "octorus (`or`) not found — install with `cargo install octorus`".into()
        }
        Err(e) => format!("Failed to launch octorus: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_action_available_for_kind_is_context_aware() {
        assert_eq!(
            ItemAction::available_for("pull_request"),
            vec![
                ItemAction::OpenBrowser,
                ItemAction::CopyUrl,
                ItemAction::RefreshItem,
                ItemAction::ViewComments,
                ItemAction::Comment,
                ItemAction::ApprovePR,
                ItemAction::MergePR,
            ]
        );
        assert_eq!(
            ItemAction::available_for("issue"),
            vec![
                ItemAction::OpenBrowser,
                ItemAction::CopyUrl,
                ItemAction::RefreshItem,
                ItemAction::ViewComments,
                ItemAction::Comment,
            ]
        );
    }

    #[test]
    fn refresh_item_available_for_both_kinds() {
        assert!(ItemAction::available_for("pull_request").contains(&ItemAction::RefreshItem));
        assert!(ItemAction::available_for("issue").contains(&ItemAction::RefreshItem));
    }

    #[test]
    fn item_actions_appends_octorus_for_prs_only() {
        let pr = item_actions("pull_request");
        assert_eq!(pr.last(), Some(&ItemAction::ReviewOctorus));
        // Only PRs get it.
        assert!(!item_actions("issue").contains(&ItemAction::ReviewOctorus));
        // It is a TUI-only addition, never surfaced by the shared `available_for`.
        assert!(!ItemAction::available_for("pull_request").contains(&ItemAction::ReviewOctorus));
    }

    #[test]
    fn osc52_sequence_wraps_base64() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let url = "https://github.com/owner/repo/pull/1";
        let seq = osc52_sequence(url);
        let expected = format!("\x1b]52;c;{}\x07", STANDARD.encode(url.as_bytes()));
        assert_eq!(seq, expected);
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));
    }
}
