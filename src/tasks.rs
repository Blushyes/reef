//! Background task coordinator.
//!
//! UI code should render cached snapshots only. Anything that can touch git,
//! the filesystem, diff generation, or syntax highlighting is routed through
//! these workers and merged back into `App` from `tick()`.

use crate::app::{CommitFileDiff, DiffHighlighted, HighlightedDiff};
use crate::backend::{Backend, RepoDiscoverOpts, RepoDiscoverResponse};
use crate::file_tree::{PreviewContent, TreeEntry};
use crate::git::graph::GraphRow;
use crate::git::{CommitDetail, DiffContent, FileEntry, RefLabel};
use crate::global_search::MatchHit;
use crate::paste_conflict::Resolution;
use crate::ui::highlight;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;

#[derive(Debug, Default)]
pub struct AsyncState {
    pub generation: u64,
    pub loading: bool,
    pub stale: bool,
    pub error: Option<String>,
}

impl AsyncState {
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    pub fn invalidate_stale(&mut self) {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.loading = false;
        self.stale = true;
    }

    pub fn begin(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.loading = true;
        self.stale = false;
        self.error = None;
        self.generation
    }

    pub fn complete_ok(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        self.stale = false;
        self.error = None;
        true
    }

    pub fn complete_err(&mut self, generation: u64, error: String) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        self.stale = true;
        self.error = Some(error);
        true
    }

    pub fn should_request(&self) -> bool {
        self.stale && !self.loading
    }
}

#[derive(Debug)]
pub struct GitStatusPayload {
    pub staged: Vec<FileEntry>,
    pub unstaged: Vec<FileEntry>,
    pub ahead_behind: Option<(usize, usize)>,
    pub branch_name: String,
    pub branches: Vec<String>,
}

#[derive(Debug)]
pub struct FileTreePayload {
    pub entries: Vec<TreeEntry>,
    pub selected_idx: usize,
}

#[derive(Debug)]
pub struct GraphPayload {
    pub rows: Vec<GraphRow>,
    pub ref_map: HashMap<String, Vec<RefLabel>>,
    pub cache_key: (String, u64),
}

