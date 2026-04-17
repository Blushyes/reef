---
name: testing-reef
description: Conventions and gotchas for writing tests in the reef Rust workspace — unit, integration, property, snapshot, benchmark, and fuzz. Use whenever adding or modifying a test in reef-host, reef-git, reef-protocol, or test-support, or when the user asks "add a test for X", "how should I test Y", "why is this test flaky", "where does this test go", or anything about test fixtures, snapshots, proptest, or CI test failures. Also use when touching clock/time code (requires splitting `*_at(now, t)` helpers), the plugin manager (UI snapshots must detach it), or any code that mutates `cwd`/`HOME` (needs process-wide lock). Covers file-placement rules, the `test-support` fixture crate, insta filters, proptest strategy construction, and the specific pitfalls we've paid for in production — plugin subprocess races, macOS tempdir symlinks, env-var sharing across parallel tests.
---

# Testing in the reef workspace

A project-specific test playbook. Follow this when adding **any** test. The goal is that tests are deterministic across Linux/macOS CI and every dev's laptop, and that no test becomes that one flaky thing everyone reruns.

## Quick decision table

| Scenario | Test type | Where it goes |
|----------|-----------|---------------|
| Pure function, no I/O | Unit test | `#[cfg(test)] mod tests` inline at bottom of the source file |
| Uses `git2::Repository`, real fs, real subprocess | Integration test | `crates/<crate>/tests/<name>_integration.rs` |
| Algorithmic invariant over random inputs | Property test | `crates/<crate>/tests/<name>_properties.rs` (uses `proptest`) |
| Full UI rendered to terminal buffer | Snapshot | `crates/reef-host/tests/<name>_snapshots.rs` (uses `insta` + `ratatui::TestBackend`) |
| Hot-path performance | Benchmark | `crates/<crate>/benches/<name>.rs` (uses `criterion`) |
| Parser / deserializer robustness | Fuzz target | `fuzz/fuzz_targets/<name>.rs` |

If you're unsure, default to unit test first; promote to integration only when real I/O is required for the test to be meaningful.

## Fixtures: always go through `test-support`

The `crates/test-support` crate exists so tests don't reinvent repo setup or span inspection. **Never write `git2::Repository::init` or manual `std::env::set_var` in a test file.** Use these helpers:

- `tempdir_repo()` — returns `(TempDir, git2::Repository)` with `user.name`/`user.email` pre-set (critical for CI where global git config is absent)
- `commit_file(&repo, path, content, subject)` — stages + commits in one call, returns the `Oid`
- `write_file(&repo, path, content)` — writes without staging (for untracked / modified paths)
- `extract_text(&styled_line)`, `assert_span_contains(&styled_line, needle)`, `find_span(&styled_line, needle)` — span assertions that survive styling changes
- `make_commit_info(oid, parents)` — fake `CommitInfo` for graph algorithm tests

Full reference: **`references/fixtures.md`**.

If something's missing from `test-support`, add it there rather than duplicating helpers across test files.

## Critical patterns

### 1. UI snapshot tests MUST detach the plugin manager

`App::new()` auto-spawns plugin subprocesses. Their output depends on whether `target/release/reef-git` happens to exist locally, on OS scheduling, and on how fast the subprocess responds. This has already burned us twice (snapshot green locally, red on CI; then green on CI, red locally). **Never snapshot a frame that includes plugin-rendered panels.**

```rust
use reef_host::plugin::manager::PluginManager;

fn detach_plugins(app: &mut App) {
    app.plugin_manager = PluginManager::new();   // drops existing subprocesses
    app.active_sidebar_panel = None;
}

let mut app = App::new();
detach_plugins(&mut app);                         // <-- do this before rendering
// ... then render + snapshot
```

Plugin-specific integration testing lives in `plugin_handshake.rs` and `plugin_manager_lifecycle.rs` where a real `echo-plugin` subprocess is spawned with a known protocol — use those as the template for plugin tests.

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

