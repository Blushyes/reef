//! Backend abstraction for reef.
//!
//! Reef today runs entirely against the local filesystem + `git2` + a local
//! `notify` watcher + local `$EDITOR`. The `Backend` trait is the seam we
//! introduce so the same UI can be driven against a remote agent (see
//! `src/backend/remote.rs`) over an SSH stdio JSON-RPC pipe.
//!
//! Phase 0 of the remote-backend work is additive only: `LocalBackend` wraps
//! the existing `GitRepo` / `fs_watcher::spawn` / `editor::launch`
//! implementations and forwards to them unchanged. No behaviour changes.
//!
//! # Threading model
//! Every `Backend` impl is `Send + Sync` so it can live behind
//! `Arc<dyn Backend>` on `App` and be cloned into background worker threads.
//! Local implementations internally reopen `git2::Repository` per call (which
//! is what the current worker code does already), so no non-Send handles
//! leak out.

use std::ffi::OsString;
use std::io;
use std::ops::{ControlFlow, Range};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;

use crate::file_tree::{PreviewContent, TreeEntry};
use crate::git::graph::GraphRow;
use crate::git::{CommitDetail, CommitInfo, DiffContent, FileEntry, RefLabel};
use std::collections::{HashMap, HashSet};

pub mod local;
pub mod remote;

pub use local::LocalBackend;
pub use remote::RemoteBackend;

/// Errors returned by `Backend` operations. Kept deliberately simple — we
/// fold git2/IO errors into strings at the boundary because the UI only
/// shows them as toasts / status messages.
#[derive(Debug, Clone)]
pub enum BackendError {
    NotFound,
    Io(String),
    Git(String),
    Rpc(String),
    Protocol(String),
    Unimplemented(String),
    /// Destination already exists — surfaced by `create_file` so callers can
    /// differentiate "EEXIST" from a generic IO error and adjust the toast.
    PathExists(String),
    /// Path escapes the workdir (absolute or contains `..` reaching above
    /// the root). Raised by the agent / any write op before touching disk.
    PathEscape(String),
    /// `trash` op fell through to nothing — neither a trash tool nor a
    /// successful remove. Client-side toast uses this to phrase the
    /// follow-up prompt.
    TrashUnavailable(String),
    Other(String),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::NotFound => f.write_str("not found"),
            BackendError::Io(s) => write!(f, "io: {s}"),
            BackendError::Git(s) => write!(f, "git: {s}"),
            BackendError::Rpc(s) => write!(f, "rpc: {s}"),
            BackendError::Protocol(s) => write!(f, "protocol: {s}"),
            BackendError::Unimplemented(s) => write!(f, "unimplemented: {s}"),
            BackendError::PathExists(s) => write!(f, "path exists: {s}"),
            BackendError::PathEscape(s) => write!(f, "path escape: {s}"),
            BackendError::TrashUnavailable(s) => write!(f, "trash unavailable: {s}"),
            BackendError::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for BackendError {}

impl From<io::Error> for BackendError {
    fn from(e: io::Error) -> Self {
        BackendError::Io(e.to_string())
    }
}

impl BackendError {
    /// Reconstruct a `BackendError` from a wire-level `(ErrorCode, message)`
    /// pair. Inverse of `wire_code` — keep the two arms in sync; the
    /// exhaustive `match` below forces an update whenever a new
    /// `ErrorCode` variant lands.
    pub fn from_wire(code: reef_proto::ErrorCode, message: String) -> Self {
        use reef_proto::ErrorCode;
        match code {
            ErrorCode::NotFound => BackendError::NotFound,
            ErrorCode::Io => BackendError::Io(message),
            ErrorCode::Git => BackendError::Git(message),
            ErrorCode::Protocol => BackendError::Protocol(message),
            ErrorCode::Unimplemented => BackendError::Unimplemented(message),
            ErrorCode::PathExists => BackendError::PathExists(message),
            ErrorCode::PathEscape => BackendError::PathEscape(message),
            ErrorCode::TrashUnavailable => BackendError::TrashUnavailable(message),
            ErrorCode::Other => BackendError::Other(message),
        }
    }

