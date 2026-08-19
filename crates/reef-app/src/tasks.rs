//! Background task coordinator.
//!
//! UI code should render cached snapshots only. Anything that can touch git,
//! the filesystem, diff generation, or syntax highlighting is routed through
//! these workers and merged back into `ReefApp` from `step()`.

use crate::app::{
    CommitFileDiff, DiffHighlighted, GLOBAL_SEARCH_MAX_LINE_CHARS, GLOBAL_SEARCH_MAX_RESULTS,
    HighlightedDiff, MatchHit,
};
use reef_core::diff::DiffContent;
use reef_core::file_ops::Resolution;
use reef_core::git::graph::GraphRow;
use reef_core::git::{CommitDetail, FileEntry, GitStatusStats, GraphScope, RefLabel};
use reef_core::preview::{PreviewBody, PreviewDocument as PreviewContent, PreviewEnrichment};
use reef_io::TreeEntry;
use reef_io::{Backend, BackendError, WalkOpts};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel as mpsc;

#[derive(Debug)]
pub struct GitStatusPayload {
    pub staged: Vec<FileEntry>,
    pub unstaged: Vec<FileEntry>,
    pub ahead_behind: Option<(usize, usize)>,
    pub branch_name: String,
}

#[derive(Debug)]
pub struct FileTreePayload {
    pub entries: Vec<TreeEntry>,
    pub selected_idx: usize,
}

#[derive(Debug)]
pub struct FileTreeSubtreePayload {
    pub parent_path: PathBuf,
    pub entries: Vec<TreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRevertPath {
    pub path: String,
    pub is_staged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitMutation {
    Stage(Vec<String>),
    Unstage(Vec<String>),
    Revert(Vec<GitRevertPath>),
}

#[derive(Debug)]
pub struct GitMutationPayload {
    pub mutation: GitMutation,
    pub touched: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug)]
pub struct GraphPayload {
    pub rows: Vec<GraphRow>,
    pub ref_map: HashMap<String, Vec<RefLabel>>,
    /// `(head_oid, refs_hash, scope_hash)` — see `GitGraphState::cache_key`.
    pub cache_key: (String, u64, u64),
    /// The scope this payload was built for. The main thread compares
    /// against the current `git_graph.scope` to detect a fall-through
    /// fallback opportunity (`Branch(missing)` → empty rows → revert to
    /// `AllRefs`).
    pub scope: GraphScope,
}

#[derive(Debug)]
pub struct DbPagePayload {
    pub path: PathBuf,
    pub key: reef_sqlite_preview::DbObjectKey,
    pub page: u64,
    pub rows: Vec<Vec<reef_sqlite_preview::SqliteValue>>,
    pub row_locators: Vec<reef_sqlite_preview::DbRowLocator>,
    pub reset_h_scroll: bool,
    /// An in-place reload of the same object and page: keep the cell
    /// and the scroll the user left behind.
    pub refresh: bool,
}

#[derive(Debug)]
pub struct DbPageRequest {
    pub path: PathBuf,
    pub key: reef_sqlite_preview::DbObjectKey,
    pub page: u64,
    pub rows_per_page: u32,
    pub reset_h_scroll: bool,
    /// An in-place reload of the same object and page: keep the cell
    /// and the scroll the user left behind.
    pub refresh: bool,
}

#[derive(Debug)]
pub struct DbDetailPayload {
    pub path: PathBuf,
    pub key: reef_sqlite_preview::DbObjectKey,
    pub detail: reef_sqlite_preview::DbObjectDetail,
}

#[derive(Debug)]
pub struct DbCellPayload {
    pub path: PathBuf,
    pub key: reef_sqlite_preview::DbObjectKey,
    pub row_offset: u64,
    pub row_locator: reef_sqlite_preview::DbRowLocator,
    pub column: usize,
    pub value: reef_sqlite_preview::SqliteValue,
}

#[derive(Debug)]
pub struct DbCellRequest {
    pub path: PathBuf,
    pub key: reef_sqlite_preview::DbObjectKey,
    pub row_offset: u64,
    pub row_locator: reef_sqlite_preview::DbRowLocator,
    pub column: usize,
    pub cancellation: reef_io::CancellationToken,
}

struct DbCellTask {
    generation: u64,
    backend: Arc<dyn Backend>,
    request: DbCellRequest,
}

#[derive(Debug, Clone)]
pub enum TreeEditMutation {
    CreateFile {
        rel: PathBuf,
        display_name: String,
    },
    CreateFolder {
        rel: PathBuf,
        display_name: String,
    },
    Rename {
        old_rel: PathBuf,
        new_rel: PathBuf,
        old_name: String,
        new_name: String,
    },
}

#[derive(Debug)]
pub struct TreeEditPlan {
    pub mutation: TreeEditMutation,
    pub select_on_done: Option<PathBuf>,
}

#[derive(Debug)]
pub enum TreeEditPlanError {
    Validation {
        error: reef_core::file_ops::FileNameError,
    },
    Backend {
        mutation: TreeEditMutation,
        error: String,
    },
}

#[derive(Debug)]
pub struct PastePlanError {
    pub op: reef_core::file_ops::ClipMode,
    pub error: String,
}

#[derive(Debug)]
pub struct PastePlanPayload {
    pub op: reef_core::file_ops::ClipMode,
    pub dest_rel: PathBuf,
    pub auto_decisions: Vec<(PathBuf, Resolution)>,
    pub pending: Vec<reef_core::file_ops::ConflictItem>,
    pub used_names: HashSet<String>,
    pub self_descent_blocked: usize,
}

#[derive(Debug)]
pub enum WorkerResult {
    FileTree {
        generation: u64,
        tree_revision: u64,
        result: Result<FileTreePayload, String>,
    },
    FileTreeSubtree {
        request_id: u64,
        parent_path: PathBuf,
        result: Result<FileTreeSubtreePayload, String>,
    },
    Preview {
        generation: u64,
        result: Result<Option<PreviewContent>, String>,
    },
    PreviewEnrichmentFinished {
        generation: u64,
        path: String,
        enrichment: Option<PreviewEnrichment>,
    },
    NavPreview {
        generation: u64,
        path: PathBuf,
        result: Result<Option<PreviewContent>, String>,
    },
    DbPage {
        generation: u64,
        result: Result<DbPagePayload, String>,
    },
    DbDetail {
        generation: u64,
        result: Result<DbDetailPayload, String>,
    },
    DbCell {
        generation: u64,
        result: Result<DbCellPayload, String>,
    },
    QuickOpenIndex {
        generation: u64,
        result: Result<Vec<crate::features::quick_open::Candidate>, String>,
    },
    QuickOpenFilter {
        generation: u64,
        matches: Vec<crate::features::quick_open::MatchEntry>,
    },
    TreeEditPlan {
        generation: u64,
        result: Result<TreeEditPlan, TreeEditPlanError>,
    },
    PastePlan {
        generation: u64,
        result: Result<PastePlanPayload, PastePlanError>,
    },
    GitStatus {
        generation: u64,
        result: Result<GitStatusPayload, String>,
    },
    GitStatusStats {
        generation: u64,
        result: Result<GitStatusStats, String>,
    },
    GitMutation {
        generation: u64,
        result: Result<GitMutationPayload, String>,
    },
    Commit {
        generation: u64,
        result: Result<(), String>,
    },
    Push {
        generation: u64,
        force: bool,
        result: Result<(), String>,
    },
    Diff {
        generation: u64,
        result: Result<Option<HighlightedDiff>, String>,
    },
    Graph {
        generation: u64,
        result: Result<GraphPayload, String>,
    },
    CommitDetail {
        generation: u64,
        result: Result<Option<CommitDetail>, String>,
    },
    CommitFileDiff {
        generation: u64,
        result: Result<Option<CommitFileDiff>, String>,
    },
    /// Merged-file list for a commit range — `parent(oldest).tree → newest.tree`.
    /// Consumed by the Graph tab's range-select mode. Per-commit subject
    /// metadata is filled in on the main thread from cached `rows`.
    RangeDetail {
        generation: u64,
        result: Result<Vec<FileEntry>, String>,
    },
    /// Single-file diff for a commit range, same semantics as `CommitFileDiff`
    /// but sourced from `Backend::range_file_diff`.
    RangeFileDiff {
        generation: u64,
        result: Result<Option<CommitFileDiff>, String>,
    },
    /// A batch of global-search hits. Streamed from the worker so the UI
    /// stays responsive on big workdirs; can fire multiple times per search
    /// before the matching `GlobalSearchDone`. Consumers drop the payload
    /// when `generation` doesn't match the current search.
    GlobalSearchChunk {
        generation: u64,
        hits: Vec<MatchHit>,
    },
    /// End-of-stream marker for a global search. `truncated=true` means we
    /// hit the result cap; the UI shows "refine query" hinting.
    GlobalSearchDone { generation: u64, truncated: bool },
    /// Place-mode drag-and-drop copy completion.
    /// `Ok(count)` is the number of top-level items successfully placed
    /// at the destination (a directory source counts as 1 regardless of
    /// how many files were recursively copied beneath it).
    FileCopy {
        generation: u64,
        result: Result<usize, String>,
    },
    /// Result of a file-tree toolbar / context-menu mutation (Create,
    /// Rename, Trash, HardDelete). `kind` is carried separately from
    /// `result` so the merge site can pick the right toast phrasing
    /// (created vs. renamed vs. deleted) without having to sniff the
    /// worker task itself.
    FsMutation {
        generation: u64,
        kind: FsMutationKind,
        result: Result<(), String>,
    },
    /// Per-file checkpoint during a `FilesTask::ReplaceInFiles` batch.
    /// Fires once per file the worker has finished processing (whether
    /// it actually changed bytes or skipped). The UI uses
    /// `(files_done, files_total)` to surface a "replacing N/M…"
    /// progress hint — drop this if `generation` doesn't match the
    /// active replace.
    ReplaceProgress {
        generation: u64,
        files_done: usize,
        files_total: usize,
    },
    /// Final marker for a `FilesTask::ReplaceInFiles` batch. Carries a
    /// `ReplaceSummary` on success (per-bucket counts so the toast can
    /// say "Replaced N lines in M files; K skipped (stale), …") or a
    /// pre-flight error string. Even on success, individual per-file
    /// errors are tucked into `summary.errors` rather than failing the
    /// whole batch — the user sees a partial result toast.
    ReplaceDone {
        generation: u64,
        result: Result<ReplaceSummary, String>,
    },
    /// Workspace symbol index build finished.
    /// Carries the whole `WorkspaceIndex` since cross-file `gd` /
    /// `gr` query against it from the main thread. Stale results are
    /// dropped via `nav_workspace_load`'s generation token.
    NavWorkspaceBuilt {
        generation: u64,
        result: Result<reef_core::nav::WorkspaceIndex, String>,
    },
    /// LSP refinement. `rel_location` is already workdir-relative when the
    /// server returned an in-workspace definition; `None` means either no
    /// definition or a definition outside the workspace. `server_returned_location`
    /// lets the merge path choose the right toast text.
    LspRefineDone {
        generation: u64,
        /// Refine-cache epoch captured at dispatch. The main thread
        /// compares it against the current epoch before inserting: a
        /// response whose epoch predates an `fs_dirty` cache-clear
        /// carries a location snapshotted from now-stale bytes, so it
        /// must NOT repopulate the just-cleared cache (it would jump to
        /// a pre-edit line on the next `gd`).
        epoch: u64,
        lang: reef_core::nav::NavLang,
        identifier: String,
        rel_location: Option<reef_core::nav::LspLocation>,
        server_returned_location: bool,
    },
    /// LSP supervisor state change; drives the status-bar badge.
    LspStateChange {
        lang: reef_core::nav::NavLang,
        state: reef_core::nav::LspBadge,
    },
}

/// Per-file unit of work for `FilesTask::ReplaceInFiles`. The worker
/// reads each `path` and replaces every occurrence of the search
/// pattern on each `lines` entry whose current text still matches the
/// UI snapshot (TOCTOU guard).
#[derive(Debug, Clone)]
pub struct ReplaceItem {
    /// Workdir-relative path to the target file.
    pub path: PathBuf,
    /// Lines the user opted into. Must be sorted ascending by
    /// `line_no` (the UI guarantees this; the worker assumes it).
    pub lines: Vec<ReplaceLine>,
}

/// One opted-in match for `ReplaceItem`: the line number plus the UI
/// snapshot of the line's text used as a TOCTOU guard. Replaces a
/// pair of parallel `Vec<usize>` / `Vec<String>` so the invariant
/// "same length, same order" can't drift.
#[derive(Debug, Clone)]
pub struct ReplaceLine {
    /// 0-indexed line number in the file.
    pub line_no: usize,
    /// Revision of the complete line observed by content search. The worker
    /// compares the current complete line before rewriting; display text is
    /// intentionally not part of this consistency check because it is capped.
    pub expected_revision: u64,
}

/// Aggregate result of a `FilesTask::ReplaceInFiles` run. Every
/// outcome category gets its own counter so the toast can surface the
/// shape of partial failures without the user having to dig into a
/// per-file error list.
#[derive(Debug, Clone, Default)]
pub struct ReplaceSummary {
    /// Files where at least one line was rewritten and persisted.
    pub files_changed: usize,
    /// Total lines rewritten across all files. Each line counts once,
    /// even if it had multiple matches replaced.
    pub lines_replaced: usize,
    /// Lines whose current text no longer matches the UI snapshot.
    /// Almost always means the user edited the file in another
    /// process between the search stream and the apply.
    pub skipped_stale: usize,
    /// Files that exceed `MAX_REPLACE_FILE_SIZE` and were skipped
    /// outright.
    pub skipped_too_large: usize,
    /// Files whose canonical path resolves outside the workdir (via
    /// symlink); refused before any IO.
    pub skipped_symlink_escape: usize,
    /// Per-file errors: read failures, write failures, regex build
    /// errors, etc. Populated alongside the bucket counters when one
    /// file fails — the rest of the batch still proceeds.
    pub errors: Vec<(PathBuf, String)>,
}

/// Hard cap on file size for the global replace path. Files over this
/// are listed in `ReplaceSummary.skipped_too_large` and left untouched.
/// Search streams arbitrarily large files, but replace must load the
/// whole thing into memory to write it back atomically — 50 MB covers
/// the long tail (lockfiles, generated SQL dumps, fat JSON fixtures)
/// without making the worker hold gigabytes resident.
pub const MAX_REPLACE_FILE_SIZE: u64 = 50 * 1024 * 1024;

/// What mutation a `FsMutation` corresponds to. The `created_name` /
/// `old_name` / `new_name` fields feed the toast text — we could resolve
/// them from the worker task by looking at the path, but carrying them
/// on the result keeps the merge path from doing path arithmetic during
/// render.
#[derive(Debug, Clone)]
pub enum FsMutationKind {
    /// A new file was created. `name` is the final basename.
    CreatedFile { name: String },
    /// A new folder was created. `name` is the final basename.
    CreatedFolder { name: String },
    /// Rename completed. Display as "old → new".
    Renamed { old_name: String, new_name: String },
    /// Entry moved to the OS Trash. `name` is the basename.
    Trashed { name: String },
    /// Entry hard-deleted (Shift+Delete). `name` is the basename.
    HardDeleted { name: String },
    /// Single-item paste-move (Cut + Paste). Display similarly to
    /// `Renamed` but with cross-directory semantics in the toast.
    Moved { old_name: String, new_name: String },
    /// Single-item paste-copy / Duplicate / Alt-drag. `name` is the
    /// final basename at the destination.
    CopiedTo { name: String },
    /// Multi-item paste-move. `count` is the number of top-level items
    /// successfully placed (Skip / failed items not counted). Used by
    /// the toast renderer.
    MovedMulti { count: usize },
    /// Multi-item paste-copy / Alt-drag with multi-selection.
    CopiedMulti { count: usize },
}

struct FileTreeRebuildTask {
    identity: FileTreeRebuildIdentity,
    backend: Arc<dyn Backend>,
    expanded: Vec<PathBuf>,
    git_statuses: HashMap<String, char>,
    selected_path: Option<PathBuf>,
    fallback_selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileTreeRebuildIdentity {
    pub generation: u64,
    pub tree_revision: u64,
}

struct FileTreeSubtreeTask {
    request_id: u64,
    backend: Arc<dyn Backend>,
    parent_path: PathBuf,
    parent_depth: usize,
    expanded: Vec<PathBuf>,
}

enum FilesTask {
    LoadPreview {
        generation: u64,
        backend: Arc<dyn Backend>,
        rel_path: PathBuf,
        wants_decoded_image: bool,
    },
    LoadDbPage {
        generation: u64,
        backend: Arc<dyn Backend>,
        request: DbPageRequest,
    },
    LoadDbDetail {
        generation: u64,
        backend: Arc<dyn Backend>,
        path: PathBuf,
        key: reef_sqlite_preview::DbObjectKey,
    },
    BuildQuickOpenIndex {
        generation: u64,
        backend: Arc<dyn Backend>,
    },
    /// Warm the preview cache for a neighbor of the currently-selected
    /// file. Same decode path as `LoadPreview`, but the result is
    /// **discarded** — the side effect is populating
    /// `LocalBackend::preview_cache`, so when the user actually
    /// cursor-steps onto the neighbor the real `LoadPreview` is a cheap
    /// clone instead of a 50-200 ms decode.
    PrefetchPreview {
        backend: Arc<dyn Backend>,
        rel_path: PathBuf,
        wants_decoded_image: bool,
    },
    PlanTreeEdit {
        generation: u64,
        backend: Arc<dyn Backend>,
        mode: crate::features::tree_edit::TreeEditMode,
        parent_rel: PathBuf,
        rename_source: Option<PathBuf>,
        name: String,
    },
    PlanPaste {
        generation: u64,
        backend: Arc<dyn Backend>,
        op: reef_core::file_ops::ClipMode,
        dest_rel: PathBuf,
        sources: Vec<PathBuf>,
    },
    /// Drag-and-drop / place-mode copy: each source lands under `dest_dir`.
    /// Local backends may optimize sources already under their workdir into
    /// native copies; remote backends always treat these paths as host-local
    /// uploads. Workdir-internal copy uses `CopyPaths` instead.
    CopyFiles {
        generation: u64,
        backend: Arc<dyn Backend>,
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
    },
    /// Create an empty file at `rel`. Fails if the parent dir is
    /// missing or the file already exists — the UI layer
    /// (`App::commit_tree_edit`) has already validated + rejected
    /// collisions before dispatch, but a race with an external
    /// process is possible so we still surface the error.
    CreateFile {
        generation: u64,
        backend: Arc<dyn Backend>,
        rel: PathBuf,
        /// Basename for the toast (worker shouldn't redo `file_name`
        /// arithmetic on a workdir-relative path — preserves the old
        /// behaviour where rootless paths still rendered cleanly).
        display_name: String,
    },
    /// `mkdir -p` on `rel`. If the directory already exists we
    /// treat that as success (the rare race window) to avoid a
    /// surprising failure after the user explicitly asked for it.
    CreateFolder {
        generation: u64,
        backend: Arc<dyn Backend>,
        rel: PathBuf,
        display_name: String,
    },
    /// `backend.rename(old_rel, new_rel)`. Caller guarantees `new_rel`
    /// doesn't already exist (checked in `App::commit_tree_edit`).
    Rename {
        generation: u64,
        backend: Arc<dyn Backend>,
        old_rel: PathBuf,
        new_rel: PathBuf,
        old_name: String,
        new_name: String,
    },
    /// Move each path to the system Trash. Uses `backend.trash`, which
    /// is cross-platform on LocalBackend (via the `trash` crate) and
    /// falls through to `gio trash` / permanent delete on RemoteBackend.
    TrashPaths {
        generation: u64,
        backend: Arc<dyn Backend>,
        rels: Vec<PathBuf>,
        first_name: String,
    },
    /// Permanent delete via `backend.hard_delete`. Reached via
    /// Shift+Delete after the confirm dialog. Files and directories
    /// both supported.
    HardDeletePaths {
        generation: u64,
        backend: Arc<dyn Backend>,
        rels: Vec<PathBuf>,
        first_name: String,
    },
    /// Cut + Paste: rename each source into `dest_dir` per the
    /// per-item `Resolution`. Conflicts have already been resolved on
    /// the App side, so the worker only consumes the decision list.
    /// Items with `Resolution::Skip` / `Resolution::Cancel` are noops.
    /// Distinct from `CopyFiles` because that task auto-renames on
    /// any collision (place-mode / OS drop semantics) which would
    /// silently override the user's pick here.
    MovePaths {
        generation: u64,
        backend: Arc<dyn Backend>,
        items: Vec<PasteItem>,
        /// Workdir-relative destination directory.
        dest_dir: PathBuf,
    },
    /// Copy + Paste / Duplicate / Alt-drag-copy. Same shape as
    /// `MovePaths` but uses `copy_file` / `copy_dir_recursive` instead
    /// of `rename`. Source rows stay put.
    CopyPaths {
        generation: u64,
        backend: Arc<dyn Backend>,
        items: Vec<PasteItem>,
        dest_dir: PathBuf,
    },
    /// Global find-and-replace. Worker reads each file in `items`,
    /// re-runs the search matcher (smart-case literal via
    /// `grep_regex::RegexMatcherBuilder` to match what `search_content`
    /// did), rewrites lines the user opted in to, and writes back via
    /// `Backend::write_file` (atomic temp+rename). Results stream as
    /// `WorkerResult::ReplaceProgress` per file then a final
    /// `ReplaceDone`.
    ReplaceInFiles {
        generation: u64,
        backend: Arc<dyn Backend>,
        /// Search pattern — must match what the UI displayed so the
        /// matcher finds the same byte ranges.
        query: String,
        /// Replacement string. Empty allowed (deletes the matched span).
        replace_text: String,
        items: Vec<ReplaceItem>,
    },
}

struct QuickOpenFilterTask {
    generation: u64,
    index: Arc<[crate::features::quick_open::Candidate]>,
    query: String,
    mru: VecDeque<PathBuf>,
}

struct PreviewEnrichmentTask {
    generation: u64,
    path: String,
    input: PreviewEnrichmentInput,
    dark: bool,
}

struct NavPreviewTask {
    generation: u64,
    backend: Arc<dyn Backend>,
    path: PathBuf,
    dark: bool,
}

enum PreviewEnrichmentInput {
    Text {
        bytes_on_disk: u64,
        lines: Vec<String>,
        source: Option<Arc<str>>,
    },
    Markdown {
        source: String,
    },
}

/// One source's contribution to a `MovePaths` / `CopyPaths` batch.
#[derive(Debug, Clone)]
pub struct PasteItem {
    /// Workdir-relative source path.
    pub source: PathBuf,
    /// `true` for directories — the worker picks `copy_dir_recursive`
    /// or `rename` semantics accordingly. Carried instead of probed
    /// because remote backends can't cheaply stat from the worker
    /// thread, and the App already knows from `TreeEntry.is_dir`.
    pub is_dir: bool,
    /// Decision recorded by the conflict prompt (or auto-decided as
    /// Replace when the destination didn't exist).
    pub resolution: Resolution,
}

enum GitTask {
    RefreshStatus {
        generation: u64,
        backend: Arc<dyn Backend>,
    },
    Mutate {
        generation: u64,
        backend: Arc<dyn Backend>,
        mutation: GitMutation,
    },
    Commit {
        generation: u64,
        backend: Arc<dyn Backend>,
        message: String,
    },
    Push {
        generation: u64,
        backend: Arc<dyn Backend>,
        force: bool,
    },
}

enum GitDiffTask {
    Load {
        generation: u64,
        backend: Arc<dyn Backend>,
        path: String,
        staged: bool,
        context_lines: u32,
        /// Picks the syntect theme (dark vs light) — same role as
        /// `LoadCommitFileDiff.dark` / `LoadPreview.dark`.
        dark: bool,
    },
}

enum GitStatusStatsTask {
    Refresh {
        generation: u64,
        backend: Arc<dyn Backend>,
    },
}

enum GlobalSearchTask {
    Run {
        generation: u64,
        cancel: Arc<AtomicBool>,
        backend: Arc<dyn Backend>,
        query: String,
    },
}

/// LSP-refine task serviced by the dedicated LSP worker.
/// Fire-and-forget: the answer lands in `nav_refine_cache` on the main
/// thread when (or if) it arrives. `identifier` carries the position
/// cache key (`refine_key`), not a symbol name.
enum LspTask {
    RefineDefinition {
        generation: u64,
        /// Refine-cache epoch at dispatch time — round-tripped back in
        /// `WorkerResult::LspRefineDone` so the main thread can drop a
        /// response that raced a cache-clear. See that variant's doc.
        epoch: u64,
        lang: reef_core::nav::NavLang,
        identifier: String,
        workspace_root: PathBuf,
        path: PathBuf,
        source: Arc<[u8]>,
        line: u32,
        character: u32,
    },
}

enum GraphRefreshTask {
    RefreshGraph {
        generation: u64,
        backend: Arc<dyn Backend>,
        limit: usize,
        scope: GraphScope,
    },
}

enum GraphContentTask {
    CommitDetail {
        generation: u64,
        backend: Arc<dyn Backend>,
        oid: String,
    },
    CommitFileDiff {
        generation: u64,
        backend: Arc<dyn Backend>,
        oid: String,
        path: String,
        context_lines: u32,
        /// Picks the syntect theme (dark vs light) so highlighted tokens
        /// read correctly against the active UI theme — same as `load_preview`.
        dark: bool,
    },
    CommitRangeDetail {
        generation: u64,
        backend: Arc<dyn Backend>,
        oldest_oid: String,
        newest_oid: String,
    },
    RangeFileDiff {
        generation: u64,
        backend: Arc<dyn Backend>,
        oldest_oid: String,
        newest_oid: String,
        path: String,
        context_lines: u32,
        dark: bool,
    },
}

pub struct TaskCoordinator {
    file_tree_rebuild_tx: mpsc::Sender<FileTreeRebuildTask>,
    file_tree_subtree_tx: mpsc::Sender<FileTreeSubtreeTask>,
    files_tx: mpsc::Sender<FilesTask>,
    quick_open_filter_tx: mpsc::Sender<QuickOpenFilterTask>,
    quick_open_filter_generation: Arc<AtomicU64>,
    /// Dedicated channel for `FilesTask::LoadPreview`. Keeping previews
    /// on their own worker thread means a slow directory rebuild or an
    /// in-flight copy never queues in front of the image the user just
    /// clicked. Both threads can hit the `LocalBackend` preview cache
    /// safely via the internal `Mutex`.
    preview_tx: mpsc::Sender<FilesTask>,
    db_cell_tx: mpsc::Sender<DbCellTask>,
    preview_enrichment_tx: mpsc::Sender<PreviewEnrichmentTask>,
    nav_preview_tx: mpsc::Sender<NavPreviewTask>,
    git_tx: mpsc::Sender<GitTask>,
    git_diff_tx: mpsc::Sender<GitDiffTask>,
    git_status_stats_tx: mpsc::Sender<GitStatusStatsTask>,
    graph_refresh_tx: mpsc::Sender<GraphRefreshTask>,
    graph_content_tx: mpsc::Sender<GraphContentTask>,
    global_search_tx: mpsc::Sender<GlobalSearchTask>,
    result_tx: WorkerResultSender,
    /// LSP worker. Holds the per-language `LspClient`s +
    /// spawn-failure backoff.
    lsp_tx: mpsc::Sender<LspTask>,
    result_rx: mpsc::Receiver<WorkerResult>,
    worker_wake_rx: mpsc::Receiver<()>,
}

#[derive(Clone)]
struct WorkerResultSender {
    result_tx: mpsc::Sender<WorkerResult>,
    worker_wake_tx: mpsc::Sender<()>,
}

impl WorkerResultSender {
    fn send(&self, result: WorkerResult) -> Result<(), ()> {
        self.result_tx.send(result).map_err(|_| ())?;
        let _ = self.worker_wake_tx.try_send(());
        Ok(())
    }
}

impl TaskCoordinator {
    pub fn new() -> Self {
        let (result_tx, result_rx) = mpsc::unbounded();
        let (worker_wake_tx, worker_wake_rx) = mpsc::bounded(1);
        let result_tx = WorkerResultSender {
            result_tx,
            worker_wake_tx,
        };
        let quick_open_filter_generation = Arc::new(AtomicU64::new(0));
        Self {
            file_tree_rebuild_tx: spawn_file_tree_rebuild_worker(result_tx.clone()),
            file_tree_subtree_tx: spawn_file_tree_subtree_workers(result_tx.clone()),
            files_tx: spawn_files_worker(result_tx.clone()),
            quick_open_filter_tx: spawn_quick_open_filter_worker(
                result_tx.clone(),
                Arc::clone(&quick_open_filter_generation),
            ),
            quick_open_filter_generation,
            preview_tx: spawn_preview_worker(result_tx.clone()),
            db_cell_tx: spawn_db_cell_worker(result_tx.clone()),
            preview_enrichment_tx: spawn_preview_enrichment_worker(result_tx.clone()),
            nav_preview_tx: spawn_nav_preview_worker(result_tx.clone()),
            git_tx: spawn_git_worker(result_tx.clone()),
            git_diff_tx: spawn_git_diff_worker(result_tx.clone()),
            git_status_stats_tx: spawn_git_status_stats_worker(result_tx.clone()),
            graph_refresh_tx: spawn_graph_refresh_worker(result_tx.clone()),
            graph_content_tx: spawn_graph_content_worker(result_tx.clone()),
            global_search_tx: spawn_global_search_worker(result_tx.clone()),
            lsp_tx: spawn_lsp_worker(result_tx.clone()),
            result_tx,
            result_rx,
            worker_wake_rx,
        }
    }

