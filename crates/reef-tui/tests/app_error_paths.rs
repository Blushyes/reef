//! `App::new(Theme::dark(), None)` error-path tests: verify graceful degradation when the
//! environment is missing pieces — most importantly when the cwd is not
//! inside a git repo, so `GitRepo::open()` returns `None`.

use reef::app::App;
use reef::ui::theme::Theme;
use std::sync::Mutex;
use tempfile::TempDir;
use test_support::CwdGuard;

static CWD_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn app_new_outside_git_repo_does_not_panic() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    // Deliberately NOT initialized as a git repo
    let _g = CwdGuard::enter(tmp.path());

    let app = App::new(Theme::dark(), None);
    assert!(app.repo.is_none(), "no repo outside a git dir");
    assert!(app.staged_files.is_empty());
    assert!(app.unstaged_files.is_empty());
}

#[test]
fn app_new_refresh_status_is_noop_without_repo() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.refresh_status(); // must not panic when repo is None
    assert!(app.staged_files.is_empty());
}

#[test]
fn app_tick_without_fs_watcher_is_safe() {
    // `App::new(Theme::dark(), None)` starts an fs_watcher thread per workdir; tick() drains
    // its channel and refreshes caches. Outside a git repo the watcher may
    // still spin up for the tempdir — tick must stay a no-op regardless.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.tick();
    app.tick();
}