    /// Pick the wire-level `ErrorCode` that best represents this error.
    /// Inverse of `from_wire`. `Rpc` collapses to `Other` because it's a
    /// client-side condition that never appears on the wire going the
    /// other direction.
    pub fn wire_code(&self) -> reef_proto::ErrorCode {
        use reef_proto::ErrorCode;
        match self {
            BackendError::NotFound => ErrorCode::NotFound,
            BackendError::Io(_) => ErrorCode::Io,
            BackendError::Git(_) => ErrorCode::Git,
            BackendError::Rpc(_) => ErrorCode::Other,
            BackendError::Protocol(_) => ErrorCode::Protocol,
            BackendError::Unimplemented(_) => ErrorCode::Unimplemented,
            BackendError::PathExists(_) => ErrorCode::PathExists,
            BackendError::PathEscape(_) => ErrorCode::PathEscape,
            BackendError::TrashUnavailable(_) => ErrorCode::TrashUnavailable,
            BackendError::Other(_) => ErrorCode::Other,
        }
    }
}

/// Snapshot of the repo status returned by `git_status`.
#[derive(Debug, Clone)]
pub struct StatusSnapshot {
    pub staged: Vec<FileEntry>,
    pub unstaged: Vec<FileEntry>,
    pub branch_name: String,
    pub ahead_behind: Option<(usize, usize)>,
}

/// Options for discovering Git repositories under the backend workdir.
#[derive(Debug, Clone)]
pub struct RepoDiscoverOpts {
    pub max_depth: usize,
    pub include_nested: bool,
    pub max_repos: Option<usize>,
}

impl Default for RepoDiscoverOpts {
    fn default() -> Self {
        Self {
            max_depth: 2,
            include_nested: false,
            max_repos: Some(100),
        }
    }
}

/// One discovered Git repository. `repo_root_rel` is the canonical identity:
/// workdir-relative, `.` for the workdir itself, never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRepoMeta {
    pub repo_root_rel: PathBuf,
    pub display_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoDiscoverResponse {
    pub repos: Vec<WorkspaceRepoMeta>,
    pub truncated: bool,
}

/// Normalize a workdir-relative repository identity. Empty and `.` both
/// canonicalize to `.`, absolute paths and parent traversal are rejected.
pub fn normalize_repo_root_rel(rel: &Path) -> Result<PathBuf, BackendError> {
    if rel.is_absolute() {
        return Err(BackendError::PathEscape(format!(
            "absolute repo path not allowed: {}",
            rel.display()
        )));
    }

    let mut out = PathBuf::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(BackendError::PathEscape(format!(
                    "parent-dir traversal not allowed in repo path: {}",
                    rel.display()
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(BackendError::PathEscape(format!(
                    "rooted repo path not allowed: {}",
                    rel.display()
                )));
            }
        }
    }

    if out.as_os_str().is_empty() {
        Ok(PathBuf::from("."))
    } else {
        Ok(out)
    }
}

pub fn repo_key(rel: &Path) -> String {
    if rel == Path::new(".") {
        ".".to_string()
    } else {
        rel.components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/")
    }
}

/// Simple graph payload returned by `list_commits` + `refs` + `ahead_behind`
/// wrapping what the local worker already computes.
#[derive(Debug, Clone)]
pub struct GraphSnapshot {
    pub rows: Vec<GraphRow>,
    pub ref_map: HashMap<String, Vec<RefLabel>>,
    pub head_oid: String,
}

/// Outcome of `Backend::trash`. `used_trash=false` means the backend had to
/// fall through to permanent-delete because no system trash tool was
/// available (common on headless remote hosts).
#[derive(Debug, Clone, Copy)]
pub struct TrashOutcome {
    pub used_trash: bool,
}

/// Options for `walk_repo_paths`. Mirrors `WalkOptsDto` on the wire but
/// declared on the domain side so the trait doesn't pull `reef-proto` into
/// every consumer. `Default` matches the values VSCode's Ctrl+P uses.
#[derive(Debug, Clone)]
pub struct WalkOpts {
    pub include_hidden: bool,
    pub respect_gitignore: bool,
    pub max_files: Option<u64>,
}

impl Default for WalkOpts {
    fn default() -> Self {
        Self {
            include_hidden: true,
            respect_gitignore: true,
            max_files: None,
        }
    }
}

/// Return value of `walk_repo_paths` — sorted workdir-relative paths + a
/// truncation marker when the walker hit the cap.
#[derive(Debug, Clone, Default)]
pub struct WalkResponse {
    pub paths: Vec<String>,
    pub truncated: bool,
}

/// One content-search hit, backend-side. Mirrors `global_search::MatchHit`
/// but the latter lives in the UI layer and carries UI invariants
/// (truncated `line_text`). We keep the shapes identical so conversion is
/// a one-liner.
#[derive(Debug, Clone)]
pub struct ContentMatchHit {
    pub path: PathBuf,
    pub display: String,
    pub line: usize,
    pub line_text: String,
    pub byte_range: Range<usize>,
}

/// Knobs for `search_content`. Mirrors `ContentSearchRequestDto`.
#[derive(Debug, Clone)]
pub struct ContentSearchRequest {
    pub pattern: String,
    pub fixed_strings: bool,
    pub case_sensitive: Option<bool>,
    pub max_results: u32,
    pub max_line_chars: u32,
}

/// Terminal response for `search_content`. Hits themselves arrive through
/// the `on_chunk` callback the caller passes in; this struct carries the
/// single boolean the walker can only know after it finishes (or aborts
/// on the cap).
#[derive(Debug, Clone, Default)]
pub struct ContentSearchCompleted {
    pub truncated: bool,
}

/// Callback type handed to `Backend::search_content`. The backend invokes
/// it once per accumulated chunk of hits; returning `ControlFlow::Break`
/// asks the backend to abort the walk early (used by the worker layer to
/// honour cancel flags from the UI). Backends are expected to respect
/// `Break` at the next walk boundary they control — streaming isn't
/// preemptive but the tail is bounded by the file currently being
/// scanned.
pub type SearchChunkSink<'a> = dyn FnMut(Vec<ContentMatchHit>) -> ControlFlow<()> + Send + 'a;

/// Everything the main loop needs to spawn an editor on the foreground
/// terminal. Local produces a direct `$VISUAL`/`$EDITOR` spec; remote
/// produces `ssh -t <host-args> "cd <remote_workdir> && $editor <rel>"`
/// so the user gets the same raw-mode editor experience over ssh.
#[derive(Debug, Clone)]
pub struct EditorLaunchSpec {
    /// The program to `Command::new()`.
    pub program: OsString,
    /// Args to pass — for local this is extra editor args + the absolute
    /// file path; for remote it's the ssh args + remote host + remote
    /// shell command string.
    pub args: Vec<OsString>,
    /// Reserved for future ssh -t handling. Today the main loop always
    /// tears down + restores the TUI around editor launch, so this field
    /// is advisory.
    pub inherit_tty: bool,
}

/// Backend abstraction. Phase 0 defines the methods the app and the workers
/// need; remote/local implementations satisfy the same contract.
pub trait Backend: Send + Sync {
    // ─── identity / workdir metadata ────────────────────────────────────────
    fn workdir_path(&self) -> PathBuf;
    fn workdir_name(&self) -> String;
    fn branch_name(&self) -> String;
    fn has_repo(&self) -> bool;
    /// `true` for any backend backed by a remote agent (ssh). Callers use
    /// this to gate features that can't meaningfully cross the boundary
    /// (external drag-drop upload, Reveal-in-Finder). Defaults to false so
    /// `LocalBackend` gets the correct answer for free.
    fn is_remote(&self) -> bool {
        false
    }

