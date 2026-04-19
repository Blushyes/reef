//! Background task coordinator.
//!
//! UI code should render cached snapshots only. Anything that can touch git,
//! the filesystem, diff generation, or syntax highlighting is routed through
//! these workers and merged back into `App` from `tick()`.

use crate::file_tree::{self, PreviewContent, TreeEntry};
use crate::git::graph::GraphRow;
use crate::git::{CommitDetail, DiffContent, FileEntry, GitRepo, RefLabel};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
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
    result_rx: mpsc::Receiver<WorkerResult>,
}

impl TaskCoordinator {
    pub fn new() -> Self {
        let (result_tx, result_rx) = mpsc::channel();
        Self {
            files_tx: spawn_files_worker(result_tx.clone()),
            git_tx: spawn_git_worker(result_tx.clone()),
            graph_tx: spawn_graph_worker(result_tx),
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