    pub fn build_nav_workspace(&self, generation: u64, backend: Arc<dyn Backend>) {
        let result_tx = self.result_tx.clone();
        let _ = thread::Builder::new()
            .name("reef-nav-index".into())
            .spawn(move || {
                let result = build_nav_workspace_index(backend.as_ref());
                let _ = result_tx.send(WorkerResult::NavWorkspaceBuilt { generation, result });
            });
    }

    /// Dispatch an LSP refine. Fire-and-forget; the answer (or
    /// a state-change update on failure) arrives as
    /// `WorkerResult::LspRefineDone` / `LspStateChange`.
    #[allow(clippy::too_many_arguments)]
    pub fn lsp_refine_definition(
        &self,
        generation: u64,
        epoch: u64,
        lang: reef_core::nav::NavLang,
        identifier: String,
        workspace_root: PathBuf,
        path: PathBuf,
        source: Arc<[u8]>,
        line: u32,
        character: u32,
    ) {
        let _ = self.lsp_tx.send(LspTask::RefineDefinition {
            generation,
            epoch,
            lang,
            identifier,
            workspace_root,
            path,
            source,
            line,
            character,
        });
    }

    pub fn try_recv(&self) -> Result<WorkerResult, mpsc::TryRecvError> {
        self.result_rx.try_recv()
    }

    pub fn worker_wake_receiver(&self) -> mpsc::Receiver<()> {
        self.worker_wake_rx.clone()
    }

    pub fn rebuild_tree(
        &self,
        identity: FileTreeRebuildIdentity,
        backend: Arc<dyn Backend>,
        expanded: Vec<PathBuf>,
        git_statuses: HashMap<String, char>,
        selected_path: Option<PathBuf>,
        fallback_selected: usize,
    ) {
        let _ = self.file_tree_rebuild_tx.send(FileTreeRebuildTask {
            identity,
            backend,
            expanded,
            git_statuses,
            selected_path,
            fallback_selected,
        });
    }

    pub fn load_tree_subtree(
        &self,
        request_id: u64,
        backend: Arc<dyn Backend>,
        parent_path: PathBuf,
        parent_depth: usize,
        expanded: Vec<PathBuf>,
    ) {
        let _ = self.file_tree_subtree_tx.send(FileTreeSubtreeTask {
            request_id,
            backend,
            parent_path,
            parent_depth,
            expanded,
        });
    }

    pub fn load_preview(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        rel_path: PathBuf,
        wants_decoded_image: bool,
    ) {
        // Route to the dedicated preview worker so an in-flight tree
        // rebuild or copy doesn't sit ahead of an image the user just
        // clicked on.
        let _ = self.preview_tx.send(FilesTask::LoadPreview {
            generation,
            backend,
            rel_path,
            wants_decoded_image,
        });
    }

    pub fn load_nav_preview(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        path: PathBuf,
        dark: bool,
    ) {
        let _ = self.nav_preview_tx.send(NavPreviewTask {
            generation,
            backend,
            path,
            dark,
        });
    }

    pub fn load_db_page(&self, generation: u64, backend: Arc<dyn Backend>, request: DbPageRequest) {
        let _ = self.preview_tx.send(FilesTask::LoadDbPage {
            generation,
            backend,
            request,
        });
    }

    pub fn load_db_detail(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        path: PathBuf,
        key: reef_sqlite_preview::DbObjectKey,
    ) {
        let _ = self.preview_tx.send(FilesTask::LoadDbDetail {
            generation,
            backend,
            path,
            key,
        });
    }

    pub fn load_db_cell(&self, generation: u64, backend: Arc<dyn Backend>, request: DbCellRequest) {
        let _ = self.db_cell_tx.send(DbCellTask {
            generation,
            backend,
            request,
        });
    }

    pub fn build_quick_open_index(&self, generation: u64, backend: Arc<dyn Backend>) {
        let _ = self.files_tx.send(FilesTask::BuildQuickOpenIndex {
            generation,
            backend,
        });
    }

    pub fn filter_quick_open(
        &self,
        generation: u64,
        index: Arc<[crate::features::quick_open::Candidate]>,
        query: String,
        mru: VecDeque<PathBuf>,
    ) {
        self.quick_open_filter_generation
            .store(generation, Ordering::Release);
        let _ = self.quick_open_filter_tx.send(QuickOpenFilterTask {
            generation,
            index,
            query,
            mru,
        });
    }

    /// Warm the preview cache for a file the user hasn't selected yet
    /// but probably will. Result is dropped by the worker; the cache
    /// side effect is the point.
    pub fn prefetch_preview(
        &self,
        backend: Arc<dyn Backend>,
        rel_path: PathBuf,
        wants_decoded_image: bool,
    ) {
        let _ = self.preview_tx.send(FilesTask::PrefetchPreview {
            backend,
            rel_path,
            wants_decoded_image,
        });
    }

    pub fn enrich_preview(&self, generation: u64, content: &PreviewContent, dark: bool) -> bool {
        let Some(task) = preview_enrichment_task(generation, content, dark) else {
            return false;
        };
        self.preview_enrichment_tx.send(task).is_ok()
    }

    pub fn copy_files(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
    ) {
        let _ = self.files_tx.send(FilesTask::CopyFiles {
            generation,
            backend,
            sources,
            dest_dir,
        });
    }

    pub fn plan_tree_edit(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        mode: crate::features::tree_edit::TreeEditMode,
        parent_rel: PathBuf,
        rename_source: Option<PathBuf>,
        name: String,
    ) {
        let _ = self.files_tx.send(FilesTask::PlanTreeEdit {
            generation,
            backend,
            mode,
            parent_rel,
            rename_source,
            name,
        });
    }

    pub fn plan_paste(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        op: reef_core::file_ops::ClipMode,
        dest_rel: PathBuf,
        sources: Vec<PathBuf>,
    ) {
        let _ = self.files_tx.send(FilesTask::PlanPaste {
            generation,
            backend,
            op,
            dest_rel,
            sources,
        });
    }

    pub fn create_file(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        rel: PathBuf,
        display_name: String,
    ) {
        let _ = self.files_tx.send(FilesTask::CreateFile {
            generation,
            backend,
            rel,
            display_name,
        });
    }

    pub fn create_folder(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        rel: PathBuf,
        display_name: String,
    ) {
        let _ = self.files_tx.send(FilesTask::CreateFolder {
            generation,
            backend,
            rel,
            display_name,
        });
    }

    pub fn rename_path(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        old_rel: PathBuf,
        new_rel: PathBuf,
        old_name: String,
        new_name: String,
    ) {
        let _ = self.files_tx.send(FilesTask::Rename {
            generation,
            backend,
            old_rel,
            new_rel,
            old_name,
            new_name,
        });
    }

    pub fn trash_paths(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        rels: Vec<PathBuf>,
        first_name: String,
    ) {
        let _ = self.files_tx.send(FilesTask::TrashPaths {
            generation,
            backend,
            rels,
            first_name,
        });
    }

    pub fn hard_delete_paths(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        rels: Vec<PathBuf>,
        first_name: String,
    ) {
        let _ = self.files_tx.send(FilesTask::HardDeletePaths {
            generation,
            backend,
            rels,
            first_name,
        });
    }

    pub fn move_paths(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        items: Vec<PasteItem>,
        dest_dir: PathBuf,
    ) {
        let _ = self.files_tx.send(FilesTask::MovePaths {
            generation,
            backend,
            items,
            dest_dir,
        });
    }

    pub fn copy_paths(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        items: Vec<PasteItem>,
        dest_dir: PathBuf,
    ) {
        let _ = self.files_tx.send(FilesTask::CopyPaths {
            generation,
            backend,
            items,
            dest_dir,
        });
    }

    /// Dispatch a global replace batch to the files worker. The caller
    /// owns generation bookkeeping — see `App::commit_replace_in_files`
    /// for the canonical pattern: `replace_load.begin()` produces the
    /// generation, `complete_ok` consumes it, and `ReefApp::step` drops
    /// stale results whose `generation` no longer matches.
    pub fn replace_in_files(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        query: String,
        replace_text: String,
        items: Vec<ReplaceItem>,
    ) {
        let _ = self.files_tx.send(FilesTask::ReplaceInFiles {
            generation,
            backend,
            query,
            replace_text,
            items,
        });
    }

    pub fn refresh_status(&self, generation: u64, backend: Arc<dyn Backend>) {
        let _ = self.git_tx.send(GitTask::RefreshStatus {
            generation,
            backend,
        });
    }

    pub fn refresh_status_stats(&self, generation: u64, backend: Arc<dyn Backend>) {
        let _ = self.git_status_stats_tx.send(GitStatusStatsTask::Refresh {
            generation,
            backend,
        });
    }

    pub fn load_diff(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        path: String,
        staged: bool,
        context_lines: u32,
        dark: bool,
    ) {
        let _ = self.git_diff_tx.send(GitDiffTask::Load {
            generation,
            backend,
            path,
            staged,
            context_lines,
            dark,
        });
    }