    fn discover_repos(&self, opts: &RepoDiscoverOpts)
    -> Result<RepoDiscoverResponse, BackendError>;

    // ─── filesystem ─────────────────────────────────────────────────────────
    /// Build the flat tree of entries for the backend's workdir. `expanded`
    /// is the set of directory paths (relative) the UI wants to show as
    /// expanded. `git_statuses` is the pre-computed status map keyed by
    /// relative path — the local impl does not need a round-trip to recompute
    /// it because the caller already has the data.
    fn build_file_tree(
        &self,
        expanded: &HashSet<PathBuf>,
        git_statuses: &HashMap<String, char>,
    ) -> Result<Vec<TreeEntry>, String>;

    /// Load a file preview (relative path). Honours backend-internal size
    /// caps (binary detection, 10k-line cap, 512KB highlight cap).
    ///
    /// `wants_decoded_image` tells the backend whether the caller will be
    /// able to actually render pixels (i.e. a graphics protocol was
    /// detected). When `false`, image files still sniff their MIME and
    /// return an `ImagePreview` with `image: None` for the friendly
    /// metadata card; skipping the full decode saves 50-200 ms on
    /// non-graphics terminals where the pixels would be thrown away.
    fn load_preview(
        &self,
        rel_path: &Path,
        dark: bool,
        wants_decoded_image: bool,
    ) -> Option<PreviewContent>;

