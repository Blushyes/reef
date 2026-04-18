//! `App::new()` error-path tests: verify graceful degradation when the
//! environment is missing pieces — most importantly when the cwd is not
//! inside a git repo, so `GitRepo::open()` returns `None`.

use reef_host::app::App;
use std::sync::Mutex;
use tempfile::TempDir;

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

#[test]
fn app_new_outside_git_repo_does_not_panic() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    // Deliberately NOT initialized as a git repo
    let _g = CwdGuard::enter(tmp.path());

    let app = App::new();
    assert!(app.repo.is_none(), "no repo outside a git dir");
    assert!(app.staged_files.is_empty());
    assert!(app.unstaged_files.is_empty());
}

#[test]
fn app_new_refresh_status_is_noop_without_repo() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new();
    app.refresh_status(); // must not panic when repo is None
    assert!(app.staged_files.is_empty());
}

#[test]
fn app_tick_without_fs_watcher_is_safe() {
    // `App::new()` starts an fs_watcher thread per workdir; tick() drains
    // its channel and refreshes caches. Outside a git repo the watcher may
    // still spin up for the tempdir — tick must stay a no-op regardless.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new();
    app.tick();
    app.tick();
}