    pub fn mutate_git(&self, generation: u64, backend: Arc<dyn Backend>, mutation: GitMutation) {
        let _ = self.git_tx.send(GitTask::Mutate {
            generation,
            backend,
            mutation,
        });
    }

    pub fn commit(&self, generation: u64, backend: Arc<dyn Backend>, message: String) {
        let _ = self.git_tx.send(GitTask::Commit {
            generation,
            backend,
            message,
        });
    }

    pub fn push(&self, generation: u64, backend: Arc<dyn Backend>, force: bool) {
        let _ = self.git_tx.send(GitTask::Push {
            generation,
            backend,
            force,
        });
    }

    pub fn refresh_graph(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        limit: usize,
        scope: GraphScope,
    ) {
        let _ = self.graph_refresh_tx.send(GraphRefreshTask::RefreshGraph {
            generation,
            backend,
            limit,
            scope,
        });
    }

    pub fn load_commit_detail(&self, generation: u64, backend: Arc<dyn Backend>, oid: String) {
        let _ = self.graph_content_tx.send(GraphContentTask::CommitDetail {
            generation,
            backend,
            oid,
        });
    }

    pub fn load_commit_file_diff(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        oid: String,
        path: String,
        context_lines: u32,
        dark: bool,
    ) {
        let _ = self
            .graph_content_tx
            .send(GraphContentTask::CommitFileDiff {
                generation,
                backend,
                oid,
                path,
                context_lines,
                dark,
            });
    }

    pub fn load_commit_range_detail(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        oldest_oid: String,
        newest_oid: String,
    ) {
        let _ = self
            .graph_content_tx
            .send(GraphContentTask::CommitRangeDetail {
                generation,
                backend,
                oldest_oid,
                newest_oid,
            });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn load_range_file_diff(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        oldest_oid: String,
        newest_oid: String,
        path: String,
        context_lines: u32,
        dark: bool,
    ) {
        let _ = self.graph_content_tx.send(GraphContentTask::RangeFileDiff {
            generation,
            backend,
            oldest_oid,
            newest_oid,
            path,
            context_lines,
            dark,
        });
    }

    /// Kick off a workdir-wide content search. The worker walks `root`
    /// (respecting `.gitignore` via the same `ignore` crate path the
    /// quick-open index uses), runs `grep-searcher` with a smart-case
    /// literal `RegexMatcher`, and streams hits back as
    /// `WorkerResult::GlobalSearchChunk` followed by a final
    /// `GlobalSearchDone`. Flipping `cancel` to `true` asks the worker to
    /// bail on its next file-boundary poll.
    pub fn search_all(
        &self,
        generation: u64,
        cancel: Arc<AtomicBool>,
        backend: Arc<dyn Backend>,
        query: String,
    ) {
        let _ = self.global_search_tx.send(GlobalSearchTask::Run {
            generation,
            cancel,
            backend,
            query,
        });
    }
}

fn spawn_file_tree_rebuild_worker(
    result_tx: WorkerResultSender,
) -> mpsc::Sender<FileTreeRebuildTask> {
    let (tx, rx) = mpsc::unbounded::<FileTreeRebuildTask>();
    let _ = thread::Builder::new()
        .name("reef-file-tree-rebuild".into())
        .spawn(move || {
            while let Ok(task) = recv_latest_file_tree_rebuild_task(&rx) {
                let result = build_file_tree_payload(
                    task.backend.as_ref(),
                    task.expanded,
                    task.git_statuses,
                    task.selected_path,
                    task.fallback_selected,
                );
                let _ = result_tx.send(WorkerResult::FileTree {
                    generation: task.identity.generation,
                    tree_revision: task.identity.tree_revision,
                    result,
                });
            }
        });
    tx
}

fn recv_latest_file_tree_rebuild_task(
    rx: &mpsc::Receiver<FileTreeRebuildTask>,
) -> Result<FileTreeRebuildTask, mpsc::RecvError> {
    let mut latest = rx.recv()?;
    while let Ok(task) = rx.try_recv() {
        latest = task;
    }
    Ok(latest)
}

fn spawn_file_tree_subtree_workers(
    result_tx: WorkerResultSender,
) -> mpsc::Sender<FileTreeSubtreeTask> {
    const WORKER_COUNT: usize = 2;
    let (tx, rx) = mpsc::unbounded::<FileTreeSubtreeTask>();
    for index in 0..WORKER_COUNT {
        let rx = rx.clone();
        let result_tx = result_tx.clone();
        let _ = thread::Builder::new()
            .name(format!("reef-file-tree-subtree-{index}"))
            .spawn(move || {
                while let Ok(task) = rx.recv() {
                    let parent_path = task.parent_path.clone();
                    let result = build_file_tree_subtree_payload(
                        task.backend.as_ref(),
                        task.parent_path,
                        task.parent_depth,
                        task.expanded,
                    );
                    let _ = result_tx.send(WorkerResult::FileTreeSubtree {
                        request_id: task.request_id,
                        parent_path,
                        result,
                    });
                }
            });
    }
    tx
}

fn spawn_files_worker(result_tx: WorkerResultSender) -> mpsc::Sender<FilesTask> {
    let (tx, rx) = mpsc::unbounded::<FilesTask>();
    let _ = thread::Builder::new()
        .name("reef-files-worker".into())
        .spawn(move || {
            while let Ok(task) = rx.recv() {
                match task {
                    FilesTask::BuildQuickOpenIndex {
                        generation,
                        backend,
                    } => {
                        let result = backend
                            .walk_repo_paths(&WalkOpts::default())
                            .map(|resp| reef_core::quick_open::build_candidates(resp.paths))
                            .map_err(|e| e.to_string());
                        let _ = result_tx.send(WorkerResult::QuickOpenIndex { generation, result });
                    }
                    FilesTask::CopyFiles {
                        generation,
                        backend,
                        sources,
                        dest_dir,
                    } => {
                        let result = reef_io::copy_local_sources_to_backend(
                            backend.as_ref(),
                            &sources,
                            &dest_dir,
                        );
                        let _ = result_tx.send(WorkerResult::FileCopy { generation, result });
                    }
                    FilesTask::PlanTreeEdit {
                        generation,
                        backend,
                        mode,
                        parent_rel,
                        rename_source,
                        name,
                    } => {
                        let result =
                            plan_tree_edit(backend.as_ref(), mode, parent_rel, rename_source, name);
                        let _ = result_tx.send(WorkerResult::TreeEditPlan { generation, result });
                    }
                    FilesTask::PlanPaste {
                        generation,
                        backend,
                        op,
                        dest_rel,
                        sources,
                    } => {
                        let result = plan_paste(backend.as_ref(), op, dest_rel, sources);
                        let _ = result_tx.send(WorkerResult::PastePlan { generation, result });
                    }
                    FilesTask::CreateFile {
                        generation,
                        backend,
                        rel,
                        display_name,
                    } => {
                        let kind = FsMutationKind::CreatedFile {
                            name: display_name.clone(),
                        };
                        let result = backend
                            .create_file(&rel)
                            .map_err(|e| format!("create {display_name:?}: {e}"));
                        let _ = result_tx.send(WorkerResult::FsMutation {
                            generation,
                            kind,
                            result,
                        });
                    }
                    FilesTask::CreateFolder {
                        generation,
                        backend,
                        rel,
                        display_name,
                    } => {
                        let kind = FsMutationKind::CreatedFolder {
                            name: display_name.clone(),
                        };
                        let result = backend
                            .create_dir_all(&rel)
                            .map_err(|e| format!("mkdir {display_name:?}: {e}"));
                        let _ = result_tx.send(WorkerResult::FsMutation {
                            generation,
                            kind,
                            result,
                        });
                    }
                    FilesTask::Rename {
                        generation,
                        backend,
                        old_rel,
                        new_rel,
                        old_name,
                        new_name,
                    } => {
                        let kind = FsMutationKind::Renamed {
                            old_name: old_name.clone(),
                            new_name: new_name.clone(),
                        };
                        let result = backend
                            .rename(&old_rel, &new_rel)
                            .map_err(|e| format!("rename {old_name:?} → {new_name:?}: {e}"));
                        let _ = result_tx.send(WorkerResult::FsMutation {
                            generation,
                            kind,
                            result,
                        });
                    }
                    FilesTask::TrashPaths {
                        generation,
                        backend,
                        rels,
                        first_name,
                    } => {
                        let kind = FsMutationKind::Trashed {
                            name: first_name.clone(),
                        };
                        let result = backend
                            .trash(&rels)
                            .map(|_| ())
                            .map_err(|e| format!("trash {first_name:?}: {e}"));
                        let _ = result_tx.send(WorkerResult::FsMutation {
                            generation,
                            kind,
                            result,
                        });
                    }
                    FilesTask::HardDeletePaths {
                        generation,
                        backend,
                        rels,
                        first_name,
                    } => {
                        let kind = FsMutationKind::HardDeleted {
                            name: first_name.clone(),
                        };
                        let result = backend
                            .hard_delete(&rels)
                            .map_err(|e| format!("delete {first_name:?}: {e}"));
                        let _ = result_tx.send(WorkerResult::FsMutation {
                            generation,
                            kind,
                            result,
                        });
                    }
                    FilesTask::MovePaths {
                        generation,
                        backend,
                        items,
                        dest_dir,
                    } => {
                        let (kind, result) =
                            run_paste_batch(backend.as_ref(), &items, &dest_dir, false);
                        let _ = result_tx.send(WorkerResult::FsMutation {
                            generation,
                            kind,
                            result,
                        });
                    }
                    FilesTask::CopyPaths {
                        generation,
                        backend,
                        items,
                        dest_dir,
                    } => {
                        let (kind, result) =
                            run_paste_batch(backend.as_ref(), &items, &dest_dir, true);
                        let _ = result_tx.send(WorkerResult::FsMutation {
                            generation,
                            kind,
                            result,
                        });
                    }
                    FilesTask::ReplaceInFiles {
                        generation,
                        backend,
                        query,
                        replace_text,
                        items,
                    } => {
                        run_replace_in_files(
                            generation,
                            backend.as_ref(),
                            &query,
                            &replace_text,
                            &items,
                            &result_tx,
                        );
                    }
                    // These routes belong to dedicated workers; these arms
                    // only satisfy exhaustiveness.
                    FilesTask::LoadPreview { .. }
                    | FilesTask::LoadDbPage { .. }
                    | FilesTask::LoadDbDetail { .. }
                    | FilesTask::PrefetchPreview { .. } => {}
                }
            }
        });
    tx
}

fn spawn_quick_open_filter_worker(
    result_tx: WorkerResultSender,
    latest_generation: Arc<AtomicU64>,
) -> mpsc::Sender<QuickOpenFilterTask> {
    let (tx, rx) = mpsc::unbounded::<QuickOpenFilterTask>();
    let _ = thread::Builder::new()
        .name("reef-quick-open-filter".into())
        .spawn(move || {
            while let Ok(task) = recv_latest(&rx) {
                let matches = reef_core::quick_open::filter_candidates_interruptible(
                    &task.index,
                    &task.query,
                    &task.mru,
                    || latest_generation.load(Ordering::Acquire) != task.generation,
                );
                if let Some(matches) = matches {
                    let _ = result_tx.send(WorkerResult::QuickOpenFilter {
                        generation: task.generation,
                        matches,
                    });
                }
            }
        });
    tx
}

// ─── FS mutation helpers ─────────────────────────────────────────────────────
//
// Production workers route through `Backend` so local and remote
// implementations stay equivalent. These direct-fs helpers remain test-only
// regression guards for the original `std::fs::*` semantics.

#[cfg(test)]
fn basename_str(path: &Path) -> String {
    // Filenames land in toast text and FsMutationKind display strings.
    // macOS allows control chars (`\n`, `\t`, bell, …) in filenames,
    // which would otherwise break single-line status-bar rendering or
    // mis-align the toast. Replace them with `?` — the sanitised
    // display form only; the actual filesystem path is never touched.
    let raw = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(String::from)
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    raw.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

#[cfg(test)]
fn run_create_file(path: &Path) -> (FsMutationKind, Result<(), String>) {
    let name = basename_str(path);
    let kind = FsMutationKind::CreatedFile { name: name.clone() };
    // `OpenOptions::create_new` refuses to overwrite an existing file — a
    // race with fs-watcher / an external editor creating the file between
    // the UI-level collision check and this syscall surfaces as a clear
    // error instead of silently clobbering.
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(|_| ())
        .map_err(|e| format!("create {name:?}: {e}"));
    (kind, result)
}

#[cfg(test)]
fn run_create_folder(path: &Path) -> (FsMutationKind, Result<(), String>) {
    let name = basename_str(path);
    let kind = FsMutationKind::CreatedFolder { name: name.clone() };
    // `create_dir_all` treats an existing directory as success; fine here
    // because the UI has already checked for name collisions with files.
    let result = std::fs::create_dir_all(path).map_err(|e| format!("mkdir {name:?}: {e}"));
    (kind, result)
}

#[cfg(test)]
fn run_rename(old: &Path, new: &Path) -> (FsMutationKind, Result<(), String>) {
    let old_name = basename_str(old);
    let new_name = basename_str(new);
    let kind = FsMutationKind::Renamed {
        old_name: old_name.clone(),
        new_name: new_name.clone(),
    };
    let result =
        std::fs::rename(old, new).map_err(|e| format!("rename {old_name:?} → {new_name:?}: {e}"));
    (kind, result)
}

#[cfg(test)]
#[allow(dead_code)]
fn run_trash(paths: &[PathBuf]) -> (FsMutationKind, Result<(), String>) {
    // The kind string reports the first path's basename to keep the toast
    // short for batch deletes.
    let name = paths.first().map(|p| basename_str(p)).unwrap_or_default();
    let kind = FsMutationKind::Trashed { name: name.clone() };
    let result = trash::delete_all(paths).map_err(|e| format!("trash {name:?}: {e}"));
    (kind, result)
}

#[cfg(test)]
fn run_hard_delete(paths: &[PathBuf]) -> (FsMutationKind, Result<(), String>) {
    let name = paths.first().map(|p| basename_str(p)).unwrap_or_default();
    let kind = FsMutationKind::HardDeleted { name: name.clone() };
    for p in paths {
        let res = if p.is_dir() {
            std::fs::remove_dir_all(p)
        } else {
            std::fs::remove_file(p)
        };
        if let Err(e) = res {
            return (kind, Err(format!("delete {:?}: {e}", basename_str(p))));
        }
    }
    (kind, Ok(()))
}

fn existing_basenames(
    backend: &dyn Backend,
    dest_rel: &Path,
) -> Result<HashSet<String>, BackendError> {
    backend
        .list_dir(dest_rel)
        .map(|entries| entries.into_iter().map(|entry| entry.name).collect())
}

fn plan_tree_edit(
    backend: &dyn Backend,
    mode: crate::features::tree_edit::TreeEditMode,
    parent_rel: PathBuf,
    rename_source: Option<PathBuf>,
    name: String,
) -> Result<TreeEditPlan, TreeEditPlanError> {
    let target_rel = parent_rel.join(&name);
    let mutation = match mode {
        crate::features::tree_edit::TreeEditMode::NewFile => TreeEditMutation::CreateFile {
            rel: target_rel.clone(),
            display_name: name.clone(),
        },
        crate::features::tree_edit::TreeEditMode::NewFolder => TreeEditMutation::CreateFolder {
            rel: target_rel.clone(),
            display_name: name.clone(),
        },
        crate::features::tree_edit::TreeEditMode::Rename => {
            let old_rel = rename_source.ok_or(TreeEditPlanError::Validation {
                error: reef_core::file_ops::FileNameError::InvalidName,
            })?;
            let old_name = old_rel
                .file_name()
                .and_then(|name| name.to_str())
                .map(String::from)
                .unwrap_or_else(|| old_rel.to_string_lossy().to_string());
            TreeEditMutation::Rename {
                old_rel,
                new_rel: target_rel.clone(),
                old_name,
                new_name: name.clone(),
            }
        }
    };

    let existing =
        existing_basenames(backend, &parent_rel).map_err(|e| TreeEditPlanError::Backend {
            mutation: mutation.clone(),
            error: e.to_string(),
        })?;
    if existing.contains(&name) {
        return Err(TreeEditPlanError::Validation {
            error: reef_core::file_ops::FileNameError::NameAlreadyExists(name),
        });
    }

    Ok(TreeEditPlan {
        mutation,
        select_on_done: Some(target_rel),
    })
}

fn plan_paste(
    backend: &dyn Backend,
    op: reef_core::file_ops::ClipMode,
    dest_rel: PathBuf,
    sources: Vec<PathBuf>,
) -> Result<PastePlanPayload, PastePlanError> {
    let existing = existing_basenames(backend, &dest_rel).map_err(|e| PastePlanError {
        op,
        error: format!("read destination {:?}: {e}", dest_rel),
    })?;
    let cls = reef_core::file_ops::classify_paste(op, &dest_rel, &sources, &existing);
    let used_names =
        reef_core::file_ops::used_names_after_auto_decisions(&existing, &cls.auto_decisions);
    Ok(PastePlanPayload {
        op,
        dest_rel,
        auto_decisions: cls.auto_decisions,
        pending: cls.pending,
        used_names,
        self_descent_blocked: cls.self_descent_blocked,
    })
}

/// Drive a Cut/Copy paste batch — `items` is the per-source decision
/// list, with conflict resolutions baked in by the App. Each item lands
/// at `dest_dir/<basename>` (or `dest_dir/<keep-both-name>`); `Replace`
/// pre-trashes the existing destination so the user can recover via OS
/// Trash. `Skip` and `Cancel` are noops.
///
/// Fail-fast on the first error to match drag-and-drop copy semantics —
/// callers prefer one clear error over a partial-completion riddle.
/// `placed` counts items that successfully landed *before* any error,
/// so the toast can still report progress.
///
/// Remote-backend cost: this loop issues one RPC per item (plus an
/// extra `trash` RPC per `Replace`). A 50-item Replace paste over SSH
/// = ~100 round-trips; on a 200ms-RTT link that's ~10s of latency
/// dominating any actual transfer cost. Batching `trash` and
/// `rename`/`copy` would need batch backend operations. Local-backend
/// per-item cost is tiny and not worth batching.
fn run_paste_batch(
    backend: &dyn Backend,
    items: &[PasteItem],
    dest_dir: &Path,
    is_copy: bool,
) -> (FsMutationKind, Result<(), String>) {
    let mut placed: usize = 0;
    let mut first_src_name: Option<String> = None;
    let mut first_dest_name: Option<String> = None;
    let mut first_err: Option<String> = None;

    for item in items {
        let dest_basename: String = match &item.resolution {
            Resolution::Skip | Resolution::Cancel => continue,
            Resolution::KeepBoth(name) => name.clone(),
            Resolution::Replace => match item.source.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => {
                    first_err.get_or_insert_with(|| {
                        format!("invalid source filename: {:?}", item.source)
                    });
                    break;
                }
            },
        };
        let src_basename = item
            .source
            .file_name()
            .and_then(|s| s.to_str())
            .map(String::from)
            .unwrap_or_else(|| dest_basename.clone());
        let dest_rel = dest_dir.join(&dest_basename);

        // Replace: pre-trash the existing destination so the operation
        // stays undoable via OS Trash. `trash` is intentionally best-
        // effort — three failure modes we silently tolerate:
        //   1. Existing entry vanished (race with fs_watcher / external
        //      delete between conflict detection and worker dispatch)
        //      → `BackendError::NotFound`. The follow-up rename/copy
        //      still succeeds at the now-empty slot.
        //   2. No system trash available (Linux without `gio` or
        //      `trash-cli`, sandboxed remote agent) → `Backend::trash`
        //      already returns `TrashOutcome { used_trash: false }` on
        //      success, but the err-path here lumps "permanent delete
        //      done" with "couldn't trash". The follow-up copy/rename
        //      will overwrite the dest unconditionally either way.
        //   3. Permission denied → user gets the overwrite without the
        //      Trash safety net. Threading the trash result into
        //      `FsMutationKind::Moved/CopiedTo` would let the toast warn
        //      "overwrote without trash".
        if matches!(item.resolution, Resolution::Replace) {
            let _ = backend.trash(std::slice::from_ref(&dest_rel));
        }

        let op_result: Result<(), String> = if is_copy {
            if item.is_dir {
                backend
                    .copy_dir_recursive(&item.source, &dest_rel)
                    .map_err(|e| format!("copy {src_basename:?} → {dest_basename:?}: {e}"))
            } else {
                backend
                    .copy_file(&item.source, &dest_rel)
                    .map_err(|e| format!("copy {src_basename:?} → {dest_basename:?}: {e}"))
            }
        } else {
            backend
                .rename(&item.source, &dest_rel)
                .map_err(|e| format!("move {src_basename:?} → {dest_basename:?}: {e}"))
        };

        match op_result {
            Ok(()) => {
                if first_src_name.is_none() {
                    first_src_name = Some(src_basename);
                    first_dest_name = Some(dest_basename);
                }
                placed += 1;
            }
            Err(e) => {
                first_err.get_or_insert(e);
                break;
            }
        }
    }

    let kind = if placed == 1 {
        if is_copy {
            FsMutationKind::CopiedTo {
                name: first_dest_name.clone().unwrap_or_default(),
            }
        } else {
            FsMutationKind::Moved {
                old_name: first_src_name.clone().unwrap_or_default(),
                new_name: first_dest_name.clone().unwrap_or_default(),
            }
        }
    } else if is_copy {
        FsMutationKind::CopiedMulti { count: placed }
    } else {
        FsMutationKind::MovedMulti { count: placed }
    };

    let result = match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    };
    (kind, result)
}

/// Dedicated worker thread for preview-adjacent file work. Keeping
/// previews on their own channel means slow tree rebuilds or copies
/// cannot queue in front of the file the user just selected. The worker
/// also owns SQLite preview paging/detail tasks, so fresh `LoadPreview`
/// requests are allowed to jump ahead of queued paging work.
/// Run a preview decode under `catch_unwind`. A panic anywhere inside
/// the backend codepath (image crate on a malformed PNG, syntect on a
/// pathological file, sqlite reader on a corrupt DB, ...) becomes an
/// `Err(String)` instead of unwinding past the worker's `while` loop
/// and killing the thread — pre-fix, one bad file took the worker
/// down, every later `LoadPreview` queued onto a dead channel, and
/// the UI got stuck on "loading…" with no recovery short of restart.
///
/// `rel_path` is captured by reference for the error message rather
/// than moved into the closure, so the caller can reuse it after the
/// guard returns (the worker still needs it for the `WorkerResult`).
fn run_preview_with_panic_guard<F>(
    rel_path: &Path,
    work: F,
) -> Result<Option<PreviewContent>, String>
where
    F: FnOnce() -> Option<PreviewContent>,
{
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
        .map_err(|_| format!("preview decoder panicked on {}", rel_path.display()))
}

fn recv_preview_worker_task(
    rx: &mpsc::Receiver<FilesTask>,
    backlog: &mut VecDeque<FilesTask>,
) -> Result<FilesTask, mpsc::RecvError> {
    let task = match backlog.pop_front() {
        Some(task) => task,
        None => rx.recv()?,
    };
    if !matches!(
        task,
        FilesTask::LoadPreview { .. } | FilesTask::PrefetchPreview { .. }
    ) {
        if let Some(load_preview) = take_pending_load_preview(rx, backlog) {
            backlog.push_front(task);
            return Ok(load_preview);
        }
    }
    Ok(coalesce_preview_worker_task(task, rx, backlog))
}

fn take_pending_load_preview(
    rx: &mpsc::Receiver<FilesTask>,
    backlog: &mut VecDeque<FilesTask>,
) -> Option<FilesTask> {
    let mut selected = None;
    while let Ok(task) = rx.try_recv() {
        match task {
            FilesTask::LoadPreview { .. } => selected = Some(task),
            other => backlog.push_back(other),
        }
    }
    selected
}

fn coalesce_preview_worker_task(
    first: FilesTask,
    rx: &mpsc::Receiver<FilesTask>,
    backlog: &mut VecDeque<FilesTask>,
) -> FilesTask {
    if matches!(
        first,
        FilesTask::LoadDbPage { .. } | FilesTask::LoadDbDetail { .. }
    ) {
        let mut selected = first;
        while let Ok(task) = rx.try_recv() {
            match task {
                FilesTask::LoadDbPage { .. } | FilesTask::LoadDbDetail { .. } => {
                    selected = task;
                }
                other => backlog.push_back(other),
            }
        }
        return selected;
    }

    let mut selected = match first {
        FilesTask::LoadPreview { .. } | FilesTask::PrefetchPreview { .. } => first,
        other => return other,
    };

    while let Ok(task) = rx.try_recv() {
        match task {
            FilesTask::LoadPreview { .. } => {
                selected = task;
            }
            FilesTask::PrefetchPreview { .. } => {
                if matches!(selected, FilesTask::PrefetchPreview { .. }) {
                    selected = task;
                }
            }
            other => backlog.push_back(other),
        }
    }

    selected
}

fn preview_enrichment_task(
    generation: u64,
    content: &PreviewContent,
    dark: bool,
) -> Option<PreviewEnrichmentTask> {
    let input = match &content.body {
        PreviewBody::Text(text) => {
            if !reef_core::preview::text_preview_can_be_enriched(
                content.bytes_on_disk,
                text.lines.len(),
            ) {
                return None;
            }
            PreviewEnrichmentInput::Text {
                bytes_on_disk: content.bytes_on_disk,
                lines: text.lines.clone(),
                source: text.source.clone(),
            }
        }
        PreviewBody::Markdown(markdown) => {
            if !reef_core::preview::text_preview_can_be_enriched(
                content.bytes_on_disk,
                markdown.line_count(),
            ) {
                return None;
            }
            PreviewEnrichmentInput::Markdown {
                source: markdown.source.clone(),
            }
        }
        _ => return None,
    };
    Some(PreviewEnrichmentTask {
        generation,
        path: content.path.clone(),
        input,
        dark,
    })
}

fn spawn_preview_enrichment_worker(
    result_tx: WorkerResultSender,
) -> mpsc::Sender<PreviewEnrichmentTask> {
    let (tx, rx) = mpsc::unbounded::<PreviewEnrichmentTask>();
    let _ = thread::Builder::new()
        .name("reef-preview-enrichment".into())
        .spawn(move || {
            reef_core::highlight::warm_up_common_syntaxes();
            while let Ok(mut task) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    task = newer;
                }
                let enrichment =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match task.input {
                        PreviewEnrichmentInput::Text {
                            bytes_on_disk,
                            ref lines,
                            ref source,
                        } => reef_core::preview::build_text_preview_enrichment(
                            &task.path,
                            bytes_on_disk,
                            lines,
                            source.as_deref(),
                            task.dark,
                        )
                        .map(PreviewEnrichment::Text),
                        PreviewEnrichmentInput::Markdown { ref source } => {
                            reef_core::markdown::build_markdown_preview_with_syntax(
                                &task.path, source, task.dark,
                            )
                            .map(PreviewEnrichment::Markdown)
                        }
                    }))
                    .ok()
                    .flatten();
                let _ = result_tx.send(WorkerResult::PreviewEnrichmentFinished {
                    generation: task.generation,
                    path: task.path,
                    enrichment,
                });
            }
        });
    tx
}

