//! `prefs::{load,save}_tree_mode` roundtrip on a sandboxed `HOME`.
//!
//! These tests mutate the process-global `HOME` env var, so they share a
//! mutex and run serially.

use reef_git::prefs;
use std::sync::Mutex;
use tempfile::TempDir;

static HOME_LOCK: Mutex<()> = Mutex::new(());

/// Swap `HOME` to a temp dir for the duration of the guard.
struct HomeGuard {
    _tmp: TempDir,
    original: Option<String>,
}

impl HomeGuard {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let original = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        Self {
            _tmp: tmp,
            original,
        }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.original {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

#[test]
fn load_returns_false_when_prefs_file_missing() {
    let _lock = HOME_LOCK.lock().unwrap();
    let _guard = HomeGuard::new();
    assert!(!prefs::load_tree_mode());
}

#[test]
fn save_then_load_true() {
    let _lock = HOME_LOCK.lock().unwrap();
    let _guard = HomeGuard::new();
    prefs::save_tree_mode(true);
    assert!(prefs::load_tree_mode());
}

#[test]
fn save_then_load_false() {
    let _lock = HOME_LOCK.lock().unwrap();
    let _guard = HomeGuard::new();
    prefs::save_tree_mode(false);
    assert!(!prefs::load_tree_mode());
}

#[test]
fn save_overwrites_previous_value() {
    let _lock = HOME_LOCK.lock().unwrap();
    let _guard = HomeGuard::new();
    prefs::save_tree_mode(true);
    prefs::save_tree_mode(false);
    assert!(!prefs::load_tree_mode());
}
