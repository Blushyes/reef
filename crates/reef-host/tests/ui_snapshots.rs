//! Full-terminal snapshot tests via `ratatui::TestBackend`.
//!
//! Strategy: drop into a controlled tempdir with a real git repo, redirect
//! `$HOME` to the same tempdir so `App::new()`'s prefs read starts from a
//! blank slate (otherwise the developer's saved tree-mode / diff-layout
//! bleeds into the snapshot), then render and assert against a committed
//! `.snap` file.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use reef_host::app::App;
use reef_host::ui;
use std::sync::Mutex;
use test_support::{commit_file, tempdir_repo, write_file};

static CWD_LOCK: Mutex<()> = Mutex::new(());

struct CwdGuard {
    original: std::path::PathBuf,
}

impl CwdGuard {
    fn enter(path: &std::path::Path) -> Self {
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self { original }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

/// Temporarily redirect `$HOME` to the given path so `~/.config/reef/prefs`
/// lookups during `App::new()` don't read the developer's real config.
/// The caller already holds `CWD_LOCK`, which also serialises HOME swaps.
struct HomeGuard {
    original: Option<std::ffi::OsString>,
}

impl HomeGuard {
    fn enter(path: &std::path::Path) -> Self {
        let original = std::env::var_os("HOME");
        // SAFETY: tests serialise HOME mutation via CWD_LOCK.
        unsafe {
            std::env::set_var("HOME", path);
        }
        Self { original }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        // SAFETY: tests serialise HOME mutation via CWD_LOCK.
        unsafe {
            if let Some(v) = self.original.take() {
                std::env::set_var("HOME", v);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }
}

fn buffer_to_text(buf: &Buffer) -> String {
    let w = buf.area().width as usize;
    let h = buf.area().height as usize;
    let mut lines = Vec::with_capacity(h);
    for y in 0..h {
        let mut row = String::with_capacity(w);
        for x in 0..w {
            let cell = buf.cell((x as u16, y as u16)).unwrap();
            row.push_str(cell.symbol());
        }
        // trim trailing padding so snapshots stay tidy
        lines.push(row.trim_end().to_string());
    }
    lines.join("\n")
}

fn render_app(app: &mut App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::render(f, app)).unwrap();
    buffer_to_text(terminal.backend().buffer())
}

/// Apply filters to mask nondeterministic tokens (tempdir name, path segments).
fn with_filters<F: FnOnce()>(body: F) {
    let mut settings = insta::Settings::clone_current();
    // TempDir names are `.tmpXXXXXX` on most platforms.
    settings.add_filter(r"\.tmp[A-Za-z0-9]{6,}", "[TMPDIR]");
    settings.add_filter(r"tmp[A-Za-z0-9]{6,}", "[TMPDIR]");
    settings.bind(body);
}

#[test]
fn snapshot_empty_repo() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _raw) = tempdir_repo();
    let _h = HomeGuard::enter(tmp.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new();
    let output = render_app(&mut app, 80, 20);
    with_filters(|| insta::assert_snapshot!("empty_repo", output));
}

#[test]
fn snapshot_with_staged_and_unstaged() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "tracked.txt", "v1\n", "init");
    write_file(&raw, "tracked.txt", "v2\n"); // unstaged modification
    write_file(&raw, "new.txt", "new\n"); // untracked
    let _h = HomeGuard::enter(tmp.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new();
    // Switch to Git tab to show staged/unstaged sections
    app.active_tab = reef_host::app::Tab::Git;
    app.refresh_status();
    let output = render_app(&mut app, 80, 20);
    with_filters(|| insta::assert_snapshot!("with_staged_and_unstaged", output));
}