fn spawn_preview_worker(result_tx: WorkerResultSender) -> mpsc::Sender<FilesTask> {
    let (tx, rx) = mpsc::unbounded();
    let _ = thread::Builder::new()
        .name("reef-preview-worker".into())
        .spawn(move || {
            let mut backlog = VecDeque::new();
            while let Ok(task) = recv_preview_worker_task(&rx, &mut backlog) {
                match task {
                    FilesTask::LoadPreview {
                        generation,
                        backend,
                        rel_path,
                        wants_decoded_image,
                    } => {
                        let result = run_preview_with_panic_guard(&rel_path, || {
                            backend.load_preview(&rel_path, wants_decoded_image)
                        });
                        let _ = result_tx.send(WorkerResult::Preview { generation, result });
                    }
                    FilesTask::LoadDbPage {
                        generation,
                        backend,
                        request,
                    } => {
                        let offset = request.page.saturating_mul(request.rows_per_page as u64);
                        let result = backend
                            .db_load_page(
                                &request.path,
                                &request.key,
                                offset,
                                request.rows_per_page,
                            )
                            .map(|page_data| DbPagePayload {
                                path: request.path,
                                key: request.key,
                                page: request.page,
                                rows: page_data.rows,
                                row_locators: page_data.row_locators,
                                reset_h_scroll: request.reset_h_scroll,
                                refresh: request.refresh,
                            })
                            .map_err(|e| e.to_string());
                        let _ = result_tx.send(WorkerResult::DbPage { generation, result });
                    }
                    FilesTask::LoadDbDetail {
                        generation,
                        backend,
                        path,
                        key,
                    } => {
                        let result = backend
                            .db_load_object_detail(&path, &key)
                            .map(|detail| DbDetailPayload { path, key, detail })
                            .map_err(|e| e.to_string());
                        let _ = result_tx.send(WorkerResult::DbDetail { generation, result });
                    }
                    FilesTask::PrefetchPreview {
                        backend,
                        rel_path,
                        wants_decoded_image,
                    } => {
                        // Fire-and-forget: the backend's LRU cache
                        // absorbs the result. Same panic guard as the
                        // `LoadPreview` arm — a bad neighbor on
                        // prefetch must not take the worker down.
                        let _ = run_preview_with_panic_guard(&rel_path, || {
                            backend.load_preview(&rel_path, wants_decoded_image)
                        });
                    }
                    _ => {}
                }
            }
        });
    tx
}

fn spawn_nav_preview_worker(result_tx: WorkerResultSender) -> mpsc::Sender<NavPreviewTask> {
    let (tx, rx) = mpsc::unbounded::<NavPreviewTask>();
    let _ = thread::Builder::new()
        .name("reef-nav-preview".into())
        .spawn(move || {
            reef_core::highlight::warm_up_common_syntaxes();
            while let Ok(mut task) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    task = newer;
                }
                let result = run_preview_with_panic_guard(&task.path, || {
                    task.backend.load_preview(&task.path, false)
                })
                .map(|content| content.map(|content| enrich_nav_preview(content, task.dark)));
                let _ = result_tx.send(WorkerResult::NavPreview {
                    generation: task.generation,
                    path: task.path,
                    result,
                });
            }
        });
    tx
}

fn enrich_nav_preview(mut content: PreviewContent, dark: bool) -> PreviewContent {
    if let PreviewBody::Text(text) = &mut content.body
        && let Some(enrichment) = reef_core::preview::build_text_preview_enrichment(
            &content.path,
            content.bytes_on_disk,
            &text.lines,
            text.source.as_deref(),
            dark,
        )
    {
        text.highlighted = enrichment.highlighted;
        text.parsed = enrichment.parsed;
    }
    content
}

fn spawn_db_cell_worker(result_tx: WorkerResultSender) -> mpsc::Sender<DbCellTask> {
    let (tx, rx) = mpsc::unbounded::<DbCellTask>();
    let _ = thread::Builder::new()
        .name("reef-db-cell-worker".into())
        .spawn(move || {
            while let Ok(task) = recv_latest_db_cell_task(&rx) {
                if task.request.cancellation.is_cancelled() {
                    continue;
                }
                let result = task
                    .backend
                    .db_load_cell(
                        &task.request.path,
                        &task.request.key,
                        &task.request.row_locator,
                        task.request.column,
                        &task.request.cancellation,
                    )
                    .map(|value| DbCellPayload {
                        path: task.request.path,
                        key: task.request.key,
                        row_offset: task.request.row_offset,
                        row_locator: task.request.row_locator,
                        column: task.request.column,
                        value,
                    })
                    .map_err(|error| error.to_string());
                let _ = result_tx.send(WorkerResult::DbCell {
                    generation: task.generation,
                    result,
                });
            }
        });
    tx
}

fn recv_latest_db_cell_task(
    rx: &mpsc::Receiver<DbCellTask>,
) -> Result<DbCellTask, mpsc::RecvError> {
    let mut task = rx.recv()?;
    while let Ok(newer) = rx.try_recv() {
        task.request.cancellation.cancel();
        task = newer;
    }
    Ok(task)
}

fn spawn_git_worker(result_tx: WorkerResultSender) -> mpsc::Sender<GitTask> {
    let (tx, rx) = mpsc::unbounded();
    let _ = thread::Builder::new()
        .name("reef-git-worker".into())
        .spawn(move || {
            while let Ok(task) = rx.recv() {
                match task {
                    GitTask::RefreshStatus {
                        generation,
                        backend,
                    } => {
                        let result = backend
                            .git_status()
                            .map(|snap| GitStatusPayload {
                                staged: snap.staged,
                                unstaged: snap.unstaged,
                                ahead_behind: snap.ahead_behind,
                                branch_name: snap.branch_name,
                            })
                            .map_err(|e| e.to_string());
                        let _ = result_tx.send(WorkerResult::GitStatus { generation, result });
                    }
                    GitTask::Mutate {
                        generation,
                        backend,
                        mutation,
                    } => {
                        let result = run_git_mutation(backend.as_ref(), mutation);
                        let _ = result_tx.send(WorkerResult::GitMutation { generation, result });
                    }
                    GitTask::Commit {
                        generation,
                        backend,
                        message,
                    } => {
                        let result = backend.commit(&message).map_err(|e| e.to_string());
                        let _ = result_tx.send(WorkerResult::Commit { generation, result });
                    }
                    GitTask::Push {
                        generation,
                        backend,
                        force,
                    } => {
                        let result = backend.push(force).map_err(|e| e.to_string());
                        let _ = result_tx.send(WorkerResult::Push {
                            generation,
                            force,
                            result,
                        });
                    }
                }
            }
        });
    tx
}

fn spawn_git_diff_worker(result_tx: WorkerResultSender) -> mpsc::Sender<GitDiffTask> {
    let (tx, rx) = mpsc::unbounded();
    let _ = thread::Builder::new()
        .name("reef-git-diff-worker".into())
        .spawn(move || {
            while let Ok(task) = recv_latest(&rx) {
                match task {
                    GitDiffTask::Load {
                        generation,
                        backend,
                        path,
                        staged,
                        context_lines,
                        dark,
                    } => {
                        let result = if staged {
                            backend.staged_diff(&path, context_lines)
                        } else {
                            backend.unstaged_diff(&path, context_lines)
                        }
                        .map_err(|e| e.to_string())
                        .map(|opt| opt.map(|diff| build_highlighted_diff(&path, diff, dark)));
                        let _ = result_tx.send(WorkerResult::Diff { generation, result });
                    }
                }
            }
        });
    tx
}

fn spawn_git_status_stats_worker(
    result_tx: WorkerResultSender,
) -> mpsc::Sender<GitStatusStatsTask> {
    let (tx, rx) = mpsc::unbounded();
    let _ = thread::Builder::new()
        .name("reef-git-stats-worker".into())
        .spawn(move || {
            while let Ok(task) = recv_latest(&rx) {
                match task {
                    GitStatusStatsTask::Refresh {
                        generation,
                        backend,
                    } => {
                        let result = backend
                            .git_status_stats()
                            .map_err(|error| error.to_string());
                        let _ = result_tx.send(WorkerResult::GitStatusStats { generation, result });
                    }
                }
            }
        });
    tx
}

fn run_git_mutation(
    backend: &dyn Backend,
    mutation: GitMutation,
) -> Result<GitMutationPayload, String> {
    let mut touched = Vec::new();
    let mut errors = Vec::new();
    match &mutation {
        GitMutation::Stage(paths) => match backend.stage_paths(paths) {
            Ok(()) => touched.extend(paths.iter().cloned()),
            Err(error) => errors.push(error.to_string()),
        },
        GitMutation::Unstage(paths) => match backend.unstage_paths(paths) {
            Ok(()) => touched.extend(paths.iter().cloned()),
            Err(error) => errors.push(error.to_string()),
        },
        GitMutation::Revert(paths) => {
            for item in paths {
                match backend.revert_path(&item.path, item.is_staged) {
                    Ok(()) => touched.push(item.path.clone()),
                    Err(error) => errors.push(format!("{}: {error}", item.path)),
                }
            }
        }
    }

    if touched.is_empty() && !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    Ok(GitMutationPayload {
        mutation,
        touched,
        errors,
    })
}

fn spawn_graph_refresh_worker(result_tx: WorkerResultSender) -> mpsc::Sender<GraphRefreshTask> {
    let (tx, rx) = mpsc::unbounded();
    let _ = thread::Builder::new()
        .name("reef-graph-refresh-worker".into())
        .spawn(move || {
            while let Ok(task) = recv_latest(&rx) {
                match task {
                    GraphRefreshTask::RefreshGraph {
                        generation,
                        backend,
                        limit,
                        scope,
                    } => {
                        let result = (|| -> Result<GraphPayload, String> {
                            let head = backend
                                .head_oid()
                                .map_err(|e| e.to_string())?
                                .unwrap_or_default();
                            let ref_map = backend.list_refs().map_err(|e| e.to_string())?;
                            let refs_hash = hash_ref_map(&ref_map);
                            let scope_hash = hash_graph_scope(&scope);
                            let commits = backend
                                .list_commits(&scope, limit)
                                .map_err(|e| e.to_string())?;
                            let rows = reef_core::git::graph::build_graph(&commits);
                            // Transient-walk-failure detector: a
                            // `Branch(X)` scope where `X` is still in
                            // ref_map but `list_commits` returned no
                            // commits is almost certainly a one-off
                            // libgit2 hiccup (lock contention, fs
                            // jitter) rather than a real "empty
                            // branch". Returning Err here keeps the
                            // previous rows on screen until the next
                            // 5s revalidate succeeds, instead of
                            // letting the main thread replace them
                            // with the empty set. The genuinely-gone
                            // case (ref missing from ref_map) still
                            // lands as a normal payload and trips the
                            // stale-branch fallback in `app.rs`.
                            if rows.is_empty()
                                && let GraphScope::Branch(target) = &scope
                                && ref_present_in_map(&ref_map, target)
                            {
                                return Err(format!("transient walk failure for {target}"));
                            }
                            Ok(GraphPayload {
                                rows,
                                ref_map,
                                cache_key: (head, refs_hash, scope_hash),
                                scope,
                            })
                        })();
                        let _ = result_tx.send(WorkerResult::Graph { generation, result });
                    }
                }
            }
        });
    tx
}

fn recv_latest<T>(rx: &mpsc::Receiver<T>) -> Result<T, mpsc::RecvError> {
    let mut latest = rx.recv()?;
    while let Ok(newer) = rx.try_recv() {
        latest = newer;
    }
    Ok(latest)
}

