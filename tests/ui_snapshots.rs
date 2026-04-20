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
use reef::app::App;
use reef::ui;
use reef::ui::theme::Theme;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use test_support::{HomeGuard, commit_file, tempdir_repo, write_file};

static CWD_LOCK: Mutex<()> = Mutex::new(());

/// Pin the UI language so the snapshot is stable regardless of the test
/// host's system locale. Called under `CWD_LOCK`; the first call seeds
/// i18n's OnceLock cache for the process so all three snapshot tests
/// render in the same language.
fn force_en_lang() {
    // SAFETY: test-only; `CWD_LOCK` serialises the three snapshot tests
    // and no other test in this binary touches env vars.
    unsafe {
        std::env::set_var("REEF_LANG", "en");
    }
}

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

// `HomeGuard` — redirect $HOME for the snapshot — lives in `test-support`.
// The local `CWD_LOCK` doubles as the HOME_LOCK here because every test in
// this file swaps both in lockstep and nothing else touches HOME concurrently.

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

fn wait_for_git_status(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.git_status_load.loading {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for background git status");
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
    force_en_lang();
    let (tmp, _raw) = tempdir_repo();
    // HOME must point outside the workdir — prefs creates `.config/reef`
    // and the file tree now shows dotfiles.
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark());
    let output = render_app(&mut app, 80, 20);
    with_filters(|| insta::assert_snapshot!("empty_repo", output));
}

#[test]
fn snapshot_with_staged_and_unstaged() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "tracked.txt", "v1\n", "init");
    write_file(&raw, "tracked.txt", "v2\n"); // unstaged modification
    write_file(&raw, "new.txt", "new\n"); // untracked
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark());
    // Switch to Git tab to show staged/unstaged sections
    app.set_active_tab(reef::app::Tab::Git);
    app.refresh_status();
    wait_for_git_status(&mut app);
    let output = render_app(&mut app, 80, 20);
    with_filters(|| insta::assert_snapshot!("with_staged_and_unstaged", output));
}

#[test]
fn snapshot_place_mode_renders_banner_and_border() {
    // Lock in the VSCode-style drag-and-drop picker visuals: double-line
    // accent border + top banner + dimmed file rows vs. accent folder rows.
    // Regressions here would indicate the place-mode render path drifted
    // — the shape of the banner and the root drop zone are both part of
    // the feature's UX contract.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    // A small tree with a folder and a file so the snapshot exercises
    // both row styles.
    commit_file(&raw, "README.md", "# hi\n", "init");
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    write_file(&raw, "src/main.rs", "fn main() {}\n");
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark());
    // Force a tree rebuild so `src/` and `README.md` show up before the
    // snapshot draw (the async load fires from `App::new` via tick).
    app.refresh_file_tree();
    // Drain worker results until the tree is populated.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.file_tree_load.loading && !app.file_tree.entries.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Source file used for the banner — has to exist on disk because
    // place mode itself is entered via `enter_place_mode` which accepts
    // any `Vec<PathBuf>`, but users would only ever land here with real
    // paths thanks to `parse_dropped_paths`.
    let source = tmp.path().join("README.md");
    app.enter_place_mode(vec![source]);

    let output = render_app(&mut app, 80, 20);
    with_filters(|| insta::assert_snapshot!("place_mode", output));
}

#[test]
fn snapshot_tree_edit_row_new_file() {
    // Locks in the VSCode-style inline editor visuals: the toolbar row,
    // the editable row injected after the anchor folder, the cursor
    // block on the first empty cell, and the placeholder text. If this
    // regresses, the user's typing would either render to the wrong
    // row or the cursor would go invisible.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "README.md", "# hi\n", "init");
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    write_file(&raw, "src/main.rs", "fn main() {}\n");
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark());
    app.refresh_file_tree();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.file_tree_load.loading && !app.file_tree.entries.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Kick off a NewFile edit anchored on the first top-level folder so
    // the editable row renders indented one deeper than the folder.
    let parent_dir = tmp.path().join("src");
    app.begin_tree_edit(
        reef::tree_edit::TreeEditMode::NewFile,
        parent_dir,
        None,
        Some(0),
    );

    let output = render_app(&mut app, 80, 20);
    with_filters(|| insta::assert_snapshot!("tree_edit_row_new_file", output));
}

#[test]
fn snapshot_tree_context_menu() {
    // Locks in the right-click context-menu popup visuals. If the menu
    // regresses the user would lose access to Rename / Delete / Reveal
    // on a pure mouse workflow.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "README.md", "# hi\n", "init");
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    write_file(&raw, "src/main.rs", "fn main() {}\n");
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark());
    app.refresh_file_tree();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.file_tree_load.loading && !app.file_tree.entries.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Open menu anchored at column 4, row 5 — arbitrary coords that
    // leave room for the popup to render inline without getting
    // clamped by the right edge. target_entry_idx=0 hits the first
    // file-tree row so the full ALL_FOR_ENTRY menu is offered.
    app.open_tree_context_menu(Some(0), (4, 5));

    let output = render_app(&mut app, 80, 20);
    with_filters(|| insta::assert_snapshot!("tree_context_menu", output));
}

#[test]
fn snapshot_with_staged_and_unstaged_light_theme() {
    // Locks in the light-theme wiring: a dark-vs-light snapshot diff must exist
    // somewhere, otherwise the Theme plumbing could silently regress to dark.
    // Text content should match the dark snapshot; only style bytes differ, but
    // TestBackend's `Cell::symbol()` drops styles, so the plain-text dump here
    // intentionally asserts "same content, same layout" — not color fidelity.
    // Color fidelity is verified by the unit tests in `src/ui/theme.rs`.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "tracked.txt", "v1\n", "init");
    write_file(&raw, "tracked.txt", "v2\n");
    write_file(&raw, "new.txt", "new\n");
    let _h = HomeGuard::enter(tmp.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::light());
    app.set_active_tab(reef::app::Tab::Git);
    app.refresh_status();
    wait_for_git_status(&mut app);
    let output = render_app(&mut app, 80, 20);
    with_filters(|| insta::assert_snapshot!("with_staged_and_unstaged_light", output));
}
