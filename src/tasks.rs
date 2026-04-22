//! Background task coordinator.
//!
//! UI code should render cached snapshots only. Anything that can touch git,
//! the filesystem, diff generation, or syntax highlighting is routed through
//! these workers and merged back into `App` from `tick()`.

use crate::app::{CommitFileDiff, DiffHighlighted, HighlightedDiff};
use crate::file_tree::{self, PreviewContent, TreeEntry};
use crate::git::graph::GraphRow;
use crate::git::{CommitDetail, DiffContent, FileEntry, GitRepo, RefLabel};
use crate::global_search::MatchHit;
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
}

enum FilesTask {
    RebuildTree {
        generation: u64,
        root: PathBuf,
        expanded: Vec<PathBuf>,
        git_statuses: HashMap<String, char>,
        selected_path: Option<PathBuf>,
        fallback_selected: usize,
    },
    LoadPreview {
        generation: u64,
        root: PathBuf,
        rel_path: PathBuf,
        dark: bool,
    },
    /// Drag-and-drop copy: each source lands under `dest_dir`. A name
    /// collision auto-renames VSCode-style (`foo.txt` → `foo (1).txt`).
    /// Directory sources are copied recursively; symlinks are skipped
    /// (documented in `copy_sources`).
    CopyFiles {
        generation: u64,
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
    },
    /// Create an empty file at `path`. Fails if the parent dir is
    /// missing or the file already exists — the UI layer
    /// (`App::commit_tree_edit`) has already validated + rejected
    /// collisions before dispatch, but a race with an external
    /// process is possible so we still surface the io::Error.
    CreateFile { generation: u64, path: PathBuf },
    /// `mkdir -p` on `path`. If the directory already exists we
    /// treat that as success (the rare race window) to avoid a
    /// surprising failure after the user explicitly asked for it.
    CreateFolder { generation: u64, path: PathBuf },
    /// `fs::rename(old, new)`. Caller guarantees `new` doesn't
    /// already exist (checked in `App::commit_tree_edit`).
    Rename {
        generation: u64,
        old_path: PathBuf,
        new_path: PathBuf,
    },
    /// Move each path to the system Trash. Uses the `trash` crate
    /// for cross-platform semantics (Finder Trash on macOS, XDG
    /// Trash on Linux, Recycle Bin on Windows).
    TrashPaths {
        generation: u64,
        paths: Vec<PathBuf>,
    },
    /// Permanent delete via `fs::remove_file` / `remove_dir_all`.
    /// Reached via Shift+Delete after the confirm dialog. Files
    /// and directories both supported; symlinks are removed by
    /// `remove_file` without dereferencing (matches `rm` on Unix).
    HardDeletePaths {
        generation: u64,
        paths: Vec<PathBuf>,
    },
}

enum GitTask {
    RefreshStatus {
        generation: u64,
        workdir: PathBuf,
    },
    LoadDiff {
        generation: u64,
        workdir: PathBuf,
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
        root: PathBuf,
        query: String,
    },
}

enum GraphTask {
    RefreshGraph {
        generation: u64,
        workdir: PathBuf,
        limit: usize,
    },
    LoadCommitDetail {
        generation: u64,
        workdir: PathBuf,
        oid: String,
    },
    LoadCommitFileDiff {
        generation: u64,
        workdir: PathBuf,
        oid: String,
        path: String,
        context_lines: u32,
        /// Picks the syntect theme (dark vs light) so highlighted tokens
        /// read correctly against the active UI theme — same as `load_preview`.
        dark: bool,
    },
    LoadCommitRangeDetail {
        generation: u64,
        workdir: PathBuf,
        oldest_oid: String,
        newest_oid: String,
    },
    LoadRangeFileDiff {
        generation: u64,
        workdir: PathBuf,
        oldest_oid: String,
        newest_oid: String,
        path: String,
        context_lines: u32,
        dark: bool,
    },
}