fn spawn_graph_content_worker(result_tx: WorkerResultSender) -> mpsc::Sender<GraphContentTask> {
    let (tx, rx) = mpsc::unbounded();
    let _ = thread::Builder::new()
        .name("reef-graph-content-worker".into())
        .spawn(move || {
            while let Ok(task) = rx.recv() {
                match task {
                    GraphContentTask::CommitDetail {
                        generation,
                        backend,
                        oid,
                    } => {
                        let result = backend.commit_detail(&oid).map_err(|e| e.to_string());
                        let _ = result_tx.send(WorkerResult::CommitDetail { generation, result });
                    }
                    GraphContentTask::CommitFileDiff {
                        generation,
                        backend,
                        oid,
                        path,
                        context_lines,
                        dark,
                    } => {
                        let result = backend
                            .commit_file_diff(&oid, &path, context_lines)
                            .map_err(|e| e.to_string())
                            .map(|opt| opt.map(|diff| build_commit_file_diff(path, diff, dark)));
                        let _ = result_tx.send(WorkerResult::CommitFileDiff { generation, result });
                    }
                    GraphContentTask::CommitRangeDetail {
                        generation,
                        backend,
                        oldest_oid,
                        newest_oid,
                    } => {
                        let result = backend
                            .range_files(&oldest_oid, &newest_oid)
                            .map_err(|e| e.to_string());
                        let _ = result_tx.send(WorkerResult::RangeDetail { generation, result });
                    }
                    GraphContentTask::RangeFileDiff {
                        generation,
                        backend,
                        oldest_oid,
                        newest_oid,
                        path,
                        context_lines,
                        dark,
                    } => {
                        let result = backend
                            .range_file_diff(&oldest_oid, &newest_oid, &path, context_lines)
                            .map_err(|e| e.to_string())
                            .map(|opt| opt.map(|diff| build_commit_file_diff(path, diff, dark)));
                        let _ = result_tx.send(WorkerResult::RangeFileDiff { generation, result });
                    }
                }
            }
        });
    tx
}

/// Skip highlighting when a diff exceeds this many content lines. Mirrors
/// the preview path's `lines.len() <= 5_000` cap (see `file_tree::load_preview`)
/// but lets diffs go further because the typical diff has way fewer lines than
/// the file it came from. Past this point syntect's per-line cost stops being
/// negligible (50k+ lines of generated code can stall the worker for
/// seconds — bad even off the render path).
const HIGHLIGHT_DIFF_LINE_CAP: usize = 10_000;

/// Skip highlighting when the diff's total content byte size exceeds this.
/// Matches `file_tree::load_preview`'s `raw.len() <= 512 * 1024` byte gate,
/// scaled up since diffs are usually a subset of the file. A 2 MB diff is
/// already in "regenerated file" territory and not interesting to colorize.
const HIGHLIGHT_DIFF_BYTE_CAP: usize = 2 * 1024 * 1024;

/// Max entries in the shared highlight cache. Each entry is one
/// `Arc<DiffHighlighted>` (deep structure of `Vec<Vec<Arc<...>>>`); 32
/// covers the typical "flip between 5-10 files in a working tree"
/// pattern without unbounded growth. Eviction is true LRU (oldest entry
/// pops when full) so a 33rd insert doesn't nuke the warm working set.
const HIGHLIGHT_CACHE_MAX: usize = 32;

/// Cell state with named variants (vs. the original `Option<Option<Arc<…>>>`
/// where the outer Option was "Pending vs Done" and the inner was
/// "highlight succeeded vs no syntax"). The double-Option encoding was
/// reader-hostile and brittle to refactors — a future flatten would
/// silently break the `while pending` loop in `wait`.
enum CellState {
    Pending,
    Done(Option<Arc<DiffHighlighted>>),
}

/// Filled-once cell that lets late callers wait on an in-flight syntect
/// computation instead of starting their own. First miss installs the
/// cell into the LRU; concurrent misses for the same key grab the same
/// `Arc<InFlightCell>` and `wait` on the Condvar until the first caller
/// stores the result and `notify_all`s.
struct InFlightCell {
    state: std::sync::Mutex<CellState>,
    cv: std::sync::Condvar,
}

impl InFlightCell {
    fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(CellState::Pending),
            cv: std::sync::Condvar::new(),
        }
    }

    /// Publish the result and wake all waiters. Idempotent — late
    /// `PublishGuard::drop` calls after the worker already published
    /// are silent no-ops (the second `Done` write would just overwrite
    /// itself with the same value).
    fn publish(&self, result: Option<Arc<DiffHighlighted>>) {
        let mut g = self.state.lock().unwrap_or_else(|p| {
            self.state.clear_poison();
            p.into_inner()
        });
        // Skip if already published — protects against the Drop guard
        // running after the explicit publish (cell.publish(result) then
        // PublishGuard::drop tries to publish(None)).
        if matches!(*g, CellState::Done(_)) {
            return;
        }
        *g = CellState::Done(result);
        self.cv.notify_all();
    }

    /// Block until `publish` runs. Poison-tolerant.
    ///
    /// The `v.clone()` happens while still holding the state mutex —
    /// `Arc::clone` on the `Option<Arc<DiffHighlighted>>` is a single
    /// atomic refcount bump so there's no real win in trying to hoist
    /// it out (and structurally we can't — the borrow is anchored to
    /// the guard). The explicit `drop(g)` makes the lock-release point
    /// obvious and ensures subsequent woken waiters serialize only on
    /// the cheap clone, not on whatever the caller does with `value`.
    fn wait(&self) -> Option<Arc<DiffHighlighted>> {
        let mut g = self.state.lock().unwrap_or_else(|p| {
            self.state.clear_poison();
            p.into_inner()
        });
        while matches!(*g, CellState::Pending) {
            g = self.cv.wait(g).unwrap_or_else(|p| {
                self.state.clear_poison();
                p.into_inner()
            });
        }
        let value = match &*g {
            CellState::Done(v) => v.clone(),
            CellState::Pending => unreachable!("loop only exits on Done"),
        };
        drop(g);
        value
    }
}

/// RAII guard that ensures `publish` runs on every exit from the
/// syntect-compute window, including panic unwind. Without this, a
/// panic between `Slot::InFlight(cell)` insertion and the explicit
/// `cell.publish(result)` would strand every future caller for the
/// same cache key on the Condvar forever — the cell would stay
/// `Pending` and `wait()` would block indefinitely.
struct PublishGuard {
    cell: Arc<InFlightCell>,
    key: u64,
    armed: bool,
}

impl PublishGuard {
    fn new(cell: Arc<InFlightCell>, key: u64) -> Self {
        Self {
            cell,
            key,
            armed: true,
        }
    }

    /// Disarm the guard — call this after the explicit `cell.publish(result)`
    /// so the subsequent `Drop` is a cheap no-op (it still runs, but the
    /// early-`return` on `!self.armed` skips the `publish`/`lock_cache`
    /// work). Use `std::mem::forget(self)` if you want to skip `Drop`
    /// entirely; we deliberately don't, because Drop's branch is one
    /// load + jump and panicking through `defuse` would still need the
    /// safety net.
    fn defuse(mut self) {
        self.armed = false;
    }
}

impl Drop for PublishGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Panic path: publish None so waiters return cleanly (render falls
        // back to plain colors). Then evict the InFlight slot from the
        // cache so the next caller re-misses instead of re-waiting on a
        // dead cell.
        self.cell.publish(None);
        let mut cache = lock_cache();
        // Only drop the slot if it's still our cell — a concurrent
        // worker may have already replaced it.
        if let Some(Slot::InFlight(c)) = cache.map.get(&self.key) {
            if Arc::ptr_eq(c, &self.cell) {
                cache.remove(&self.key);
            }
        }
    }
}

/// LRU slot. `Ready` is a finished entry (highlight result or `None` for
/// "we decided not to highlight"); `InFlight` is a syntect job currently
/// running on some worker — late callers grab the Arc and `wait()`.
enum Slot {
    Ready(Option<Arc<DiffHighlighted>>),
    InFlight(Arc<InFlightCell>),
}

/// Tiny hand-rolled LRU. 32 entries → linear scan is faster than a
/// proper linked-list LRU's allocator overhead, and avoids a 3rd-party
/// dep. `order` lists keys oldest→newest; on access, we move the touched
/// key to the back.
struct HighlightLru {
    map: std::collections::HashMap<u64, Slot>,
    order: std::collections::VecDeque<u64>,
}

impl HighlightLru {
    fn new() -> Self {
        Self {
            map: std::collections::HashMap::with_capacity(HIGHLIGHT_CACHE_MAX),
            order: std::collections::VecDeque::with_capacity(HIGHLIGHT_CACHE_MAX),
        }
    }

    /// Cheap structural-invariant guard. The map and order deque must
    /// agree on which keys are live; both insert/touch/promote/remove
    /// maintain this. Centralising the assert here surfaces any future
    /// helper that drifts.
    ///
    /// The whole body is gated behind `cfg(debug_assertions)`:
    ///   * release builds get a true no-op (no TLS read, no len calls),
    ///   * debug builds run the panic-skip + `debug_assert_eq`. The
    ///     skip is necessary because `PublishGuard::drop` calls into
    ///     this method during unwind — a debug-only mismatch there
    ///     would otherwise trigger Rust's panic-while-panicking abort.
    #[cfg_attr(debug_assertions, inline)]
    #[cfg_attr(not(debug_assertions), inline(always))]
    fn assert_invariant(&self) {
        #[cfg(debug_assertions)]
        {
            if std::thread::panicking() {
                return;
            }
            debug_assert_eq!(
                self.map.len(),
                self.order.len(),
                "HighlightLru: map.len() must equal order.len()"
            );
        }
    }

    /// Peek the slot for `key` *without* taking ownership. Updates LRU
    /// access order on hit. The two Slot variants return different shapes
    /// so the caller (`highlight_diff`) can branch:
    ///   * `Ready(v)` → clone the inner Option<Arc> and return immediately
    ///   * `InFlight(cell)` → Arc::clone the cell, drop the cache lock,
    ///     wait on the cell
    fn touch(&mut self, key: &u64) -> Option<SlotView> {
        self.assert_invariant();
        let slot = self.map.get(key)?;
        let view = match slot {
            Slot::Ready(v) => SlotView::Ready(v.clone()),
            Slot::InFlight(c) => SlotView::InFlight(Arc::clone(c)),
        };
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
            self.order.push_back(*key);
        }
        Some(view)
    }

    fn insert(&mut self, key: u64, slot: Slot) {
        self.assert_invariant();
        if self.map.contains_key(&key) {
            if let Some(pos) = self.order.iter().position(|k| k == &key) {
                self.order.remove(pos);
            }
        } else if self.order.len() >= HIGHLIGHT_CACHE_MAX {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
        // Map insert first, order push second. Panic-safety analysis:
        //   * New-key, non-full: neither pre-branch fires; on `map.insert`
        //     rehash-panic both halves stay at old len → invariant holds.
        //   * New-key, full: eviction popped front of order AND removed
        //     from map (both decremented in lockstep above); on rehash
        //     panic both stay at old-1 → invariant holds.
        //   * Existing-key: `order.remove(pos)` ran above (order = old-1);
        //     `map.insert` here is a *replace* which never rehashes,
        //     so no panic window. If it did panic, order would be
        //     old-1 vs map old → DESYNC. Currently unreachable because
        //     all call sites guarantee key absence via `touch()`+None,
        //     but a future caller adding `insert(existing_key, _)`
        //     would re-open the bug — keep the eviction symmetry and
        //     existing-key replace semantics in mind when extending.
        self.map.insert(key, slot);
        self.order.push_back(key);
    }

    /// Remove `key` from both halves of the LRU. Used by `PublishGuard`
    /// to evict a dead InFlight slot on the panic-unwind path.
    fn remove(&mut self, key: &u64) {
        self.assert_invariant();
        if self.map.remove(key).is_some() {
            if let Some(pos) = self.order.iter().position(|k| k == key) {
                self.order.remove(pos);
            }
        }
    }

    /// Promote an `InFlight(our_cell)` slot to `Ready(value)`, but ONLY
    /// if the slot still holds `our_cell` — a concurrent worker may
    /// have evicted us and installed its own InFlight, and overwriting
    /// that slot would corrupt the second worker's cache view. Also
    /// rotates the key to the back of the LRU order so a freshly-
    /// computed result isn't immediately evictable just because the
    /// InFlight slot sat near the front during the syntect window.
    fn promote_if_owner(
        &mut self,
        key: u64,
        our_cell: &Arc<InFlightCell>,
        value: Option<Arc<DiffHighlighted>>,
    ) {
        self.assert_invariant();
        match self.map.get(&key) {
            Some(Slot::InFlight(c)) if Arc::ptr_eq(c, our_cell) => {
                self.map.insert(key, Slot::Ready(value));
                if let Some(pos) = self.order.iter().position(|k| k == &key) {
                    self.order.remove(pos);
                    self.order.push_back(key);
                }
            }
            _ => {
                // Either evicted (None) or replaced by a concurrent
                // worker's InFlight (Slot::InFlight with a different
                // cell pointer). Either way, don't touch the slot —
                // the other worker's promote will handle it.
            }
        }
    }

    /// Clear the entire cache. Test/bench-only helper exposed via the
    /// public reset hook so a unit test calling `highlight_diff` directly
    /// can flush process-global state between tests.
    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

enum SlotView {
    Ready(Option<Arc<DiffHighlighted>>),
    InFlight(Arc<InFlightCell>),
}

type HighlightCache = std::sync::Mutex<HighlightLru>;

fn highlight_cache() -> &'static HighlightCache {
    use std::sync::OnceLock;
    static CACHE: OnceLock<HighlightCache> = OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(HighlightLru::new()))
}

/// Poison-tolerant lock: if a previous holder panicked, recover the
/// inner state (it's just a cache — discarding the panic-time write is
/// fine) and clear the poison flag so subsequent callers see a clean
/// Mutex. Without this, one panic anywhere in any worker silently
/// deadlocks the cache for the rest of the process lifetime.
fn lock_cache() -> std::sync::MutexGuard<'static, HighlightLru> {
    let m = highlight_cache();
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            m.clear_poison();
            poisoned.into_inner()
        }
    }
}

/// Compute the per-diff cache key. Hashed inputs:
///   - `dark` flag (1 byte) so theme toggle is its own lookup
///   - path length prefix + path bytes (so `foo.rs`+`bar` and `foo`+`.rsbar`
///     don't collide — without the length-prefix their byte streams match)
///   - per-hunk: hunk_lens, header length + bytes (so two diffs with
///     identical flat content but different hunk groupings produce
///     different keys — without this the cached `Vec<Vec<...>>` shape
///     can mismatch the requesting diff and silently mis-render)
///   - per-line: length prefix + bytes
fn highlight_cache_key(path: &str, diff: &DiffContent, dark: bool) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write_u8(u8::from(dark));
    h.write_usize(path.len());
    h.write(path.as_bytes());
    h.write_usize(diff.hunks.len());
    for hunk in &diff.hunks {
        h.write_usize(hunk.header.len());
        h.write(hunk.header.as_bytes());
        h.write_usize(hunk.lines.len());
        for line in &hunk.lines {
            h.write_usize(line.content.len());
            h.write(line.content.as_bytes());
        }
    }
    h.finish()
}

/// Run syntect over the diff's content lines once per file and split the
/// flat result into per-hunk slices so the renderer can index by
/// `(hunk, line)`. Runs in worker threads — keeps the UI smooth on large
/// diffs (a 10k-line diff takes ~50ms).
///
/// Returns an `Arc<DiffHighlighted>` so cache hits and downstream sharing
/// are O(1) refcount bumps instead of a deep clone of `Vec<Vec<Arc<...>>>`.
/// `None` means we deliberately didn't highlight (no syntax resolves,
/// or exceeds the size guards) — render falls back to plain per-tag colors.
pub fn highlight_diff(path: &str, diff: &DiffContent, dark: bool) -> Option<Arc<DiffHighlighted>> {
    let key = highlight_cache_key(path, diff, dark);

    // First lookup: hit → return immediately; in-flight → wait on the cell;
    // miss → install an `InFlight` cell and commit to running syntect ourselves.
    let cell = {
        let mut cache = lock_cache();
        match cache.touch(&key) {
            Some(SlotView::Ready(v)) => return v,
            Some(SlotView::InFlight(c)) => {
                drop(cache);
                return c.wait();
            }
            None => {
                // Size guards run on the FIRST caller's path only — concurrent
                // callers with the same (path, diff, dark) key see Slot::InFlight
                // and `wait()` on the cell, inheriting whatever decision this
                // worker makes. That's safe today because the guards depend
                // only on `diff` (which is part of the cache key, so equal-
                // keyed callers see equal guards); if the guards ever grow a
                // caller-dependent dimension (theme-conditional thresholds,
                // viewport-dependent caps) this dedup needs to re-evaluate
                // on each path.
                let total_lines: usize = diff.hunks.iter().map(|h| h.lines.len()).sum();
                if total_lines > HIGHLIGHT_DIFF_LINE_CAP {
                    cache.insert(key, Slot::Ready(None));
                    return None;
                }
                let total_bytes: usize = diff
                    .hunks
                    .iter()
                    .flat_map(|h| h.lines.iter().map(|l| l.content.len()))
                    .sum();
                if total_bytes > HIGHLIGHT_DIFF_BYTE_CAP {
                    cache.insert(key, Slot::Ready(None));
                    return None;
                }
                // Claim — concurrent callers from here on `wait()` on this cell.
                let cell = Arc::new(InFlightCell::new());
                cache.insert(key, Slot::InFlight(Arc::clone(&cell)));
                cell
            }
        }
    };

    // RAII guard: on panic between here and `defuse()` below, the Drop
    // impl publishes `None` so any waiters return cleanly (no permanent
    // deadlock) and evicts the dead InFlight slot from the LRU.
    let guard = PublishGuard::new(Arc::clone(&cell), key);

    // Hold no lock during syntect.
    let total_lines: usize = diff.hunks.iter().map(|h| h.lines.len()).sum();
    let mut flat: Vec<String> = Vec::with_capacity(total_lines);
    let mut hunk_lens: Vec<usize> = Vec::with_capacity(diff.hunks.len());
    for hunk in &diff.hunks {
        hunk_lens.push(hunk.lines.len());
        for line in &hunk.lines {
            flat.push(line.content.to_string());
        }
    }
    let result = reef_core::highlight::highlight_file(path, &flat, dark).map(|flat_tokens| {
        // Wrap each line's tokens in `Arc` so downstream `tokens_for(li)`
        // clones are O(1). The iterator-based split hands each Arc to its
        // owning hunk without re-bumping refcounts.
        let mut per_line = flat_tokens.into_iter().map(Arc::new);
        let mut out = Vec::with_capacity(hunk_lens.len());
        for &n in &hunk_lens {
            let mut hunk = Vec::with_capacity(n);
            for _ in 0..n {
                hunk.push(
                    per_line
                        .next()
                        .expect("line count matches highlight_file output"),
                );
            }
            out.push(hunk);
        }
        out
    });
    let result = result.map(Arc::new);

    // Publish to waiters first (releases anyone blocked on the cell).
    cell.publish(result.clone());

    // Promote the cache slot from `InFlight` to `Ready` — but only if we
    // still own the slot. A concurrent worker may have evicted us and
    // installed its own InFlight cell; overwriting that would orphan
    // the other worker's waiters from the cache (they still get woken
    // via their own cell's publish, but new callers would see Ready
    // instead of InFlight and miss the deduplication). Also rotates
    // the key to the back of the LRU order so a long syntect job's
    // result isn't immediately evictable.
    {
        let mut cache = lock_cache();
        cache.promote_if_owner(key, &cell, result.clone());
    }

    // Success path — disarm the guard so its Drop doesn't fire a
    // redundant `publish(None)` (idempotent thanks to the early-return
    // in `publish`, but still cheaper to skip the lock).
    guard.defuse();

    result
}

