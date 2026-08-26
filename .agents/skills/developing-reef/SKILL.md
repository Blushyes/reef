---
name: developing-reef
description: REQUIRED before any non-test code change in Reef. Load this skill before modifying Reef runtime architecture, `App` state, tabs/panels, rendering code, input dispatch (`crates/reef-tui/src/input.rs`, any picker overlay, the commit textarea, any new text-input field), background work, git/file-tree/diff/graph loading, performance-sensitive paths, or when the user asks about "how Reef is structured", "render blocking", "heavy tasks", "new tab", "new feature architecture", "input handling", "text input", "picker", "PickerCore", "input_edit", or "project conventions". Do NOT start editing Reef source without loading this skill first — the architecture has non-obvious invariants (render-pure, async generation tokens, three-layer text-input stack) whose violation has been re-introduced and re-fixed across multiple PRs. Pair with `testing-reef` whenever adding or changing tests.
---

# Developing Reef

Use this skill as the project onboarding guide for non-test Reef changes. The workspace contains a
renderer-neutral Rust app engine plus the Reef TUI frontend. Every host must stay responsive, and
expensive work must not run from a renderer.

## Core Architecture Rules

- Keep `ui::*::render` on cached state only. Do not call git, filesystem walks, diff generation, syntax highlighting, external processes, or long formatting from render. Renderer-specific media geometry may be cached during render; decoder open/rebuild work starts from the host tick.
- Treat input handlers as intent dispatchers. They decode terminal input and dispatch `reef_app::AppCommand`; they must not directly own business state or do blocking host work.
- Route expensive work through `reef-app`'s task coordinator; merge worker results from
  `ReefApp::step`.
- Put UI-independent logic in `crates/reef-core`; shared host filesystem services such as the unified preference store belong in `crates/reef-io`; keep ratatui/crossterm rendering and input orchestration in `crates/reef-tui`.
- Put renderer-neutral app state, async scheduling, worker-result merge, settings state, nav/history, preview/search/git/graph orchestration in `crates/reef-app`.
- Renderer hosts dispatch preview definition requests with a typed source cursor and viewport budget. For local backends, `reef-app` marks the initial workspace index stale and schedules its build from `step`; remote backends keep that unsupported work idle. `reef-app` owns enrichment- and workspace-index-bound retry, definition/reference resolution, candidate state, selected-candidate preview, history, and committed jumps; hosts own only coordinate mapping and popup geometry.
- Navigation locations retain the exact source line and UTF-8 byte range for jumps and highlights. Their bounded display snippets are cropped around that target range so every renderer can keep the matched identifier visible without reading candidate files or reconstructing source lines.
- Location history is renderer-neutral and stores the active surface, path, UTF-8 cursor, and vertical/horizontal scroll. Hosts sample their current native position only when dispatching an explicit navigation and pass it to the shared back/forward commands; ordinary cursor movement and scrolling do not create history entries.
- Graph Changed-files selection is renderer-neutral. Hosts dispatch file-navigation commands; `reef-app` follows the visible list/tree order, skips collapsed descendants, updates selection immediately, and schedules the matching commit diff without moving focus away from the Changed-files panel.
- Hosts construct `ReefApp` from `AppConfig`; `AppState` and its construction details stay private
  to `reef-app`.
- Keep terminal-only state in `crates/reef-tui`: ratatui layout caches, hit-test registry, terminal image protocol, text-selection geometry, mouse row/column mapping, scroll pacing, leader/chord timers, popup rects, and the live TUI theme object.
- Keep inline-video decode and terminal protocol encoding off the TUI loop. Bound decoded frame dimensions and playback FPS by terminal wire cost before starting ffmpeg; playback ticks may drain decoded frames, submit only the newest due frame to a bounded encoder, and merge completed protocol payloads without waiting.
- Prefer stale cached UI over blocking. Show old data plus loading/stale/error status instead of waiting during tab switches or hover/mouse movement.
- Use generation tokens for async results. Late results from older requests must not overwrite newer selections or newer snapshots.
- Keep each tab/panel independently refreshable. Adding a feature should not require another tab to render before data can update.
- When a renderer needs preview content beyond the bounded display projection, expose that content through a typed, renderer-neutral `PreviewBodySnapshot`; do not make a host reparse source content while selecting or rendering a preview.
- Preserve complete structured source separately from its bounded visible-line projection. Reef must not identify or name third-party formats; renderer-specific recognition belongs to the renderer that consumes this generic source.

## Runtime Data Flow

1. User input in `reef-tui` becomes an `AppCommand`.
2. `ReefApp::dispatch` mutates renderer-neutral state or requests work.
3. The request method marks an `AsyncState`, increments its generation, and sends a worker request through `TaskCoordinator`.
4. Workers do git/filesystem/diff/highlight work off the render path and send `WorkerResult`.
5. The host calls `ReefApp::step` after input, a coalesced worker wake notification, a filesystem
   watcher notification, or the reported `next_deadline`.
6. `step` drains results, accepts only matching generations, updates state, emits runtime events,
   and reports the next deadline.
7. Render reads `AppSnapshot` plus explicit read-only accessors. It must never be required for
   progress beyond drawing.

Read `references/runtime-architecture.md` before changing `crates/reef-app/src/**`, `crates/reef-tui/src/app/mod.rs`, `crates/reef-tui/src/input.rs`, or any tab/panel render path.

## File/Module Habits

