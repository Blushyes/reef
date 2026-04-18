---
name: testing-reef
description: Conventions and gotchas for writing tests in the reef Rust workspace — unit, integration, property, snapshot, benchmark, and fuzz. Use whenever adding or modifying a test in reef or test-support, or when the user asks "add a test for X", "how should I test Y", "why is this test flaky", "where does this test go", or anything about test fixtures, snapshots, proptest, or CI test failures. Also use when touching clock/time code (requires splitting `*_at(now, t)` helpers), or any code that mutates `cwd`/`HOME` (needs process-wide lock). Covers file-placement rules, the `test-support` fixture crate, insta filters, proptest strategy construction, and the specific pitfalls we've paid for in production — macOS tempdir symlinks, env-var sharing across parallel tests, and the HOME-redirect pattern for prefs-reading code.
---

# Testing in the reef workspace

A project-specific test playbook. Follow this when adding **any** test. The goal is that tests are deterministic across Linux/macOS CI and every dev's laptop, and that no test becomes that one flaky thing everyone reruns.

## Workspace layout (post-deplugin)

Reef is a single-process binary — there is no plugin subsystem. The repo root IS the `reef` crate; the workspace exists only to hang the test-fixture helper off it:

- **Root (`reef` crate)** — `src/` (app state, UI panels, git operations), `tests/` (integration), `benches/` (criterion).
- **`crates/test-support`** — shared fixtures (`tempdir_repo`, `commit_file`, `write_file`, `HomeGuard`).
- **`fuzz/`** — fuzz targets. Separate package, not in the workspace; run via `cargo +nightly fuzz run <target>`.

## Quick decision table

| Scenario | Test type | Where it goes |
|----------|-----------|---------------|
| Pure function, no I/O | Unit test | `#[cfg(test)] mod tests` inline at bottom of the source file |
| Uses `git2::Repository`, real fs | Integration test | `tests/<name>_integration.rs` |
| Algorithmic invariant over random inputs | Property test | `tests/<name>_properties.rs` (uses `proptest`) |
| Full UI rendered to terminal buffer | Snapshot | `tests/<name>_snapshots.rs` (uses `insta` + `ratatui::TestBackend`) |
| Hot-path performance | Benchmark | `benches/<name>.rs` (uses `criterion`) |
| Parser / deserializer robustness | Fuzz target | `fuzz/fuzz_targets/<name>.rs` |

If you're unsure, default to unit test first; promote to integration only when real I/O is required for the test to be meaningful.

## Fixtures: always go through `test-support`

The `crates/test-support` crate exists so tests don't reinvent repo setup. **Never write `git2::Repository::init` or manual `std::env::set_var` in a test file.** Use these helpers:

- `tempdir_repo()` — returns `(TempDir, git2::Repository)` with `user.name`/`user.email` pre-set (critical for CI where global git config is absent)
- `commit_file(&repo, path, content, subject)` — stages + commits in one call, returns the `Oid`
- `write_file(&repo, path, content)` — writes without staging (for untracked / modified paths)

Full reference: **`references/fixtures.md`**.

If something's missing from `test-support`, add it there rather than duplicating helpers across test files.

## Critical patterns

### 1. UI snapshot tests must redirect `$HOME` to a tempdir

`App::new()` reads `~/.config/reef/prefs` (and, once, the legacy `~/.config/reef/git.prefs`) to seed view-mode toggles. If the test runs with the developer's real `HOME`, the saved `status.tree_mode = true` on their machine bleeds into the snapshot and CI disagrees. Wrap each snapshot test with a `HomeGuard` that points `HOME` at the test's tempdir before calling `App::new()`.

```rust
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

let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());  // serialises HOME swaps too
let (tmp, _raw) = tempdir_repo();
let _home = HomeGuard::enter(tmp.path());
let _cwd  = CwdGuard::enter(tmp.path());
let mut app = App::new();
// ... render + snapshot
```

The `CWD_LOCK` does double duty: it also guards the HOME mutation, so we don't need a second lock. `App::new()` runs `prefs::migrate_legacy_prefs()`; the migrator is a no-op (no fs write) when the tempdir has no prefs file, so the tempdir isn't polluted with a spurious `.config/reef/prefs`.

Full snapshot recipe: **`references/recipes/snapshot.md`**.

### 2. Split time-dependent code into pure `*_at(now, ...)` helpers

Any function that reads `SystemTime::now()` can't be tested deterministically. When you add such code, factor like this:

```rust
pub fn relative_time(t: i64) -> String {                   // thin SystemTime wrapper
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(t);
    relative_time_at(now, t)
}

pub fn relative_time_at(now: i64, t: i64) -> String {      // pure, testable
    // ... all the logic here
}
```