/// Drop every entry from the process-global highlight cache. Exposed
/// for tests / benches that call `highlight_diff` directly — without
/// this the cache state leaks across tests within the same binary,
/// making "is this a hit or miss?" assertions non-deterministic.
///
/// **Caller responsibility**: only call when no worker thread is mid-
/// `highlight_diff` for any key you care about. Clearing the LRU drops
/// any live `Slot::InFlight(cell)` entries without notifying the cell;
/// the worker's later `cell.publish` still wakes any waiters it had
/// (they got the cell Arc before the clear), but new callers post-clear
/// will miss and may run syntect again concurrently for the same key.
/// Functionally correct (no deadlock, deterministic syntect output),
/// but means the next two calls for that key duplicate the syntect run.
///
/// Marked `#[doc(hidden)]` since it's not part of the supported API.
#[doc(hidden)]
pub fn _reset_highlight_cache() {
    lock_cache().clear();
}

fn build_commit_file_diff(path: String, diff: DiffContent, dark: bool) -> CommitFileDiff {
    let highlighted = highlight_diff(&path, &diff, dark);
    CommitFileDiff::new(path, diff, highlighted)
}

fn build_highlighted_diff(path: &str, diff: DiffContent, dark: bool) -> HighlightedDiff {
    let highlighted = highlight_diff(path, &diff, dark);
    HighlightedDiff::new(diff, highlighted)
}

fn build_file_tree_payload(
    backend: &dyn Backend,
    expanded: Vec<PathBuf>,
    git_statuses: HashMap<String, char>,
    selected_path: Option<PathBuf>,
    fallback_selected: usize,
) -> Result<FileTreePayload, String> {
    let expanded: std::collections::HashSet<PathBuf> = expanded.into_iter().collect();
    let entries = backend.build_file_tree(&expanded, &git_statuses)?;
    let selected_idx = selected_path
        .as_ref()
        .and_then(|path| entries.iter().position(|entry| &entry.path == path))
        .unwrap_or_else(|| fallback_selected.min(entries.len().saturating_sub(1)));
    Ok(FileTreePayload {
        entries,
        selected_idx,
    })
}

fn build_file_tree_subtree_payload(
    backend: &dyn Backend,
    parent_path: PathBuf,
    parent_depth: usize,
    expanded: Vec<PathBuf>,
) -> Result<FileTreeSubtreePayload, String> {
    let expanded: HashSet<PathBuf> = expanded.into_iter().collect();
    let mut entries = Vec::new();
    collect_file_tree_subtree(
        backend,
        &parent_path,
        parent_depth + 1,
        &expanded,
        &mut entries,
    )?;
    Ok(FileTreeSubtreePayload {
        parent_path,
        entries,
    })
}

fn collect_file_tree_subtree(
    backend: &dyn Backend,
    parent_path: &Path,
    depth: usize,
    expanded: &HashSet<PathBuf>,
    entries: &mut Vec<TreeEntry>,
) -> Result<(), String> {
    let mut children = backend
        .list_dir(parent_path)
        .map_err(|error| error.to_string())?;
    children.retain(|entry| entry.name != ".git");
    children.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    for child in children {
        let path = parent_path.join(&child.name);
        let is_expanded = child.is_dir && expanded.contains(&path);
        entries.push(TreeEntry {
            path: path.clone(),
            name: child.name,
            depth,
            is_dir: child.is_dir,
            has_children: child.has_children,
            is_expanded,
            git_status: None,
        });
        if is_expanded {
            collect_file_tree_subtree(backend, &path, depth + 1, expanded, entries)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod file_tree_subtree_tests {
    use std::fs;

    use reef_io::LocalBackend;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn subtree_load_reads_parent_and_preserves_nested_expansion() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src/nested")).unwrap();
        fs::create_dir_all(tmp.path().join("outside")).unwrap();
        fs::write(tmp.path().join("src/a.rs"), "a").unwrap();
        fs::write(tmp.path().join("src/nested/b.rs"), "b").unwrap();
        fs::write(tmp.path().join("outside/c.rs"), "c").unwrap();
        let backend = LocalBackend::open_at(tmp.path().to_path_buf());
        let payload = build_file_tree_subtree_payload(
            &backend,
            PathBuf::from("src"),
            0,
            vec![PathBuf::from("src"), PathBuf::from("src/nested")],
        )
        .unwrap();

        assert_eq!(payload.parent_path, Path::new("src"));
        assert_eq!(
            payload
                .entries
                .iter()
                .map(|entry| (entry.path.as_path(), entry.depth, entry.is_expanded))
                .collect::<Vec<_>>(),
            vec![
                (Path::new("src/nested"), 1, true),
                (Path::new("src/nested/b.rs"), 2, false),
                (Path::new("src/a.rs"), 1, false),
            ]
        );
        assert_eq!(payload.entries[2].git_status, None);
    }
}

#[cfg(test)]
mod file_tree_worker_coalescing_tests {
    use super::*;

    fn backend() -> Arc<dyn Backend> {
        Arc::new(reef_io::LocalBackend::open_at(std::env::temp_dir()))
    }

    fn rebuild(generation: u64) -> FileTreeRebuildTask {
        FileTreeRebuildTask {
            identity: FileTreeRebuildIdentity {
                generation,
                tree_revision: generation,
            },
            backend: backend(),
            expanded: Vec::new(),
            git_statuses: HashMap::new(),
            selected_path: None,
            fallback_selected: 0,
        }
    }

    #[test]
    fn queued_full_tree_rebuilds_coalesce_to_latest_generation() {
        let (tx, rx) = mpsc::unbounded();
        tx.send(rebuild(1)).unwrap();
        tx.send(rebuild(3)).unwrap();

        let task = recv_latest_file_tree_rebuild_task(&rx).unwrap();

        assert_eq!(task.identity.generation, 3);
        assert_eq!(task.identity.tree_revision, 3);
        assert!(rx.is_empty());
    }

    #[test]
    fn single_full_tree_rebuild_is_preserved() {
        let (tx, rx) = mpsc::unbounded();
        tx.send(rebuild(4)).unwrap();

        let task = recv_latest_file_tree_rebuild_task(&rx).unwrap();

        assert_eq!(task.identity.generation, 4);
    }
}

fn build_nav_workspace_index(
    backend: &dyn Backend,
) -> Result<reef_core::nav::WorkspaceIndex, String> {
    let root = backend.workdir_path();
    let walk = backend
        .walk_repo_paths(&WalkOpts {
            include_hidden: true,
            respect_gitignore: true,
            max_files: None,
        })
        .map_err(|e| e.to_string())?;
    let files = walk.paths.into_iter().filter_map(|path| {
        let rel = PathBuf::from(path);
        let _lang = reef_core::nav::NavLang::from_path(&rel)?;
        let bytes = backend
            .read_file(&rel, reef_core::nav::workspace::MAX_FILE_BYTES_INDEX + 1)
            .ok()?;
        if bytes.len() as u64 > reef_core::nav::workspace::MAX_FILE_BYTES_INDEX {
            return None;
        }
        Some(reef_core::nav::WorkspaceIndexFile {
            path: rel,
            source: Arc::from(bytes.into_boxed_slice()),
        })
    });
    Ok(reef_core::nav::build_workspace_index(root, files))
}

/// `true` if `target` (a fully-qualified ref like `refs/heads/main`)
/// is reachable through any `RefLabel::Branch` / `RefLabel::RemoteBranch`
/// entry in `ref_map`. Used by the graph worker to distinguish a
/// genuinely-deleted branch (target missing → fallback to AllRefs)
/// from a transient walk failure (target still present → return Err
/// so main keeps the previous rows).
fn ref_present_in_map(ref_map: &HashMap<String, Vec<RefLabel>>, target: &str) -> bool {
    ref_map.values().any(|labels| {
        labels.iter().any(|label| match label {
            RefLabel::Branch(name) => format!("refs/heads/{name}") == target,
            RefLabel::RemoteBranch(name) => format!("refs/remotes/{name}") == target,
            _ => false,
        })
    })
}

fn hash_graph_scope(scope: &GraphScope) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match scope {
        GraphScope::AllRefs => 0u8.hash(&mut hasher),
        GraphScope::Branch(s) => {
            1u8.hash(&mut hasher);
            s.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn hash_ref_map(map: &HashMap<String, Vec<RefLabel>>) -> u64 {
    let mut entries: Vec<(&String, &Vec<RefLabel>)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (oid, labels) in entries {
        oid.hash(&mut hasher);
        for label in labels {
            match label {
                RefLabel::Head => 0u8.hash(&mut hasher),
                RefLabel::Branch(s) => {
                    1u8.hash(&mut hasher);
                    s.hash(&mut hasher);
                }
                RefLabel::RemoteBranch(s) => {
                    2u8.hash(&mut hasher);
                    s.hash(&mut hasher);
                }
                RefLabel::Tag(s) => {
                    3u8.hash(&mut hasher);
                    s.hash(&mut hasher);
                }
            }
        }
    }
    hasher.finish()
}

// ─── Global-search worker ───────────────────────────────────────────────────

fn spawn_global_search_worker(result_tx: WorkerResultSender) -> mpsc::Sender<GlobalSearchTask> {
    let (tx, rx) = mpsc::unbounded();
    let _ = thread::Builder::new()
        .name("reef-global-search-worker".into())
        .spawn(move || {
            // Drain new tasks as they arrive. A task starting while the previous
            // one is still running won't happen in practice (`ReefApp::step` only
            // kicks off a new task after flipping the old `cancel` flag), but
            // if it did, the previous search would finish and then this one
            // would run — the old `generation` keeps its chunks from leaking.
            while let Ok(task) = rx.recv() {
                match task {
                    GlobalSearchTask::Run {
                        generation,
                        cancel,
                        backend,
                        query,
                    } => {
                        let truncated = run_global_search_via_backend(
                            generation,
                            cancel,
                            backend.as_ref(),
                            &query,
                            &result_tx,
                        );
                        // If the search was cancelled mid-walk we still send
                        // Done so the UI can flip in_flight=false; the UI side
                        // will drop late chunks via generation mismatch anyway.
                        let _ = result_tx.send(WorkerResult::GlobalSearchDone {
                            generation,
                            truncated,
                        });
                    }
                }
            }
        });
    tx
}

/// Dedicated LSP worker thread. Owns the per-language `LspClient`s
/// and a spawn-failure backoff so a missing/broken server isn't
/// re-spawned (with its 15s init handshake) on every single click.
fn spawn_lsp_worker(result_tx: WorkerResultSender) -> mpsc::Sender<LspTask> {
    let (tx, rx) = mpsc::unbounded();
    let _ = thread::Builder::new()
        .name("reef-lsp-worker".into())
        .spawn(move || {
            use std::collections::HashMap;
            use std::time::{Duration, Instant};
            // Don't re-attempt a spawn that just failed for this long.
            // Auto-recovers without cross-thread signaling: after the
            // window, the next `gd` retries (so a completed install is
            // picked up within ~30s with no extra plumbing).
            const SPAWN_BACKOFF: Duration = Duration::from_secs(30);

            let mut clients: HashMap<reef_core::nav::NavLang, reef_core::nav::LspClient> =
                HashMap::new();
            let mut failed_spawn: HashMap<reef_core::nav::NavLang, Instant> = HashMap::new();

            while let Ok(task) = rx.recv() {
                let LspTask::RefineDefinition {
                    generation,
                    epoch,
                    lang,
                    identifier,
                    workspace_root,
                    path,
                    source,
                    line,
                    character,
                } = task;
                if lang.profile().lsp.is_none() {
                    continue;
                }
                if let std::collections::hash_map::Entry::Vacant(slot) = clients.entry(lang) {
                    // Backoff: skip the (potentially 15s-blocking)
                    // spawn if we failed recently. Emits Off so a
                    // waiting pending-jump is cleared rather than
                    // hanging forever.
                    if let Some(t) = failed_spawn.get(&lang) {
                        if t.elapsed() < SPAWN_BACKOFF {
                            let _ = result_tx.send(WorkerResult::LspStateChange {
                                lang,
                                state: reef_core::nav::LspBadge::Off,
                            });
                            continue;
                        }
                        failed_spawn.remove(&lang);
                    }
                    let _ = result_tx.send(WorkerResult::LspStateChange {
                        lang,
                        state: reef_core::nav::LspBadge::Booting,
                    });
                    match reef_core::nav::LspClient::spawn(lang, workspace_root.clone()) {
                        Ok(c) => {
                            slot.insert(c);
                            let _ = result_tx.send(WorkerResult::LspStateChange {
                                lang,
                                state: reef_core::nav::LspBadge::Ready,
                            });
                        }
                        Err(_) => {
                            failed_spawn.insert(lang, Instant::now());
                            let _ = result_tx.send(WorkerResult::LspStateChange {
                                lang,
                                state: reef_core::nav::LspBadge::Off,
                            });
                            continue;
                        }
                    }
                }
                let Some(client) = clients.get(&lang) else {
                    continue;
                };
                let location = match client.goto_definition(&path, &source, line, character, lang) {
                    Ok(loc) => loc,
                    Err(_) => {
                        // Client crashed mid-request — drop it so
                        // the next refine respawns (subject to
                        // backoff).
                        clients.remove(&lang);
                        failed_spawn.insert(lang, Instant::now());
                        let _ = result_tx.send(WorkerResult::LspStateChange {
                            lang,
                            state: reef_core::nav::LspBadge::Crashed,
                        });
                        None
                    }
                };
                let server_returned_location = location.is_some();
                let rel_location = location.as_ref().and_then(|loc| {
                    reef_io::workdir_relative_path(&workspace_root, &loc.path).map(|rel| {
                        reef_core::nav::LspLocation {
                            path: rel,
                            line: loc.line,
                            character: loc.character,
                            character_end: loc.character_end,
                        }
                    })
                });
                let _ = result_tx.send(WorkerResult::LspRefineDone {
                    generation,
                    epoch,
                    lang,
                    identifier,
                    rel_location,
                    server_returned_location,
                });
            }
        });
    tx
}

/// Run one global search via `backend.search_content`, forwarding each
/// backend-emitted chunk as a `WorkerResult::GlobalSearchChunk` so the
/// UI sees partial results within ~one chunk of walker output instead
/// of waiting for the whole walk. Returns `truncated = true` iff the
/// backend reported hitting the hit cap.
///
/// Cancellation is carried into the backend as well as checked by the sink.
/// Local file reads are interruptible, while remote walks receive a protocol
/// cancellation request, so obsolete work does not delay a newer query.
fn run_global_search_via_backend(
    generation: u64,
    cancel: Arc<AtomicBool>,
    backend: &dyn Backend,
    query: &str,
    result_tx: &WorkerResultSender,
) -> bool {
    const PUBLISH_BATCH_SIZE: usize = 64;
    const PUBLISH_INTERVAL: Duration = Duration::from_millis(16);

    if query.is_empty() {
        return false;
    }
    let request = reef_io::ContentSearchRequest {
        pattern: query.to_string(),
        fixed_strings: true,
        case_sensitive: None,
        max_results: GLOBAL_SEARCH_MAX_RESULTS as u32,
        max_line_chars: GLOBAL_SEARCH_MAX_LINE_CHARS as u32,
        cancellation: reef_io::CancellationToken::from_flag(Arc::clone(&cancel)),
    };

    let mut pending = Vec::with_capacity(PUBLISH_BATCH_SIZE);
    let mut last_publish = Instant::now();
    let publish = |pending: &mut Vec<MatchHit>| -> std::ops::ControlFlow<()> {
        if pending.is_empty() {
            return std::ops::ControlFlow::Continue(());
        }
        let hits = std::mem::replace(pending, Vec::with_capacity(PUBLISH_BATCH_SIZE));
        if result_tx
            .send(WorkerResult::GlobalSearchChunk { generation, hits })
            .is_err()
        {
            return std::ops::ControlFlow::Break(());
        }
        std::ops::ControlFlow::Continue(())
    };
    let mut on_chunk = |hits: Vec<reef_io::ContentMatchHit>| -> std::ops::ControlFlow<()> {
        if cancel.load(Ordering::Relaxed) {
            return std::ops::ControlFlow::Break(());
        }
        if hits.is_empty() {
            return std::ops::ControlFlow::Continue(());
        }
        pending.extend(hits.into_iter().map(|h| MatchHit {
            path: h.path,
            display: h.display,
            line: h.line,
            line_text: h.line_text,
            line_revision: h.line_revision,
            byte_range: h.byte_range,
        }));
        if pending.len() >= PUBLISH_BATCH_SIZE || last_publish.elapsed() >= PUBLISH_INTERVAL {
            let flow = publish(&mut pending);
            last_publish = Instant::now();
            return flow;
        }
        std::ops::ControlFlow::Continue(())
    };

    let completed = backend.search_content(&request, &mut on_chunk);
    if !cancel.load(Ordering::Relaxed) {
        let _ = publish(&mut pending);
    }
    completed.map(|result| result.truncated).unwrap_or(false)
}

/// Run one `FilesTask::ReplaceInFiles` batch. Streams a
/// `WorkerResult::ReplaceProgress` per file and a final
/// `WorkerResult::ReplaceDone`.
///
/// Each backend owns the complete guarded transform and atomic write. For a
/// remote workspace this keeps the source file on the agent and transfers
/// only the pattern, replacement, line revisions, and bounded outcome.
fn run_replace_in_files(
    generation: u64,
    backend: &dyn Backend,
    query: &str,
    replace_text: &str,
    items: &[ReplaceItem],
    result_tx: &WorkerResultSender,
) {
    let mut summary = ReplaceSummary::default();
    let total = items.len();

    if query.is_empty() {
        let _ = result_tx.send(WorkerResult::ReplaceDone {
            generation,
            result: Err("empty search pattern".to_string()),
        });
        return;
    }
    for (file_idx, item) in items.iter().enumerate() {
        let path = &item.path;
        let request = reef_io::ReplaceFileRequest {
            pattern: query.to_string(),
            replacement: replace_text.as_bytes().to_vec(),
            lines: item
                .lines
                .iter()
                .map(|line| reef_io::ReplaceLineGuard {
                    line_no: line.line_no as u64,
                    expected_revision: line.expected_revision,
                })
                .collect(),
            max_file_size: MAX_REPLACE_FILE_SIZE,
        };
        match backend.replace_file(path, &request) {
            Ok(reef_io::ReplaceFileOutcome::Changed {
                lines_replaced,
                stale,
            }) => {
                summary.files_changed += 1;
                summary.lines_replaced += lines_replaced as usize;
                summary.skipped_stale += stale as usize;
            }
            Ok(reef_io::ReplaceFileOutcome::NoMatch { stale }) => {
                summary.skipped_stale += stale as usize;
            }
            Ok(reef_io::ReplaceFileOutcome::TooLarge) => summary.skipped_too_large += 1,
            Err(reef_io::BackendError::PathEscape(_)) => summary.skipped_symlink_escape += 1,
            Err(error) => summary.errors.push((path.clone(), error.to_string())),
        }
        let _ = result_tx.send(WorkerResult::ReplaceProgress {
            generation,
            files_done: file_idx + 1,
            files_total: total,
        });
    }

    let _ = result_tx.send(WorkerResult::ReplaceDone {
        generation,
        result: Ok(summary),
    });
}

#[cfg(test)]
mod fs_mutation_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn create_file_writes_empty_file() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("hello.rs");
        let (kind, result) = run_create_file(&target);
        assert!(result.is_ok());
        assert!(matches!(kind, FsMutationKind::CreatedFile { .. }));
        assert_eq!(fs::read_to_string(&target).unwrap(), "");
    }

    #[test]
    fn create_file_refuses_to_overwrite() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("dup.txt");
        fs::write(&target, "existing").unwrap();
        let (_, result) = run_create_file(&target);
        assert!(result.is_err());
        // Original untouched.
        assert_eq!(fs::read_to_string(&target).unwrap(), "existing");
    }

    #[test]
    fn create_folder_makes_dir_and_parents() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a").join("b").join("c");
        let (kind, result) = run_create_folder(&target);
        assert!(result.is_ok());
        assert!(matches!(kind, FsMutationKind::CreatedFolder { .. }));
        assert!(target.is_dir());
    }

    #[test]
    fn rename_moves_path() {
        let tmp = TempDir::new().unwrap();
        let old = tmp.path().join("old.txt");
        let new = tmp.path().join("new.txt");
        fs::write(&old, "content").unwrap();
        let (kind, result) = run_rename(&old, &new);
        assert!(result.is_ok());
        assert!(matches!(kind, FsMutationKind::Renamed { .. }));
        assert!(!old.exists());
        assert_eq!(fs::read_to_string(&new).unwrap(), "content");
    }

    #[test]
    fn rename_fails_on_missing_source() {
        let tmp = TempDir::new().unwrap();
        let old = tmp.path().join("nope.txt");
        let new = tmp.path().join("new.txt");
        let (_, result) = run_rename(&old, &new);
        assert!(result.is_err());
    }

    #[test]
    fn hard_delete_removes_file_and_dir() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a.txt");
        let dir = tmp.path().join("d");
        fs::write(&file, "").unwrap();
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("nested.txt"), "").unwrap();

        let (_, res) = run_hard_delete(&[file.clone(), dir.clone()]);
        assert!(res.is_ok());
        assert!(!file.exists());
        assert!(!dir.exists());
    }

    #[test]
    fn hard_delete_propagates_first_error() {
        // Missing path — `remove_file` returns ENOENT.
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("ghost.txt");
        let (_, res) = run_hard_delete(std::slice::from_ref(&missing));
        assert!(res.is_err());
    }

    // ── paste_batch (Cut/Copy + Paste) end-to-end ──────────────────

    fn make_local(tmp: &TempDir) -> reef_io::LocalBackend {
        reef_io::LocalBackend::open_at(tmp.path().to_path_buf())
    }

    fn item(rel: &str, is_dir: bool, r: Resolution) -> PasteItem {
        PasteItem {
            source: PathBuf::from(rel),
            is_dir,
            resolution: r,
        }
    }

    #[test]
    fn paste_batch_cut_cross_dir_moves_file() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::create_dir(tmp.path().join("dst")).unwrap();
        fs::write(tmp.path().join("src/a.txt"), "data").unwrap();

        let backend = make_local(&tmp);
        let items = vec![item("src/a.txt", false, Resolution::Replace)];
        let (kind, result) =
            run_paste_batch(&backend, &items, Path::new("dst"), /*is_copy=*/ false);
        assert!(result.is_ok(), "got error: {:?}", result);
        assert!(matches!(kind, FsMutationKind::Moved { .. }));
        assert!(
            !tmp.path().join("src/a.txt").exists(),
            "source should be gone after Cut"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/a.txt")).unwrap(),
            "data"
        );
    }

    #[test]
    fn paste_batch_copy_cross_dir_keeps_source() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::create_dir(tmp.path().join("dst")).unwrap();
        fs::write(tmp.path().join("src/a.txt"), "data").unwrap();

        let backend = make_local(&tmp);
        let items = vec![item("src/a.txt", false, Resolution::Replace)];
        let (kind, result) =
            run_paste_batch(&backend, &items, Path::new("dst"), /*is_copy=*/ true);
        assert!(result.is_ok());
        assert!(matches!(kind, FsMutationKind::CopiedTo { .. }));
        assert!(
            tmp.path().join("src/a.txt").exists(),
            "source must stay on Copy"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/a.txt")).unwrap(),
            "data"
        );
    }

    #[test]
    fn paste_batch_copy_recurses_into_directories() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::create_dir(tmp.path().join("src/pkg")).unwrap();
        fs::write(tmp.path().join("src/pkg/a.txt"), "deep").unwrap();
        fs::create_dir(tmp.path().join("dst")).unwrap();

        let backend = make_local(&tmp);
        let items = vec![item("src/pkg", true, Resolution::Replace)];
        let (_, result) = run_paste_batch(&backend, &items, Path::new("dst"), true);
        assert!(result.is_ok());
        assert!(tmp.path().join("dst/pkg/a.txt").exists());
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/pkg/a.txt")).unwrap(),
            "deep"
        );
    }

    #[test]
    fn paste_batch_keep_both_uses_provided_basename() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::create_dir(tmp.path().join("dst")).unwrap();
        fs::write(tmp.path().join("src/a.txt"), "new").unwrap();
        fs::write(tmp.path().join("dst/a.txt"), "old").unwrap();

        let backend = make_local(&tmp);
        let items = vec![item(
            "src/a.txt",
            false,
            Resolution::KeepBoth("a copy.txt".to_string()),
        )];
        let (_, result) = run_paste_batch(&backend, &items, Path::new("dst"), true);
        assert!(result.is_ok());
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/a.txt")).unwrap(),
            "old"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/a copy.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn paste_batch_replace_overwrites_via_trash() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::create_dir(tmp.path().join("dst")).unwrap();
        fs::write(tmp.path().join("src/a.txt"), "new").unwrap();
        fs::write(tmp.path().join("dst/a.txt"), "old").unwrap();

        let backend = make_local(&tmp);
        let items = vec![item("src/a.txt", false, Resolution::Replace)];
        let (_, result) = run_paste_batch(&backend, &items, Path::new("dst"), true);
        assert!(result.is_ok());
        // After Replace, the destination carries the source's content.
        // (The `old` content was either moved to OS Trash or removed —
        // both are acceptable; we only assert the post-state of dst/.)
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/a.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn paste_batch_skip_is_a_noop() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::create_dir(tmp.path().join("dst")).unwrap();
        fs::write(tmp.path().join("src/a.txt"), "new").unwrap();
        fs::write(tmp.path().join("dst/a.txt"), "old").unwrap();

        let backend = make_local(&tmp);
        let items = vec![item("src/a.txt", false, Resolution::Skip)];
        let (kind, result) = run_paste_batch(&backend, &items, Path::new("dst"), true);
        assert!(result.is_ok());
        // No item placed → MovedMulti/CopiedMulti with count = 0.
        assert!(
            matches!(kind, FsMutationKind::CopiedMulti { count: 0 }),
            "kind = {:?}",
            kind
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/a.txt")).unwrap(),
            "old",
            "Skip must leave dest untouched"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("src/a.txt")).unwrap(),
            "new",
            "Skip must leave source untouched"
        );
    }

    #[test]
    fn paste_batch_multi_item_count_in_kind() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::create_dir(tmp.path().join("dst")).unwrap();
        fs::write(tmp.path().join("src/a.txt"), "a").unwrap();
        fs::write(tmp.path().join("src/b.txt"), "b").unwrap();
        fs::write(tmp.path().join("src/c.txt"), "c").unwrap();

        let backend = make_local(&tmp);
        let items = vec![
            item("src/a.txt", false, Resolution::Replace),
            item("src/b.txt", false, Resolution::Replace),
            item("src/c.txt", false, Resolution::Replace),
        ];
        let (kind, result) = run_paste_batch(&backend, &items, Path::new("dst"), true);
        assert!(result.is_ok());
        assert!(
            matches!(kind, FsMutationKind::CopiedMulti { count: 3 }),
            "kind = {:?}",
            kind
        );
        for f in ["a.txt", "b.txt", "c.txt"] {
            assert!(tmp.path().join("dst").join(f).exists(), "missing dst/{f}");
        }
    }

    #[test]
    fn paste_batch_fail_fast_on_missing_source() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("dst")).unwrap();

        let backend = make_local(&tmp);
        let items = vec![item("ghost.txt", false, Resolution::Replace)];
        let (_, result) = run_paste_batch(&backend, &items, Path::new("dst"), false);
        assert!(result.is_err(), "missing source must surface as Err");
    }

    #[test]
    fn paste_batch_lifts_nested_file_to_workspace_root() {
        // dest_dir is the empty PathBuf — workspace root. Mirrors the
        // "drop on tree empty space" path (commit_tree_drag, hover_idx
        // == None) and the "right-click empty space → Paste" path
        // (dispatch_context_menu_item, ALL_FOR_ROOT).
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/a.txt"), "data").unwrap();

        let backend = make_local(&tmp);
        let items = vec![item("src/a.txt", false, Resolution::Replace)];
        // is_copy=false → Cut/Move semantics; an empty dest_dir
        // resolves to `workdir.join("a.txt")` after the worker's
        // `dest_dir.join(basename)`.
        let (_, result) = run_paste_batch(&backend, &items, Path::new(""), false);
        assert!(result.is_ok(), "got error: {:?}", result);
        assert!(
            !tmp.path().join("src/a.txt").exists(),
            "source row should have moved out of src/"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "data",
            "moved file must land at workspace root, not anywhere else"
        );
    }

    #[test]
    fn tree_edit_plan_uses_backend_dir_listing_for_collisions() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/existing.rs"), "").unwrap();
        let backend = make_local(&tmp);

        let err = plan_tree_edit(
            &backend,
            crate::features::tree_edit::TreeEditMode::NewFile,
            PathBuf::from("src"),
            None,
            "existing.rs".to_string(),
        )
        .unwrap_err();

        assert!(matches!(
            err,
            TreeEditPlanError::Validation {
                error: reef_core::file_ops::FileNameError::NameAlreadyExists(name)
            } if name == "existing.rs"
        ));
    }

    #[test]
    fn tree_edit_plan_builds_relative_rename_mutation() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/old.rs"), "").unwrap();
        let backend = make_local(&tmp);

        let plan = plan_tree_edit(
            &backend,
            crate::features::tree_edit::TreeEditMode::Rename,
            PathBuf::from("src"),
            Some(PathBuf::from("src/old.rs")),
            "new.rs".to_string(),
        )
        .unwrap();

        assert_eq!(plan.select_on_done, Some(PathBuf::from("src/new.rs")));
        assert!(matches!(
            plan.mutation,
            TreeEditMutation::Rename {
                old_rel,
                new_rel,
                old_name,
                new_name
            } if old_rel == Path::new("src/old.rs")
                && new_rel == Path::new("src/new.rs")
                && old_name == "old.rs"
                && new_name == "new.rs"
        ));
    }

    #[test]
    fn paste_plan_uses_backend_dir_listing_for_conflicts_and_keep_both_names() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::create_dir(tmp.path().join("dst")).unwrap();
        fs::write(tmp.path().join("src/a.txt"), "new").unwrap();
        fs::write(tmp.path().join("dst/a.txt"), "old").unwrap();
        fs::write(tmp.path().join("dst/a copy.txt"), "older copy").unwrap();
        let backend = make_local(&tmp);

        let payload = plan_paste(
            &backend,
            reef_core::file_ops::ClipMode::Copy,
            PathBuf::from("dst"),
            vec![PathBuf::from("src/a.txt")],
        )
        .unwrap();

        assert!(payload.auto_decisions.is_empty());
        assert_eq!(payload.pending.len(), 1);
        let mut prompt = reef_core::file_ops::PasteConflictPrompt::new(
            payload.op,
            payload.dest_rel,
            payload.auto_decisions,
            payload.pending,
            payload.used_names,
        );
        assert_eq!(
            prompt.keep_both_name_for_current().as_deref(),
            Some("a copy 2.txt")
        );
        prompt.resolve_one(Resolution::KeepBoth("a copy 2.txt".to_string()));
        assert_eq!(
            prompt.into_decisions(),
            vec![(
                PathBuf::from("src/a.txt"),
                Resolution::KeepBoth("a copy 2.txt".to_string())
            )]
        );
    }
}

