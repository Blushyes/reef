# `test-support` fixture reference

All shared test helpers live in `crates/test-support/src/lib.rs`. Every crate has `test-support = { path = "../test-support" }` as a dev-dependency, so `use test_support::...` works from any `tests/*.rs` or `benches/*.rs`.

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

Keep the `TempDir` binding alive for the whole test. Don't pass `tmp.path()` into code that escapes the test's scope (threads, subprocesses) without confirming they'll finish before `tmp` drops.

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

## Styled-line inspection

UI tests and plugin tests often need to assert that some piece of text or styling appeared in a `StyledLine`. Don't do `line.spans[3].text == "expected"` — span indices shift whenever the layout changes and every test breaks.

### `extract_text(&styled_line) -> String`

Concatenates all span texts into one string. Good for substring assertions without caring about span boundaries.

```rust
let text = extract_text(&line);
assert!(text.contains("modified"));
```

### `assert_span_contains(&styled_line, needle)`

Asserts some span's text contains `needle`. Panics with a helpful message showing all span texts if not found.

```rust
assert_span_contains(&line, "HEAD");
```

### `find_span(&styled_line, needle) -> Option<&Span>`

Returns the first span whose text contains `needle`, so you can inspect its styling:

```rust
let head_span = find_span(&line, " HEAD ").expect("HEAD label present");
assert_eq!(head_span.bg, Some(Color::named("cyan")));
assert_eq!(head_span.bold, Some(true));
```

## Commit graph fixtures

### `make_commit_info(oid, parents) -> CommitInfo`

Builds a `reef_git::git::CommitInfo` with sensible defaults (author "Tester", time 0, subject derived from oid). Used for testing `build_graph` and other pure graph algorithms without needing a real repo.

```rust
let commits = vec![
    make_commit_info("c0", &["c1"]),
    make_commit_info("c1", &["c2"]),
    make_commit_info("c2", &[]),
];
let rows = build_graph(&commits);
```

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
