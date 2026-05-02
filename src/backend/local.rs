//! Local backend — wraps the existing `GitRepo`, `file_tree`, `fs_watcher`
//! and `editor` helpers. Phase 0 is additive: this impl forwards to the
//! current code unchanged so TUI behaviour stays byte-identical with main.
//!
//! Every git operation re-opens `git2::Repository::discover(workdir)` rather
//! than caching a handle — matches what the background workers in
//! `src/tasks.rs` already do and keeps `LocalBackend` `Send + Sync` without
//! any `Mutex` dance around `Repository` (which is non-Send).

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock, mpsc};

use super::{
    Backend, BackendError, ContentMatchHit, ContentSearchCompleted, ContentSearchRequest,
    EditorLaunchSpec, SearchChunkSink, StatusSnapshot, TrashOutcome, WalkOpts, WalkResponse,
};
use crate::file_tree::{self, PreviewContent, TreeEntry};
use crate::git::{CommitDetail, CommitInfo, DiffContent, FileEntry, GitRepo, RefLabel};
use std::ops::ControlFlow;

/// How many decoded previews to keep around. 8 covers the typical
/// "click around a directory" workflow without inflating memory for
/// pathological browsing sessions. Each entry can hold a decoded
/// `DynamicImage` (up to ~16 MB after `MAX_PROTOCOL_DIM` downscaling),
/// so a full cache is bounded at ~128 MB worst-case but usually much
/// smaller (text files are tiny, most images are small).
const PREVIEW_CACHE_CAP: usize = 8;

/// Key shape that makes cache hits correct across:
/// - file edits (`mtime_ns` changes → miss → fresh decode)
/// - file truncation / growth (`size` changes → miss)
/// - theme toggles (`dark` changes → syntect highlight differs)
/// - graphics-protocol toggles (`wants_decoded_image` changes → need
///   the decoded `DynamicImage` vs the lighter metadata-only shape)
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewCacheKey {
    rel_path: PathBuf,
    mtime_ns: Option<i128>,
    size: u64,
    dark: bool,
    wants_decoded_image: bool,
}

#[derive(Default)]
struct PreviewCache {
    /// Front = least-recently-used, back = most. Eight linear-scan
    /// lookups per load is far cheaper than a PNG decode, so we don't
    /// bother with a hash index.
    entries: VecDeque<(PreviewCacheKey, PreviewContent)>,
}

impl PreviewCache {
    fn get(&mut self, key: &PreviewCacheKey) -> Option<PreviewContent> {
        let pos = self.entries.iter().position(|(k, _)| k == key)?;
        let (k, v) = self.entries.remove(pos).expect("position checked");
        let cloned = v.clone();
        self.entries.push_back((k, v));
        Some(cloned)
    }

    fn put(&mut self, key: PreviewCacheKey, value: PreviewContent) {
        while self.entries.len() >= PREVIEW_CACHE_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back((key, value));
    }
}

/// Local filesystem + libgit2 backend.
pub struct LocalBackend {
    workdir: PathBuf,
    /// Cached `fs::canonicalize(workdir)`. Populated lazily on the first
    /// symlink-safe read because `workdir` is immutable after
    /// construction — without the cache every `read_file` / preview
    /// would pay one extra syscall to canonicalise the root before
    /// resolving the target.
    canon_workdir: OnceLock<PathBuf>,
    /// Decoded-preview LRU. Re-selecting a file — whether via cursor
    /// bounce, quick-open, or fs-watcher refresh with no actual change
    /// — returns the cached `PreviewContent` (deep-clone, ~ms for the
    /// biggest images) instead of re-running the 50-200 ms PNG decode.
    /// Wrapped in a `Mutex` so the worker thread can hit it from
    /// `load_preview` without requiring `&mut self`.
    preview_cache: Mutex<PreviewCache>,
}

impl LocalBackend {
    /// Open a backend rooted at the current working directory. Unlike
    /// `GitRepo::open`, this always succeeds — we need a backend even when
    /// the cwd is not a git repo (the Files tab still works).
    pub fn open_cwd() -> std::io::Result<Self> {
        let workdir = std::env::current_dir()?;
        Ok(Self {
            workdir,
            canon_workdir: OnceLock::new(),
            preview_cache: Mutex::new(PreviewCache::default()),
        })
    }

