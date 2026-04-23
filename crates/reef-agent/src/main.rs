//! reef-agent — the remote daemon spawned by `reef --agent-exec`.
//!
//! Speaks the length-prefixed JSON-RPC protocol from `reef-proto` over
//! stdin/stdout. Internally it's a thin dispatcher over `reef::backend::
//! LocalBackend` — every Phase 0 operation we gave the trait has a one-to-
//! one RPC counterpart here.
//!
//! Threading:
//!   - main thread: read stdin, dispatch requests, write responses
//!   - fs-watcher thread: wait on `LocalBackend::subscribe_fs_events()`
//!     and push `Notification::FsChanged` frames to stdout
//!
//! Both threads share a `Mutex<Stdout>` to serialise writes.

use std::io::{self, BufReader, BufWriter, Stdout, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use reef::backend::{Backend, LocalBackend};
use reef_proto::{
    CommitDetailDto, CommitInfoDto, ContentSearchCompletedDto, DiffContentDto, DiffHunkDto,
    DiffLineDto, DirEntryDto, Envelope, ErrorCode, FileEntryDto, FileStatusDto, Frame,
    HandshakeResponse, LineTagDto, MatchHitDto, Notification, PROTOCOL_VERSION, ReadFileResponse,
    RefLabelDto, Request, Response, StatusSnapshotDto, TrashResponseDto, WalkResponseDto,
    encode_frame, read_envelope,
};

struct Args {
    stdio: bool,
    workdir: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        stdio: false,
        workdir: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--stdio" => args.stdio = true,
            "--workdir" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--workdir needs a path".to_string())?;
                args.workdir = Some(PathBuf::from(v));
            }
            "--version" => {
                println!("reef-agent {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--protocol-version" => {
                println!("{PROTOCOL_VERSION}");
                std::process::exit(0);
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(args)
}

fn print_usage() {
    eprintln!("reef-agent — remote daemon for reef");
    eprintln!();
    eprintln!("USAGE:");
    eprintln!("    reef-agent --stdio [--workdir <path>]");
    eprintln!();
    eprintln!("Speaks length-prefixed JSON-RPC on stdin/stdout (see crates/reef-proto).");
}

#[cfg(windows)]
fn set_stdio_binary() {
    // Windows: default C runtime translates `\n` ↔ `\r\n` on stdio
    // streams opened in text mode. That mangles length-prefixed JSON
    // frames (the 4-byte BE length counts bytes, not characters). Flip
    // stdin and stdout to raw binary so frames round-trip intact.
    use std::os::windows::io::AsRawHandle;
    // MSVC CRT exposes `_setmode(fd, _O_BINARY=0x8000)`. We call through
    // `libc` which re-exports it in its Windows target.
    unsafe extern "C" {
        fn _setmode(fd: i32, mode: i32) -> i32;
    }
    const O_BINARY: i32 = 0x8000;
    // stdin fd=0, stdout fd=1 on Windows just like POSIX.
    let _ = std::io::stdin().as_raw_handle();
    let _ = std::io::stdout().as_raw_handle();
    unsafe {
        _setmode(0, O_BINARY);
        _setmode(1, O_BINARY);
    }
}

#[cfg(not(windows))]
fn set_stdio_binary() {
    // POSIX stdio is raw bytes by default — nothing to do.
}

fn main() -> io::Result<()> {
    set_stdio_binary();

    let args = parse_args().map_err(io::Error::other)?;
    if !args.stdio {
        eprintln!("reef-agent: --stdio is required (this binary has no interactive mode)");
        std::process::exit(2);
    }

    let workdir = match args.workdir {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    std::env::set_current_dir(&workdir)?;
    // Canonicalise once so the symlink-escape guard on every `ReadFile`
    // doesn't repeat the syscall. `workdir` is immutable for the
    // agent's lifetime, so a single call covers every later request.
    let workdir = std::fs::canonicalize(&workdir)?;

    let backend = Arc::new(LocalBackend::open_at(workdir.clone()));
    let stdout = Arc::new(Mutex::new(BufWriter::new(io::stdout())));

    // Start watcher thread eagerly — reef's Subscribe is idempotent and we
    // want the channel drained from the moment the agent starts.
    let watcher_rx = backend.subscribe_fs_events();
    let watcher_stdout = Arc::clone(&stdout);
    let _watcher = thread::Builder::new()
        .name("reef-agent-watcher".into())
        .spawn(move || {
            while watcher_rx.recv().is_ok() {
                let frame = Frame::Notification(Notification::FsChanged);
                if let Ok(mut w) = watcher_stdout.lock() {
                    if encode_frame(&mut *w, &frame).is_err() {
                        break;
                    }
                    let _ = w.flush();
                }
            }
        })?;

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin);

    loop {
        let envelope = match read_envelope(&mut reader) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                eprintln!("[reef-agent] read error: {e}");
                break;
            }
        };

        // SearchContent is the only op that needs to push frames to
        // stdout mid-dispatch (streaming `SearchChunk` notifications
        // before the final response). We special-case it here so the
        // generic `dispatch()` can stay synchronous + writer-free.
        let response = if let Request::SearchContent { request } = &envelope.body {
            dispatch_search_content(&*backend, envelope.id, request.clone(), Arc::clone(&stdout))
        } else {
            dispatch(&*backend, &workdir, envelope)
        };
        let should_shutdown =
            matches!(&response, Some(Response::Ok { .. }) if is_shutdown_reply(&response));
        if let Some(resp) = response {
            let frame = Frame::Response(resp);
            if let Ok(mut w) = stdout.lock() {
                encode_frame(&mut *w, &frame)?;
                w.flush()?;
            }
        }
        if should_shutdown {
            break;
        }
    }

    Ok(())
}

/// We overload `result == {"shutting_down": true}` to signal "server should
/// exit after this reply". Keeps the protocol surface small.
fn is_shutdown_reply(resp: &Option<Response>) -> bool {
    match resp {
        Some(Response::Ok { result, .. }) => {
            result.get("shutting_down").and_then(|v| v.as_bool()) == Some(true)
        }
        _ => false,
    }
}

fn dispatch(backend: &dyn Backend, workdir: &Path, env: Envelope) -> Option<Response> {
    let id = env.id;
    let result: Result<serde_json::Value, (ErrorCode, String)> = match env.body {
        Request::Handshake => serde_json::to_value(HandshakeResponse {
            workdir: workdir.display().to_string(),
            workdir_name: backend.workdir_name(),
            branch_name: backend.branch_name(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
        })
        .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),

        Request::Shutdown => Ok(serde_json::json!({"shutting_down": true})),

        Request::Subscribe => Ok(serde_json::json!({"subscribed": true})),

        Request::ReadDir { path } => {
            let rel = PathBuf::from(&path);
            let abs = if rel.as_os_str().is_empty() {
                workdir.to_path_buf()
            } else {
                workdir.join(&rel)
            };
            match std::fs::read_dir(&abs) {
                Ok(iter) => {
                    let mut entries = Vec::new();
                    for entry in iter.flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        let is_dir = entry.path().is_dir();
                        entries.push(DirEntryDto { name, is_dir });
                    }
                    serde_json::to_value(entries)
                        .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}")))
                }
                Err(e) => Err((ErrorCode::Io, e.to_string())),
            }
        }

        Request::ReadFile { path, max_bytes } => read_file_response(workdir, &path, max_bytes),

        Request::GitStatus => match backend.git_status() {
            Ok(snap) => serde_json::to_value(StatusSnapshotDto {
                staged: snap.staged.into_iter().map(file_entry_to_dto).collect(),
                unstaged: snap.unstaged.into_iter().map(file_entry_to_dto).collect(),
                branch_name: snap.branch_name,
                ahead_behind: snap.ahead_behind,
            })
            .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),
            Err(e) => Err(backend_err(e)),
        },

        Request::StagedDiff {
            path,
            context_lines,
        } => match backend.staged_diff(&path, context_lines) {
            Ok(diff) => serde_json::to_value(diff.map(diff_to_dto))
                .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),
            Err(e) => Err(backend_err(e)),
        },
        Request::UnstagedDiff {
            path,
            context_lines,
        } => match backend.unstaged_diff(&path, context_lines) {
            Ok(diff) => serde_json::to_value(diff.map(diff_to_dto))
                .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),
            Err(e) => Err(backend_err(e)),
        },
        Request::UntrackedDiff { path } => match backend.untracked_diff(&path) {
            Ok(diff) => serde_json::to_value(diff.map(diff_to_dto))
                .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),
            Err(e) => Err(backend_err(e)),
        },

        Request::Stage { path } => match backend.stage(&path) {
            Ok(()) => Ok(serde_json::json!({"ok": true})),
            Err(e) => Err(backend_err(e)),
        },
        Request::Unstage { path } => match backend.unstage(&path) {
            Ok(()) => Ok(serde_json::json!({"ok": true})),
            Err(e) => Err(backend_err(e)),
        },
        Request::Restore { path } => match backend.restore(&path) {
            Ok(()) => Ok(serde_json::json!({"ok": true})),
            Err(e) => Err(backend_err(e)),
        },
        Request::RevertPath { path, is_staged } => match backend.revert_path(&path, is_staged) {
            Ok(()) => Ok(serde_json::json!({"ok": true})),
            Err(e) => Err(backend_err(e)),
        },
        Request::Push { force } => match backend.push(force) {
            Ok(()) => Ok(serde_json::json!({"ok": true})),
            Err(e) => Err(backend_err(e)),
        },
        Request::Commit { message } => match backend.commit(&message) {
            Ok(()) => Ok(serde_json::json!({"ok": true})),
            Err(e) => Err(backend_err(e)),
        },

        Request::ListCommits { limit } => match backend.list_commits(limit as usize) {
            Ok(list) => {
                let dtos: Vec<CommitInfoDto> = list.into_iter().map(commit_info_to_dto).collect();
                serde_json::to_value(dtos)
                    .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}")))
            }
            Err(e) => Err(backend_err(e)),
        },

        Request::ListRefs => match backend.list_refs() {
            Ok(map) => {
                let mut out = std::collections::HashMap::new();
                for (k, v) in map.into_iter() {
                    out.insert(k, v.into_iter().map(ref_label_to_dto).collect::<Vec<_>>());
                }
                serde_json::to_value(out).map_err(|e| (ErrorCode::Protocol, format!("encode: {e}")))
            }
            Err(e) => Err(backend_err(e)),
        },

        Request::HeadOid => match backend.head_oid() {
            Ok(opt) => {
                serde_json::to_value(opt).map_err(|e| (ErrorCode::Protocol, format!("encode: {e}")))
            }
            Err(e) => Err(backend_err(e)),
        },

        Request::CommitDetail { oid } => match backend.commit_detail(&oid) {
            Ok(opt) => serde_json::to_value(opt.map(commit_detail_to_dto))
                .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),
            Err(e) => Err(backend_err(e)),
        },

        Request::CommitFileDiff {
            oid,
            path,
            context_lines,
        } => match backend.commit_file_diff(&oid, &path, context_lines) {
            Ok(opt) => serde_json::to_value(opt.map(diff_to_dto))
                .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),
            Err(e) => Err(backend_err(e)),
        },

        Request::RangeFiles {
            oldest_oid,
            newest_oid,
        } => match backend.range_files(&oldest_oid, &newest_oid) {
            Ok(files) => {
                let dtos: Vec<FileEntryDto> = files.into_iter().map(file_entry_to_dto).collect();
                serde_json::to_value(dtos)
                    .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}")))
            }
            Err(e) => Err(backend_err(e)),
        },
        Request::RangeFileDiff {
            oldest_oid,
            newest_oid,
            path,
            context_lines,
        } => match backend.range_file_diff(&oldest_oid, &newest_oid, &path, context_lines) {
            Ok(opt) => serde_json::to_value(opt.map(diff_to_dto))
                .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),
            Err(e) => Err(backend_err(e)),
        },

        // ── M3 Track 1: write operations ────────────────────────────────
        Request::CreateFile { rel_path } => match backend.create_file(Path::new(&rel_path)) {
            Ok(()) => Ok(serde_json::json!({"ok": true})),
            Err(e) => Err(backend_err(e)),
        },
        Request::CreateDirAll { rel_path } => match backend.create_dir_all(Path::new(&rel_path)) {
            Ok(()) => Ok(serde_json::json!({"ok": true})),
            Err(e) => Err(backend_err(e)),
        },
        Request::Rename { from_rel, to_rel } => {
            match backend.rename(Path::new(&from_rel), Path::new(&to_rel)) {
                Ok(()) => Ok(serde_json::json!({"ok": true})),
                Err(e) => Err(backend_err(e)),
            }
        }
        Request::CopyFile { from_rel, to_rel } => {
            match backend.copy_file(Path::new(&from_rel), Path::new(&to_rel)) {
                Ok(()) => Ok(serde_json::json!({"ok": true})),
                Err(e) => Err(backend_err(e)),
            }
        }
        Request::CopyDirRecursive { from_rel, to_rel } => {
            match backend.copy_dir_recursive(Path::new(&from_rel), Path::new(&to_rel)) {
                Ok(()) => Ok(serde_json::json!({"ok": true})),
                Err(e) => Err(backend_err(e)),
            }
        }
        Request::RemoveFile { rel_path } => match backend.remove_file(Path::new(&rel_path)) {
            Ok(()) => Ok(serde_json::json!({"ok": true})),
            Err(e) => Err(backend_err(e)),
        },
        Request::RemoveDirAll { rel_path } => match backend.remove_dir_all(Path::new(&rel_path)) {
            Ok(()) => Ok(serde_json::json!({"ok": true})),
            Err(e) => Err(backend_err(e)),
        },
        Request::Trash { rel_paths } => {
            let abs_paths: Vec<PathBuf> = rel_paths.iter().map(PathBuf::from).collect();
            // Try `gio trash` for headless Linux parity with the GNOME
            // desktop's trash; fall back to `fs::remove_*` if it's not
            // installed. `reef` side reads `used_trash` to choose the
            // toast phrasing.
            match agent_trash_delete(workdir, &abs_paths) {
                Ok(used_trash) => serde_json::to_value(TrashResponseDto { used_trash })
                    .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),
                Err(e) => Err(e),
            }
        }
        Request::HardDelete { rel_paths } => {
            let abs_paths: Vec<PathBuf> = rel_paths.iter().map(PathBuf::from).collect();
            match backend.hard_delete(&abs_paths) {
                Ok(()) => Ok(serde_json::json!({"ok": true})),
                Err(e) => Err(backend_err(e)),
            }
        }

        // ── M3 Track 2: walk + search ───────────────────────────────────
        Request::WalkRepoPaths { opts } => {
            let domain = reef::backend::WalkOpts {
                include_hidden: opts.include_hidden,
                respect_gitignore: opts.respect_gitignore,
                max_files: opts.max_files,
            };
            match backend.walk_repo_paths(&domain) {
                Ok(resp) => serde_json::to_value(WalkResponseDto {
                    paths: resp.paths,
                    truncated: resp.truncated,
                })
                .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),
                Err(e) => Err(backend_err(e)),
            }
        }
        Request::SearchContent { .. } => {
            // Handled by `dispatch_search_content` at the call site so
            // the streaming `SearchChunk` frames can reach stdout
            // without widening this function's signature. Reaching
            // this arm means the special-case routing above was
            // bypassed — treat as a protocol bug.
            Err((
                ErrorCode::Protocol,
                "SearchContent must be routed through dispatch_search_content".to_string(),
            ))
        }
    };

    match result {
        Ok(result) => Some(Response::Ok { id, result }),
        Err((code, message)) => Some(Response::Err { id, code, message }),
    }
}

