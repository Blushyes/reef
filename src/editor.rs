//! Shell out to the user's external editor.
//!
//! Reef has no built-in text editor by design. When the user presses Enter
//! on a file, we suspend the TUI (leave alt-screen, disable raw mode and
//! mouse capture), run `$VISUAL` / `$EDITOR` as a subprocess on the real
//! terminal, then restore the TUI when it exits. Same pattern as git, tig,
//! lazygit.
//!
//! Command parsing is whitespace-split — handles the common cases
//! (`EDITOR=vim`, `EDITOR="code -w"`) but not shell-quoted args with
//! embedded spaces. That's fine for a prototype; if it comes up we can
//! pull in `shell-words`.

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::Backend;
use std::io;
use std::path::Path;
use std::process::Command;

/// Parse an `$EDITOR`-style string into (program, extra_args).
///
/// Returns None if the string is empty or whitespace-only. Extra args are
/// passed before the file path on the final command line, so
/// `"code -w"` → `code -w <file>`.
pub fn parse_editor_command(s: &str) -> Option<(String, Vec<String>)> {
    let mut parts = s.split_whitespace();
    let prog = parts.next()?.to_string();
    let args = parts.map(|s| s.to_string()).collect();
    Some((prog, args))
}

/// Resolve `$VISUAL`, then `$EDITOR`, then a platform fallback.
fn resolve_editor() -> Option<(String, Vec<String>)> {
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(s) = std::env::var(var) {
            if let Some(cmd) = parse_editor_command(&s) {
                return Some(cmd);
            }
        }
    }
    // POSIX guarantees `vi`; on Windows we refuse to guess.
    if cfg!(unix) {
        Some(("vi".to_string(), Vec::new()))
    } else {
        None
    }
}

/// Suspend the TUI, run the user's editor on `path`, then restore the TUI.
///
/// `mouse_capture_was_on` tells us whether to re-enable mouse capture on
/// resume — the caller tracks this because `v` (select mode) may have
/// disabled it before the user triggered the edit.
pub fn launch<B: Backend>(
    terminal: &mut Terminal<B>,
    path: &Path,
    mouse_capture_was_on: bool,
) -> io::Result<()> {
    let (prog, extra_args) = resolve_editor().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no editor set (VISUAL / EDITOR)")
    })?;

    // Tear down the TUI. Order mirrors main.rs's cleanup on quit.
    disable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, LeaveAlternateScreen, DisableMouseCapture)?;

    // Editor inherits stdin/stdout/stderr and runs to completion on the
    // real terminal. We don't care about its exit status — the user made
    // their save/discard decision inside the editor.
    let run_result = Command::new(&prog).args(&extra_args).arg(path).status();

    // Restore the TUI regardless of whether the editor succeeded. If we
    // skipped restoration on error, the terminal would be left in cooked
    // mode on the regular buffer — unusable.
    let restore = (|| -> io::Result<()> {
        enable_raw_mode()?;
        execute!(stdout, EnterAlternateScreen)?;
        if mouse_capture_was_on {
            execute!(stdout, EnableMouseCapture)?;
        }
        // `B::Error` isn't guaranteed to convert to io::Error without a
        // bound; stringify it so this stays generic over backends.
        terminal
            .clear()
            .map_err(|e| io::Error::other(format!("{e:?}")))?;
        Ok(())
    })();

    // Surface whichever failure came first.
    match (run_result, restore) {
        (Err(e), _) => Err(e),
        (_, Err(e)) => Err(e),
        (Ok(_), Ok(())) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_editor_command("").is_none());
        assert!(parse_editor_command("   ").is_none());
    }

    #[test]
    fn parse_bare_program() {
        let (prog, args) = parse_editor_command("vim").unwrap();
        assert_eq!(prog, "vim");
        assert!(args.is_empty());
    }

    #[test]
    fn parse_program_with_args() {
        let (prog, args) = parse_editor_command("code -w -n").unwrap();
        assert_eq!(prog, "code");
        assert_eq!(args, vec!["-w", "-n"]);
    }

    #[test]
    fn parse_collapses_runs_of_whitespace() {
        let (prog, args) = parse_editor_command("  nvim\t --clean ").unwrap();
        assert_eq!(prog, "nvim");
        assert_eq!(args, vec!["--clean"]);
    }
}
