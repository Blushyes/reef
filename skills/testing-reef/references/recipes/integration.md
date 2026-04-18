# Integration test recipe

For tests that use real `git2::Repository` or real filesystem. Live under `crates/reef-host/tests/<name>_integration.rs`.

## Basic skeleton

```rust
//! What this file tests. One sentence.

use std::sync::Mutex;
use test_support::{commit_file, tempdir_repo, write_file};

// ── Process-global state isolation ───────────────────────────────
// Include this block ONLY if the tests in this file change cwd or env vars.
// If they don't, skip the lock — unneeded serialization slows the suite.
static CWD_LOCK: Mutex<()> = Mutex::new(());

struct CwdGuard { original: std::path::PathBuf }

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

// ── Tests ────────────────────────────────────────────────────────
#[test]
fn scenario_expected_outcome() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "v1", "init");
    let _g = CwdGuard::enter(tmp.path());

    // ... exercise the code
    // ... assert
}
```

The `CwdGuard` and `CWD_LOCK` pattern is identical across files (`git_repo_integration.rs`, `ui_snapshots.rs`, `app_error_paths.rs`). Copy it; it hasn't needed to diverge.

## HOME isolation pattern

For tests that call `App::new()` or anything else that reads `std::env::var("HOME")` — specifically `crates/reef-host/src/prefs.rs`:

```rust
static HOME_LOCK: Mutex<()> = Mutex::new(());   // or reuse CWD_LOCK if paired

struct HomeGuard {
    original: Option<std::ffi::OsString>,
}

impl HomeGuard {
    fn enter(path: &std::path::Path) -> Self {
        let original = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", path); }
        Self { original }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(v) = self.original.take() {
                std::env::set_var("HOME", v);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }
}
```

`unsafe` is required because `set_var` is not threadsafe. The lock is the contract that makes our usage safe — never call `set_var` without holding one.

`ui_snapshots.rs` pairs `HomeGuard` with `CwdGuard` under a single `CWD_LOCK`, because every HOME swap in that file is paired with a cwd swap and nothing else touches HOME concurrently.

## Coverage goal

One integration test per public method of a complex impl. For `GitRepo`, that's 15+ tests — one per method covering the happy path. Edge cases (empty repo, renamed file, octopus merge) are separate `#[test]` functions within the same file.

When a method has multiple meaningful outcomes (e.g., `get_status` distinguishes staged/unstaged/untracked), write one test per outcome. Don't cram multiple assertions into one `#[test]` — failure messages are much clearer when each test has a specific name.

## When to prefer unit over integration

If you can write the same assertion with no filesystem, no git2 — do that. A unit test inside the source file is an order of magnitude faster and less flaky. Promote to integration only when you're genuinely testing the boundary with external state.