/// Drive `backend.search_content` with a streaming sink that pushes
/// `Notification::SearchChunk { request_id, hits }` frames to stdout
/// as the walker produces them. Returns the terminal `Response` that
/// the caller writes to stdout once the walk finishes (carrying only
/// the `truncated` marker; hits already shipped in the notifications).
fn dispatch_search_content(
    backend: &dyn Backend,
    id: u64,
    request: reef_proto::ContentSearchRequestDto,
    stdout: Arc<Mutex<BufWriter<Stdout>>>,
) -> Option<Response> {
    let domain = reef::backend::ContentSearchRequest {
        pattern: request.pattern,
        fixed_strings: request.fixed_strings,
        case_sensitive: request.case_sensitive,
        max_results: request.max_results,
        max_line_chars: request.max_line_chars,
    };

    // The closure needs to reach `stdout`; it's an `Arc<Mutex<_>>` so
    // we move a clone in. If the frame write ever fails (broken pipe,
    // the client went away) we flip `broken` and return
    // `ControlFlow::Break` to short-circuit the walker so we don't
    // keep doing work for nobody.
    let mut broken = false;
    let mut sink = |hits: Vec<reef::backend::ContentMatchHit>| -> ControlFlow<()> {
        let dto_hits: Vec<MatchHitDto> = hits
            .into_iter()
            .map(|h| MatchHitDto {
                path: h.path.to_string_lossy().to_string(),
                display: h.display,
                line: h.line as u64,
                line_text: h.line_text,
                byte_range_start: h.byte_range.start as u32,
                byte_range_end: h.byte_range.end as u32,
            })
            .collect();
        let frame = Frame::Notification(Notification::SearchChunk {
            request_id: id,
            hits: dto_hits,
        });
        let mut guard = match stdout.lock() {
            Ok(g) => g,
            Err(_) => {
                broken = true;
                return ControlFlow::Break(());
            }
        };
        if encode_frame(&mut *guard, &frame).is_err() {
            broken = true;
            return ControlFlow::Break(());
        }
        if guard.flush().is_err() {
            broken = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    };

    let result: Result<serde_json::Value, (ErrorCode, String)> =
        match backend.search_content(&domain, &mut sink) {
            Ok(completed) => serde_json::to_value(ContentSearchCompletedDto {
                truncated: completed.truncated,
            })
            .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),
            Err(e) => Err(backend_err(e)),
        };

    // If stdout is wedged the final response frame can't land either;
    // just drop the response — the client already saw the pipe close.
    if broken {
        return None;
    }
    match result {
        Ok(result) => Some(Response::Ok { id, result }),
        Err((code, message)) => Some(Response::Err { id, code, message }),
    }
}

