# Snapshot test recipe

For asserting on rendered terminal output. Uses `ratatui::backend::TestBackend` to render to an in-memory buffer, then `insta::assert_snapshot!` to compare against a committed `.snap` file.

## Skeleton

```rust
//! UI snapshot tests via `ratatui::TestBackend`.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use reef::app::App;
use reef::ui;
use std::sync::Mutex;
use test_support::{commit_file, tempdir_repo, write_file};

// One lock covers both cwd and HOME swaps — every test in this file does
// both and nothing else touches HOME concurrently. If you add a test that
// only touches HOME, use a separate HOME_LOCK.
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
    fn drop(&mut self) { let _ = std::env::set_current_dir(&self.original); }
}

/// Redirect `$HOME` to the given path so `App::new()` reads a blank
/// `~/.config/reef/prefs` instead of the developer's real one.
/// The caller must hold `CWD_LOCK` (or an equivalent HOME_LOCK).
struct HomeGuard { original: Option<std::ffi::OsString> }
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

fn buffer_to_text(buf: &Buffer) -> String {
    let w = buf.area().width as usize;
    let h = buf.area().height as usize;
    let mut lines = Vec::with_capacity(h);
    for y in 0..h {
        let mut row = String::with_capacity(w);
        for x in 0..w {
            row.push_str(buf.cell((x as u16, y as u16)).unwrap().symbol());
        }
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

/// Mask nondeterministic tokens (tempdir names, absolute paths, etc.)
/// from the snapshot so it's stable across machines and runs.
fn with_filters<F: FnOnce()>(body: F) {
    let mut settings = insta::Settings::clone_current();
    settings.add_filter(r"\.tmp[A-Za-z0-9]{6,}", "[TMPDIR]");
    settings.add_filter(r"tmp[A-Za-z0-9]{6,}", "[TMPDIR]");
    settings.bind(body);
}

#[test]
fn snapshot_<scenario>() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "v1", "init");        // set up repo state
    let _home = HomeGuard::enter(tmp.path());        // <-- NEVER skip this
    let _cwd  = CwdGuard::enter(tmp.path());

    let mut app = App::new();
    app.active_tab = reef::app::Tab::Git;       // optional: set UI state
    app.refresh_status();

    let output = render_app(&mut app, 80, 20);
    with_filters(|| insta::assert_snapshot!("<scenario>", output));
}
```

## Why `HomeGuard` is non-negotiable

See `references/pitfalls.md` → "App::new() reads the developer's real ~/.config/reef/prefs". Summary: `App::new()` calls `load_bool_pref("status.tree_mode", ...)` and similar for commit-detail toggles. Without HOME isolation, your snapshot captures the dev's saved view-mode and CI gets a different one.

`App::new()` also runs `prefs::migrate_legacy_prefs()`. The migrator is intentionally a no-op when there's nothing to migrate, so pointing HOME at a clean tempdir doesn't leave a spurious `.config/reef/prefs` file that would show up as untracked in `git status` and pollute the snapshot.

## Picking buffer dimensions

`TestBackend::new(80, 20)` — 80 cols × 20 rows. Match the aspect ratio of real terminals; don't go wider than 120 or the snapshot becomes unreadable in diff review. Height depends on how much of the UI you need to see.

Don't use tiny buffers to "simplify" the snapshot. If the UI wraps or truncates differently at 40 cols vs 80, you'll miss bugs that affect real users on 80+ col terminals.

## Updating a snapshot intentionally

When you change the UI and the snapshot should change:

```bash
INSTA_UPDATE=always cargo test -p reef --test ui_snapshots
```

This accepts the new output as the new baseline. **Always** review the diff before committing:

```bash
cargo insta review
# or just: git diff tests/snapshots/
```

Commit the updated `.snap` file. **Never** commit `.snap.new` files — those are pending snapshots that weren't accepted. The `.gitignore` already excludes them.

## Verifying stability

After creating or updating a snapshot, run the test twice in a row without `INSTA_UPDATE`:

```bash
cargo test -p reef --test ui_snapshots
cargo test -p reef --test ui_snapshots
```

Both must pass. If the second run fails, the snapshot has nondeterministic content — add more insta filters or fix the rendering code.

## Adding new filters

Any regex-matchable volatile token can be masked:

```rust
settings.add_filter(r"\b[0-9a-f]{7,40}\b", "[OID]");    // git commit hashes
settings.add_filter(r"\d{4}-\d{2}-\d{2}", "[DATE]");    // dates
settings.add_filter(r"/var/folders/[^\s]+", "[TMPDIR]"); // absolute paths
```

Keep filters narrow — overly broad patterns can mask actual bugs (e.g., filtering all numbers hides off-by-one errors).