Then test every branch of `relative_time_at` with fixed `now` values. The wrapper stays untested (it's three lines of plumbing). This is how `src/ui/git_graph_panel.rs` does it — match that pattern.

### 3. Tests that mutate process-global state must share a lock

Two sources of process-global state in reef tests:

- **cwd** (`std::env::set_current_dir`) — integration tests that call `GitRepo::open()` or `App::new()` need to be in a real git directory.
- **HOME** (`std::env::set_var("HOME", ...)`) — snapshot tests redirect HOME so `App::new()`'s prefs read doesn't leak the developer's machine state.

Cargo runs tests in parallel threads by default. Without a lock, test A changes cwd, test B reads it mid-operation, both fail unpredictably. Always use this pattern:

```rust
use std::sync::Mutex;
static CWD_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn my_test() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    //                          ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
    // Recover from poisoning — earlier panics shouldn't block later tests.
    // ... test body that changes cwd
}
```

In practice `ui_snapshots.rs` uses a single `CWD_LOCK` for both cwd and HOME because every HOME swap in this codebase is paired with a cwd swap. If you add a test that touches only HOME, a separate `HOME_LOCK` is fine; don't over-share.

See existing examples: `tests/ui_snapshots.rs`, `tests/git_repo_integration.rs`.

### 4. Canonicalize tempdir paths on macOS before handing them to watchers

macOS `TempDir` returns `/var/folders/...` which is a symlink to `/private/var/folders/...`. The `notify` crate delivers canonical paths. If your watcher code does `path.strip_prefix(workdir)` or `path.starts_with(workdir)`, and you pass the non-canonical tempdir path in, the prefix match fails and events are dropped.

Fix in tests:
```rust
let workdir = std::fs::canonicalize(tmp.path()).unwrap_or(tmp.path().to_path_buf());
```

Apply this whenever a test hands a tempdir path to anything that re-receives paths through a fs event channel.

### 5. Insta snapshots need filters for nondeterministic tokens

Tempdir names (`.tmpXYZ123`), absolute paths, and timestamps break snapshot stability across runs. Use insta's filter feature:

```rust
fn with_filters<F: FnOnce()>(body: F) {
    let mut settings = insta::Settings::clone_current();
    settings.add_filter(r"\.tmp[A-Za-z0-9]{6,}", "[TMPDIR]");
    settings.bind(body);
}

#[test]
fn my_snapshot() {
    // ... setup
    with_filters(|| insta::assert_snapshot!("name", output));
}
```

The `insta` dep must be `{ features = ["filters"] }` (already configured in `reef/Cargo.toml`).

Full recipe: **`references/recipes/snapshot.md`**.

## Property tests: assert invariants, don't enumerate

`proptest` is for "this property holds over a wide input space", not "this specific input produces this specific output" (that's a unit test). Good properties look like:

- "Every row in `build_graph(commits)` has `cells[node_col] == LaneCell::Node`"
- "`parser(any_bytes)` never panics"
- "The length of `build_graph`'s output equals the length of the input"

Generating structured inputs (like topologically-ordered commit sequences) is the bulk of the work. See **`references/recipes/property.md`** for the strategy patterns we use.

## Naming convention

Test function names are `<scenario>_<expected_outcome>`:

- `flatten_collapsed_dir_skips_children`
- `stage_then_unstage_roundtrip`
- `relative_time_at_months`

Not: `test1`, `test_stage`, `stage_works`. When reading `cargo test` output, the name should tell you what broke without reading the body. If you can't write a short `scenario_outcome` name, the test is probably doing too much — split it.

## Don't fight clippy at test time

The workspace's `[workspace.lints.clippy]` policy allows style-only lints (`collapsible_if`, `new_without_default`, etc.) but enforces correctness lints (`unused_imports`, `io_other_error`, `needless_borrow`). If a clippy warning fires in your test code, it's telling you about a real issue — fix the code, don't `#[allow(...)]` it. If a lint is genuinely bothersome project-wide, update `Cargo.toml` workspace lints, not individual tests.

## Before committing a test

1. `cargo test -p reef` passes
2. Run the same test twice to check for flakiness
3. `cargo clippy --workspace --all-targets -- -D warnings` clean
4. `cargo fmt --all -- --check` clean
5. If the test uses `insta::assert_snapshot!`, the `.snap` file is staged (NOT `.snap.new`)
6. If the test mutates process-global state, verify a lock is held before the mutation

## Reference files

Read these when the relevant topic comes up — don't try to memorize everything from SKILL.md:

- **`references/fixtures.md`** — Full `test-support` API reference with examples
- **`references/pitfalls.md`** — Every non-obvious failure mode we've hit, with root cause and fix
- **`references/recipes/integration.md`** — Template for new integration tests (real git repo, env isolation)
- **`references/recipes/snapshot.md`** — Template for new UI snapshots (HOME redirect, insta filters, stability check)
- **`references/recipes/property.md`** — proptest strategy patterns for reef's data types
