//! Background task coordinator.
//!
//! UI code should render cached snapshots only. Anything that can touch git,
//! the filesystem, diff generation, or syntax highlighting is routed through
//! these workers and merged back into `App` from `tick()`.

use crate::file_tree::{self, PreviewContent, TreeEntry};
use crate::git::graph::GraphRow;
use crate::git::{CommitDetail, DiffContent, FileEntry, GitRepo, RefLabel};
use crate::global_search::MatchHit;
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
        result: Result<Option<DiffContent>, String>,
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
        result: Result<Option<(String, DiffContent)>, String>,
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
    ) {
        let _ = self.git_tx.send(GitTask::LoadDiff {
            generation,
            workdir,
            path,
            staged,
            context_lines,
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
    ) {
        let _ = self.graph_tx.send(GraphTask::LoadCommitFileDiff {
            generation,
            workdir,
            oid,
            path,
            context_lines,
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
                    } => {
                        let result = open_repo(&workdir)
                            .map(|repo| repo.get_diff(&path, staged, context_lines));
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
                    } => {
                        let result = open_repo(&workdir).map(|repo| {
                            repo.get_commit_file_diff(&oid, &path, context_lines)
                                .map(|diff| (path, diff))
                        });
                        let _ = result_tx.send(WorkerResult::CommitFileDiff { generation, result });
                    }
                }
            }
        });
    tx
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
