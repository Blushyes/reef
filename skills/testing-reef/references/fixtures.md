# `test-support` fixture reference

All shared test helpers live in `crates/test-support/src/lib.rs`. Every consumer has `test-support = { path = "../test-support" }` as a dev-dependency, so `use test_support::...` works from any `tests/*.rs` or `benches/*.rs`.

Don't reach into `git2` directly from tests or copy these helpers around — extend the crate when you need something new. One canonical source means every test gets the same fix when we find a better way.

## Repository fixtures

### `tempdir_repo() -> (TempDir, git2::Repository)`

Initializes a real git repo in a fresh `TempDir`. Sets `user.name = "Tester"` and `user.email = "tester@example.com"` in the repo config. This is **required** for later `commit()` calls to succeed in CI, where no global git identity exists.

```rust
use test_support::tempdir_repo;

let (tmp, raw) = tempdir_repo();
// tmp is a TempDir guard — repo is deleted when it drops
// raw is the git2::Repository handle
```

Keep the `TempDir` binding alive for the whole test. Don't pass `tmp.path()` into code that escapes the test's scope (threads, watchers) without confirming they'll finish before `tmp` drops.

### `commit_file(repo, path, content, subject) -> git2::Oid`

One-shot: write `<workdir>/<path>` with `content`, stage it, commit with `subject`. Creates parent directories as needed. Handles both initial commit and subsequent commits (looks up HEAD as parent if present).

```rust
let oid = commit_file(&raw, "src/main.rs", "fn main() {}", "initial commit");
```

Use for setting up known baseline state before the test exercises the code path you care about.

### `write_file(repo, path, content)`

Writes without staging. Use to create:

- **Untracked files** — file exists in workdir but not in index (triggers `FileStatus::Untracked`)
- **Modified unstaged files** — after a previous `commit_file`, overwrite with different content (triggers `FileStatus::Modified` on the unstaged side)

```rust
commit_file(&raw, "a.txt", "v1", "init");       // tracked, clean
write_file(&raw, "a.txt", "v2");                 // now unstaged-modified
write_file(&raw, "new.txt", "untracked");        // untracked
```

## Asserting on ratatui panel output

There are no span-level helpers here for ratatui output. When a test needs to assert that specific text appears in a rendered panel, use the snapshot recipe (`references/recipes/snapshot.md`) to render to a `TestBackend` and assert against the buffer as a string. Ratatui's `Line` / `Span` types aren't meant to be introspected in tests — span boundaries shift as layouts evolve and every index-based assertion becomes a trap.

For the inline panels' pure logic (e.g. `tree::build` in `crates/reef-host/src/git/tree.rs`), assert on the returned data structure directly — don't route through rendering.

## Commit graph fixtures

If you need a `CommitInfo` for graph algorithm tests, build one by hand — it's six fields, half trivial:

```rust
use reef_host::git::CommitInfo;

fn fake_commit(oid: &str, parents: &[&str]) -> CommitInfo {
    CommitInfo {
        oid: oid.into(),
        short_oid: oid.chars().take(7).collect(),
        parents: parents.iter().map(|s| (*s).into()).collect(),
        author_name: String::new(),
        author_email: String::new(),
        time: 0,
        subject: String::new(),
    }
}
```

Existing examples:

- `crates/reef-host/src/git/graph.rs` — unit tests inside the module itself
- `crates/reef-host/tests/git_graph_properties.rs` — the proptest version, which generates topologically-ordered commit vectors from a random shape
- `crates/reef-host/benches/graph.rs` — the bench version with a deterministic fork/merge pattern

Topological order (child before parent) is the caller's responsibility — `build_graph` assumes it.

## Extending `test-support`

Add a helper when:

- Three or more test files would duplicate the same setup
- A non-obvious configuration (like `user.email` for commits on CI) needs to be centralized so it can't be forgotten
- A helper hides a gotcha (macOS canonicalization, env var scoping, etc.) that should never have to be rediscovered

Don't add a helper when:

- Only one test needs it — put it in the test file
- It's trivial (one line) and the test reads more clearly inline

Whenever you extend `test-support`, update this file so future tests discover the helper.
