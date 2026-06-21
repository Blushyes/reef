# Pitfalls and their root causes

Every entry here represents time paid. Read before writing a test that touches the relevant area.

## `App::new()` reads the developer's real `~/.config/reef/prefs`

**Symptom**: Snapshot test passes for the author but fails for someone else (or on CI). The diff shows the Git sidebar's view-mode toggle flipping between "视图: 列表" and "视图: 树形", or the commit-file view switching tree/flat, with no obvious source.

**Root cause**: `App::new()` runs `load_bool_pref("status.tree_mode", ...)` and the equivalents for `commit.diff_layout`, `commit.diff_mode`, `commit.files_tree_mode`. Without test-level isolation, these reads hit the tester's real home directory — which has been used as a real Reef user and has non-default prefs persisted.

**Fix**: Redirect `$HOME` to the test's tempdir before calling `App::new()`, serialised behind a lock that also covers cwd mutation (see SKILL.md critical pattern #1 for the `HomeGuard` template). `prefs::migrate_legacy_prefs()` is a no-op when the tempdir has no prefs file, so it won't create a spurious `.config/reef/prefs` that shows up as an untracked file in the snapshot's `git status`.

**Don't fix by**: Sprinkling `prefs::set(...)` at the top of each test to force known values. You'd have to know every key `App::new()` reads, and the list will grow.

## macOS tempdir symlink vs. `notify` canonicalization

**Symptom**: Watcher integration test works on Linux, silently produces zero events on macOS. No error, test just times out expecting a notification.

**Root cause**: On macOS, `TempDir` returns `/var/folders/<...>` which is a symlink to `/private/var/folders/<...>`. The `notify` crate (via FSEvents) delivers events with the **canonical** path (`/private/var/...`). The watcher's `is_relevant()` does `path.starts_with(workdir)`; if `workdir` is the non-canonical `/var/...` form, the match fails and every event is discarded.

**Fix**: Canonicalize before passing to `fs_watcher::spawn`:

```rust
let workdir = std::fs::canonicalize(tmp.path()).unwrap_or(tmp.path().to_path_buf());
let gitdir = std::fs::canonicalize(workdir.join(".git")).unwrap_or(workdir.join(".git"));
fs_watcher::spawn(workdir);
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

In practice you often need both cwd and HOME locked together (e.g. `ui_snapshots.rs`). A single `CWD_LOCK` covers both, since the HOME swap in that file is always paired with a cwd swap and nothing else touches HOME concurrently. Don't over-share locks across files that don't need to serialise.

**Don't fix by**: Running tests with `--test-threads=1`. That serializes everything, including tests that don't need it, and hides the bug rather than expressing the real dependency.

## Graph cache not invalidated → stale DAG on ref moves

**Symptom**: Tab::Graph shows the commit DAG from five minutes ago. New commits don't appear; HEAD doesn't move.

**Root cause**: `App::refresh_graph()` keys on `(head_oid, refs_hash)` and skips the `revwalk` when unchanged. This is deliberately aggressive — we don't want working-tree edits (which can fire fs-watcher events every keystroke) to trigger a 500-commit revwalk on large repos. But any code path that moves HEAD or refs must clear `app.git_graph.cache_key = None` explicitly.

**Fix**: In any new code path that can change HEAD or refs (commit, checkout, reset, push, fetch, rebase), add `self.git_graph.cache_key = None;` next to the mutation. Existing examples: `App::run_push` clears it on push success/failure.

**Don't fix by**: Invalidating cache_key in `App::tick()` or on every fs-watcher event. That reintroduces the 500-commit revwalk per keystroke.

## Cargo.lock drift in commits

**Symptom**: `cargo build` works on your machine but not a colleague's. `Cargo.lock` shows as modified in `git status` after every `cargo test`.

**Root cause**: Dev-dependencies (especially `criterion`, `proptest`, `insta`) pull in large dep trees. When someone adds a dev-dep and forgets to commit the resulting `Cargo.lock` change, the next person's build resolves a different lockfile.

**Fix**: Always commit `Cargo.lock` changes alongside `Cargo.toml` changes. `Cargo.lock` is NOT in `.gitignore` for this workspace (it's a binary crate workspace, locking is the correct choice).

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