fn backend_err(e: reef::backend::BackendError) -> (ErrorCode, String) {
    (e.wire_code(), e.to_string())
}

/// Agent-side `ReadFile` dispatcher. Rejects lexical escapes *and*
/// symlink escapes — a workdir containing `link → /etc/passwd` would
/// otherwise let a malicious client exfiltrate any file the agent user
/// can read. `NotFound` is folded into `is_file: false` so the client
/// contract stays "no error on missing file"; `PathEscape` and other
/// filesystem errors surface through the normal error channel.
fn read_file_response(
    workdir: &Path,
    rel: &str,
    max_bytes: u64,
) -> Result<serde_json::Value, (ErrorCode, String)> {
    use reef::backend::BackendError;
    let missing = || {
        serde_json::to_value(ReadFileResponse {
            is_file: false,
            bytes: Vec::new(),
            size: 0,
        })
        .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}")))
    };
    let abs = match reef::backend::local::canonical_child_within(workdir, Path::new(rel)) {
        Ok(p) => p,
        Err(BackendError::NotFound) => return missing(),
        Err(e) => return Err(backend_err(e)),
    };
    if !abs.is_file() {
        return missing();
    }
    let raw = std::fs::read(&abs).map_err(|e| (ErrorCode::Io, e.to_string()))?;
    let size = raw.len() as u64;
    let bytes = if size > max_bytes {
        raw[..max_bytes as usize].to_vec()
    } else {
        raw
    };
    serde_json::to_value(ReadFileResponse {
        is_file: true,
        bytes,
        size,
    })
    .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}")))
}