    /// Open at an explicit workdir. Used by `reef-agent --workdir`.
    pub fn open_at(workdir: PathBuf) -> Self {
        Self {
            workdir,
            canon_workdir: OnceLock::new(),
            preview_cache: Mutex::new(PreviewCache::default()),
        }
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    fn repo(&self) -> Result<GitRepo, BackendError> {
        GitRepo::open_at(&self.workdir).map_err(|e| BackendError::Git(e.message().to_string()))
    }

    /// Join a workdir-relative path to the backend's workdir root, rejecting
    /// absolute paths (workdir is the security boundary) and parent-dir
    /// traversal. Accepts the root itself (`""` or `.`).
    fn resolve_rel(&self, rel: &Path) -> Result<PathBuf, BackendError> {
        resolve_rel_within(&self.workdir, rel)
    }

    fn canonical_workdir(&self) -> Result<&Path, BackendError> {
        if let Some(p) = self.canon_workdir.get() {
            return Ok(p);
        }
        let canon = canonicalize_or_backend_err(&self.workdir, "canonicalize workdir")?;
        let _ = self.canon_workdir.set(canon);
        Ok(self
            .canon_workdir
            .get()
            .expect("just populated via OnceLock::set"))
    }
}

/// Workdir-relative path resolver. Broken out of `LocalBackend::resolve_rel`
/// so the search / walk helpers can reuse it without needing a backend
/// handle.
pub fn resolve_rel_within(root: &Path, rel: &Path) -> Result<PathBuf, BackendError> {
    if rel.is_absolute() {
        return Err(BackendError::PathEscape(format!(
            "absolute path not allowed: {}",
            rel.display()
        )));
    }
    for comp in rel.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(BackendError::PathEscape(format!(
                    "parent-dir traversal not allowed: {}",
                    rel.display()
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(BackendError::PathEscape(format!(
                    "rooted path not allowed: {}",
                    rel.display()
                )));
            }
        }
    }
    Ok(root.join(rel))
}

/// `fs::canonicalize` lifted into `BackendError`. Missing path → `NotFound`;
/// anything else → `Io` with the provided `context` prefix.
fn canonicalize_or_backend_err(path: &Path, context: &str) -> Result<PathBuf, BackendError> {
    std::fs::canonicalize(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => BackendError::NotFound,
        _ => BackendError::Io(format!("{context}: {e}")),
    })
}

/// Canonicalising variant of `resolve_rel_within` for *read* paths.
/// Follows symlinks and rejects targets outside `canon_root` — without
/// this, a workdir containing `link → /etc/passwd` would let a caller
/// read the symlink target. Only matters in remote/agent mode (malicious
/// workdir threat model); applied uniformly so local and remote behave
/// the same.
///
/// `canon_root` MUST already be canonicalised (callers typically cache
/// it — see `LocalBackend::canonical_workdir`). TOCTOU: a symlink swap
/// between this call and the subsequent open is possible; `openat2`
/// would close it but is Linux-only.
pub fn canonical_child_within(canon_root: &Path, rel: &Path) -> Result<PathBuf, BackendError> {
    let joined = resolve_rel_within(canon_root, rel)?;
    let canon_target = canonicalize_or_backend_err(&joined, "canonicalize target")?;
    if !canon_target.starts_with(canon_root) {
        return Err(BackendError::PathEscape(format!(
            "symlink escapes workdir: {}",
            rel.display()
        )));
    }
    Ok(canon_target)
}

/// Build a `grep_regex::RegexMatcher` configured the way reef's
/// content search expects. Shared between `search_content_local`
/// (the streaming search worker) and `tasks::replace_one_file` (the
/// global-replace worker) so they're guaranteed to find the exact
/// same matches — anything else risks a UI showing one set of hits
/// and a replace touching a different set.
///
/// `case_sensitive` follows ripgrep's convention: `Some(true)` forces
/// case-sensitive, `Some(false)` forces insensitive, `None` means
/// smart-case (insensitive iff the pattern contains no uppercase).
/// `fixed_strings = true` makes regex metacharacters (`.`, `*`, …)
/// literal, matching VSCode's "match whole word off / regex off" mode.
pub fn build_smart_case_matcher(
    pattern: &str,
    fixed_strings: bool,
    case_sensitive: Option<bool>,
) -> Result<grep_regex::RegexMatcher, grep_regex::Error> {
    let mut builder = grep_regex::RegexMatcherBuilder::new();
    match case_sensitive {
        Some(true) => {
            builder.case_insensitive(false).case_smart(false);
        }
        Some(false) => {
            builder.case_insensitive(true).case_smart(false);
        }
        None => {
            builder.case_smart(true);
        }
    }
    builder.fixed_strings(fixed_strings);
    builder.build(pattern)
}

