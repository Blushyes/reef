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

use crate::backend::EditorLaunchSpec;

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

/// Priority: `editor.command` pref → `$VISUAL` → `$EDITOR` → `vi`. The
/// pref is consulted first so a value chosen in the Settings page wins
/// over an inherited shell environment.
pub(crate) fn resolve_editor() -> Option<(String, Vec<String>)> {
    if let Some(s) = crate::prefs::get("editor.command") {
        if let Some(cmd) = parse_editor_command(&s) {
            return Some(cmd);
        }
    }
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
#[allow(dead_code)]
pub fn launch<B: Backend>(terminal: &mut Terminal<B>, path: &Path) -> io::Result<()> {
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
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
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

/// Same as `launch`, but sourced from a `Backend::editor_launch_spec` —
/// so the program + args are whatever the backend decided. Local
/// backend produces `$EDITOR <abs>`; remote backend produces
/// `ssh -t <ssh_args> host "cd <workdir> && $editor <rel>"`.
///
/// Keeps the TUI teardown/restore dance identical to `launch()` so the
/// terminal always ends up back in alt-screen + raw mode even when the
/// child editor / ssh returns non-zero.
pub fn launch_spec<B: Backend>(
    terminal: &mut Terminal<B>,
    spec: &EditorLaunchSpec,
) -> io::Result<()> {
    disable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, LeaveAlternateScreen, DisableMouseCapture)?;

    let run_result = Command::new(&spec.program).args(&spec.args).status();

    let restore = (|| -> io::Result<()> {
        enable_raw_mode()?;
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        terminal
            .clear()
            .map_err(|e| io::Error::other(format!("{e:?}")))?;
        Ok(())
    })();

    match (run_result, restore) {
        (Err(e), _) => Err(e),
        (_, Err(e)) => Err(e),
        (Ok(_), Ok(())) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use test_support::{HOME_LOCK, HomeGuard};

    /// `resolve_editor` reads from prefs (which lives under $HOME) and
    /// from $VISUAL / $EDITOR. Tests that exercise the precedence
    /// ladder must clear both env vars and set $HOME to an empty
    /// tempdir so the user's real prefs / shell config don't leak in.
    /// Shares `HOME_LOCK` with `prefs::tests` and `settings::tests` —
    /// all three modules compile into the same `cargo test --lib`
    /// binary and would race on $HOME otherwise.
    fn isolated_env() -> (
        std::sync::MutexGuard<'static, ()>,
        HomeGuard,
        TempDir,
        Option<String>,
        Option<String>,
    ) {
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_visual = std::env::var("VISUAL").ok();
        let prev_editor = std::env::var("EDITOR").ok();
        // SAFETY: serialised by HOME_LOCK; no other test thread reads
        // these for the duration of the test.
        unsafe {
            std::env::remove_var("VISUAL");
            std::env::remove_var("EDITOR");
        }
        let tmp = TempDir::new().unwrap();
        let home = HomeGuard::enter(tmp.path());
        (lock, home, tmp, prev_visual, prev_editor)
    }

    fn restore_env(prev_visual: Option<String>, prev_editor: Option<String>) {
        // SAFETY: caller holds HOME_LOCK.
        unsafe {
            match prev_visual {
                Some(v) => std::env::set_var("VISUAL", v),
                None => std::env::remove_var("VISUAL"),
            }
            match prev_editor {
                Some(v) => std::env::set_var("EDITOR", v),
                None => std::env::remove_var("EDITOR"),
            }
        }
    }

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

    #[test]
    fn pref_wins_over_env_vars() {
        let (_lock, _home, _tmp, prev_visual, prev_editor) = isolated_env();
        // SAFETY: under EDITOR_LOCK.
        unsafe {
            std::env::set_var("VISUAL", "vim");
            std::env::set_var("EDITOR", "vi");
        }
        crate::prefs::set("editor.command", "nvim --clean");
        let (prog, args) = resolve_editor().unwrap();
        assert_eq!(prog, "nvim");
        assert_eq!(args, vec!["--clean"]);
        restore_env(prev_visual, prev_editor);
    }

    #[test]
    fn falls_back_to_visual_then_editor_then_vi() {
        let (_lock, _home, _tmp, prev_visual, prev_editor) = isolated_env();
        // No pref, no env: unix gets `vi`.
        let (prog, _) = resolve_editor().unwrap();
        assert_eq!(prog, "vi");
        // EDITOR alone wins over the platform fallback.
        // SAFETY: under EDITOR_LOCK.
        unsafe {
            std::env::set_var("EDITOR", "helix");
        }
        let (prog, _) = resolve_editor().unwrap();
        assert_eq!(prog, "helix");
        // VISUAL trumps EDITOR.
        // SAFETY: under EDITOR_LOCK.
        unsafe {
            std::env::set_var("VISUAL", "code -w");
        }
        let (prog, args) = resolve_editor().unwrap();
        assert_eq!(prog, "code");
        assert_eq!(args, vec!["-w"]);
        restore_env(prev_visual, prev_editor);
    }

    #[test]
    fn whitespace_pref_is_ignored() {
        // A pref of "   " parses to None; we should fall through to env
        // vars rather than blow up later trying to spawn an empty
        // program.
        let (_lock, _home, _tmp, prev_visual, prev_editor) = isolated_env();
        crate::prefs::set("editor.command", "   ");
        // SAFETY: under EDITOR_LOCK.
        unsafe {
            std::env::set_var("EDITOR", "ed");
        }
        let (prog, _) = resolve_editor().unwrap();
        assert_eq!(prog, "ed");
        restore_env(prev_visual, prev_editor);
    }
}