    /// Raw file bytes. Returns `BackendError::NotFound` if the path isn't a
    /// regular file. `max_bytes` caps how many bytes are returned; remote
    /// transports use it to bound response size.
    fn read_file(&self, rel_path: &Path, max_bytes: u64) -> Result<Vec<u8>, BackendError>;

    /// Size in bytes of the regular file at `rel_path`. Lets callers
    /// short-circuit before paying for the bytes themselves — e.g. the
    /// global-replace worker uses this to skip files over its 50 MB cap
    /// without round-tripping a truncated copy. `NotFound` if the path
    /// doesn't resolve to a regular file under the workdir.
    fn file_size(&self, rel_path: &Path) -> Result<u64, BackendError>;

    /// Load one page of rows from a table inside a SQLite database at
    /// `rel_path`. Used by the Files-tab preview pane's pagination
    /// flow — `load_preview` builds the initial card with the first
    /// page bundled in, then `[`/`]`/`PgUp`/`PgDn` route through here
    /// to reissue with a fresh `(offset, limit)` window.
    ///
    /// `offset` and `limit` map directly to `LIMIT N OFFSET M`, with
    /// the same caveat: cost grows with M for tables without a usable
    /// index. The reef-sqlite-preview reader documents the keyset
    /// pagination follow-up if this becomes a real hotspot.
    fn db_load_page(
        &self,
        rel_path: &Path,
        table: &str,
        offset: u64,
        limit: u32,
    ) -> Result<reef_sqlite_preview::DbPage, BackendError>;

    // ─── git: status / diff / stage ─────────────────────────────────────────
    fn git_status(&self) -> Result<StatusSnapshot, BackendError>;
    fn git_status_for(&self, repo_root_rel: &Path) -> Result<StatusSnapshot, BackendError>;