/// Probe once for `gio` and cache the result. `0` = unknown, `1` =
/// available, `-1` = unavailable. Avoids re-fork-ing on every Trash
/// request.
static GIO_PRESENT: AtomicI8 = AtomicI8::new(0);

fn has_gio() -> bool {
    match GIO_PRESENT.load(Ordering::Relaxed) {
        1 => true,
        -1 => false,
        _ => {
            let ok = std::process::Command::new("gio")
                .arg("--help")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            GIO_PRESENT.store(if ok { 1 } else { -1 }, Ordering::Relaxed);
            ok
        }
    }
}

/// Remote-side trash. Tries `gio trash <abs>` first on Linux; falls
/// through to `fs::remove_*` when no trash tool is available. Returns
/// `Ok(true)` when the trash tool succeeded, `Ok(false)` when we fell
/// back to permanent delete.
fn agent_trash_delete(workdir: &Path, rel_paths: &[PathBuf]) -> Result<bool, (ErrorCode, String)> {
    use reef::backend::local::resolve_rel_within;
    // Validate workdir-relative up front so a bad path aborts before any
    // side-effect.
    let abs_paths: Vec<PathBuf> = rel_paths
        .iter()
        .map(|r| resolve_rel_within(workdir, r).map_err(backend_err))
        .collect::<Result<_, _>>()?;

    if has_gio() {
        let mut cmd = std::process::Command::new("gio");
        cmd.arg("trash");
        for p in &abs_paths {
            cmd.arg(p);
        }
        let status = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .status();
        match status {
            Ok(s) if s.success() => return Ok(true),
            Ok(_) => {
                // gio trash failed for this specific path (mount doesn't
                // expose a trash dir, etc.) — fall through to remove_* so
                // the user still gets the delete they asked for.
            }
            Err(e) => {
                eprintln!("[reef-agent] gio trash spawn failed: {e}");
            }
        }
    }

    for abs in &abs_paths {
        let res = if abs.is_dir() {
            std::fs::remove_dir_all(abs)
        } else {
            std::fs::remove_file(abs)
        };
        res.map_err(|e| (ErrorCode::Io, format!("delete {}: {}", abs.display(), e)))?;
    }
    Ok(false)
}