#[derive(Debug)]
pub enum WorkerResult {
    RepoCatalog {
        generation: u64,
        result: Result<RepoDiscoverResponse, String>,
    },
    FileTree {
        generation: u64,
        result: Result<FileTreePayload, String>,
    },
    Preview {
        generation: u64,
        result: Result<Option<PreviewContent>, String>,
    },
    GitStatus {
        generation: u64,
        result: Result<GitStatusPayload, String>,
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
    /// but sourced from `GitRepo::get_range_file_diff`.
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
}

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

enum FilesTask {
    RebuildTree {
        generation: u64,
        backend: Arc<dyn Backend>,
        expanded: Vec<PathBuf>,
        git_statuses: HashMap<String, char>,
        selected_path: Option<PathBuf>,
        fallback_selected: usize,
    },
    LoadPreview {
        generation: u64,
        backend: Arc<dyn Backend>,
        rel_path: PathBuf,
        dark: bool,
        wants_decoded_image: bool,
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
        dark: bool,
        wants_decoded_image: bool,
    },
    /// Drag-and-drop copy: each source lands under `dest_dir`. A name
    /// collision auto-renames VSCode-style (`foo.txt` → `foo (1).txt`).
    /// Directory sources are copied recursively; symlinks are skipped
    /// (documented in `copy_sources`).
    ///
    /// Paths here are still absolute because copy sources can be external
    /// (drag-drop from Finder) or inside the workdir; the `App` layer
    /// guards external drag-drop on remote backends.
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
    DiscoverRepos {
        generation: u64,
        backend: Arc<dyn Backend>,
        opts: RepoDiscoverOpts,
    },
    RefreshStatus {
        generation: u64,
        backend: Arc<dyn Backend>,
        repo_root_rel: PathBuf,
    },
    LoadDiff {
        generation: u64,
        backend: Arc<dyn Backend>,
        repo_root_rel: PathBuf,
        path: String,
        staged: bool,
        context_lines: u32,
        /// Picks the syntect theme (dark vs light) — same role as
        /// `LoadCommitFileDiff.dark` / `LoadPreview.dark`.
        dark: bool,
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

enum GraphTask {
    RefreshGraph {
        generation: u64,
        backend: Arc<dyn Backend>,
        repo_root_rel: PathBuf,
        limit: usize,
    },
    LoadCommitDetail {
        generation: u64,
        backend: Arc<dyn Backend>,
        repo_root_rel: PathBuf,
        oid: String,
    },
    LoadCommitFileDiff {
        generation: u64,
        backend: Arc<dyn Backend>,
        repo_root_rel: PathBuf,
        oid: String,
        path: String,
        context_lines: u32,
        /// Picks the syntect theme (dark vs light) so highlighted tokens
        /// read correctly against the active UI theme — same as `load_preview`.
        dark: bool,
    },
    LoadCommitRangeDetail {
        generation: u64,
        backend: Arc<dyn Backend>,
        repo_root_rel: PathBuf,
        oldest_oid: String,
        newest_oid: String,
    },
    LoadRangeFileDiff {
        generation: u64,
        backend: Arc<dyn Backend>,
        repo_root_rel: PathBuf,
        oldest_oid: String,
        newest_oid: String,
        path: String,
        context_lines: u32,
        dark: bool,
    },
}

pub struct TaskCoordinator {
    files_tx: mpsc::Sender<FilesTask>,
    /// Dedicated channel for `FilesTask::LoadPreview`. Keeping previews
    /// on their own worker thread means a slow directory rebuild or an
    /// in-flight copy never queues in front of the image the user just
    /// clicked. Both threads can hit the `LocalBackend` preview cache
    /// safely via the internal `Mutex`.
    preview_tx: mpsc::Sender<FilesTask>,
    git_tx: mpsc::Sender<GitTask>,
    graph_tx: mpsc::Sender<GraphTask>,
    global_search_tx: mpsc::Sender<GlobalSearchTask>,
    result_rx: mpsc::Receiver<WorkerResult>,
}

impl TaskCoordinator {
    pub fn new() -> Self {
        let (result_tx, result_rx) = mpsc::channel();
        Self {
            files_tx: spawn_files_worker(result_tx.clone()),
            preview_tx: spawn_preview_worker(result_tx.clone()),
            git_tx: spawn_git_worker(result_tx.clone()),
            graph_tx: spawn_graph_worker(result_tx.clone()),
            global_search_tx: spawn_global_search_worker(result_tx),
            result_rx,
        }
    }

    pub fn try_recv(&self) -> Result<WorkerResult, mpsc::TryRecvError> {
        self.result_rx.try_recv()
    }

    pub fn rebuild_tree(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        expanded: Vec<PathBuf>,
        git_statuses: HashMap<String, char>,
        selected_path: Option<PathBuf>,
        fallback_selected: usize,
    ) {
        let _ = self.files_tx.send(FilesTask::RebuildTree {
            generation,
            backend,
            expanded,
            git_statuses,
            selected_path,
            fallback_selected,
        });
    }

    pub fn load_preview(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        rel_path: PathBuf,
        dark: bool,
        wants_decoded_image: bool,
    ) {
        // Route to the dedicated preview worker so an in-flight tree
        // rebuild or copy doesn't sit ahead of an image the user just
        // clicked on.
        let _ = self.preview_tx.send(FilesTask::LoadPreview {
            generation,
            backend,
            rel_path,
            dark,
            wants_decoded_image,
        });
    }

    /// Warm the preview cache for a file the user hasn't selected yet
    /// but probably will. Result is dropped by the worker; the cache
    /// side effect is the point.
    pub fn prefetch_preview(
        &self,
        backend: Arc<dyn Backend>,
        rel_path: PathBuf,
        dark: bool,
        wants_decoded_image: bool,
    ) {
        let _ = self.preview_tx.send(FilesTask::PrefetchPreview {
            backend,
            rel_path,
            dark,
            wants_decoded_image,
        });
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

    pub fn refresh_status(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        repo_root_rel: PathBuf,
    ) {
        let _ = self.git_tx.send(GitTask::RefreshStatus {
            generation,
            backend,
            repo_root_rel,
        });
    }

    pub fn discover_repos(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        opts: RepoDiscoverOpts,
    ) {
        let _ = self.git_tx.send(GitTask::DiscoverRepos {
            generation,
            backend,
            opts,
        });
    }

    pub fn load_diff(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        repo_root_rel: PathBuf,
        path: String,
        staged: bool,
        context_lines: u32,
        dark: bool,
    ) {
        let _ = self.git_tx.send(GitTask::LoadDiff {
            generation,
            backend,
            repo_root_rel,
            path,
            staged,
            context_lines,
            dark,
        });
    }

    pub fn refresh_graph(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        repo_root_rel: PathBuf,
        limit: usize,
    ) {
        let _ = self.graph_tx.send(GraphTask::RefreshGraph {
            generation,
            backend,
            repo_root_rel,
            limit,
        });
    }

    pub fn load_commit_detail(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        repo_root_rel: PathBuf,
        oid: String,
    ) {
        let _ = self.graph_tx.send(GraphTask::LoadCommitDetail {
            generation,
            backend,
            repo_root_rel,
            oid,
        });
    }

    pub fn load_commit_file_diff(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        repo_root_rel: PathBuf,
        oid: String,
        path: String,
        context_lines: u32,
        dark: bool,
    ) {
        let _ = self.graph_tx.send(GraphTask::LoadCommitFileDiff {
            generation,
            backend,
            repo_root_rel,
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
        repo_root_rel: PathBuf,
        oldest_oid: String,
        newest_oid: String,
    ) {
        let _ = self.graph_tx.send(GraphTask::LoadCommitRangeDetail {
            generation,
            backend,
            repo_root_rel,
            oldest_oid,
            newest_oid,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn load_range_file_diff(
        &self,
        generation: u64,
        backend: Arc<dyn Backend>,
        repo_root_rel: PathBuf,
        oldest_oid: String,
        newest_oid: String,
        path: String,
        context_lines: u32,
        dark: bool,
    ) {
        let _ = self.graph_tx.send(GraphTask::LoadRangeFileDiff {
            generation,
            backend,
            repo_root_rel,
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
    /// `WorkerResult::GlobalSearchChunk` followed by a terminal
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

fn spawn_files_worker(result_tx: mpsc::Sender<WorkerResult>) -> mpsc::Sender<FilesTask> {
    let (tx, rx) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("reef-files-worker".into())
        .spawn(move || {
            while let Ok(task) = rx.recv() {
                match task {
                    FilesTask::RebuildTree {
                        generation,
                        backend,
                        expanded,
                        git_statuses,
                        selected_path,
                        fallback_selected,
                    } => {
                        let result = build_file_tree_payload(
                            backend.as_ref(),
                            expanded,
                            git_statuses,
                            selected_path,
                            fallback_selected,
                        );
                        let _ = result_tx.send(WorkerResult::FileTree { generation, result });
                    }
                    FilesTask::LoadPreview {
                        generation,
                        backend,
                        rel_path,
                        dark,
                        wants_decoded_image,
                    } => {
                        let result = Ok(backend.load_preview(&rel_path, dark, wants_decoded_image));
                        let _ = result_tx.send(WorkerResult::Preview { generation, result });
                    }
                    FilesTask::CopyFiles {
                        generation,
                        backend,
                        sources,
                        dest_dir,
                    } => {
                        let result = copy_sources(backend.as_ref(), &sources, &dest_dir);
                        let _ = result_tx.send(WorkerResult::FileCopy { generation, result });
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
                    // Prefetch routes to the preview worker; this arm
                    // never fires in practice but exhaustiveness needs
                    // it.
                    FilesTask::PrefetchPreview { .. } => {}
                }
            }
        });
    tx
}

// ─── FS mutation helpers ─────────────────────────────────────────────────────
//
// These direct-fs helpers used to live on the worker path; M3 routed the
// workers through `Backend` so the local/remote implementations stay byte-
// equivalent. The helpers remain because the unit tests in
// `fs_mutation_tests` still exercise them as a regression guard for the
// original `std::fs::*` semantics.

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
    // short; if the user trashed many at once (future: multi-select) the
    // toast can reach into the worker task's list itself.
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

// ─── Drag-and-drop copy helpers ──────────────────────────────────────────────

/// Copy every `source` into `dest_dir`, auto-renaming on name collision
/// (`foo.txt` → `foo (1).txt` → `foo (2).txt`, …). Directory sources are
/// recursively copied; the rename rule only applies to the top-level name.
///
/// Symlinks encountered during a recursive directory walk are skipped —
/// Finder's default would be to dereference them and copy the target, but
/// that widens scope (cycles, broken links, permission surprises) beyond
/// what this first cut needs to handle. The renderer-side banner flags
/// "recursive copy"; if symlink fidelity becomes important we can revisit.
///
/// Returns the count of top-level items successfully placed, or the first
/// error encountered. We fail fast on the first error rather than
/// best-effort so a partial copy doesn't silently miss a file and leave
/// the user thinking everything succeeded.
fn copy_sources(
    backend: &dyn Backend,
    sources: &[PathBuf],
    dest_dir: &Path,
) -> Result<usize, String> {
    let workdir = backend.workdir_path();
    let dest_rel = dest_dir
        .strip_prefix(&workdir)
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let is_remote = backend.is_remote();

    // On remote, `dest_dir` is an absolute path on the REMOTE host (the
    // workdir passed to `--ssh`). It doesn't exist on this machine, so
    // the local-filesystem probes below (`canonicalize`, `.is_dir()`)
    // would fail. We defer all existence checks to the backend for
    // remote and only run the name-conflict arithmetic off the
    // workdir-relative shape.
    let canon_dest = if is_remote {
        None
    } else {
        if !dest_dir.is_dir() {
            return Err(format!("destination is not a directory: {:?}", dest_dir));
        }
        // Canonicalise dest_dir once up front so the self-reference
        // check below works even when `dest_dir` was passed in as a
        // relative or symlinked path.
        Some(
            dest_dir
                .canonicalize()
                .map_err(|e| format!("cannot canonicalize destination {:?}: {}", dest_dir, e))?,
        )
    };

    let mut count = 0;
    for source in sources {
        let basename = source
            .file_name()
            .ok_or_else(|| format!("source has no basename: {:?}", source))?;

        // P0 safety for local copies: block copying a directory INTO
        // itself or any of its descendants. Remote uploads can't hit
        // this case — the source is on the client and the dest is on
        // the server.
        if !is_remote && source.is_dir() {
            let canon_src = source
                .canonicalize()
                .map_err(|e| format!("cannot canonicalize source {:?}: {}", source, e))?;
            let canon_dest = canon_dest.as_ref().unwrap();
            if canon_dest == &canon_src || canon_dest.starts_with(&canon_src) {
                return Err(format!(
                    "cannot copy {:?} into itself or a descendant {:?}",
                    source, dest_dir
                ));
            }
        }

        // `resolve_name_conflict` probes the local disk with `.exists()`.
        // On remote we don't have access to the tree, so we skip auto-
        // rename and let the agent reject a collision via
        // `BackendError::PathExists`. Dropping a duplicate name twice on
        // a remote tree is an error rather than an auto-rename; that's a
        // step down from local behaviour but matches the protocol:
        // `CreateFile`/`CopyFile` use `create_new` semantics.
        let final_dest: PathBuf = if is_remote {
            dest_dir.join(basename)
        } else {
            resolve_name_conflict(dest_dir, basename)
        };

        let src_rel = source.strip_prefix(&workdir).ok();
        let final_rel = final_dest
            .strip_prefix(&workdir)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| {
                // For remote external uploads, `final_dest` starts with
                // the remote workdir string so strip_prefix succeeds;
                // for local out-of-tree paths we fall back to the
                // basename relative to `dest_rel`.
                dest_rel.join(basename)
            });

        if source.is_dir() {
            match src_rel {
                Some(s) => backend
                    .copy_dir_recursive(s, &final_rel)
                    .map_err(|e| format!("copy {:?} → {:?}: {}", source, final_dest, e))?,
                None => {
                    // External (host-local) source → needs the upload
                    // hook so remote backends can scp and local
                    // backends can still do a plain recursive copy.
                    backend
                        .upload_from_local(source, &final_rel)
                        .map_err(|e| format!("upload {:?} → {:?}: {}", source, final_dest, e))?;
                }
            }
        } else {
            match src_rel {
                Some(s) => backend
                    .copy_file(s, &final_rel)
                    .map_err(|e| format!("copy {:?} → {:?}: {}", source, final_dest, e))?,
                None => backend
                    .upload_from_local(source, &final_rel)
                    .map_err(|e| format!("upload {:?} → {:?}: {}", source, final_dest, e))?,
            }
        }
        count += 1;
    }
    Ok(count)
}

/// Drive a Cut/Copy paste batch — `items` is the per-source decision
/// list, with conflict resolutions baked in by the App. Each item lands
/// at `dest_dir/<basename>` (or `dest_dir/<keep-both-name>`); `Replace`
/// pre-trashes the existing destination so the user can recover via OS
/// Trash. `Skip` and `Cancel` are noops.
///
/// Fail-fast on the first error to match `copy_sources` semantics —
/// callers prefer one clear error over a partial-completion riddle.
/// `placed` counts items that successfully landed *before* any error,
/// so the toast can still report progress.
///
/// Remote-backend cost: this loop issues one RPC per item (plus an
/// extra `trash` RPC per `Replace`). A 50-item Replace paste over SSH
/// = ~100 round-trips; on a 200ms-RTT link that's ~10s of latency
/// dominating any actual transfer cost. Batching `trash` and
/// `rename`/`copy` would need new `Backend::trash_multi` /
/// `Backend::rename_multi` entry points and matching agent-side
/// handlers — out of scope for v1, but the obvious follow-up if real-
/// world reports surface it. Local-backend per-item cost is in the
/// microseconds and not worth batching.
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
        //      Trash safety net. Semi-surprising, but flagging it
        //      reliably needs a probe at startup; v1 trade-off.
        // Follow-up worth doing if real-world reports surface: thread
        // the trash result back into `FsMutationKind::Moved/CopiedTo`
        // so the toast can warn "overwrote without trash".
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

/// Find the first non-existing destination filename by appending
/// ` (N)` to the stem for N = 1, 2, 3… . Matches VSCode's Finder-style
/// duplicate behavior. Dotfiles (leading-dot, no extension) get the
/// counter after the whole name: `.env` → `.env (1)`.
fn resolve_name_conflict(dest_dir: &Path, basename: &std::ffi::OsStr) -> PathBuf {
    let candidate = dest_dir.join(basename);
    if !candidate.exists() {
        return candidate;
    }
    let name = basename.to_string_lossy().into_owned();
    let (stem, ext) = split_stem_ext(&name);
    for n in 1..u32::MAX {
        let renamed = match ext {
            Some(e) => format!("{} ({}).{}", stem, n, e),
            None => format!("{} ({})", stem, n),
        };
        let c = dest_dir.join(&renamed);
        if !c.exists() {
            return c;
        }
    }
    // Astronomically unlikely; fall back to a timestamp-style name so we
    // never loop forever or panic in a production run.
    dest_dir.join(format!(
        "{}-{}",
        name,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ))
}

/// Split a filename into `(stem, ext)` at the LAST dot — matching the way
/// users read filenames. Leading-dot files (no embedded dot) are treated
/// as "no extension" so `.env` → (".env", None), not ("", "env").
fn split_stem_ext(name: &str) -> (&str, Option<&str>) {
    // A leading dot doesn't count as an extension separator.
    let trimmed = name.trim_start_matches('.');
    let leading_dots = name.len() - trimmed.len();
    match trimmed.rfind('.') {
        Some(rel) => {
            let abs = leading_dots + rel;
            let (stem, ext) = name.split_at(abs);
            // ext starts with '.', skip it
            (stem, Some(&ext[1..]))
        }
        None => (name, None),
    }
}

/// Dedicated worker thread for `FilesTask::LoadPreview`. Same task
/// shape as the main files worker, but sitting on its own channel so
/// slow preview decodes can't queue behind a big tree rebuild or a
/// long-running copy. Non-preview tasks arriving here are silently
/// ignored — they're never routed to `preview_tx` in practice.
fn spawn_preview_worker(result_tx: mpsc::Sender<WorkerResult>) -> mpsc::Sender<FilesTask> {
    let (tx, rx) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("reef-preview-worker".into())
        .spawn(move || {
            while let Ok(task) = rx.recv() {
                match task {
                    FilesTask::LoadPreview {
                        generation,
                        backend,
                        rel_path,
                        dark,
                        wants_decoded_image,
                    } => {
                        let result = Ok(backend.load_preview(&rel_path, dark, wants_decoded_image));
                        let _ = result_tx.send(WorkerResult::Preview { generation, result });
                    }
                    FilesTask::PrefetchPreview {
                        backend,
                        rel_path,
                        dark,
                        wants_decoded_image,
                    } => {
                        // Fire-and-forget: the backend's LRU cache
                        // absorbs the result. No WorkerResult because
                        // the main thread has nothing to apply here.
                        let _ = backend.load_preview(&rel_path, dark, wants_decoded_image);
                    }
                    _ => {}
                }
            }
        });
    tx
}

fn spawn_git_worker(result_tx: mpsc::Sender<WorkerResult>) -> mpsc::Sender<GitTask> {
    let (tx, rx) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("reef-git-worker".into())
        .spawn(move || {
            while let Ok(task) = rx.recv() {
                match task {
                    GitTask::DiscoverRepos {
                        generation,
                        backend,
                        opts,
                    } => {
                        let result = backend.discover_repos(&opts).map_err(|e| e.to_string());
                        let _ = result_tx.send(WorkerResult::RepoCatalog { generation, result });
                    }
                    GitTask::RefreshStatus {
                        generation,
                        backend,
                        repo_root_rel,
                    } => {
                        let result = (|| -> Result<GitStatusPayload, String> {
                            let snap = backend
                                .git_status_for(&repo_root_rel)
                                .map_err(|e| e.to_string())?;
                            let ref_map = backend
                                .list_refs_for(&repo_root_rel)
                                .map_err(|e| e.to_string())?;
                            Ok(GitStatusPayload {
                                staged: snap.staged,
                                unstaged: snap.unstaged,
                                ahead_behind: snap.ahead_behind,
                                branch_name: snap.branch_name,
                                branches: branch_names_from_refs(&ref_map),
                            })
                        })();
                        let _ = result_tx.send(WorkerResult::GitStatus { generation, result });
                    }
                    GitTask::LoadDiff {
                        generation,
                        backend,
                        repo_root_rel,
                        path,
                        staged,
                        context_lines,
                        dark,
                    } => {
                        // Merge: diff data via backend (remote-aware),
                        // then apply v0.14.0's syntect highlighting on
                        // the client side.
                        let result = if staged {
                            backend.staged_diff_for(&repo_root_rel, &path, context_lines)
                        } else {
                            backend.unstaged_diff_for(&repo_root_rel, &path, context_lines)
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

fn spawn_graph_worker(result_tx: mpsc::Sender<WorkerResult>) -> mpsc::Sender<GraphTask> {
    let (tx, rx) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("reef-graph-worker".into())
        .spawn(move || {
            while let Ok(task) = rx.recv() {
                match task {
                    GraphTask::RefreshGraph {
                        generation,
                        backend,
                        repo_root_rel,
                        limit,
                    } => {
                        let result = (|| -> Result<GraphPayload, String> {
                            let head = backend
                                .head_oid_for(&repo_root_rel)
                                .map_err(|e| e.to_string())?
                                .unwrap_or_default();
                            let ref_map = backend
                                .list_refs_for(&repo_root_rel)
                                .map_err(|e| e.to_string())?;
                            let refs_hash = hash_ref_map(&ref_map);
                            let commits = backend
                                .list_commits_for(&repo_root_rel, limit)
                                .map_err(|e| e.to_string())?;
                            let rows = crate::git::graph::build_graph(&commits);
                            Ok(GraphPayload {
                                rows,
                                ref_map,
                                cache_key: (head, refs_hash),
                            })
                        })();
                        let _ = result_tx.send(WorkerResult::Graph { generation, result });
                    }
                    GraphTask::LoadCommitDetail {
                        generation,
                        backend,
                        repo_root_rel,
                        oid,
                    } => {
                        let result = backend
                            .commit_detail_for(&repo_root_rel, &oid)
                            .map_err(|e| e.to_string());
                        let _ = result_tx.send(WorkerResult::CommitDetail { generation, result });
                    }
                    GraphTask::LoadCommitFileDiff {
                        generation,
                        backend,
                        repo_root_rel,
                        oid,
                        path,
                        context_lines,
                        dark,
                    } => {
                        let result = backend
                            .commit_file_diff_for(&repo_root_rel, &oid, &path, context_lines)
                            .map_err(|e| e.to_string())
                            .map(|opt| opt.map(|diff| build_commit_file_diff(path, diff, dark)));
                        let _ = result_tx.send(WorkerResult::CommitFileDiff { generation, result });
                    }
                    GraphTask::LoadCommitRangeDetail {
                        generation,
                        backend,
                        repo_root_rel,
                        oldest_oid,
                        newest_oid,
                    } => {
                        let result = backend
                            .range_files_for(&repo_root_rel, &oldest_oid, &newest_oid)
                            .map_err(|e| e.to_string());
                        let _ = result_tx.send(WorkerResult::RangeDetail { generation, result });
                    }
                    GraphTask::LoadRangeFileDiff {
                        generation,
                        backend,
                        repo_root_rel,
                        oldest_oid,
                        newest_oid,
                        path,
                        context_lines,
                        dark,
                    } => {
                        let result = backend
                            .range_file_diff_for(
                                &repo_root_rel,
                                &oldest_oid,
                                &newest_oid,
                                &path,
                                context_lines,
                            )
                            .map_err(|e| e.to_string())
                            .map(|opt| opt.map(|diff| build_commit_file_diff(path, diff, dark)));
                        let _ = result_tx.send(WorkerResult::RangeFileDiff { generation, result });
                    }
                }
            }
        });
    tx
}

/// Run syntect over the diff's content lines once per file and split the
/// flat result into per-hunk slices so the renderer can index by
/// `(hunk, line)`. Runs in worker threads — keeps the UI smooth on large
/// diffs (a 10k-line diff takes ~50ms). Lines are fed through a single
/// `HighlightLines` instance so state (e.g. open block comments) persists
/// across hunks; this matches delta/bat's pragmatic approach of treating
/// the hunk stream as a pseudo-file when the full file isn't available.
/// Added/removed/context lines are mixed together — accepted imprecision
/// for the 90% case. Returns `None` when no syntax resolves (unknown
/// extension, binary, etc.), letting the renderer fall back to plain
/// per-tag colors.
fn highlight_diff(path: &str, diff: &DiffContent, dark: bool) -> Option<DiffHighlighted> {
    let mut flat: Vec<String> = Vec::new();
    let mut hunk_lens: Vec<usize> = Vec::with_capacity(diff.hunks.len());
    for hunk in &diff.hunks {
        hunk_lens.push(hunk.lines.len());
        for line in &hunk.lines {
            flat.push(line.content.clone());
        }
    }
    highlight::highlight_file(path, &flat, dark).map(|flat_tokens| {
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
    })
}

fn build_commit_file_diff(path: String, diff: DiffContent, dark: bool) -> CommitFileDiff {
    let highlighted = highlight_diff(&path, &diff, dark);
    CommitFileDiff {
        path,
        diff,
        highlighted,
    }
}

fn build_highlighted_diff(path: &str, diff: DiffContent, dark: bool) -> HighlightedDiff {
    let highlighted = highlight_diff(path, &diff, dark);
    HighlightedDiff { diff, highlighted }
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

fn branch_names_from_refs(map: &HashMap<String, Vec<RefLabel>>) -> Vec<String> {
    let mut branches: Vec<String> = map
        .values()
        .flat_map(|labels| labels.iter())
        .filter_map(|label| match label {
            RefLabel::Branch(name) => Some(name.clone()),
            _ => None,
        })
        .collect();
    branches.sort();
    branches.dedup();
    branches
}

// ─── Global-search worker ───────────────────────────────────────────────────

fn spawn_global_search_worker(
    result_tx: mpsc::Sender<WorkerResult>,
) -> mpsc::Sender<GlobalSearchTask> {
    let (tx, rx) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("reef-global-search-worker".into())
        .spawn(move || {
            // Drain new tasks as they arrive. A task starting while the previous
            // one is still running won't happen in practice (App::tick only
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

/// Run one global search via `backend.search_content`, forwarding each
/// backend-emitted chunk as a `WorkerResult::GlobalSearchChunk` so the
/// UI sees partial results within ~one chunk of walker output instead
/// of waiting for the whole walk. Returns `truncated = true` iff the
/// backend reported hitting the hit cap.
///
/// Cancellation: the sink returns `ControlFlow::Break(())` once `cancel`
/// flips. The Local backend honours this at the next file boundary; the
/// Remote backend stops forwarding to the UI but lets the agent finish
/// the walk naturally (we don't have a "cancel this request" wire op
/// yet — adding one would be the obvious follow-up if mis-typing a
/// pattern on a huge remote monorepo proves costly).
fn run_global_search_via_backend(
    generation: u64,
    cancel: Arc<AtomicBool>,
    backend: &dyn Backend,
    query: &str,
    result_tx: &mpsc::Sender<WorkerResult>,
) -> bool {
    if query.is_empty() {
        return false;
    }
    let request = crate::backend::ContentSearchRequest {
        pattern: query.to_string(),
        fixed_strings: true,
        case_sensitive: None,
        max_results: crate::global_search::MAX_RESULTS as u32,
        max_line_chars: crate::global_search::MAX_LINE_CHARS as u32,
    };

    let mut on_chunk = |hits: Vec<crate::backend::ContentMatchHit>| -> std::ops::ControlFlow<()> {
        if cancel.load(Ordering::Relaxed) {
            return std::ops::ControlFlow::Break(());
        }
        if hits.is_empty() {
            return std::ops::ControlFlow::Continue(());
        }
        let ui_hits: Vec<MatchHit> = hits
            .into_iter()
            .map(|h| MatchHit {
                path: h.path,
                display: h.display,
                line: h.line,
                line_text: h.line_text,
                byte_range: h.byte_range,
            })
            .collect();
        // If the result channel is gone the App has torn down; stop
        // trying to push chunks but let the backend tidy up on its
        // own schedule.
        if result_tx
            .send(WorkerResult::GlobalSearchChunk {
                generation,
                hits: ui_hits,
            })
            .is_err()
        {
            return std::ops::ControlFlow::Break(());
        }
        std::ops::ControlFlow::Continue(())
    };

    match backend.search_content(&request, &mut on_chunk) {
        Ok(completed) => completed.truncated,
        Err(_) => false,
    }
}

#[cfg(test)]
mod copy_tests {
    use super::*;
    use crate::backend::LocalBackend;
    use std::fs;
    use tempfile::TempDir;

    /// Build a `LocalBackend` rooted at `root`. The backend is wrapped in
    /// `Arc<dyn Backend>` because `copy_sources` now takes a `&dyn Backend`.
    fn test_backend(root: &std::path::Path) -> LocalBackend {
        LocalBackend::open_at(root.to_path_buf())
    }

    #[test]
    fn split_stem_ext_basic() {
        assert_eq!(split_stem_ext("foo.txt"), ("foo", Some("txt")));
        assert_eq!(
            split_stem_ext("archive.tar.gz"),
            ("archive.tar", Some("gz"))
        );
        assert_eq!(split_stem_ext("README"), ("README", None));
        // Leading dot alone is not an extension separator.
        assert_eq!(split_stem_ext(".env"), (".env", None));
        // But `.env.local` has a real separator after the leading dot.
        assert_eq!(split_stem_ext(".env.local"), (".env", Some("local")));
    }

    #[test]
    fn resolve_name_conflict_increments() {
        let tmp = TempDir::new().unwrap();
        let basename = std::ffi::OsString::from("foo.txt");

        // First call returns the plain name.
        let p0 = resolve_name_conflict(tmp.path(), &basename);
        assert_eq!(p0.file_name().unwrap(), "foo.txt");

        // Once the plain name exists, the next call renames to "(1)".
        fs::write(&p0, "").unwrap();
        let p1 = resolve_name_conflict(tmp.path(), &basename);
        assert_eq!(p1.file_name().unwrap(), "foo (1).txt");

        fs::write(&p1, "").unwrap();
        let p2 = resolve_name_conflict(tmp.path(), &basename);
        assert_eq!(p2.file_name().unwrap(), "foo (2).txt");
    }

    #[test]
    fn resolve_name_conflict_dotfile() {
        let tmp = TempDir::new().unwrap();
        let basename = std::ffi::OsString::from(".env");
        fs::write(tmp.path().join(".env"), "").unwrap();
        let p = resolve_name_conflict(tmp.path(), &basename);
        assert_eq!(p.file_name().unwrap(), ".env (1)");
    }

    #[test]
    fn copy_sources_file_into_dir() {
        let src_tmp = TempDir::new().unwrap();
        let dst_tmp = TempDir::new().unwrap();
        let src = src_tmp.path().join("alpha.txt");
        fs::write(&src, "hello").unwrap();

        let b = test_backend(dst_tmp.path());
        let count = copy_sources(&b, &[src], dst_tmp.path()).unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            fs::read_to_string(dst_tmp.path().join("alpha.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn copy_sources_recurses_into_directories() {
        let src_tmp = TempDir::new().unwrap();
        let dst_tmp = TempDir::new().unwrap();

        let pkg = src_tmp.path().join("pkg");
        fs::create_dir(&pkg).unwrap();
        fs::write(pkg.join("one.txt"), "1").unwrap();
        fs::create_dir(pkg.join("nested")).unwrap();
        fs::write(pkg.join("nested").join("two.txt"), "2").unwrap();

        let b = test_backend(dst_tmp.path());
        let count = copy_sources(&b, std::slice::from_ref(&pkg), dst_tmp.path()).unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            fs::read_to_string(dst_tmp.path().join("pkg").join("one.txt")).unwrap(),
            "1"
        );
        assert_eq!(
            fs::read_to_string(dst_tmp.path().join("pkg").join("nested").join("two.txt")).unwrap(),
            "2"
        );
    }

    #[test]
    fn copy_sources_auto_renames_on_collision() {
        let src_tmp = TempDir::new().unwrap();
        let dst_tmp = TempDir::new().unwrap();

        let src = src_tmp.path().join("dup.txt");
        fs::write(&src, "new").unwrap();
        // Pre-populate dest with the same basename.
        fs::write(dst_tmp.path().join("dup.txt"), "old").unwrap();

        let b = test_backend(dst_tmp.path());
        copy_sources(&b, &[src], dst_tmp.path()).unwrap();
        // Original untouched.
        assert_eq!(
            fs::read_to_string(dst_tmp.path().join("dup.txt")).unwrap(),
            "old"
        );
        // New landed with counter.
        assert_eq!(
            fs::read_to_string(dst_tmp.path().join("dup (1).txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn copy_sources_rejects_non_directory_dest() {
        let dst_tmp = TempDir::new().unwrap();
        let not_a_dir = dst_tmp.path().join("nope.txt");
        fs::write(&not_a_dir, "").unwrap();
        let src_tmp = TempDir::new().unwrap();
        let src = src_tmp.path().join("x");
        fs::write(&src, "").unwrap();
        let b = test_backend(dst_tmp.path());
        assert!(copy_sources(&b, &[src], &not_a_dir).is_err());
    }

    #[test]
    fn copy_sources_blocks_copy_into_self() {
        // Regression guard for the infinite-recursion bug where dropping
        // a directory onto itself would walk the tree while creating
        // new subdirectories under the walked path, blowing out
        // PATH_MAX. Users hit this by dragging `src/` from Finder and
        // dropping it onto `src/` in the tree — the bug fills disk.
        let tmp = TempDir::new().unwrap();
        let pkg = tmp.path().join("pkg");
        fs::create_dir(&pkg).unwrap();
        fs::write(pkg.join("a.txt"), "").unwrap();
        let b = test_backend(tmp.path());
        let err = copy_sources(&b, std::slice::from_ref(&pkg), &pkg).unwrap_err();
        assert!(
            err.contains("into itself") || err.contains("descendant"),
            "expected self-copy rejection, got: {err}"
        );
    }

    #[test]
    fn copy_sources_blocks_copy_into_descendant() {
        // Subcase: dropping `src/` onto `src/ui/` — still recursive,
        // still blocked.
        let tmp = TempDir::new().unwrap();
        let pkg = tmp.path().join("pkg");
        let nested = pkg.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let b = test_backend(tmp.path());
        let err = copy_sources(&b, std::slice::from_ref(&pkg), &nested).unwrap_err();
        assert!(err.contains("into itself") || err.contains("descendant"));
    }

    #[test]
    fn copy_sources_allows_sibling_dest_same_parent() {
        // Sanity check: dropping `src/` onto its parent directory (so
        // the copy lands as an auto-renamed sibling) must still work.
        // This is the most common "duplicate" case.
        let tmp = TempDir::new().unwrap();
        let pkg = tmp.path().join("pkg");
        fs::create_dir(&pkg).unwrap();
        fs::write(pkg.join("a.txt"), "hello").unwrap();
        let b = test_backend(tmp.path());
        let count = copy_sources(&b, std::slice::from_ref(&pkg), tmp.path()).unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            fs::read_to_string(tmp.path().join("pkg (1)").join("a.txt")).unwrap(),
            "hello"
        );
    }
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

    fn make_local(tmp: &TempDir) -> crate::backend::LocalBackend {
        crate::backend::LocalBackend::open_at(tmp.path().to_path_buf())
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
}
