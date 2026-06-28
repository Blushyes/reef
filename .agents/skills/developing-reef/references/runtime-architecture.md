# Reef Runtime Architecture

Use this reference when changing app state, rendering, input dispatch, background work, or any feature that can touch git/filesystem/diff/preview/graph loading.

## Render Contract

Render is for drawing only:

- Allowed: read `AppSnapshot` / read-only `ReefApp` accessors, build `ratatui` lines/widgets, clamp terminal-local scroll to valid bounds, register hit-test regions.
- Not allowed: `git2` calls, `std::fs::read_dir` tree walks, `std::fs::read` previews, diff generation, commit walks, syntax highlighting, shell commands, blocking sleeps, or waiting on channels.
- Hover and mouse movement must stay cheap. If moving the cursor can trigger a heavy operation, the architecture is wrong.

## Background Task Pattern

Every expensive feature follows the same shape:

```text
input/action/search
  -> reef-tui decodes terminal input into AppCommand
  -> ReefApp dispatch updates renderer-neutral state
  -> request method starts AsyncState generation
  -> TaskCoordinator sends worker request
  -> worker computes result
  -> ReefApp::tick drains WorkerResult
  -> generation match updates snapshot
  -> render displays cached snapshot
```

Use this pattern for git status, diffs, file preview/highlighting, file-tree rebuilds, commit graph, commit detail, and commit-file diffs.

## AsyncState Rules

- Call `begin()` only when sending a new worker request.
- Call `mark_stale()` when data may be outdated but the UI can keep showing the old snapshot.
- Accept results only through `complete_ok(generation)` / `complete_err(generation, error)`.
- Never manually overwrite `loading`, `stale`, or `generation` from render/panel code.
- If a result is older than the current generation, drop it silently.

## Tab Responsibilities

### Files

- Tree structure changes (expand/collapse/reveal/fs events) may rebuild the tree through the files worker.
- Git decorations update visible entries in place; they must not rebuild the tree by themselves.
- Preview loads run through the `reef-app` task coordinator because they read files and may syntax-highlight.

### Git

- Git status, ahead/behind, and branch label are cached from the git worker.
- Selecting a file requests a diff asynchronously.
- Stage/unstage/discard/push may do command-side effects, then mark status/diff/graph state stale instead of forcing render-time refresh.

### Graph

- Graph refresh walks commits/refs in the graph worker.
- Commit detail and per-file commit diffs are separate async requests.
- Ref/head changes should invalidate graph state by marking it stale; do not rewalk commits on worktree-only fs events.

## Common Pitfalls

- Dispatching refresh commands inside a render function reintroduces hover lag.
- Reading `repo.head_oid()` or `repo.ahead_behind()` in render is still a git call; cache it in worker payloads.
- Rebuilding `FileTree` just to update status markers makes large repos slow.
- Mutating `engine.state` directly skips command outcomes and stale marking; dispatch `AppCommand` or use the TUI adapter method.
- Making tests assert immediately after an async request is flaky; drive `app.tick()` until the relevant `AsyncState` completes.

## Adding a New Expensive Feature

Before coding, decide these names and locations:

- Renderer-neutral state field in `reef-app`, or terminal-local state in `TuiApp` only when it depends on terminal geometry/protocol.
- Snapshot/result data type.
- `AsyncState` field.
- Worker request and result variant in `crates/reef-app/src/tasks.rs`.
- `AppCommand` dispatch branch / renderer-neutral request method.
- `ReefApp::tick` result-merge branch.
- Active-tab work kickoff branch if it refreshes automatically.
- Render fallback for empty/loading/stale/error.

If any of these feel unnecessary, the feature may be cheap enough to remain synchronous. Verify it does not touch host I/O or git.