- `crates/reef-app/src/engine.rs` owns the public mutable app boundary. New mutable business entry points must be represented as `AppCommand`; if a new public `&mut self` method is truly needed, update `scripts/check-architecture.sh` and explain why in the PR.
- `crates/reef-app/src/app/**` owns renderer-neutral app state orchestration: tab state, async state, result merging, and command side effects.
- `crates/reef-core/src/**` owns UI-independent git, diff, highlight, markdown, nav/LSP, preview loading, file-op, host parsing, and history logic.
- `crates/reef-app/src/tasks.rs` owns background worker definitions and should stay free of UI concerns.
- `crates/reef-tui/src/app/mod.rs` owns the terminal adapter: image protocol, terminal-local selections, hit-test/layout cache, and effect handling.
- `crates/reef-tui/src/ui/**` owns rendering and local panel command dispatch; keep it pure except transient hit-test registration.
- `crates/reef-tui/src/ui/preview/**` owns the TUI preview renderers; core preview models must stay free of ratatui/crossterm types.
- `crates/reef-tui/src/input.rs` owns key/mouse routing; use `AppCommand` / `TuiApp::set_active_tab` instead of assigning `active_tab` directly.
- `crates/reef-tui/src/input_edit{,_multi}.rs` and `crates/reef-tui/src/picker_core.rs` own the shared text-input vocabulary. Don't hand-roll a key table; embed one of the three layers (see "Text Input Stack" below).
- `crates/reef-core/src/file_tree.rs` owns pure file-tree ordering/navigation helpers; `crates/reef-app/src/features/file_tree.rs` owns renderer-neutral file-tree state. `reef-tui` renders snapshots and dispatches commands only.
- `crates/reef-tui/src/keymap.rs` owns shortcut bindings by scope; handlers should dispatch commands, not duplicate key matching tables.

## App Boundary Guardrails

- `reef-tui` must not directly own business state that belongs in `reef-app`. Settings, preview/search/git/graph/nav/history state should flow through `ReefApp` commands, snapshots, or read-only accessors.
- SQLite hosts dispatch the shared database commands and read `DbPreviewState` plus its
  page/detail/cell load status through `ReefApp`. Paginated rows keep bounded TEXT values and
  bounded row locators; oversized primary keys use an offset plus fingerprint. Opening a cell
  requests its complete value through `DbLoadCell` using the locator returned with its page.
  Remote TEXT delivery streams frame-bounded chunks from that one read and validates the
  terminal full-cell revision. A newer cell selection cancels the previous local or remote read;
  cell work runs separately from ordinary file and database-page previews. The opened cell is
  also the grid's cell cursor: hosts move it, close it, and scroll its value through commands,
  and report the grid's body height so `reef-app` can keep the cursor row on screen. Laying the
  value out — pane width, wrapping, JSON coloring — is renderer-owned derived layout and must
  stay out of `reef-app`. Table row counts
  provide a known last page; views keep an unknown last page until a short or empty follow-up page
  establishes the boundary. Schema expansion,
  object selection, paging, typed rows, and details must not be reimplemented by a renderer adapter.
- `reef-app` must not depend on `ratatui`, `crossterm`, or `ratatui-image`.
- Trusted interactive hosts construct `LocalBackend` with
  `open_at_with_external_previews`, allowing preview-only reads to follow a workspace entry's
  symlink target outside the workspace. `reef-agent` and other untrusted request boundaries use
  `open_at`; all non-preview reads and every write remain workspace-bound.
- Worker result merge paths belong in `reef-app`; TUI may adapt terminal-only payloads such as image protocol state before dispatching the merge command.
- `scripts/check-architecture.sh` is the cheap CI tripwire. If it blocks a legitimate change, prefer changing the whitelist with a short explanation over adding another bypass.

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
- For actions that move HEAD/refs or change index/worktree, mark the affected snapshots stale and
  let the next `step` refresh them.
- For selections that load content, request async work immediately and rely on generations to drop stale responses.
- Keep user-facing behavior stable when possible; avoid broad rewrites of keybindings, file layout, or visual style while solving performance/architecture issues.

## Testing Expectations

- Use `$testing-reef` before adding or modifying tests.
- Add unit tests for pure helpers and state transitions when practical.
- Update snapshot tests only when rendered text/layout intentionally changes.
- For async UI behavior, tests should drive `step` after worker wake notifications or due
  deadlines instead of assuming synchronous state.

## Architecture Documentation Contract

- Treat this Skill and `references/runtime-architecture.md` as part of the architecture, not as
  optional prose.
- When a change alters crate ownership, the `ReefApp` public boundary, command/snapshot/event flow,
  task scheduling, wake/deadline behavior, module responsibilities, validation commands, or a
  documented invariant, update the affected Skill/reference in the same change.
- When a public Reef change alters the contract consumed by another renderer or bridge, update the
  renderer-facing contract here and notify the downstream repository in the same development
  cycle.
- Do not merge an architecture change with knowingly stale Skill guidance. The implementation,
  architecture checks, tests, and Skills must describe one current path.

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

- **`cargo test --lib` is not enough.** It skips integration tests (`crates/reef-app/tests/*.rs`, `crates/reef-io/tests/*.rs`, `crates/reef-tui/tests/*.rs`), snapshot tests (`crates/reef-tui/tests/ui_snapshots.rs`), and doctests. CI runs `--workspace --all-features` — so must you.
- **Clippy without `--workspace` misses `test-support`.** The scope flag is load-bearing; drop it and a broken lint in a helper crate sails through locally then trips CI.
- **`cargo fmt` edits in place; `--check` only verifies.** Run the first to fix, the second to gate. Omit the first and every stylebot nit becomes a second commit on your PR.

Coverage (`cargo llvm-cov`) is advisory and doesn't block PRs; no need to run it locally unless you're inspecting coverage deltas.
