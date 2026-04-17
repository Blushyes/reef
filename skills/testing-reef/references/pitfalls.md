# Pitfalls and their root causes

Every entry here represents time paid. Read before writing a test that touches the relevant area.

## Plugin subprocess races in UI snapshots

**Symptom**: Snapshot test passes locally, fails on CI (or vice versa). The diff shows a whole panel's worth of content appearing/disappearing.

**Root cause**: `App::new()` calls `plugin_manager.load_plugins()`, which spawns `reef-git` as a subprocess. The plugin answers the first render request asynchronously. Whether your `terminal.draw(...)` call captures the plugin's output depends on:

1. **Does the plugin binary exist?** The plugin manifest points at `target/release/reef-git`. On a dev machine that's been `cargo build --release`'d, it exists. On CI running `cargo test` (which only builds debug binaries), it doesn't — plugin spawn fails silently, panel is empty.
2. **How fast does the plugin respond?** Even when the binary exists, `terminal.draw()` right after `App::new()` may capture a frame before the plugin's first render arrives.

**Fix**: Detach the plugin manager before snapshotting:

```rust
app.plugin_manager = PluginManager::new();
app.active_sidebar_panel = None;
```

Snapshot only host-native rendering. Test plugin behavior separately in `plugin_handshake.rs` / `plugin_manager_lifecycle.rs` where you deterministically spawn the test-only `echo-plugin` binary.

**Don't fix by**: Polling until the plugin responds — works today, breaks when the build changes. The binary's availability is outside the test's control.

## macOS tempdir symlink vs. `notify` canonicalization

**Symptom**: Watcher integration test works on Linux, silently produces zero events on macOS. No error, test just times out expecting a notification.

**Root cause**: On macOS, `TempDir` returns `/var/folders/<...>` which is a symlink to `/private/var/folders/<...>`. The `notify` crate (via FSEvents) delivers events with the **canonical** path (`/private/var/...`). The watcher's `is_relevant()` does `path.starts_with(workdir)`; if `workdir` is the non-canonical `/var/...` form, the match fails and every event is discarded.

**Fix**: Canonicalize before passing to `watcher::spawn`:

```rust
let workdir = std::fs::canonicalize(tmp.path()).unwrap_or(tmp.path().to_path_buf());
let gitdir = std::fs::canonicalize(workdir.join(".git")).unwrap_or(workdir.join(".git"));
watcher::spawn(workdir, gitdir, writer);
```

**Generalizes to**: Any code that compares paths received from the OS (fs events, resolved exe paths) against a path you constructed. If you constructed via `TempDir` on macOS, canonicalize it first.

## Process-global env mutation races

**Symptom**: Test passes alone, fails when run as part of the suite. Error looks like "HOME not set" or "prefs file missing" or "Repository::open failed".

**Root cause**: Cargo runs `#[test]` functions in parallel threads by default. `std::env::set_var("HOME", ...)` and `std::env::set_current_dir(...)` mutate process-global state. Test A sets HOME, test B reads it mid-operation, both see unexpected values.

**Fix**: Per-state `Mutex`, held for the duration of the mutation:

```rust
static HOME_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn my_test() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("HOME", tmp.path()); }
    // ... test body
    // Drop guard restores HOME in the Drop impl
}
```

Keep `CWD_LOCK` and `HOME_LOCK` separate. Two tests that mutate different env vars don't need to serialize.

**Don't fix by**: Running tests with `--test-threads=1`. That serializes everything, including tests that don't need it, and hides the bug rather than expressing the real dependency.

## Cargo.lock drift in commits

**Symptom**: `cargo build` works on your machine but not a colleague's. `Cargo.lock` shows as modified in `git status` after every `cargo test`.

**Root cause**: Dev-dependencies (especially `criterion`, `proptest`, `insta`) pull in large dep trees. When someone adds a dev-dep and forgets to commit the resulting `Cargo.lock` change, the next person's build resolves a different lockfile.

**Fix**: Always commit `Cargo.lock` changes alongside `Cargo.toml` changes. `cargo.lock` is NOT in `.gitignore` for this workspace (it's a binary crate workspace, locking is the correct choice).

**Check before pushing**: `git status Cargo.lock` — if it's modified, stage and include in the relevant commit.

## Insta snapshot "passes" locally but `.snap.new` in diff

**Symptom**: Test passes locally, but a `.snap.new` file appears. CI fails on the same test.

**Root cause**: Running tests with `INSTA_UPDATE=always` (or having it in environment) regenerates snapshots silently. You see green locally because insta accepted whatever output you produced. CI runs with default `INSTA_UPDATE=no` and compares against the committed `.snap` — the mismatch fails.

**Fix**: Only set `INSTA_UPDATE=always` intentionally, and review the `.snap.new` diff with `cargo insta review` before committing. The committed file must be `.snap`, not `.snap.new`. Our `.gitignore` excludes `*.snap.new` to prevent accidental commits of pending snapshots.

## Clippy warnings CI fails on warnings that "work locally"

**Symptom**: `cargo test` works, but `cargo clippy --workspace --all-targets -- -D warnings` (what CI runs) fails.

**Root cause**: Local `cargo clippy` shows warnings as warnings (doesn't fail). CI escalates with `-D warnings`. A lint that existed before your change will now fail CI even though you didn't cause it.

**Fix**: Run `cargo clippy --workspace --all-targets -- -D warnings` locally before pushing. This is step 3 of the pre-commit checklist in SKILL.md.

**If the lint is style-only and project-wide**: Add it to `[workspace.lints.clippy] <lint> = "allow"` in root `Cargo.toml`. If it catches real issues, fix the code.

## Tests spawning subprocesses without waiting for them

**Symptom**: Tests pass, but occasionally zombie processes accumulate. Or: test times out waiting for a plugin that never responds.

**Root cause**: `PluginProcess` holds a `Child` in a field but doesn't actively kill it on drop. The spawned process reads stdin; when the test drops the PluginProcess, stdin is closed, and the subprocess exits on EOF — but this relies on the subprocess checking for EOF, not blocking elsewhere.

**Fix**:
1. Always give a bounded timeout when polling `drain_messages`
2. For tests that call `mgr.shutdown()`, rely on `reef/shutdown` notification to terminate plugins cleanly
3. If a test spawns a subprocess directly, call `.kill()` or send shutdown explicitly at test end — don't rely on drop

The `echo-plugin` binary exits on EOF specifically to survive this scenario. New test-only subprocess binaries should do the same.