#[cfg(test)]
mod replace_tests {
    //! Worker-level tests for the replace-in-files backend contract.
    //! Drive `LocalBackend` against a tempdir so the same code path the
    //! UI hits in production is exercised end-to-end (read → match →
    //! rewrite → atomic write). Uses the same worker-result sender shape
    //! that `ReefApp::step` drains in production.
    use super::*;
    use reef_io::LocalBackend;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn run(
        backend: Arc<dyn Backend>,
        query: &str,
        replace_text: &str,
        items: Vec<ReplaceItem>,
    ) -> ReplaceSummary {
        let (result_tx, rx) = mpsc::unbounded();
        let (worker_wake_tx, _worker_wake_rx) = mpsc::bounded(1);
        let tx = WorkerResultSender {
            result_tx,
            worker_wake_tx,
        };
        run_replace_in_files(0, backend.as_ref(), query, replace_text, &items, &tx);
        // Drain the channel — `Done` is the last frame.
        let mut summary = None;
        while let Ok(msg) = rx.try_recv() {
            if let WorkerResult::ReplaceDone { result, .. } = msg {
                summary = Some(result);
            }
        }
        match summary.expect("ReplaceDone never emitted") {
            Ok(s) => s,
            Err(e) => panic!("replace failed: {e}"),
        }
    }

    fn item(path: &str, lines: &[(usize, &str)]) -> ReplaceItem {
        ReplaceItem {
            path: PathBuf::from(path),
            lines: lines
                .iter()
                .map(|(line_no, expected_text)| ReplaceLine {
                    line_no: *line_no,
                    expected_revision: reef_io::content_line_revision(expected_text.as_bytes()),
                })
                .collect(),
        }
    }

    #[test]
    fn replaces_all_occurrences_on_targeted_line() {
        // Sole-line target with two matches: both rewritten in one
        // pass, untargeted lines untouched.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("a.txt"),
            "foo bar foo\nuntouched foo\nfoo at end\n",
        )
        .unwrap();
        let backend: Arc<dyn Backend> = Arc::new(LocalBackend::open_at(tmp.path().to_path_buf()));
        let summary = run(
            backend,
            "foo",
            "BAZ",
            vec![item("a.txt", &[(0, "foo bar foo"), (2, "foo at end")])],
        );
        assert_eq!(summary.files_changed, 1);
        assert_eq!(summary.lines_replaced, 2);
        assert_eq!(summary.skipped_stale, 0);
        let after = fs::read_to_string(tmp.path().join("a.txt")).unwrap();
        assert_eq!(after, "BAZ bar BAZ\nuntouched foo\nBAZ at end\n");
    }

    #[test]
    fn skips_lines_whose_text_no_longer_matches_snapshot() {
        // The user opted into line 0 expecting "foo bar"; the file now
        // has "edited" on that line. Worker must not rewrite — counted
        // as stale instead.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "edited\nfoo here\n").unwrap();
        let backend: Arc<dyn Backend> = Arc::new(LocalBackend::open_at(tmp.path().to_path_buf()));
        let summary = run(
            backend,
            "foo",
            "BAR",
            vec![item("a.txt", &[(0, "foo bar")])],
        );
        assert_eq!(summary.files_changed, 0);
        assert_eq!(summary.lines_replaced, 0);
        assert_eq!(summary.skipped_stale, 1);
        // File untouched on disk.
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "edited\nfoo here\n"
        );
    }

    #[test]
    fn skips_line_when_edit_is_beyond_search_display_cap() {
        let tmp = TempDir::new().unwrap();
        let original = format!("foo{}", "a".repeat(GLOBAL_SEARCH_MAX_LINE_CHARS));
        let edited = format!("{}b\n", &original[..original.len() - 1]);
        fs::write(tmp.path().join("a.txt"), &edited).unwrap();
        let backend: Arc<dyn Backend> = Arc::new(LocalBackend::open_at(tmp.path().to_path_buf()));

        let summary = run(
            backend,
            "foo",
            "BAR",
            vec![item("a.txt", &[(0, &original)])],
        );

        assert_eq!(summary.skipped_stale, 1);
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            edited
        );
    }

    #[test]
    fn preserves_crlf_line_endings() {
        // CRLF file: terminator must survive the rewrite. Plain `\n`
        // files must not pick up `\r` either.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("crlf.txt"), b"foo line\r\nfoo two\r\n").unwrap();
        let backend: Arc<dyn Backend> = Arc::new(LocalBackend::open_at(tmp.path().to_path_buf()));
        let summary = run(
            backend,
            "foo",
            "x",
            vec![item("crlf.txt", &[(0, "foo line"), (1, "foo two")])],
        );
        assert_eq!(summary.lines_replaced, 2);
        let after = fs::read(tmp.path().join("crlf.txt")).unwrap();
        assert_eq!(after, b"x line\r\nx two\r\n");
    }

    #[test]
    fn preserves_non_utf8_bytes_outside_targeted_lines() {
        // A non-UTF-8 byte (0xFF) on an untouched line must round-trip
        // exactly. Lines with broken UTF-8 we *did* target must skip
        // (not panic).
        let tmp = TempDir::new().unwrap();
        let mut bytes: Vec<u8> = b"foo line\n".to_vec();
        bytes.extend_from_slice(&[0xFFu8, b'\n']);
        bytes.extend_from_slice(b"foo trailing\n");
        fs::write(tmp.path().join("nb.txt"), &bytes).unwrap();
        let backend: Arc<dyn Backend> = Arc::new(LocalBackend::open_at(tmp.path().to_path_buf()));
        let summary = run(
            backend,
            "foo",
            "X",
            vec![item("nb.txt", &[(0, "foo line"), (2, "foo trailing")])],
        );
        assert_eq!(summary.lines_replaced, 2);
        let after = fs::read(tmp.path().join("nb.txt")).unwrap();
        assert_eq!(after, b"X line\n\xFF\nX trailing\n");
    }

    #[test]
    fn idempotent_when_pattern_no_longer_present() {
        // Run twice in a row: first edits the file, second is a no-op
        // (the search target already no longer matches, so every
        // requested line counts as stale).
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "foo here\n").unwrap();
        let backend: Arc<dyn Backend> = Arc::new(LocalBackend::open_at(tmp.path().to_path_buf()));

        let s1 = run(
            backend.clone(),
            "foo",
            "X",
            vec![item("a.txt", &[(0, "foo here")])],
        );
        assert_eq!(s1.lines_replaced, 1);
        let s2 = run(backend, "foo", "X", vec![item("a.txt", &[(0, "foo here")])]);
        assert_eq!(s2.lines_replaced, 0);
        assert_eq!(s2.skipped_stale, 1);
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "X here\n"
        );
    }

    #[test]
    fn empty_replacement_deletes_match() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "say foo!\n").unwrap();
        let backend: Arc<dyn Backend> = Arc::new(LocalBackend::open_at(tmp.path().to_path_buf()));
        let summary = run(backend, "foo", "", vec![item("a.txt", &[(0, "say foo!")])]);
        assert_eq!(summary.lines_replaced, 1);
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "say !\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escaping_workdir() {
        // Plant a symlink inside the workdir pointing outside; replace
        // must refuse to rewrite the target file.
        let outer = TempDir::new().unwrap();
        let outside = outer.path().join("outside.txt");
        fs::write(&outside, b"untouched\n").unwrap();
        let work = TempDir::new_in(outer.path()).unwrap();
        std::os::unix::fs::symlink(&outside, work.path().join("link.txt")).unwrap();

        let backend: Arc<dyn Backend> = Arc::new(LocalBackend::open_at(work.path().to_path_buf()));
        let summary = run(
            backend,
            "untouched",
            "TOUCHED",
            vec![item("link.txt", &[(0, "untouched")])],
        );
        assert_eq!(summary.skipped_symlink_escape, 1);
        assert_eq!(fs::read(&outside).unwrap(), b"untouched\n");
    }

    #[test]
    fn no_op_replacement_skips_write() {
        // Replacing "foo" with "foo" produces byte-identical output.
        // The worker should detect the no-op and skip both the atomic
        // write and the `Changed` outcome — otherwise every idempotent
        // press of Apply would churn the file's mtime and fire a
        // spurious fs-watcher event.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a.txt");
        fs::write(&path, "foo here\n").unwrap();
        let mtime_before = fs::metadata(&path).unwrap().modified().unwrap();
        // Wait long enough that an actual write would advance mtime.
        std::thread::sleep(std::time::Duration::from_millis(20));

        let backend: Arc<dyn Backend> = Arc::new(LocalBackend::open_at(tmp.path().to_path_buf()));
        let summary = run(
            backend,
            "foo",
            "foo",
            vec![item("a.txt", &[(0, "foo here")])],
        );
        assert_eq!(
            summary.lines_replaced, 0,
            "no-op must not count as replaced"
        );
        assert_eq!(summary.files_changed, 0);
        let mtime_after = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "no-op replacement must not touch the file on disk"
        );
    }

    #[test]
    fn replaces_a_file_below_the_size_cap() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("ok.txt"), "tiny needle line\n").unwrap();
        let backend: Arc<dyn Backend> = Arc::new(LocalBackend::open_at(tmp.path().to_path_buf()));
        let summary = run(
            backend,
            "needle",
            "x",
            vec![item("ok.txt", &[(0, "tiny needle line")])],
        );
        assert_eq!(summary.lines_replaced, 1);
        assert_eq!(summary.skipped_too_large, 0);
    }

    #[test]
    fn fixed_strings_does_not_treat_dots_as_regex_metachars() {
        // The matcher uses `fixed_strings: true`, so `.` is a literal —
        // a query "a.b" must not match "axb".
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "match a.b here\nmiss axb\n").unwrap();
        let backend: Arc<dyn Backend> = Arc::new(LocalBackend::open_at(tmp.path().to_path_buf()));
        let summary = run(
            backend,
            "a.b",
            "OK",
            vec![item("a.txt", &[(0, "match a.b here"), (1, "miss axb")])],
        );
        assert_eq!(summary.lines_replaced, 1);
        assert_eq!(summary.skipped_stale, 1);
        let after = fs::read_to_string(tmp.path().join("a.txt")).unwrap();
        assert_eq!(after, "match OK here\nmiss axb\n");
    }
}

