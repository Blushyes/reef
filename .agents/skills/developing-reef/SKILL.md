---
name: developing-reef
description: REQUIRED before any non-test code change in Reef. Load this skill before modifying Reef runtime architecture, `App` state, tabs/panels, rendering code, input dispatch (`crates/reef-tui/src/input.rs`, any picker overlay, the commit textarea, any new text-input field), background work, git/file-tree/diff/graph loading, performance-sensitive paths, or when the user asks about "how Reef is structured", "render blocking", "heavy tasks", "new tab", "new feature architecture", "input handling", "text input", "picker", "PickerCore", "input_edit", or "project conventions". Do NOT start editing Reef source without loading this skill first — the architecture has non-obvious invariants (render-pure, async generation tokens, three-layer text-input stack) whose violation has been re-introduced and re-fixed across multiple PRs. Pair with `testing-reef` whenever adding or changing tests.
---

# Developing Reef

Use this skill as the project onboarding guide for non-test Reef changes. Reef is a minimal single-process Rust TUI: UI must stay responsive, and expensive host work must not run from render.

## Core Architecture Rules

- Keep `ui::*::render` on cached state only. Do not call git, filesystem walks, diff generation, syntax highlighting, or long formatting from render.
- Treat input handlers as intent dispatchers. They may update cheap UI state (selection, scroll, hover, active tab) and request work, but must not do blocking host work.
- Route expensive work through the background task coordinator in `crates/reef-tui/src/tasks.rs`; merge results only from `App::tick`.
- Put UI-independent logic in `crates/reef-core`; keep ratatui/crossterm rendering and input orchestration in `crates/reef-tui`.
- Prefer stale cached UI over blocking. Show old data plus loading/stale/error status instead of waiting during tab switches or hover/mouse movement.
- Use generation tokens for async results. Late results from older requests must not overwrite newer selections or newer snapshots.
- Keep each tab/panel independently refreshable. Adding a feature should not require another tab to render before data can update.

## Runtime Data Flow

1. User input mutates cheap UI state or calls an `App` request method such as `refresh_status`, `load_diff`, or `load_preview`.
2. The request method marks an `AsyncState`, increments its generation, and sends a worker request through `TaskCoordinator`.
3. Workers do git/filesystem/diff/highlight work off the render path and send `WorkerResult`.
4. `App::tick` drains results, accepts only matching generations, updates snapshots/state, and schedules follow-up work when needed.
5. Render reads the latest state and status flags. It must never be required for progress beyond drawing.

Read `references/runtime-architecture.md` before changing `crates/reef-tui/src/app/mod.rs`, `crates/reef-tui/src/tasks.rs`, `crates/reef-tui/src/input.rs`, or any tab/panel render path.

## File/Module Habits

- `crates/reef-tui/src/app/mod.rs` owns state orchestration: tab state, snapshot fields, async state, result merging, and command side effects.
- `crates/reef-core/src/**` owns UI-independent git, diff, highlight, markdown, nav/LSP, preview loading, file-op, host parsing, and history logic.
- `crates/reef-tui/src/tasks.rs` owns background worker definitions and should stay free of UI concerns.
- `crates/reef-tui/src/ui/**` owns rendering and local panel command dispatch; keep it pure except transient hit-test registration.
- `crates/reef-tui/src/ui/preview/**` owns the TUI preview renderers; core preview models must stay free of ratatui/crossterm types.
- `crates/reef-tui/src/input.rs` owns key/mouse routing; use `App::set_active_tab` instead of assigning `active_tab` directly.
- `crates/reef-tui/src/input_edit{,_multi}.rs` and `crates/reef-tui/src/picker_core.rs` own the shared text-input vocabulary. Don't hand-roll a key table; embed one of the three layers (see "Text Input Stack" below).
- `crates/reef-core/src/file_tree.rs` owns file-tree state/navigation; `crates/reef-tui/src/file_tree.rs` only wraps it with IO rebuilds. Preview data/loading lives in `crates/reef-core/src/preview/**`.
- `crates/reef-tui/src/keymap.rs` owns shortcut bindings by scope; handlers should dispatch commands, not duplicate key matching tables.

## Text Input Stack

Every text input in Reef routes through one of three layers — L0
`input_edit` (single-line readline/VSCode vocabulary), L1
`input_edit_multi` (textarea: Enter→\n, line-aware Up/Down/Home/End),
or L2 `picker_core::PickerCore` (overlay scaffold: filter + cursor +
selected_idx + dispatch). 12+ input sites are already migrated; net
~700 lines of duplicated key dispatch was removed.

**Do NOT add a new text input without reading
`references/text-input-stack.md` first.** It documents the canonical
example for each layer plus seven mandatory invariants whose
violation has been re-introduced and re-fixed across multiple
review rounds (strict-bare-Enter, Ctrl+letter swallow in textareas,
close-on-confirm-None, edit-derived work on `Edited`-only, UTF-8
cursor boundary, exhaustive `InputOutcome` match, protocol bump
discipline).

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

## Pre-PR Checks

Run these locally before `git push`. CI runs exactly these commands — mirroring them avoids the "commit → push → CI red → fmt fix → force push" round-trip on every PR.

```
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-features
cargo test --workspace --doc
```

Failure modes that keep catching people:

- **`cargo test --lib` is not enough.** It skips integration tests (`crates/reef-tui/tests/*.rs`), snapshot tests (`crates/reef-tui/tests/ui_snapshots.rs`), and doctests. CI runs `--workspace --all-features` — so must you.
- **Clippy without `--workspace` misses `test-support`.** The scope flag is load-bearing; drop it and a broken lint in a helper crate sails through locally then trips CI.
- **`cargo fmt` edits in place; `--check` only verifies.** Run the first to fix, the second to gate. Omit the first and every stylebot nit becomes a second commit on your PR.

Coverage (`cargo llvm-cov`) is advisory and doesn't block PRs; no need to run it locally unless you're inspecting coverage deltas.