    fn staged_diff(
        &self,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError>;
    fn staged_diff_for(
        &self,
        repo_root_rel: &Path,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError>;
    fn unstaged_diff(
        &self,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError>;
    fn unstaged_diff_for(
        &self,
        repo_root_rel: &Path,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError>;
    fn untracked_diff(&self, path: &str) -> Result<Option<DiffContent>, BackendError>;
    fn untracked_diff_for(
        &self,
        repo_root_rel: &Path,
        path: &str,
    ) -> Result<Option<DiffContent>, BackendError>;

    fn stage(&self, path: &str) -> Result<(), BackendError>;
    fn stage_for(&self, repo_root_rel: &Path, path: &str) -> Result<(), BackendError>;
    fn unstage(&self, path: &str) -> Result<(), BackendError>;
    fn unstage_for(&self, repo_root_rel: &Path, path: &str) -> Result<(), BackendError>;
    fn restore(&self, path: &str) -> Result<(), BackendError>;
    /// Combined "discard one path" op used by the Git tab's folder /
    /// section discard flows. Staged paths are first unstaged, then the
    /// workdir restored to HEAD; unstaged paths only get workdir restore.
    /// Collapsed into a single trait method so `RemoteBackend` can reach
    /// the agent-side `git2::Repository` in one round-trip (the free
    /// `revert_path` helper in `app.rs` assumed a local repo handle and
    /// silently no-op'd on remote before M4).
    fn revert_path(&self, path: &str, is_staged: bool) -> Result<(), BackendError>;
    fn revert_path_for(
        &self,
        repo_root_rel: &Path,
        path: &str,
        is_staged: bool,
    ) -> Result<(), BackendError>;

    fn push(&self, force: bool) -> Result<(), BackendError>;
    fn push_for(&self, repo_root_rel: &Path, force: bool) -> Result<(), BackendError>;
    fn publish_branch(&self) -> Result<(), BackendError>;
    fn publish_branch_for(&self, repo_root_rel: &Path) -> Result<(), BackendError>;
    fn pull(&self) -> Result<(), BackendError>;
    fn pull_for(&self, repo_root_rel: &Path) -> Result<(), BackendError>;
    fn checkout_branch(&self, branch: &str) -> Result<(), BackendError>;
    fn checkout_branch_for(&self, repo_root_rel: &Path, branch: &str) -> Result<(), BackendError>;
    fn create_branch(&self, branch: &str, base: Option<&str>) -> Result<(), BackendError>;
    fn create_branch_for(
        &self,
        repo_root_rel: &Path,
        branch: &str,
        base: Option<&str>,
    ) -> Result<(), BackendError>;

    /// Commit the staged index with `message`. Same shell-out rationale
    /// as `push` — respects hooks, signing, and the user's git config.
    /// Errors from `git commit` bubble up as `BackendError::Git`.
    fn commit(&self, message: &str) -> Result<(), BackendError>;
    fn commit_for(&self, repo_root_rel: &Path, message: &str) -> Result<(), BackendError>;

    // ─── git: history ───────────────────────────────────────────────────────
    fn list_commits(&self, limit: usize) -> Result<Vec<CommitInfo>, BackendError>;
    fn list_commits_for(
        &self,
        repo_root_rel: &Path,
        limit: usize,
    ) -> Result<Vec<CommitInfo>, BackendError>;
    fn list_refs(&self) -> Result<HashMap<String, Vec<RefLabel>>, BackendError>;
    fn list_refs_for(
        &self,
        repo_root_rel: &Path,
    ) -> Result<HashMap<String, Vec<RefLabel>>, BackendError>;
    fn head_oid(&self) -> Result<Option<String>, BackendError>;
    fn head_oid_for(&self, repo_root_rel: &Path) -> Result<Option<String>, BackendError>;
    fn commit_detail(&self, oid: &str) -> Result<Option<CommitDetail>, BackendError>;
    fn commit_detail_for(
        &self,
        repo_root_rel: &Path,
        oid: &str,
    ) -> Result<Option<CommitDetail>, BackendError>;
    fn commit_file_diff(
        &self,
        oid: &str,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError>;
    fn commit_file_diff_for(
        &self,
        repo_root_rel: &Path,
        oid: &str,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError>;

    /// Union of files changed across the commit range `oldest..=newest`.
    fn range_files(
        &self,
        oldest_oid: &str,
        newest_oid: &str,
    ) -> Result<Vec<FileEntry>, BackendError>;
    fn range_files_for(
        &self,
        repo_root_rel: &Path,
        oldest_oid: &str,
        newest_oid: &str,
    ) -> Result<Vec<FileEntry>, BackendError>;

    /// Single-file diff for a commit range (same tree baseline as `range_files`).
    fn range_file_diff(
        &self,
        oldest_oid: &str,
        newest_oid: &str,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError>;
    fn range_file_diff_for(
        &self,
        repo_root_rel: &Path,
        oldest_oid: &str,
        newest_oid: &str,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError>;

    // ─── fs watcher / editor ────────────────────────────────────────────────
    /// Subscribe to debounced fs-change events. Each backend decides whether
    /// to spawn a local watcher (LocalBackend) or relay notifications from
    /// the remote agent (RemoteBackend).
    fn subscribe_fs_events(&self) -> mpsc::Receiver<()>;

    /// Best-effort editor launch hook. M1 remote backend leaves this as
    /// `BackendError::Unimplemented` — the main loop still calls the local
    /// `editor::launch` via the shared terminal to preserve behaviour until
    /// Phase 6 ships SSH -t transparent forwarding.
    fn launch_editor(&self, rel_path: &Path) -> Result<(), BackendError>;

    /// Build a `Command` spec for spawning the user's editor on the
    /// foreground terminal. Local backend resolves `$VISUAL`/`$EDITOR`
    /// and passes the absolute file path; remote backend assembles an
    /// `ssh -t host "cd <workdir> && $editor <rel>"` invocation that
    /// reuses the existing ControlMaster socket.
    ///
    /// The caller still owns TUI teardown/restore — this method just
    /// returns the argv + program so `main.rs` can `Command::spawn` on
    /// the real terminal without hard-coding the local-vs-remote split.
    fn editor_launch_spec(&self, rel_path: &Path) -> Result<EditorLaunchSpec, BackendError>;

    // ─── M3 Track 1: write operations (all paths workdir-relative) ──────────
    /// Create an empty file at `rel_path`. Fails `PathExists` if it
    /// already exists (`OpenOptions::create_new` semantics — no truncate
    /// race with an external writer).
    fn create_file(&self, rel_path: &Path) -> Result<(), BackendError>;
    /// Idempotent `mkdir -p` at `rel_path`. An existing directory is
    /// treated as success.
    fn create_dir_all(&self, rel_path: &Path) -> Result<(), BackendError>;
    /// `fs::rename` within the workdir. Caller is expected to have
    /// validated that `to_rel` doesn't already exist.
    fn rename(&self, from_rel: &Path, to_rel: &Path) -> Result<(), BackendError>;
    /// Copy a single file (not a directory). Mirrors `std::fs::copy`.
    fn copy_file(&self, from_rel: &Path, to_rel: &Path) -> Result<(), BackendError>;
    /// Recursive directory copy. Symlinks are skipped (matches the legacy
    /// `copy_dir_recursive` helper — no cycles, no broken links).
    fn copy_dir_recursive(&self, from_rel: &Path, to_rel: &Path) -> Result<(), BackendError>;
    /// Copy a single file or directory from an **absolute local path** to
    /// `remote_dst_rel` under the backend workdir. LocalBackend: direct
    /// `fs::copy` / `copy_dir_recursive`. RemoteBackend: `scp` subprocess
    /// reusing the session's ControlMaster.
    ///
    /// This is the M4 Track C "drag-drop from local Finder onto a remote
    /// tree" primitive; the caller (`tasks::copy_sources`) picks it over
    /// `copy_file` / `copy_dir_recursive` when the source path isn't
    /// under the workdir.
    fn upload_from_local(
        &self,
        local_src: &Path,
        remote_dst_rel: &Path,
    ) -> Result<(), BackendError>;
    /// Remove a single file / symlink. `fs::remove_file` semantics.
    fn remove_file(&self, rel_path: &Path) -> Result<(), BackendError>;
    /// Recursive directory removal. `fs::remove_dir_all` semantics.
    fn remove_dir_all(&self, rel_path: &Path) -> Result<(), BackendError>;
    /// Overwrite the regular file at `rel_path` with `content`. Used by
    /// the global find-and-replace path. Atomic: implementations write to
    /// a sibling tempfile and `rename` into place so a mid-write crash
    /// leaves the original intact. The original file's mode bits are
    /// preserved across the swap (without this every replaced file
    /// silently chmods to the tempfile default of `0600`).
    ///
    /// The path must already exist and resolve to a regular file under
    /// the workdir. Symlinks are followed during canonicalisation; if
    /// the link target escapes the workdir, returns
    /// `BackendError::PathEscape`. If the target doesn't exist, returns
    /// `BackendError::NotFound` — replace is never used to create new
    /// files.
    fn write_file(&self, rel_path: &Path, content: &[u8]) -> Result<(), BackendError>;
    /// Move each path to the OS Trash. On hosts without a trash tool the
    /// backend falls back to `fs::remove_*` and reports
    /// `TrashOutcome { used_trash: false }` so the UI can phrase the
    /// follow-up toast accordingly.
    fn trash(&self, rel_paths: &[PathBuf]) -> Result<TrashOutcome, BackendError>;
    /// Permanent delete. Unlike `trash`, never attempts a recycle-bin
    /// detour.
    fn hard_delete(&self, rel_paths: &[PathBuf]) -> Result<(), BackendError>;

    // ─── M3 Track 2: walk + content search ──────────────────────────────────
    /// Walk every file under the workdir (respecting `.gitignore` when
    /// `opts.respect_gitignore` is set). Output is sorted + capped. Used
    /// by the quick-open palette.
    fn walk_repo_paths(&self, opts: &WalkOpts) -> Result<WalkResponse, BackendError>;
    /// Content search: run `grep_searcher` with a smart-case literal (or
    /// regex) matcher over the walk, streaming hits as they're found via
    /// the `on_chunk` callback. The returned `ContentSearchCompleted`
    /// carries only the `truncated` flag; callers that want every hit
    /// should accumulate them in the closure.
    ///
    /// The callback may return `ControlFlow::Break(())` to abort the
    /// walk early (cancellation). Backends honour `Break` at the next
    /// walk boundary — the in-flight file is still scanned, but no
    /// further files are opened and the final `truncated` will be
    /// whatever the accumulator had observed at that point.
    fn search_content(
        &self,
        request: &ContentSearchRequest,
        on_chunk: &mut SearchChunkSink<'_>,
    ) -> Result<ContentSearchCompleted, BackendError>;
}
