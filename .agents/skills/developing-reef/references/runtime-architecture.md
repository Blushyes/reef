# Reef Runtime Architecture

Use this reference when changing app state, rendering, input dispatch, background work, or any feature that can touch git/filesystem/diff/preview/graph loading.

## Render Contract

Render is for drawing only:

- Allowed: read `AppSnapshot` / read-only `ReefApp` accessors, build `ratatui` lines/widgets, clamp terminal-local scroll to valid bounds, register hit-test regions.
- Not allowed: `git2` calls, `std::fs::read_dir` tree walks, `std::fs::read` previews, diff generation, commit walks, syntax highlighting, shell commands, video decoder open/rebuild, blocking sleeps, or waiting on channels.
- Renderers may cache terminal-local media geometry. The host tick compares that cache with the active player and schedules decoder work off-thread under a latest-request generation.
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
  -> worker sends WorkerResult and a coalesced wake notification
  -> host calls ReefApp::step
  -> step drains WorkerResult and reports runtime events + next_deadline
  -> generation match updates snapshot
  -> render displays cached snapshot
```

Use this pattern for git status, diffs, file preview/highlighting, file-tree rebuilds, commit graph, commit detail, and commit-file diffs.

## Runtime Progress Contract

- Hosts supply immutable startup dependencies through `AppConfig`; they never construct or retain
  `AppState`.
- `ReefApp` does not own a polling loop. The host owns waiting and calls `step` after user input,
  worker wake notification, filesystem watcher notification, or the `next_deadline` returned by
  the previous step.
- A host adapter that consumes a concrete filesystem watcher event must forward the complete
  semantic change, including `workspace_paths`, `git_metadata_changed`, and
  `repo_presence_changed`, through `AppCommand::ApplyFsChange`; it must not add a public mutable
  `ReefApp` entry point. Hosts that leave the watcher receiver to the engine simply call `step`
  after their wake notification.
- Local filesystem watcher events carry workspace-relative changed paths. The file tree may
  refresh for any workspace event, but an already-open preview reloads only when one of the
  dependency paths declared by its `PreviewDocument` changes. Backends without path-level watcher
  data leave the path list empty, which intentionally keeps the conservative preview-refresh
  behavior.
- Filesystem event coalescing deduplicates precise paths and keeps the aggregate bounded. When a
  burst exceeds that bound, the coalescer emits an empty path list so consumers perform the same
  conservative whole-workspace refresh instead of dropping future notifications.
- A remote backend connection is ready only after its filesystem-event subscription succeeds.
  Subscription failure fails the connection instead of exposing a backend whose cached state can
  never be invalidated.
- Worker wake notifications are coalesced signals only. `ReefApp::step` remains the only owner of
  consuming and merging `WorkerResult`.
- Scheduled work must contribute its earliest due time to `next_deadline`; do not add fixed-rate
  polling to compensate for an omitted deadline.
- `AppStepOutcome.changed` tells the host whether renderer-visible state may need refreshing.
  `runtime_events` carry adapter work that cannot be completed inside renderer-neutral state.
- Hosts may choose different waiting primitives, but they must preserve this command/wake/deadline
  contract.

## AsyncState Rules

- Call `begin()` only when sending a new worker request.
- Call `mark_stale()` when data may be outdated but the UI can keep showing the old snapshot. If
  it runs during a load, the matching successful completion preserves that invalidation for one
  follow-up load.
- `begin()` clears the invalidation being serviced. `complete_ok(generation)` accepts only the
  matching generation and preserves any newer invalidation raised during the load.
- `complete_err(generation, error)` records an error and preserves any newer invalidation raised
  during the load. Without a newer invalidation, a filesystem event, user action, or explicit
  refresh must invalidate the state before the request can run again.
- Never manually overwrite `loading`, `stale`, or `generation` from render/panel code.
- If a result is older than the current generation, drop it silently.

## Tab Responsibilities

### Search

- `reef-app` owns the global-search query, debounce deadline, generation, streaming result merge, selection, exclusions, replacement state, and selected-result preview synchronization.
- Confirming a global-search hit validates the selected path through the preview worker before changing tabs or recording navigation history. A missing target keeps search open and removes that path's stale hits.
- Renderers send full-value semantic commands for query and replacement edits. They do not mutate search cursors or result state directly.
- Hosts pass a preview viewport height to `SyncGlobalSearchPreviewToSelected` and `SyncGlobalSearchPreviewIfStale`; `reef-app` keeps the target path and request generation aligned, then reveals the selected match even when the preview content is reused.
- Large result sets are exposed through paged row snapshots. Renderers request visible windows and keep previously loaded rows and their preview visible while a newer generation is loading. The app swaps generations atomically when the first new chunk arrives, or clears the old generation when an empty search completes; query/progress events alone must not publish an empty row window.
- Streamed hits are frame-batched before publication and merged into the existing path/line order;
  do not re-sort the complete accumulated result set for every backend chunk.
- Host adapters expose search panel state and visible search rows as narrow projections. Search
  progress must not invalidate the full application snapshot, and syntax enrichment for result
  rows must not run on the command/event runtime thread.
- Every search request carries a cancellation token into the backend. Remote agents run content
  scans on a dedicated worker so their connection thread can process `CancelSearch`; obsolete
  queued or active scans stop during the current file read instead of delaying the newest query.
- Replace-in-files sends only the pattern, replacement, and complete-line revision guards to the
  backend. The backend owns the bounded read, guarded transform, and atomic write; remote source
  files never cross the single-frame protocol boundary.

### Files

- Tree structure changes from filesystem events or external reveal requests use the full-tree
  rebuild worker. Its queued rebuilds coalesce to the newest generation.
- Interactive directory expansion uses a separate bounded subtree worker pool. Each parent path
  owns an independent request ID, so expanding one directory never cancels or waits behind a
  sibling expansion. Collapsing or re-expanding a parent invalidates only that parent's old result.
  Expansion publishes the parent presentation and inserted descendants as one structural update;
  collapse remains immediate because its visible descendants are already resident.
- Directory children are resolved lazily. Listing one level must perform one directory enumeration;
  it must not open every child directory merely to decide whether to draw a disclosure indicator.
  Directory rows remain potentially expandable until their own subtree result proves they are
  empty, at which point the shared tree model resolves the row to a leaf.
- Quick Open indexing and filesystem mutations use the general files worker. Quick Open filtering
  uses its own latest-wins worker over the shared immutable candidate index, and Preview uses a
  separate latest-wins worker; none of those queues may delay an interactive tree expansion or
  file Preview. Adapters receive Quick Open changes as a narrow projection event rather than as a
  full application snapshot. Workspace changes mark both the candidate index and its active load
  stale; an open palette immediately schedules the newest generation, while a change observed
  during a build survives that completion and schedules one follow-up build.
- Selecting an entry already present in the visible file-tree projection uses
  `SelectVisibleFileTreePath`: it updates selection and schedules Preview without revealing or
  rebuilding the tree. Commands that originate outside the visible tree, such as Quick Open and
  navigation history, use the reveal path so their target can be materialized first.
- Git decorations update visible entries in place; they must not rebuild the tree by themselves.
  Subtree workers return structure only; accepted children are decorated from the current cached
  status map in O(inserted rows), rather than cloning or rebuilding the repository-wide status
  snapshot for every expansion.
- Preview loads run through the `reef-app` task coordinator. The preview worker publishes the base document first; only after that result is accepted does a separate enrichment worker add syntax highlighting and tree-sitter data. Renderers must accept the plain snapshot immediately and treat enrichment as an in-place revision update. Adapter actions that need enrichment, such as TUI code navigation or deferred UTF-16 highlights, must retain a generation/path-bound intent and retry it from `RetryDeferredPreviewActions`; they must not discard the input while the enrichment request is pending.
- Multi-result code navigation keeps the candidate model and selected file preview in `reef-app`.
  Candidates are sorted and grouped by workspace-relative file; changing the selection dispatches a
  latest-wins navigation-preview request on its own worker when the expanded Peek mode is active.
  Compact mode renders candidates without requesting that preview. The main Preview document
  remains untouched until the user confirms a jump. Renderers may own the Peek anchor and hit
  geometry and pass the current candidate viewport row count with navigation open, selection,
  group-toggle, mode-toggle, and scroll commands. The app uses that row count to keep selection and
  scroll bounds valid without owning terminal geometry. Renderers must render the cached navigation
  document and never read the selected file themselves.
- Native renderers map their platform navigation modifier-click to a typed file cursor and dispatch
  `NavigatePreviewDefinitionAt`. `reef-app` schedules the initial workspace-index build from its
  stale async state, retains a generation/path-bound request while preview
  enrichment or the workspace index is pending, resolves definitions with the workspace index, falls through to
  references at declarations, and owns candidate confirmation plus navigation history.
- Preview snapshots expose separate content and presentation revisions. `source_revision` changes only when accepted raw preview content changes; `revision` may also change when asynchronous enrichment arrives. Content-relative state such as find, selection, and navigation uses `source_revision`, while renderer caches that include styling use `revision`.
- OS drag-and-drop and place-mode sources use `CopyFiles`. A remote backend treats every such path
  as host-local and uploads it; workdir-internal clipboard copies use `CopyPaths`. Placement uses
  keep-both names on both backend types; remote placement reserves them from one destination
  snapshot per batch.
- SQLite row pages keep TEXT cells bounded for predictable local and remote payload sizes. Every
  page row includes a bounded backend locator: an unshadowed hidden rowid alias when available,
  primary-key values when they fit the locator budget, or an offset plus fingerprint when the
  object exposes no stable key or its key is too large. A renderer that opens one cell dispatches
  `DbLoadCell` with that locator; the backend
  performs one complete-value read. Remote agents stream frame-bounded TEXT chunks from that read
  and finish with a full-cell revision that the client validates before publishing. Generation
  plus object/locator/column identity prevents a late result from replacing a newer cell selection.
  Complete-cell reads have a dedicated latest-wins worker and a cancellation token; replacing or
  closing the selection cancels the local SQLite query or remote agent stream without blocking
  ordinary preview/page requests.
  Tables derive their last page from the eager row count. Views keep the last page unknown and
  discover it from a short page or an empty request immediately after a full page.
- Database preview state records the accepted preview source revision. When the same path receives
  changed source content, `reef-app` preserves a still-valid object selection but schedules a fresh
  page or detail load; path equality alone must never keep rows from an older database snapshot.

### Git

- Git status, ahead/behind, branch label, and mutations use the general Git worker. Interactive
  file diffs use a separate latest-wins worker, so repository-wide status refreshes and queued
  obsolete selections cannot delay the current preview.
- Selecting a different file requests a diff asynchronously. Re-selecting the same staged/path
  identity is a no-op and must not enqueue another diff or clear renderer selection.
- Renderer bridges expose Git diff content and its loading/error state through a dedicated,
  revisioned payload. File selection and diff completion must not serialize the application
  snapshot or unrelated Graph rows; keep the previously accepted diff visible until the new
  payload arrives.
- Renderer bridges publish Git status row structure and Git status selection independently.
  Selecting a file updates only the selected row identity and requests its diff; it must not
  rebuild, fetch, or reload the status-row window. Diff renderers prepare highlighting off the
  main thread and replace the visible document once per accepted payload.
- Stage and unstage submit the selected paths as one logical batch. Local backends pass NUL-delimited
  literal pathspecs to one native mutating Git process. Remote backends stream frame-bounded path
  chunks under one operation id; the agent accumulates them and invokes one native Git mutation
  only after the terminal chunk arrives.
- Status refresh classifies file state without computing repository-wide content line counts.
  A dedicated Git-stats worker computes `+N/-M` asynchronously under its own generation and
  merges those counts into the already-visible status snapshot. A failed stats request keeps the
  cached counts and waits for the next status invalidation before retrying.
- Stage/unstage/discard/push may do command-side effects, then mark status/diff/graph state stale instead of forcing render-time refresh.

### Graph

- Graph refresh walks commits/refs in the graph-refresh worker.
- The graph-refresh queue coalesces pending requests to the newest generation before each commit
  walk.
- Commit detail and per-file commit diffs run on the separate graph-content worker, so a periodic
  commit walk never queues a user-selected file diff behind it.
- Ref/head changes should invalidate graph state by marking it stale; do not rewalk commits on worktree-only fs events.

## Common Pitfalls

- Dispatching refresh commands inside a render function reintroduces hover lag.
- Reading `repo.head_oid()` or `repo.ahead_behind()` in render is still a git call; cache it in worker payloads.
- Rebuilding `FileTree` just to update status markers makes large repos slow.
- Mutating `engine.state` directly skips command outcomes and stale marking; dispatch `AppCommand` or use the TUI adapter method.
- Making tests assert immediately after an async request is flaky; wait for the worker wake or due
  deadline and drive `app.step(...)` until the relevant `AsyncState` completes.

## Adding a New Expensive Feature

Before coding, decide these names and locations:

- Renderer-neutral state field in `reef-app`, or terminal-local state in `TuiApp` only when it depends on terminal geometry/protocol.
- Snapshot/result data type.
- `AsyncState` field.
- Worker request and result variant in `crates/reef-app/src/tasks.rs`.
- `AppCommand` dispatch branch / renderer-neutral request method.
- `ReefApp::step` result-merge branch.
- Active-tab work kickoff branch if it refreshes automatically.
- Render fallback for empty/loading/stale/error.

If any of these feel unnecessary, the feature may be cheap enough to remain synchronous. Verify it does not touch host I/O or git.

## Search preview projection

- `reef-app` owns the selected global-search hit and projects its path, query, same-file
  occurrence, row, and byte range into the renderer-neutral snapshot.
- Renderer hosts consume that projection to reveal and highlight the exact preview match. They
  must not reconstruct selection identity from rendered rows or maintain a second search state.
