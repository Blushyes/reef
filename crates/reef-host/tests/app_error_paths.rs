//! `App::new()` error-path tests: verify graceful degradation when the
//! environment is missing pieces (no git repo, no plugin dir).

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
fn app_new_plugin_manager_is_functional() {
    // `App::new()` probes several well-known paths for plugins. Whether or
    // not any get loaded (depends on repo layout), the manager API should
    // remain safe to call without panics.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new();
    app.plugin_manager.tick();
    app.plugin_manager.invalidate_panels();
    // sidebar_panels returns a borrowed slice — just ensure it doesn't panic.
    let _ = app.plugin_manager.sidebar_panels();
}