pub struct TaskCoordinator {
    files_tx: mpsc::Sender<FilesTask>,
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
        root: PathBuf,
        expanded: Vec<PathBuf>,
        git_statuses: HashMap<String, char>,
        selected_path: Option<PathBuf>,
        fallback_selected: usize,
    ) {
        let _ = self.files_tx.send(FilesTask::RebuildTree {
            generation,
            root,
            expanded,
            git_statuses,
            selected_path,
            fallback_selected,
        });
    }

    pub fn load_preview(&self, generation: u64, root: PathBuf, rel_path: PathBuf, dark: bool) {
        let _ = self.files_tx.send(FilesTask::LoadPreview {
            generation,
            root,
            rel_path,
            dark,
        });
    }

    pub fn copy_files(&self, generation: u64, sources: Vec<PathBuf>, dest_dir: PathBuf) {
        let _ = self.files_tx.send(FilesTask::CopyFiles {
            generation,
            sources,
            dest_dir,
        });
    }

    pub fn create_file(&self, generation: u64, path: PathBuf) {
        let _ = self
            .files_tx
            .send(FilesTask::CreateFile { generation, path });
    }

    pub fn create_folder(&self, generation: u64, path: PathBuf) {
        let _ = self
            .files_tx
            .send(FilesTask::CreateFolder { generation, path });
    }

    pub fn rename_path(&self, generation: u64, old_path: PathBuf, new_path: PathBuf) {
        let _ = self.files_tx.send(FilesTask::Rename {
            generation,
            old_path,
            new_path,
        });
    }

    pub fn trash_paths(&self, generation: u64, paths: Vec<PathBuf>) {
        let _ = self
            .files_tx
            .send(FilesTask::TrashPaths { generation, paths });
    }

    pub fn hard_delete_paths(&self, generation: u64, paths: Vec<PathBuf>) {
        let _ = self
            .files_tx
            .send(FilesTask::HardDeletePaths { generation, paths });
    }

    pub fn refresh_status(&self, generation: u64, workdir: PathBuf) {
        let _ = self.git_tx.send(GitTask::RefreshStatus {
            generation,
            workdir,
        });
    }

    pub fn load_diff(
        &self,
        generation: u64,
        workdir: PathBuf,
        path: String,
        staged: bool,
        context_lines: u32,
        dark: bool,
    ) {
        let _ = self.git_tx.send(GitTask::LoadDiff {
            generation,
            workdir,
            path,
            staged,
            context_lines,
            dark,
        });
    }

    pub fn refresh_graph(&self, generation: u64, workdir: PathBuf, limit: usize) {
        let _ = self.graph_tx.send(GraphTask::RefreshGraph {
            generation,
            workdir,
            limit,
        });
    }

    pub fn load_commit_detail(&self, generation: u64, workdir: PathBuf, oid: String) {
        let _ = self.graph_tx.send(GraphTask::LoadCommitDetail {
            generation,
            workdir,
            oid,
        });
    }

    pub fn load_commit_file_diff(
        &self,
        generation: u64,
        workdir: PathBuf,
        oid: String,
        path: String,
        context_lines: u32,
        dark: bool,
    ) {
        let _ = self.graph_tx.send(GraphTask::LoadCommitFileDiff {
            generation,
            workdir,
            oid,
            path,
            context_lines,
            dark,
        });
    }

    pub fn load_commit_range_detail(
        &self,
        generation: u64,
        workdir: PathBuf,
        oldest_oid: String,
        newest_oid: String,
    ) {
        let _ = self.graph_tx.send(GraphTask::LoadCommitRangeDetail {
            generation,
            workdir,
            oldest_oid,
            newest_oid,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn load_range_file_diff(
        &self,
        generation: u64,
        workdir: PathBuf,
        oldest_oid: String,
        newest_oid: String,
        path: String,
        context_lines: u32,
        dark: bool,
    ) {
        let _ = self.graph_tx.send(GraphTask::LoadRangeFileDiff {
            generation,
            workdir,
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
        root: PathBuf,
        query: String,
    ) {
        let _ = self.global_search_tx.send(GlobalSearchTask::Run {
            generation,
            cancel,
            root,
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
                        root,
                        expanded,
                        git_statuses,
                        selected_path,
                        fallback_selected,
                    } => {
                        let result = Ok(build_file_tree_payload(
                            root,
                            expanded,
                            git_statuses,
                            selected_path,
                            fallback_selected,
                        ));
                        let _ = result_tx.send(WorkerResult::FileTree { generation, result });
                    }
                    FilesTask::LoadPreview {
                        generation,
                        root,
                        rel_path,
                        dark,
                    } => {
                        let result = Ok(file_tree::load_preview(&root, &rel_path, dark));
                        let _ = result_tx.send(WorkerResult::Preview { generation, result });
                    }
                    FilesTask::CopyFiles {
                        generation,
                        sources,
                        dest_dir,
                    } => {
                        let result = copy_sources(&sources, &dest_dir);
                        let _ = result_tx.send(WorkerResult::FileCopy { generation, result });
                    }
                    FilesTask::CreateFile { generation, path } => {
                        let (kind, result) = run_create_file(&path);
                        let _ = result_tx.send(WorkerResult::FsMutation {
                            generation,
                            kind,
                            result,
                        });
                    }
                    FilesTask::CreateFolder { generation, path } => {
                        let (kind, result) = run_create_folder(&path);
                        let _ = result_tx.send(WorkerResult::FsMutation {
                            generation,
                            kind,
                            result,
                        });
                    }
                    FilesTask::Rename {
                        generation,
                        old_path,
                        new_path,
                    } => {
                        let (kind, result) = run_rename(&old_path, &new_path);
                        let _ = result_tx.send(WorkerResult::FsMutation {
                            generation,
                            kind,
                            result,
                        });
                    }
                    FilesTask::TrashPaths { generation, paths } => {
                        let (kind, result) = run_trash(&paths);
                        let _ = result_tx.send(WorkerResult::FsMutation {
                            generation,
                            kind,
                            result,
                        });
                    }
                    FilesTask::HardDeletePaths { generation, paths } => {
                        let (kind, result) = run_hard_delete(&paths);
                        let _ = result_tx.send(WorkerResult::FsMutation {
                            generation,
                            kind,
                            result,
                        });
                    }
                }
            }
        });
    tx
}

// ─── FS mutation helpers ─────────────────────────────────────────────────────

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

fn run_create_folder(path: &Path) -> (FsMutationKind, Result<(), String>) {
    let name = basename_str(path);
    let kind = FsMutationKind::CreatedFolder { name: name.clone() };
    // `create_dir_all` treats an existing directory as success; fine here
    // because the UI has already checked for name collisions with files.
    let result = std::fs::create_dir_all(path).map_err(|e| format!("mkdir {name:?}: {e}"));
    (kind, result)
}

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

fn run_trash(paths: &[PathBuf]) -> (FsMutationKind, Result<(), String>) {
    // The kind string reports the first path's basename to keep the toast
    // short; if the user trashed many at once (future: multi-select) the
    // toast can reach into the worker task's list itself.
    let name = paths.first().map(|p| basename_str(p)).unwrap_or_default();
    let kind = FsMutationKind::Trashed { name: name.clone() };
    let result = trash::delete_all(paths).map_err(|e| format!("trash {name:?}: {e}"));
    (kind, result)
}

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
fn copy_sources(sources: &[PathBuf], dest_dir: &Path) -> Result<usize, String> {
    if !dest_dir.is_dir() {
        return Err(format!("destination is not a directory: {:?}", dest_dir));
    }
    // Canonicalise dest_dir once up front so the self-reference check below
    // works even when `dest_dir` was passed in as a relative or symlinked
    // path. `canonicalize` resolves symlinks to their real targets, which
    // is exactly what we want — a symlinked dest pointing INTO a source
    // is just as dangerous as a direct path.
    let canon_dest = dest_dir
        .canonicalize()
        .map_err(|e| format!("cannot canonicalize destination {:?}: {}", dest_dir, e))?;
    let mut count = 0;
    for source in sources {
        let basename = source
            .file_name()
            .ok_or_else(|| format!("source has no basename: {:?}", source))?;

        // P0 safety: block copying a directory INTO itself or any of its
        // descendants. Without this, `copy_dir_recursive` walks the
        // source tree while the destination (which we just created under
        // it) keeps appearing as a new subdirectory — producing an
        // infinite nest of folders until the filesystem rejects the
        // path length. Seen concretely when a user drags a project's
        // `src/` folder out of Finder and drops it back onto `src/` in
        // the file tree.
        if source.is_dir() {
            let canon_src = source
                .canonicalize()
                .map_err(|e| format!("cannot canonicalize source {:?}: {}", source, e))?;
            if canon_dest == canon_src || canon_dest.starts_with(&canon_src) {
                return Err(format!(
                    "cannot copy {:?} into itself or a descendant {:?}",
                    source, dest_dir
                ));
            }
        }

        let final_dest = resolve_name_conflict(dest_dir, basename);
        if source.is_dir() {
            copy_dir_recursive(source, &final_dest)
                .map_err(|e| format!("copy {:?} → {:?}: {}", source, final_dest, e))?;
        } else {
            std::fs::copy(source, &final_dest)
                .map_err(|e| format!("copy {:?} → {:?}: {}", source, final_dest, e))?;
        }
        count += 1;
    }
    Ok(count)
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

/// Recursive directory copy using a plain DFS walk. Mirrors the source
/// tree under `dst`, creating intermediate directories as needed.
/// Symlinks are intentionally skipped (see `copy_sources` doc comment).
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let sub_src = entry.path();
        let sub_dst = dst.join(entry.file_name());
        if file_type.is_symlink() {
            continue;
        } else if file_type.is_dir() {
            copy_dir_recursive(&sub_src, &sub_dst)?;
        } else if file_type.is_file() {
            std::fs::copy(&sub_src, &sub_dst)?;
        }
    }
    Ok(())
}

fn spawn_git_worker(result_tx: mpsc::Sender<WorkerResult>) -> mpsc::Sender<GitTask> {
    let (tx, rx) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("reef-git-worker".into())
        .spawn(move || {
            while let Ok(task) = rx.recv() {
                match task {
                    GitTask::RefreshStatus {
                        generation,
                        workdir,
                    } => {
                        let result = open_repo(&workdir).map(|repo| {
                            let (staged, unstaged) = repo.get_status();
                            GitStatusPayload {
                                staged,
                                unstaged,
                                ahead_behind: repo.ahead_behind(),
                                branch_name: repo.branch_name(),
                            }
                        });
                        let _ = result_tx.send(WorkerResult::GitStatus { generation, result });
                    }
                    GitTask::LoadDiff {
                        generation,
                        workdir,
                        path,
                        staged,
                        context_lines,
                        dark,
                    } => {
                        let result = open_repo(&workdir).map(|repo| {
                            repo.get_diff(&path, staged, context_lines)
                                .map(|diff| build_highlighted_diff(&path, diff, dark))
                        });
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
                        workdir,
                        limit,
                    } => {
                        let result = open_repo(&workdir).map(|repo| {
                            let head = repo.head_oid().unwrap_or_default();
                            let ref_map = repo.list_refs();
                            let refs_hash = hash_ref_map(&ref_map);
                            let commits = repo.list_commits(limit);
                            let rows = crate::git::graph::build_graph(&commits);
                            GraphPayload {
                                rows,
                                ref_map,
                                cache_key: (head, refs_hash),
                            }
                        });
                        let _ = result_tx.send(WorkerResult::Graph { generation, result });
                    }
                    GraphTask::LoadCommitDetail {
                        generation,
                        workdir,
                        oid,
                    } => {
                        let result = open_repo(&workdir).map(|repo| repo.get_commit(&oid));
                        let _ = result_tx.send(WorkerResult::CommitDetail { generation, result });
                    }
                    GraphTask::LoadCommitFileDiff {
                        generation,
                        workdir,
                        oid,
                        path,
                        context_lines,
                        dark,
                    } => {
                        let result = open_repo(&workdir).map(|repo| {
                            repo.get_commit_file_diff(&oid, &path, context_lines)
                                .map(|diff| build_commit_file_diff(path, diff, dark))
                        });
                        let _ = result_tx.send(WorkerResult::CommitFileDiff { generation, result });
                    }
                    GraphTask::LoadCommitRangeDetail {
                        generation,
                        workdir,
                        oldest_oid,
                        newest_oid,
                    } => {
                        let result = open_repo(&workdir)
                            .map(|repo| repo.get_range_files(&oldest_oid, &newest_oid));
                        let _ = result_tx.send(WorkerResult::RangeDetail { generation, result });
                    }
                    GraphTask::LoadRangeFileDiff {
                        generation,
                        workdir,
                        oldest_oid,
                        newest_oid,
                        path,
                        context_lines,
                        dark,
                    } => {
                        let result = open_repo(&workdir).map(|repo| {
                            repo.get_range_file_diff(&oldest_oid, &newest_oid, &path, context_lines)
                                .map(|diff| build_commit_file_diff(path, diff, dark))
                        });
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
    root: PathBuf,
    expanded: Vec<PathBuf>,
    git_statuses: HashMap<String, char>,
    selected_path: Option<PathBuf>,
    fallback_selected: usize,
) -> FileTreePayload {
    let expanded: std::collections::HashSet<PathBuf> = expanded.into_iter().collect();
    let entries = file_tree::build_entries(&root, &expanded, &git_statuses);
    let selected_idx = selected_path
        .as_ref()
        .and_then(|path| entries.iter().position(|entry| &entry.path == path))
        .unwrap_or_else(|| fallback_selected.min(entries.len().saturating_sub(1)));
    FileTreePayload {
        entries,
        selected_idx,
    }
}

fn open_repo(workdir: &Path) -> Result<GitRepo, String> {
    GitRepo::open_at(workdir).map_err(|e| e.message().to_string())
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
                        root,
                        query,
                    } => {
                        let truncated =
                            run_global_search(generation, cancel, &root, &query, &result_tx);
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

/// Run one global search, streaming chunks into `result_tx`. Returns
/// `truncated` = true iff we hit [`crate::global_search::MAX_RESULTS`]
/// before the walker finished.
///
/// Error handling: any per-file IO / decode error is silently skipped — the
/// user sees fewer hits, which is strictly better than the whole search
/// falling over on a single weird file. A malformed query is also absorbed:
/// `RegexMatcherBuilder::fixed_strings(true)` means we'd only fail on some
/// extremely pathological input, in which case we return early with
/// truncated=false and no results.
fn run_global_search(
    generation: u64,
    cancel: Arc<AtomicBool>,
    root: &Path,
    query: &str,
    result_tx: &mpsc::Sender<WorkerResult>,
) -> bool {
    use grep_regex::RegexMatcherBuilder;
    use grep_searcher::{BinaryDetection, SearcherBuilder};

    if query.is_empty() {
        return false;
    }

    let matcher = match RegexMatcherBuilder::new()
        .case_smart(true)
        .fixed_strings(true)
        .build(query)
    {
        Ok(m) => m,
        Err(_) => return false,
    };

    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|dent| dent.file_name() != ".git")
        .build();

    // BinaryDetection::quit(0) skips any file whose first 8 KiB contains a
    // NUL byte (matches ripgrep's default). Default `Searcher::new()` is
    // `none` which would happily return hits from inside binaries.
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .build();
    let mut total: usize = 0;
    let mut pending: Vec<MatchHit> = Vec::with_capacity(CHUNK_SIZE);
    let mut truncated = false;

    'walk: for result in walker {
        // Poll cancel at every file boundary. Cheap (relaxed atomic load)
        // and lets a superseded search bail within a handful of files.
        if cancel.load(Ordering::Relaxed) {
            break 'walk;
        }

        let Ok(entry) = result else { continue };
        let is_file = entry.file_type().map(|ft| ft.is_file()).unwrap_or(false);
        if !is_file {
            continue;
        }
        let abs = entry.path();
        let Ok(rel) = abs.strip_prefix(root) else {
            continue;
        };
        let display = rel.to_string_lossy().to_string();
        if display.is_empty() {
            continue;
        }

        let mut sink = ChunkSink {
            rel_path: rel.to_path_buf(),
            display: display.clone(),
            pending: &mut pending,
            total: &mut total,
            cap: crate::global_search::MAX_RESULTS,
            truncated: &mut truncated,
            generation,
            result_tx,
            matcher: &matcher,
        };
        // search_path internally handles binary detection (NUL byte) and
        // UTF-8 decoding; errors are per-file and we just skip them.
        let _ = searcher.search_path(&matcher, abs, &mut sink);

        // Flush any buffered hits at each file boundary — keeps the UI
        // updating steadily on big workdirs even if a particular file
        // contributes few matches.
        if !pending.is_empty() {
            let chunk = std::mem::replace(&mut pending, Vec::with_capacity(CHUNK_SIZE));
            let _ = result_tx.send(WorkerResult::GlobalSearchChunk {
                generation,
                hits: chunk,
            });
        }

        if truncated {
            break;
        }
    }

    // Flush trailing buffer (only reached on cancel between a file's sink
    // finishing and the per-file flush — in practice rare, but defensive).
    if !pending.is_empty() {
        let _ = result_tx.send(WorkerResult::GlobalSearchChunk {
            generation,
            hits: pending,
        });
    }

    truncated
}

const CHUNK_SIZE: usize = 50;

/// Implements `grep_searcher::Sink`. Accumulates matches into `pending`,
/// checks total against `cap`, sets `truncated` when hit. Doesn't send
/// chunks itself — `run_global_search` flushes on file boundaries to keep
/// the streaming cadence tied to a natural unit (per-file) rather than an
/// arbitrary in-file batch size that breaks on single-file-heavy workdirs.
struct ChunkSink<'a> {
    rel_path: PathBuf,
    display: String,
    pending: &'a mut Vec<MatchHit>,
    total: &'a mut usize,
    cap: usize,
    truncated: &'a mut bool,
    generation: u64,
    result_tx: &'a mpsc::Sender<WorkerResult>,
    /// Borrowed so we can re-run `Matcher::find` on the line bytes to
    /// recover the in-line byte range for the highlight overlay. The
    /// `Searcher` already has this info but `SinkMatch` doesn't surface
    /// it — one extra `find` per hit is cheap for fixed-strings.
    matcher: &'a grep_regex::RegexMatcher,
}

impl<'a> grep_searcher::Sink for ChunkSink<'a> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        use grep_matcher::Matcher;

        if *self.total >= self.cap {
            *self.truncated = true;
            return Ok(false);
        }

        // SinkMatch.bytes() contains the full matched line (including the
        // trailing newline); trim it. line_number() is 1-indexed per ripgrep
        // convention — convert to 0-indexed to match our MatchLoc scheme.
        let raw = mat.bytes();
        let raw = strip_trailing_newline(raw);
        let line_text_full = match std::str::from_utf8(raw) {
            Ok(s) => s,
            Err(_) => return Ok(true), // non-UTF-8 line: skip, continue file
        };
        let line_text = crate::global_search::truncate_line(line_text_full);

        // Recover in-line match byte range. `fixed_strings + case_smart`
        // can't match a newline, so a single `find` on the line's bytes
        // suffices. Clip to the truncated line length so the renderer
        // doesn't slice past its end.
        let byte_range = self
            .matcher
            .find(raw)
            .ok()
            .flatten()
            .and_then(|m| crate::global_search::clip_range(m.start()..m.end(), line_text.len()))
            .unwrap_or(0..0);

        let hit = MatchHit {
            path: self.rel_path.clone(),
            display: self.display.clone(),
            line: mat.line_number().unwrap_or(1).saturating_sub(1) as usize,
            line_text,
            byte_range,
        };
        self.pending.push(hit);
        *self.total += 1;

        if self.pending.len() >= CHUNK_SIZE {
            let chunk = std::mem::replace(self.pending, Vec::with_capacity(CHUNK_SIZE));
            let _ = self.result_tx.send(WorkerResult::GlobalSearchChunk {
                generation: self.generation,
                hits: chunk,
            });
        }
        Ok(true)
    }
}

fn strip_trailing_newline(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    if end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && bytes[end - 1] == b'\r' {
        end -= 1;
    }
    &bytes[..end]
}

#[cfg(test)]
mod copy_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

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

        let count = copy_sources(&[src], dst_tmp.path()).unwrap();
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

        let count = copy_sources(std::slice::from_ref(&pkg), dst_tmp.path()).unwrap();
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

        copy_sources(&[src], dst_tmp.path()).unwrap();
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
        assert!(copy_sources(&[src], &not_a_dir).is_err());
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
        let err = copy_sources(std::slice::from_ref(&pkg), &pkg).unwrap_err();
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
        let err = copy_sources(std::slice::from_ref(&pkg), &nested).unwrap_err();
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
        let count = copy_sources(std::slice::from_ref(&pkg), tmp.path()).unwrap();
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
}
