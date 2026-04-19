---
name: developing-reef
description: Architecture and coding conventions for changing Reef itself. Use before modifying Reef runtime architecture, `App` state, tabs/panels, rendering code, input dispatch, background work, git/file-tree/diff/graph loading, performance-sensitive paths, or when the user asks about "how Reef is structured", "render blocking", "heavy tasks", "new tab", "new feature architecture", or "project conventions". Pair with `testing-reef` whenever adding or changing tests.
---

# Developing Reef

Use this skill as the project onboarding guide for non-test Reef changes. Reef is a minimal single-process Rust TUI: UI must stay responsive, and expensive host work must not run from render.

## Core Architecture Rules

- Keep `ui::*::render` on cached state only. Do not call git, filesystem walks, diff generation, syntax highlighting, or long formatting from render.
- Treat input handlers as intent dispatchers. They may update cheap UI state (selection, scroll, hover, active tab) and request work, but must not do blocking host work.
- Route expensive work through the background task coordinator in `src/tasks.rs`; merge results only from `App::tick`.
- Prefer stale cached UI over blocking. Show old data plus loading/stale/error status instead of waiting during tab switches or hover/mouse movement.
- Use generation tokens for async results. Late results from older requests must not overwrite newer selections or newer snapshots.
- Keep each tab/panel independently refreshable. Adding a feature should not require another tab to render before data can update.

## Runtime Data Flow

1. User input mutates cheap UI state or calls an `App` request method such as `refresh_status`, `load_diff`, or `load_preview`.
2. The request method marks an `AsyncState`, increments its generation, and sends a worker request through `TaskCoordinator`.
3. Workers do git/filesystem/diff/highlight work off the render path and send `WorkerResult`.
4. `App::tick` drains results, accepts only matching generations, updates snapshots/state, and schedules follow-up work when needed.
5. Render reads the latest state and status flags. It must never be required for progress beyond drawing.

Read `references/runtime-architecture.md` before changing `src/app.rs`, `src/tasks.rs`, `src/input.rs`, or any tab/panel render path.

## File/Module Habits

- `src/app.rs` owns state orchestration: tab state, snapshot fields, async state, result merging, and command side effects.
- `src/tasks.rs` owns background worker definitions and should stay free of UI concerns.
- `src/ui/**` owns rendering and local panel command dispatch; keep it pure except transient hit-test registration.
- `src/input.rs` owns key/mouse routing; use `App::set_active_tab` instead of assigning `active_tab` directly.
- `src/file_tree.rs` owns tree structure and preview data types; updating Git decorations must not rebuild the tree unless structure changed.
- `src/git/**` owns direct git2 operations and pure graph algorithms. Open repositories inside workers with `GitRepo::open_at`.

## Adding or Changing Features

- For a new tab or expensive panel, define: UI state, cached data snapshot, `AsyncState`, worker request/result, request method, result merge path, and render fallback for stale/loading/error.
- For a cheap UI-only feature, keep it local and synchronous, but verify it never calls host I/O through helpers.
- For actions that move HEAD/refs or change index/worktree, mark the affected snapshots stale and let `tick` refresh them.
- For selections that load content, request async work immediately and rely on generations to drop stale responses.
- Keep user-facing behavior stable when possible; avoid broad rewrites of keybindings, file layout, or visual style while solving performance/architecture issues.

## Testing Expectations

- Use `$testing-reef` before adding or modifying tests.
- Add unit tests for pure helpers and state transitions when practical.
- Update snapshot tests only when rendered text/layout intentionally changes.
- For async UI behavior, tests should wait for `tick` to consume worker results instead of assuming synchronous state.
- Run at least focused tests for touched areas; for architecture changes prefer `cargo check`, `cargo test --lib`, relevant integration/snapshot tests, `cargo fmt --all -- --check`, and clippy with `-D warnings`.