#[cfg(test)]
mod preview_panic_guard_tests {
    //! Regression coverage for the preview-worker panic guard. The bug it
    //! fixes is hard to reproduce by hand (needs a malformed file that
    //! trips a decoder panic in the `image` / `syntect` / sqlite reader
    //! crates), so the guard helper itself is exercised directly here —
    //! every Backend impl in the workspace shares it via
    //! `spawn_preview_worker`, so locking down its semantics protects
    //! the worker loop from silently regressing back to the
    //! "loading…-forever" failure mode.
    use super::*;

    #[test]
    fn run_preview_with_panic_guard_translates_panic_to_err() {
        let result = run_preview_with_panic_guard(Path::new("dir/oops.png"), || {
            panic!("simulated decoder panic");
        });
        let err = result.expect_err("panic must surface as Err, not propagate");
        assert!(
            err.contains("dir/oops.png"),
            "rel_path missing from message: {err}"
        );
    }

    #[test]
    fn run_preview_with_panic_guard_passes_through_none() {
        let result = run_preview_with_panic_guard(Path::new("x.txt"), || None);
        assert!(matches!(result, Ok(None)), "got {result:?}");
    }

    #[test]
    fn run_preview_with_panic_guard_passes_through_some() {
        let preview = PreviewContent {
            path: "x.txt".into(),
            resolved_path: None,
            local_path: None,
            bytes_on_disk: 2,
            mime: Some("text/plain".into()),
            body: reef_core::preview::PreviewBody::Text(reef_core::preview::TextPreview {
                lines: vec!["hi".into()],
                source: None,
                highlighted: None,
                parsed: None,
            }),
        };
        let result = run_preview_with_panic_guard(Path::new("x.txt"), move || Some(preview));
        let got = result.expect("Ok").expect("Some");
        assert_eq!(got.path, "x.txt");
    }
}

#[cfg(test)]
mod preview_worker_coalescing_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn backend() -> Arc<dyn Backend> {
        Arc::new(reef_io::LocalBackend::open_at(std::env::temp_dir()))
    }

    fn load_preview(generation: u64, path: &str) -> FilesTask {
        FilesTask::LoadPreview {
            generation,
            backend: backend(),
            rel_path: PathBuf::from(path),
            wants_decoded_image: false,
        }
    }

    fn prefetch_preview(path: &str) -> FilesTask {
        FilesTask::PrefetchPreview {
            backend: backend(),
            rel_path: PathBuf::from(path),
            wants_decoded_image: false,
        }
    }

    fn build_quick_open_index(generation: u64) -> FilesTask {
        FilesTask::BuildQuickOpenIndex {
            generation,
            backend: backend(),
        }
    }

    fn load_db_page(generation: u64) -> FilesTask {
        FilesTask::LoadDbPage {
            generation,
            backend: backend(),
            request: DbPageRequest {
                path: PathBuf::from("data.sqlite"),
                key: reef_sqlite_preview::DbObjectKey {
                    schema: "main".to_string(),
                    name: "items".to_string(),
                    kind: reef_sqlite_preview::DbObjectKind::Table,
                },
                page: 0,
                rows_per_page: 100,
                reset_h_scroll: false,
                refresh: false,
            },
        }
    }

    fn load_db_detail(generation: u64) -> FilesTask {
        FilesTask::LoadDbDetail {
            generation,
            backend: backend(),
            path: PathBuf::from("data.sqlite"),
            key: reef_sqlite_preview::DbObjectKey {
                schema: "main".to_string(),
                name: "items_by_name".to_string(),
                kind: reef_sqlite_preview::DbObjectKind::Index,
            },
        }
    }

    fn load_db_cell(generation: u64, cancellation: reef_io::CancellationToken) -> DbCellTask {
        DbCellTask {
            generation,
            backend: backend(),
            request: DbCellRequest {
                path: PathBuf::from("data.sqlite"),
                key: reef_sqlite_preview::DbObjectKey {
                    schema: "main".to_string(),
                    name: "items".to_string(),
                    kind: reef_sqlite_preview::DbObjectKind::Table,
                },
                row_offset: 0,
                row_locator: reef_sqlite_preview::DbRowLocator::RowId(1),
                column: 0,
                cancellation,
            },
        }
    }

    fn assert_load_preview(task: FilesTask, generation: u64, path: &str) {
        match task {
            FilesTask::LoadPreview {
                generation: got_generation,
                rel_path,
                ..
            } => {
                assert_eq!(got_generation, generation);
                assert_eq!(rel_path, PathBuf::from(path));
            }
            _ => panic!("expected LoadPreview"),
        }
    }

    #[test]
    fn coalescing_keeps_latest_load_preview_and_backlogs_other_work() {
        let (tx, rx) = mpsc::unbounded();
        let mut backlog = VecDeque::new();
        tx.send(prefetch_preview("prefetched.md")).unwrap();
        tx.send(load_preview(2, "latest.html")).unwrap();
        tx.send(build_quick_open_index(9)).unwrap();

        let selected = coalesce_preview_worker_task(load_preview(1, "old.html"), &rx, &mut backlog);

        assert_load_preview(selected, 2, "latest.html");
        assert_eq!(backlog.len(), 1);
        match backlog.pop_front().unwrap() {
            FilesTask::BuildQuickOpenIndex { generation, .. } => assert_eq!(generation, 9),
            _ => panic!("expected backlogged BuildQuickOpenIndex"),
        }
    }

    #[test]
    fn prefetch_does_not_replace_selected_load_preview() {
        let (tx, rx) = mpsc::unbounded();
        let mut backlog = VecDeque::new();
        tx.send(prefetch_preview("neighbor.md")).unwrap();

        let selected =
            coalesce_preview_worker_task(load_preview(3, "selected.md"), &rx, &mut backlog);

        assert_load_preview(selected, 3, "selected.md");
        assert!(backlog.is_empty());
    }

    #[test]
    fn pending_load_preview_jumps_ahead_of_backlogged_non_preview_work() {
        let (tx, rx) = mpsc::unbounded();
        let mut backlog = VecDeque::from([build_quick_open_index(11)]);
        tx.send(load_preview(4, "clicked.html")).unwrap();

        let selected = recv_preview_worker_task(&rx, &mut backlog).unwrap();

        assert_load_preview(selected, 4, "clicked.html");
        assert_eq!(backlog.len(), 1);
        match backlog.pop_front().unwrap() {
            FilesTask::BuildQuickOpenIndex { generation, .. } => assert_eq!(generation, 11),
            _ => panic!("expected BuildQuickOpenIndex to stay queued"),
        }
    }

    #[test]
    fn pending_load_preview_jumps_ahead_of_received_db_page() {
        let (tx, rx) = mpsc::unbounded();
        let mut backlog = VecDeque::new();
        tx.send(load_db_page(12)).unwrap();
        tx.send(load_preview(5, "clicked.html")).unwrap();

        let selected = recv_preview_worker_task(&rx, &mut backlog).unwrap();

        assert_load_preview(selected, 5, "clicked.html");
        assert_eq!(backlog.len(), 1);
        match backlog.pop_front().unwrap() {
            FilesTask::LoadDbPage { generation, .. } => assert_eq!(generation, 12),
            _ => panic!("expected LoadDbPage to stay queued"),
        }
    }

    #[test]
    fn database_content_coalescing_keeps_only_the_latest_selection() {
        let (tx, rx) = mpsc::unbounded();
        let mut backlog = VecDeque::new();
        tx.send(load_db_detail(2)).unwrap();
        tx.send(load_db_page(3)).unwrap();
        tx.send(build_quick_open_index(9)).unwrap();

        let selected = coalesce_preview_worker_task(load_db_page(1), &rx, &mut backlog);

        match selected {
            FilesTask::LoadDbPage { generation, .. } => assert_eq!(generation, 3),
            _ => panic!("expected latest database content request"),
        }
        assert_eq!(backlog.len(), 1);
        assert!(matches!(
            backlog.pop_front(),
            Some(FilesTask::BuildQuickOpenIndex { generation: 9, .. })
        ));
    }

    #[test]
    fn database_cell_queue_cancels_obsolete_requests() {
        let (tx, rx) = mpsc::unbounded();
        let obsolete_cancellation = reef_io::CancellationToken::default();
        tx.send(load_db_cell(1, obsolete_cancellation.clone()))
            .unwrap();
        tx.send(load_db_cell(2, reef_io::CancellationToken::default()))
            .unwrap();

        let selected = recv_latest_db_cell_task(&rx).unwrap();

        assert_eq!(selected.generation, 2);
        assert!(obsolete_cancellation.is_cancelled());
        assert!(!selected.request.cancellation.is_cancelled());
    }

    #[test]
    fn preview_worker_publishes_plain_content_before_enrichment_is_requested() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("main.rs"), "fn main() {}\n").unwrap();
        let tasks = TaskCoordinator::new();
        let wake = tasks.worker_wake_receiver();
        tasks.load_preview(
            1,
            Arc::new(reef_io::LocalBackend::open_at(tmp.path().to_path_buf())),
            PathBuf::from("main.rs"),
            false,
        );

        let first = recv_worker_result(&tasks, &wake);
        let WorkerResult::Preview {
            generation,
            result: Ok(Some(content)),
        } = first
        else {
            panic!("base preview must be published first");
        };
        assert_eq!(generation, 1);
        let PreviewBody::Text(text) = &content.body else {
            panic!("expected text preview");
        };
        assert!(text.highlighted.is_none());
        assert!(text.parsed.is_none());

        assert!(tasks.enrich_preview(generation, &content, false));
        let second = recv_worker_result(&tasks, &wake);
        let WorkerResult::PreviewEnrichmentFinished {
            generation,
            path,
            enrichment,
        } = second
        else {
            panic!("preview enrichment must follow base content");
        };
        assert_eq!(generation, 1);
        assert_eq!(path, "main.rs");
        let Some(PreviewEnrichment::Text(enrichment)) = enrichment else {
            panic!("expected text enrichment");
        };
        assert!(enrichment.highlighted.is_some());
        assert!(enrichment.parsed.is_some());
    }

    #[test]
    fn nav_preview_worker_publishes_enriched_text() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("main.rs"), "fn main() {}\n").unwrap();
        let tasks = TaskCoordinator::new();
        let wake = tasks.worker_wake_receiver();
        tasks.load_nav_preview(
            1,
            Arc::new(reef_io::LocalBackend::open_at(tmp.path().to_path_buf())),
            PathBuf::from("main.rs"),
            false,
        );

        let result = recv_worker_result(&tasks, &wake);

        assert!(matches!(
            result,
            WorkerResult::NavPreview {
                generation: 1,
                result: Ok(Some(PreviewContent {
                    body: PreviewBody::Text(ref text),
                    ..
                })),
                ..
            } if text.highlighted.is_some() && text.parsed.is_some()
        ));
    }

    #[test]
    fn markdown_preview_worker_publishes_unstyled_base_before_syntax_enrichment() {
        let tmp = tempfile::tempdir().unwrap();
        let source = "```rs\nfn main() {}\n```\n";
        std::fs::write(tmp.path().join("README.md"), source).unwrap();
        let tasks = TaskCoordinator::new();
        let wake = tasks.worker_wake_receiver();
        tasks.load_preview(
            7,
            Arc::new(reef_io::LocalBackend::open_at(tmp.path().to_path_buf())),
            PathBuf::from("README.md"),
            false,
        );

        let first = recv_worker_result(&tasks, &wake);
        let WorkerResult::Preview {
            generation,
            result: Ok(Some(content)),
        } = first
        else {
            panic!("base markdown preview must be published first");
        };
        let PreviewBody::Markdown(markdown) = &content.body else {
            panic!("expected markdown preview");
        };
        assert!(
            markdown
                .rows()
                .expect("small markdown preview should include a render model")
                .iter()
                .flatten()
                .all(|span| span.syntax.is_none())
        );

        assert!(tasks.enrich_preview(generation, &content, false));
        let second = recv_worker_result(&tasks, &wake);
        let WorkerResult::PreviewEnrichmentFinished {
            generation,
            path,
            enrichment,
        } = second
        else {
            panic!("markdown syntax enrichment must follow base content");
        };
        assert_eq!(generation, 7);
        assert_eq!(path, "README.md");
        let Some(PreviewEnrichment::Markdown(markdown)) = enrichment else {
            panic!("expected markdown enrichment");
        };
        assert_eq!(markdown.source, source);
        assert!(
            markdown
                .rows()
                .expect("enriched markdown preview should include a render model")
                .iter()
                .flatten()
                .any(|span| span.syntax.is_some())
        );
    }

    #[test]
    fn large_markdown_preview_skips_syntax_enrichment() {
        let source = format!("# Title\n\n{}", "large markdown paragraph ".repeat(24_000));
        let content = PreviewContent {
            path: "README.md".into(),
            resolved_path: None,
            local_path: None,
            bytes_on_disk: source.len() as u64,
            mime: Some("text/markdown".into()),
            body: reef_core::preview::build_textual_preview_body("README.md", &source),
        };

        assert!(matches!(content.body, PreviewBody::Markdown(_)));
        assert!(preview_enrichment_task(1, &content, false).is_none());
    }

    #[test]
    fn large_structured_preview_skips_enrichment_worker() {
        let source = r#"{"event":"open"}"#;
        let content = PreviewContent {
            path: "events.jsonl".into(),
            resolved_path: None,
            local_path: None,
            bytes_on_disk: 513 * 1024,
            mime: Some("application/x-ndjson".into()),
            body: reef_core::preview::build_textual_preview_body("events.jsonl", source),
        };

        let PreviewBody::Text(text) = &content.body else {
            panic!("expected text preview");
        };
        assert!(text.source.is_some());
        assert!(preview_enrichment_task(1, &content, false).is_none());
    }

    fn recv_worker_result(tasks: &TaskCoordinator, wake: &mpsc::Receiver<()>) -> WorkerResult {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(result) = tasks.try_recv() {
                return result;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for worker result");
            let _ = wake.recv_timeout(remaining);
        }
    }
}

#[cfg(test)]
mod graph_worker_helpers_tests {
    use super::*;

    #[test]
    fn recv_latest_discards_obsolete_queued_work() {
        let (tx, rx) = mpsc::unbounded();
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        tx.send(3).unwrap();

        assert_eq!(recv_latest(&rx).unwrap(), 3);
    }

    #[test]
    fn ref_present_in_map_matches_local_branch() {
        let mut map: HashMap<String, Vec<RefLabel>> = HashMap::new();
        map.insert("oid".into(), vec![RefLabel::Branch("main".into())]);
        assert!(ref_present_in_map(&map, "refs/heads/main"));
    }

    #[test]
    fn ref_present_in_map_matches_remote_branch() {
        let mut map: HashMap<String, Vec<RefLabel>> = HashMap::new();
        map.insert(
            "oid".into(),
            vec![RefLabel::RemoteBranch("origin/main".into())],
        );
        assert!(ref_present_in_map(&map, "refs/remotes/origin/main"));
    }

    #[test]
    fn ref_present_in_map_rejects_missing_ref() {
        let mut map: HashMap<String, Vec<RefLabel>> = HashMap::new();
        map.insert("oid".into(), vec![RefLabel::Branch("main".into())]);
        assert!(!ref_present_in_map(&map, "refs/heads/feature"));
    }

    #[test]
    fn ref_present_in_map_ignores_tags_and_head() {
        let mut map: HashMap<String, Vec<RefLabel>> = HashMap::new();
        map.insert(
            "oid".into(),
            vec![RefLabel::Head, RefLabel::Tag("v1".into())],
        );
        // Even though refs/tags/v1 exists, scope is fully-qualified
        // refs/heads/ or refs/remotes/ only. Tags / HEAD don't count.
        assert!(!ref_present_in_map(&map, "refs/heads/v1"));
        assert!(!ref_present_in_map(&map, "refs/tags/v1"));
    }

    #[test]
    fn ref_present_in_map_empty_map_returns_false() {
        let map: HashMap<String, Vec<RefLabel>> = HashMap::new();
        assert!(!ref_present_in_map(&map, "refs/heads/anything"));
    }
}