/// Atomic file overwrite: write `content` to a tempfile in the same
/// parent dir, then `persist` (rename) over `rel_path`. Shared between
/// `LocalBackend::write_file` and the agent's `WriteFile` handler so
/// both code paths keep identical guarantees.
///
/// Path validation goes through `canonical_child_within` — the target
/// must already exist (we don't create files via this op), must be a
/// regular file, and must not resolve outside `canon_root` even via
/// symlink. The tempfile inherits its default mode (0600); we read the
/// original's mode beforehand and re-apply it after `persist` so the
/// replaced file keeps its original permissions.
pub fn write_file_atomic(
    canon_root: &Path,
    rel_path: &Path,
    content: &[u8],
) -> Result<(), BackendError> {
    let canon_target = canonical_child_within(canon_root, rel_path)?;
    let meta = std::fs::metadata(&canon_target).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => BackendError::NotFound,
        _ => BackendError::Io(format!("stat target: {e}")),
    })?;
    if !meta.is_file() {
        return Err(BackendError::Io(format!(
            "not a regular file: {}",
            rel_path.display()
        )));
    }
    let parent = canon_target
        .parent()
        .ok_or_else(|| BackendError::Io(format!("no parent dir for {}", rel_path.display())))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| BackendError::Io(format!("create tempfile: {e}")))?;
    use std::io::Write;
    tmp.write_all(content)
        .map_err(|e| BackendError::Io(format!("write tempfile: {e}")))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| BackendError::Io(format!("fsync tempfile: {e}")))?;
    tmp.persist(&canon_target)
        .map_err(|e| BackendError::Io(format!("persist tempfile: {}", e.error)))?;
    // `NamedTempFile` defaults to 0600 — restore the original mode so a
    // replace doesn't silently chmod every touched file.
    if let Err(e) = std::fs::set_permissions(&canon_target, meta.permissions()) {
        return Err(BackendError::Io(format!("restore mode: {e}")));
    }
    Ok(())
}

impl Backend for LocalBackend {
    fn workdir_path(&self) -> PathBuf {
        self.workdir.clone()
    }

    fn workdir_name(&self) -> String {
        // Prefer the git workdir basename when a repo is present; otherwise
        // fall back to the cwd basename, matching the pre-Backend
        // behaviour in `App::new`.
        if let Ok(repo) = self.repo() {
            return repo.workdir_name();
        }
        self.workdir
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
            .unwrap_or_else(|| "repo".to_string())
    }

    fn branch_name(&self) -> String {
        self.repo()
            .map(|r| r.branch_name())
            .unwrap_or_else(|_| String::new())
    }

    fn has_repo(&self) -> bool {
        self.repo().is_ok()
    }

    fn build_file_tree(
        &self,
        expanded: &HashSet<PathBuf>,
        git_statuses: &HashMap<String, char>,
    ) -> Result<Vec<TreeEntry>, String> {
        Ok(file_tree::build_entries(
            &self.workdir,
            expanded,
            git_statuses,
        ))
    }