fn file_entry_to_dto(e: reef::git::FileEntry) -> FileEntryDto {
    FileEntryDto {
        path: e.path,
        status: file_status_to_dto(e.status),
        additions: e.additions,
        deletions: e.deletions,
    }
}

fn file_status_to_dto(s: reef::git::FileStatus) -> FileStatusDto {
    use reef::git::FileStatus;
    match s {
        FileStatus::Modified => FileStatusDto::Modified,
        FileStatus::Added => FileStatusDto::Added,
        FileStatus::Deleted => FileStatusDto::Deleted,
        FileStatus::Renamed => FileStatusDto::Renamed,
        FileStatus::Untracked => FileStatusDto::Untracked,
    }
}

fn diff_to_dto(d: reef::git::DiffContent) -> DiffContentDto {
    DiffContentDto {
        file_path: d.file_path,
        hunks: d.hunks.into_iter().map(diff_hunk_to_dto).collect(),
    }
}

fn diff_hunk_to_dto(h: reef::git::DiffHunk) -> DiffHunkDto {
    DiffHunkDto {
        header: h.header,
        lines: h.lines.into_iter().map(diff_line_to_dto).collect(),
    }
}

fn diff_line_to_dto(l: reef::git::DiffLine) -> DiffLineDto {
    DiffLineDto {
        tag: line_tag_to_dto(l.tag),
        content: l.content,
        old_lineno: l.old_lineno,
        new_lineno: l.new_lineno,
    }
}