fn relative_time_at(now: i64, t: i64) -> String {          // pure, testable
    // ... all the logic here
}
```

Then test every branch of `relative_time_at` with fixed `now` values. The wrapper stays untested (it's three lines of plumbing). This is how `reef-git/src/main.rs` does it — match that pattern.

### 3. Tests that mutate process-global state must share a lock

Two sources of process-global state in reef tests:
- **cwd** (`std::env::set_current_dir`) — integration tests that call `GitRepo::open()` or `App::new()` need to be in a real git directory
- **HOME** (`std::env::set_var("HOME", ...)`) — `prefs_roundtrip.rs` redirects HOME to a tempdir

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

Keep the lock **per state type**, not one global lock. `CWD_LOCK` and `HOME_LOCK` are independent; sharing a lock serializes more than needed.

See existing examples: `reef-host/tests/ui_snapshots.rs`, `reef-git/tests/prefs_roundtrip.rs`, `reef-git/tests/git_repo_integration.rs`.

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

The `insta` dep must be `{ features = ["filters"] }` (already configured in `reef-host/Cargo.toml`).

Full recipe: **`references/recipes/snapshot.md`**.

## Property tests: assert invariants, don't enumerate

`proptest` is for "this property holds over a wide input space", not "this specific input produces this specific output" (that's a unit test). Good properties look like:

- "Every row in `build_graph(commits)` has `cells[node_col] == LaneCell::Node`"
- "`write_message` then `read_message` returns an equivalent message for any valid message"
- "`read_message` never panics on any byte input"

Generating structured inputs (like topologically-ordered commit sequences) is the bulk of the work. See **`references/recipes/property.md`** for the strategy patterns we use.

## Writing integration tests that spawn binaries

When a test needs a real subprocess (plugin handshake, CLI invocation), declare the binary in Cargo.toml and use `env!("CARGO_BIN_EXE_<name>")` to locate it at runtime:

```rust
const ECHO_PLUGIN: &str = env!("CARGO_BIN_EXE_echo-plugin");
```

This resolves to the debug binary during `cargo test`. See `reef-host/tests/plugin_handshake.rs` for the full pattern, including polling `drain_messages` with a bounded timeout.

Full recipe: **`references/recipes/integration.md`**.

## Naming convention

Test function names are `<scenario>_<expected_outcome>`:

- `flatten_collapsed_dir_skips_children`
- `stage_then_unstage_roundtrip`
- `relative_time_at_months`

Not: `test1`, `test_stage`, `stage_works`. When reading `cargo test` output, the name should tell you what broke without reading the body. If you can't write a short `scenario_outcome` name, the test is probably doing too much — split it.

## Don't fight clippy at test time

The workspace's `[workspace.lints.clippy]` policy allows style-only lints (`collapsible_if`, `new_without_default`, etc.) but enforces correctness lints (`unused_imports`, `io_other_error`, `needless_borrow`). If a clippy warning fires in your test code, it's telling you about a real issue — fix the code, don't `#[allow(...)]` it. If a lint is genuinely bothersome project-wide, update `Cargo.toml` workspace lints, not individual tests.

## Before committing a test

1. `cargo test -p <crate>` passes
2. Run the same test twice to check for flakiness
3. `cargo clippy --workspace --all-targets -- -D warnings` clean
4. `cargo fmt --all -- --check` clean
5. If the test uses `insta::assert_snapshot!`, the `.snap` file is staged (NOT `.snap.new`)
6. If the test mutates process-global state, verify a lock is held before the mutation
7. If the test spawns a subprocess, verify cleanup works on panic (usually handled by `Drop` impls but check)

## Reference files

Read these when the relevant topic comes up — don't try to memorize everything from SKILL.md:

- **`references/fixtures.md`** — Full `test-support` API reference with examples
- **`references/pitfalls.md`** — Every non-obvious failure mode we've hit, with root cause and fix
- **`references/recipes/integration.md`** — Template for new integration tests (real git repo, env isolation, subprocess spawn)
- **`references/recipes/snapshot.md`** — Template for new UI snapshots (plugin detachment, insta filters, stability check)
- **`references/recipes/property.md`** — proptest strategy patterns for reef's data types