    fn load_preview(
        &self,
        rel_path: &Path,
        dark: bool,
        wants_decoded_image: bool,
    ) -> Option<PreviewContent> {
        // Build the cache key from a single `metadata()` call — cheaper
        // than `file_tree::load_preview`'s full `File::open + read
        // header + decode` path, so the lookup is effectively free on
        // hits. On cache miss (fresh file or changed mtime/size) we
        // fall through to the decoder.
        let full = self.workdir.join(rel_path);
        let meta = std::fs::metadata(&full).ok();
        let key = PreviewCacheKey {
            rel_path: rel_path.to_path_buf(),
            mtime_ns: meta.as_ref().and_then(|m| {
                m.modified().ok().and_then(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_nanos() as i128)
                })
            }),
            size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
            dark,
            wants_decoded_image,
        };

        if let Ok(mut cache) = self.preview_cache.lock() {
            if let Some(cached) = cache.get(&key) {
                return Some(cached);
            }
        }

        // Symlink-escape gate only fires on cache miss — hits return
        // content we already validated on a prior read, so no fresh
        // filesystem traversal is about to happen. `file_tree::load_preview`
        // below does a raw `File::open` that follows symlinks without
        // any boundary check, so without this gate a workdir-relative
        // symlink could exfiltrate arbitrary files.
        let canon_root = self.canonical_workdir().ok()?;
        canonical_child_within(canon_root, rel_path).ok()?;

        let fresh = file_tree::load_preview(&self.workdir, rel_path, dark, wants_decoded_image)?;

        if let Ok(mut cache) = self.preview_cache.lock() {
            cache.put(key, fresh.clone());
        }
        Some(fresh)
    }

    fn file_size(&self, rel_path: &Path) -> Result<u64, BackendError> {
        let full = canonical_child_within(self.canonical_workdir()?, rel_path)?;
        let meta = std::fs::metadata(&full).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => BackendError::NotFound,
            _ => BackendError::Io(format!("stat: {e}")),
        })?;
        if !meta.is_file() {
            return Err(BackendError::NotFound);
        }
        Ok(meta.len())
    }

    fn read_file(&self, rel_path: &Path, max_bytes: u64) -> Result<Vec<u8>, BackendError> {
        // `canonical_child_within` enforces the workdir boundary *after*
        // symlink resolution. Without it a workdir-relative symlink
        // would let a caller exfiltrate any file the backend process
        // can read — critical in remote/agent mode where the caller is
        // untrusted relative to the host filesystem.
        let full = canonical_child_within(self.canonical_workdir()?, rel_path)?;
        if !full.is_file() {
            return Err(BackendError::NotFound);
        }
        let bytes = std::fs::read(&full).map_err(|e| BackendError::Io(e.to_string()))?;
        let capped = if (bytes.len() as u64) > max_bytes {
            bytes[..max_bytes as usize].to_vec()
        } else {
            bytes
        };
        Ok(capped)
    }

    fn db_load_page(
        &self,
        rel_path: &Path,
        table: &str,
        offset: u64,
        limit: u32,
    ) -> Result<reef_sqlite_preview::DbPage, BackendError> {
        // Symlink-escape gate, same as `read_file`. SQLite preview opens
        // the file with `mode=ro&immutable=1`, so an attacker can't
        // mutate state via this path — but they could still ship the
        // contents of a sensitive DB outside the workdir if we followed
        // a symlink without checking.
        let full = canonical_child_within(self.canonical_workdir()?, rel_path)?;
        reef_sqlite_preview::load_page(&full, table, offset, limit)
            .map_err(|e| BackendError::Other(format!("sqlite: {e}")))
    }

    fn git_status(&self) -> Result<StatusSnapshot, BackendError> {
        let repo = self.repo()?;
        let (staged, unstaged) = repo.get_status();
        Ok(StatusSnapshot {
            staged,
            unstaged,
            branch_name: repo.branch_name(),
            ahead_behind: repo.ahead_behind(),
        })
    }

    fn staged_diff(
        &self,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError> {
        Ok(self.repo()?.get_diff(path, true, context_lines))
    }

    fn unstaged_diff(
        &self,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError> {
        Ok(self.repo()?.get_diff(path, false, context_lines))
    }

    fn untracked_diff(&self, path: &str) -> Result<Option<DiffContent>, BackendError> {
        // `get_diff(staged=false)` already dispatches to untracked-diff when
        // the path is new — so we route through the same entry point and
        // keep the UNtracked-only API available for explicit use.
        Ok(self.repo()?.get_diff(path, false, 3))
    }

    fn stage(&self, path: &str) -> Result<(), BackendError> {
        self.repo()?
            .stage_file(path)
            .map_err(|e| BackendError::Git(e.message().to_string()))
    }

    fn unstage(&self, path: &str) -> Result<(), BackendError> {
        self.repo()?
            .unstage_file(path)
            .map_err(|e| BackendError::Git(e.message().to_string()))
    }

    fn restore(&self, path: &str) -> Result<(), BackendError> {
        self.repo()?
            .restore_file(path)
            .map_err(|e| BackendError::Git(e.message().to_string()))
    }

    fn revert_path(&self, path: &str, is_staged: bool) -> Result<(), BackendError> {
        // Staged-path revert = unstage → restore-workdir. Unstaged-path
        // revert = just restore-workdir. Mirrors the free `revert_path`
        // helper in `app.rs` (removed in M4 Track A-0.1) — errors on
        // either side are swallowed the same way the UI did before,
        // except we surface the *last* error so the backend contract
        // stays `Result<(), _>`.
        let repo = self.repo()?;
        if is_staged {
            let _ = repo.unstage_file(path);
        }
        repo.restore_file(path)
            .map_err(|e| BackendError::Git(e.message().to_string()))
    }

    fn push(&self, force: bool) -> Result<(), BackendError> {
        // Reuse the free function so we don't need a GitRepo handle — it
        // shells out to `git push` and is the same thing the foreground
        // App::run_push() uses on its worker thread.
        crate::git::push_at(&self.workdir, force).map_err(BackendError::Git)
    }

    fn commit(&self, message: &str) -> Result<(), BackendError> {
        crate::git::commit_at(&self.workdir, message).map_err(BackendError::Git)
    }

    fn list_commits(&self, limit: usize) -> Result<Vec<CommitInfo>, BackendError> {
        Ok(self.repo()?.list_commits(limit))
    }

    fn list_refs(&self) -> Result<HashMap<String, Vec<RefLabel>>, BackendError> {
        Ok(self.repo()?.list_refs())
    }

    fn head_oid(&self) -> Result<Option<String>, BackendError> {
        Ok(self.repo()?.head_oid())
    }

    fn commit_detail(&self, oid: &str) -> Result<Option<CommitDetail>, BackendError> {
        Ok(self.repo()?.get_commit(oid))
    }

    fn commit_file_diff(
        &self,
        oid: &str,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError> {
        Ok(self.repo()?.get_commit_file_diff(oid, path, context_lines))
    }

    fn range_files(
        &self,
        oldest_oid: &str,
        newest_oid: &str,
    ) -> Result<Vec<FileEntry>, BackendError> {
        Ok(self.repo()?.get_range_files(oldest_oid, newest_oid))
    }

    fn range_file_diff(
        &self,
        oldest_oid: &str,
        newest_oid: &str,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError> {
        Ok(self
            .repo()?
            .get_range_file_diff(oldest_oid, newest_oid, path, context_lines))
    }

    fn subscribe_fs_events(&self) -> mpsc::Receiver<()> {
        crate::fs_watcher::spawn(self.workdir.clone())
    }

    fn launch_editor(&self, _rel_path: &Path) -> Result<(), BackendError> {
        // The host loop in `main.rs` owns the terminal and still calls
        // `editor::launch` directly against the shared `Terminal<B>` so it
        // can tear down / restore raw-mode around the spawn. This hook is
        // kept to round out the trait surface — LocalBackend returns Ok(())
        // so callers that route through the backend get a no-op, and
        // remote's implementation can return Unimplemented until Phase 6.
        Ok(())
    }

    fn editor_launch_spec(&self, rel_path: &Path) -> Result<EditorLaunchSpec, BackendError> {
        use std::ffi::OsString;
        let abs = if rel_path.is_absolute() {
            // Callers historically pass an already-absolute path (see
            // `input.rs`). Accept it as long as it lives under the workdir;
            // otherwise the editor happily opens a sibling file, which is
            // fine behaviour we don't want to surprise local users by
            // regressing.
            rel_path.to_path_buf()
        } else {
            self.resolve_rel(rel_path)?
        };
        let (program, extra_args) = crate::editor::resolve_editor()
            .ok_or_else(|| BackendError::Io("no editor set (VISUAL / EDITOR)".to_string()))?;
        let mut args: Vec<OsString> = extra_args.into_iter().map(OsString::from).collect();
        args.push(abs.into_os_string());
        Ok(EditorLaunchSpec {
            program: OsString::from(program),
            args,
            inherit_tty: true,
        })
    }

    fn create_file(&self, rel_path: &Path) -> Result<(), BackendError> {
        let abs = self.resolve_rel(rel_path)?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&abs)
        {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(BackendError::PathExists(abs.display().to_string()))
            }
            Err(e) => Err(BackendError::Io(e.to_string())),
        }
    }

    fn create_dir_all(&self, rel_path: &Path) -> Result<(), BackendError> {
        let abs = self.resolve_rel(rel_path)?;
        std::fs::create_dir_all(&abs).map_err(|e| BackendError::Io(e.to_string()))
    }

    fn rename(&self, from_rel: &Path, to_rel: &Path) -> Result<(), BackendError> {
        let from = self.resolve_rel(from_rel)?;
        let to = self.resolve_rel(to_rel)?;
        std::fs::rename(&from, &to).map_err(|e| BackendError::Io(e.to_string()))
    }

    fn copy_file(&self, from_rel: &Path, to_rel: &Path) -> Result<(), BackendError> {
        let from = self.resolve_rel(from_rel)?;
        let to = self.resolve_rel(to_rel)?;
        std::fs::copy(&from, &to)
            .map(|_| ())
            .map_err(|e| BackendError::Io(e.to_string()))
    }

    fn copy_dir_recursive(&self, from_rel: &Path, to_rel: &Path) -> Result<(), BackendError> {
        let from = self.resolve_rel(from_rel)?;
        let to = self.resolve_rel(to_rel)?;
        copy_dir_recursive_inner(&from, &to).map_err(|e| BackendError::Io(e.to_string()))
    }

    fn upload_from_local(
        &self,
        local_src: &Path,
        remote_dst_rel: &Path,
    ) -> Result<(), BackendError> {
        // For LocalBackend the "remote" is just the workdir, so upload
        // degenerates to a plain copy. Keeping the shape identical to the
        // remote implementation means `tasks::copy_sources` can call the
        // same method on either backend and forget about the split.
        let to = self.resolve_rel(remote_dst_rel)?;
        if local_src.is_dir() {
            copy_dir_recursive_inner(local_src, &to).map_err(|e| BackendError::Io(e.to_string()))
        } else {
            std::fs::copy(local_src, &to)
                .map(|_| ())
                .map_err(|e| BackendError::Io(e.to_string()))
        }
    }

    fn remove_file(&self, rel_path: &Path) -> Result<(), BackendError> {
        let abs = self.resolve_rel(rel_path)?;
        std::fs::remove_file(&abs).map_err(|e| BackendError::Io(e.to_string()))
    }

    fn remove_dir_all(&self, rel_path: &Path) -> Result<(), BackendError> {
        let abs = self.resolve_rel(rel_path)?;
        std::fs::remove_dir_all(&abs).map_err(|e| BackendError::Io(e.to_string()))
    }

    fn write_file(&self, rel_path: &Path, content: &[u8]) -> Result<(), BackendError> {
        write_file_atomic(self.canonical_workdir()?, rel_path, content)
    }

    fn trash(&self, rel_paths: &[PathBuf]) -> Result<TrashOutcome, BackendError> {
        // Resolve every path first so a `PathEscape` fails atomically
        // before any side-effects.
        let abs: Vec<PathBuf> = rel_paths
            .iter()
            .map(|r| self.resolve_rel(r))
            .collect::<Result<_, _>>()?;
        trash::delete_all(&abs)
            .map(|_| TrashOutcome { used_trash: true })
            .map_err(|e| BackendError::Io(format!("trash: {e}")))
    }

    fn hard_delete(&self, rel_paths: &[PathBuf]) -> Result<(), BackendError> {
        for r in rel_paths {
            let abs = self.resolve_rel(r)?;
            let res = if abs.is_dir() {
                std::fs::remove_dir_all(&abs)
            } else {
                std::fs::remove_file(&abs)
            };
            res.map_err(|e| BackendError::Io(format!("delete {}: {}", abs.display(), e)))?;
        }
        Ok(())
    }

    fn walk_repo_paths(&self, opts: &WalkOpts) -> Result<WalkResponse, BackendError> {
        Ok(walk_paths(&self.workdir, opts))
    }

    fn search_content(
        &self,
        request: &ContentSearchRequest,
        on_chunk: &mut SearchChunkSink<'_>,
    ) -> Result<ContentSearchCompleted, BackendError> {
        Ok(search_content_local(&self.workdir, request, on_chunk))
    }
}

/// Recursive directory copy, DFS walk. Mirrors the legacy helper in
/// `tasks.rs` — skips symlinks to avoid cycles / dangling-target
/// surprises. `src` must already exist and be a directory.
pub(crate) fn copy_dir_recursive_inner(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let sub_src = entry.path();
        let sub_dst = dst.join(entry.file_name());
        if file_type.is_symlink() {
            continue;
        } else if file_type.is_dir() {
            copy_dir_recursive_inner(&sub_src, &sub_dst)?;
        } else if file_type.is_file() {
            std::fs::copy(&sub_src, &sub_dst)?;
        }
    }
    Ok(())
}