fn line_tag_to_dto(t: reef::git::LineTag) -> LineTagDto {
    use reef::git::LineTag;
    match t {
        LineTag::Context => LineTagDto::Context,
        LineTag::Added => LineTagDto::Added,
        LineTag::Removed => LineTagDto::Removed,
    }
}

fn commit_info_to_dto(c: reef::git::CommitInfo) -> CommitInfoDto {
    CommitInfoDto {
        oid: c.oid,
        short_oid: c.short_oid,
        parents: c.parents,
        author_name: c.author_name,
        author_email: c.author_email,
        time: c.time,
        subject: c.subject,
    }
}

fn commit_detail_to_dto(c: reef::git::CommitDetail) -> CommitDetailDto {
    CommitDetailDto {
        info: commit_info_to_dto(c.info),
        message: c.message,
        committer_name: c.committer_name,
        committer_time: c.committer_time,
        files: c.files.into_iter().map(file_entry_to_dto).collect(),
    }
}

fn ref_label_to_dto(r: reef::git::RefLabel) -> RefLabelDto {
    use reef::git::RefLabel;
    match r {
        RefLabel::Head => RefLabelDto::Head,
        RefLabel::Branch(s) => RefLabelDto::Branch(s),
        RefLabel::RemoteBranch(s) => RefLabelDto::RemoteBranch(s),
        RefLabel::Tag(s) => RefLabelDto::Tag(s),
    }
}