/// Walk the workdir with an `ignore::WalkBuilder`, returning sorted
/// workdir-relative file paths (no directories) with an optional cap.
///
/// `MAX_WALK_PATHS` is the hard ceiling — exceeding it sets
/// `truncated=true` and the remainder of the walk is discarded.
pub(crate) fn walk_paths(root: &Path, opts: &WalkOpts) -> WalkResponse {
    use ignore::WalkBuilder;
    let hard_cap: u64 = reef_proto::MAX_WALK_PATHS;
    let cap = match opts.max_files {
        Some(n) => n.min(hard_cap),
        None => hard_cap,
    };

    let mut out: Vec<String> = Vec::new();
    let mut truncated = false;

    let walker = WalkBuilder::new(root)
        .hidden(!opts.include_hidden)
        .git_ignore(opts.respect_gitignore)
        .git_exclude(opts.respect_gitignore)
        .git_global(opts.respect_gitignore)
        .filter_entry(|dent| dent.file_name() != ".git")
        .build();

    for result in walker {
        let Ok(entry) = result else { continue };
        let is_file = entry.file_type().map(|ft| ft.is_file()).unwrap_or(false);
        if !is_file {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let display = rel.to_string_lossy().to_string();
        if display.is_empty() {
            continue;
        }
        if (out.len() as u64) >= cap {
            truncated = true;
            break;
        }
        out.push(display);
    }
    out.sort();
    WalkResponse {
        paths: out,
        truncated,
    }
}

/// Run `grep_searcher` with a smart-case matcher over `root`, streaming
/// matches out through `on_chunk` in batches of up to
/// [`reef_proto::CHUNK_TARGET_HITS`]. Stops after `min(max_results,
/// MAX_SEARCH_HITS)` cumulative hits and reports `truncated=true`.
///
/// Streaming means the walker order (BFS-ish, by filename) is what the
/// caller observes — no global sort. Callers that want the hits
/// grouped by file can rely on the walker returning all matches from a
/// given file contiguously (one `search_path` call per file produces
/// hits in line order, and the outer walk processes each file exactly
/// once).
pub(crate) fn search_content_local(
    root: &Path,
    request: &ContentSearchRequest,
    on_chunk: &mut SearchChunkSink<'_>,
) -> ContentSearchCompleted {
    use grep_matcher::Matcher;
    use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, SinkMatch};
    use ignore::WalkBuilder;

    if request.pattern.is_empty() {
        return ContentSearchCompleted::default();
    }
    let hard_cap: u32 = reef_proto::MAX_SEARCH_HITS;
    let cap: usize = request.max_results.min(hard_cap) as usize;
    if cap == 0 {
        return ContentSearchCompleted::default();
    }

    let matcher = match build_smart_case_matcher(
        &request.pattern,
        request.fixed_strings,
        request.case_sensitive,
    ) {
        Ok(m) => m,
        Err(_) => return ContentSearchCompleted::default(),
    };

    let walker = WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|dent| dent.file_name() != ".git")
        .build();

    let mut searcher: Searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .build();
    // Accumulator for the current in-flight chunk. The `Collector` sink
    // pushes into this buffer; once it crosses `CHUNK_TARGET_HITS` the
    // outer loop flushes via `on_chunk`. We flush at file boundaries
    // rather than from inside the sink to keep the sink `Send`-free and
    // avoid reborrow gymnastics through grep-searcher's Sink trait.
    let mut buffer: Vec<ContentMatchHit> = Vec::with_capacity(reef_proto::CHUNK_TARGET_HITS);
    let mut total_emitted: usize = 0;
    let mut truncated = false;
    let mut aborted = false;
    let max_line_chars = request.max_line_chars as usize;
    let chunk_target = reef_proto::CHUNK_TARGET_HITS;

    struct Collector<'a> {
        rel: PathBuf,
        display: String,
        buffer: &'a mut Vec<ContentMatchHit>,
        total_emitted: &'a mut usize,
        cap: usize,
        truncated: &'a mut bool,
        max_line_chars: usize,
        matcher: &'a grep_regex::RegexMatcher,
    }
    impl grep_searcher::Sink for Collector<'_> {
        type Error = std::io::Error;
        fn matched(
            &mut self,
            _s: &grep_searcher::Searcher,
            mat: &SinkMatch<'_>,
        ) -> Result<bool, Self::Error> {
            if *self.total_emitted + self.buffer.len() >= self.cap {
                *self.truncated = true;
                return Ok(false);
            }
            let raw = strip_trailing_newline(mat.bytes());
            let line_text_full = match std::str::from_utf8(raw) {
                Ok(s) => s,
                Err(_) => return Ok(true),
            };
            let line_text = truncate_to_chars(line_text_full, self.max_line_chars);
            let line_text_len = line_text.len();
            let byte_range = self
                .matcher
                .find(raw)
                .ok()
                .flatten()
                .and_then(|m| clip_range(m.start()..m.end(), line_text_len))
                .unwrap_or(0..0);
            self.buffer.push(ContentMatchHit {
                path: self.rel.clone(),
                display: self.display.clone(),
                line: mat.line_number().unwrap_or(1).saturating_sub(1) as usize,
                line_text: line_text.into_owned(),
                byte_range,
            });
            Ok(true)
        }
    }

    'walk: for result in walker {
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

        let mut sink = Collector {
            rel: rel.to_path_buf(),
            display: display.clone(),
            buffer: &mut buffer,
            total_emitted: &mut total_emitted,
            cap,
            truncated: &mut truncated,
            max_line_chars,
            matcher: &matcher,
        };
        let _ = searcher.search_path(&matcher, abs, &mut sink);

        // Flush any accumulated chunks. We flush once per file as long as
        // the buffer is non-empty — small buffers still get forwarded
        // promptly because the *next* file is the gate, not some timer
        // we'd otherwise have to wire up. If the buffer is bigger than
        // `chunk_target` we may slice into multiple chunks so each frame
        // stays bounded.
        while buffer.len() >= chunk_target {
            let chunk: Vec<ContentMatchHit> = buffer.drain(..chunk_target).collect();
            total_emitted += chunk.len();
            match on_chunk(chunk) {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(()) => {
                    aborted = true;
                    break 'walk;
                }
            }
        }
        if !buffer.is_empty() {
            let chunk = std::mem::take(&mut buffer);
            total_emitted += chunk.len();
            match on_chunk(chunk) {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(()) => {
                    aborted = true;
                    break 'walk;
                }
            }
        }

        if truncated {
            break 'walk;
        }
    }

    // Flush a tail buffer if we bailed without a final file-boundary
    // flush. Under truncation the sink returned `false` after pushing
    // the very last hit so the tail is still in `buffer`.
    if !aborted && !buffer.is_empty() {
        let chunk = std::mem::take(&mut buffer);
        let _ = on_chunk(chunk);
    }

    ContentSearchCompleted { truncated }
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

fn truncate_to_chars(text: &str, max_chars: usize) -> Cow<'_, str> {
    let mut chars_seen = 0usize;
    let mut byte_end = 0usize;
    for (bi, c) in text.char_indices() {
        if chars_seen >= max_chars {
            break;
        }
        byte_end = bi + c.len_utf8();
        chars_seen += 1;
    }
    if byte_end >= text.len() {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(text[..byte_end].to_string())
    }
}

fn clip_range(range: std::ops::Range<usize>, max_end: usize) -> Option<std::ops::Range<usize>> {
    if range.start >= max_end {
        return None;
    }
    let end = range.end.min(max_end);
    Some(range.start..end)
}
