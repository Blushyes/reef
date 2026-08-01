//! reef-agent — the remote daemon spawned by `reef --agent-exec`.
//!
//! Speaks the length-prefixed JSON-RPC protocol from `reef-proto` over
//! stdin/stdout. Internally it's a thin dispatcher over `reef_io::
//! LocalBackend` — every Phase 0 operation we gave the trait has a one-to-
//! one RPC counterpart here.
//!
//! Threading:
//!   - main thread: read stdin, dispatch requests, write responses
//!   - fs-watcher thread: wait on `LocalBackend::subscribe_fs_events()`
//!     and push `Notification::FsChanged` frames to stdout
//!   - database-cell thread: read and stream complete SQLite cells without
//!     blocking unrelated RPC dispatch
//!   - search thread: scan content and stream hits while the main thread stays
//!     available to receive cancellation requests
//!
//! All writers share a `Mutex<Stdout>` to serialise frames.

use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Stdout, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use reef_io::{Backend, LocalBackend};
use reef_proto::{
    CommitDetailDto, CommitInfoDto, ContentSearchCompletedDto, DiffContentDto, DiffHunkDto,
    DiffLineDto, DirEntryDto, Envelope, ErrorCode, FileEntryDto, FileStatusDto, Frame,
    GitPathMutationKindDto, GitStatusStatsDto, HandshakeResponse, LineTagDto, MatchHitDto,
    Notification, PROTOCOL_VERSION, ReadFileResponse, RefLabelDto, ReplaceFileOutcomeDto, Request,
    Response, StatusSnapshotDto, TrashResponseDto, WalkResponseDto, encode_frame, read_envelope,
};

struct Args {
    stdio: bool,
    workdir: Option<PathBuf>,
}

struct DbCellTask {
    id: u64,
    rel_path: String,
    key: reef_sqlite_preview::DbObjectKey,
    locator: reef_sqlite_preview::DbRowLocator,
    column: usize,
    cancellation: reef_io::CancellationToken,
}

struct PendingGitPathMutation {
    kind: GitPathMutationKindDto,
    paths: Vec<String>,
}

struct SearchTask {
    id: u64,
    request: reef_proto::ContentSearchRequestDto,
    cancellation: reef_io::CancellationToken,
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
    let db_cell_cancellations = Arc::new(Mutex::new(HashMap::new()));
    let db_cell_tx = spawn_db_cell_worker(
        workdir.clone(),
        Arc::clone(&stdout),
        Arc::clone(&db_cell_cancellations),
    )?;
    let search_cancellations = Arc::new(Mutex::new(HashMap::new()));
    let search_tx = spawn_search_worker(
        Arc::clone(&backend),
        Arc::clone(&stdout),
        Arc::clone(&search_cancellations),
    )?;

    // Start watcher thread eagerly — reef's Subscribe is idempotent and we
    // want the channel drained from the moment the agent starts.
    let watcher_rx = backend.subscribe_fs_events();
    let watcher_stdout = Arc::clone(&stdout);
    let watcher_backend = Arc::clone(&backend);
    let _watcher = thread::Builder::new()
        .name("reef-agent-watcher".into())
        .spawn(move || {
            while let Ok(change) = watcher_rx.recv() {
                let frame =
                    Frame::Notification(fs_change_notification(change, watcher_backend.has_repo()));
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
    let mut pending_git_path_mutations = HashMap::new();

    loop {
        let envelope = match read_envelope(&mut reader) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                eprintln!("[reef-agent] read error: {e}");
                break;
            }
        };

        let response = if matches!(envelope.body, Request::GitPathMutationChunk { .. }) {
            dispatch_git_path_mutation_chunk(&*backend, envelope, &mut pending_git_path_mutations)
        } else if let Request::SearchContent { request } = &envelope.body {
            let cancellation = reef_io::CancellationToken::default();
            search_cancellations
                .lock()
                .map_err(|_| io::Error::other("search cancellation lock poisoned"))?
                .insert(envelope.id, cancellation.clone());
            search_tx
                .send(SearchTask {
                    id: envelope.id,
                    request: request.clone(),
                    cancellation,
                })
                .err()
                .map(|_| {
                    if let Ok(mut searches) = search_cancellations.lock() {
                        searches.remove(&envelope.id);
                    }
                    Response::Err {
                        id: envelope.id,
                        code: ErrorCode::Other,
                        message: "search worker stopped".into(),
                    }
                })
        } else if let Request::CancelSearch { request_id } = envelope.body {
            let cancelled = search_cancellations
                .lock()
                .map_err(|_| io::Error::other("search cancellation lock poisoned"))?
                .get(&request_id)
                .map(|cancellation| cancellation.cancel())
                .is_some();
            Some(Response::Ok {
                id: envelope.id,
                result: serde_json::json!({ "cancelled": cancelled }),
            })
        } else if let Request::LoadDbCell {
            rel_path,
            schema,
            kind,
            name,
            locator,
            column,
        } = &envelope.body
        {
            let cancellation = reef_io::CancellationToken::default();
            db_cell_cancellations
                .lock()
                .map_err(|_| io::Error::other("database cell cancellation lock poisoned"))?
                .insert(envelope.id, cancellation.clone());
            let task = DbCellTask {
                id: envelope.id,
                rel_path: rel_path.clone(),
                key: reef_sqlite_preview::DbObjectKey {
                    schema: schema.clone(),
                    name: name.clone(),
                    kind: db_object_kind_from_dto(*kind),
                },
                locator: db_row_locator_from_dto(locator.clone()),
                column: *column,
                cancellation,
            };
            db_cell_tx.send(task).err().map(|_| {
                if let Ok(mut cells) = db_cell_cancellations.lock() {
                    cells.remove(&envelope.id);
                }
                Response::Err {
                    id: envelope.id,
                    code: ErrorCode::Other,
                    message: "database cell worker stopped".into(),
                }
            })
        } else if let Request::CancelDbCell { request_id } = envelope.body {
            let cancelled = db_cell_cancellations
                .lock()
                .map_err(|_| io::Error::other("database cell cancellation lock poisoned"))?
                .get(&request_id)
                .map(|cancellation| cancellation.cancel())
                .is_some();
            Some(Response::Ok {
                id: envelope.id,
                result: serde_json::json!({ "cancelled": cancelled }),
            })
        } else {
            dispatch(&*backend, &workdir, envelope)
        };
        let should_shutdown =
            matches!(&response, Some(Response::Ok { .. }) if is_shutdown_reply(&response));
        if let Some(resp) = response {
            write_response(&stdout, resp)?;
        }
        if should_shutdown {
            break;
        }
    }

    Ok(())
}

fn dispatch_git_path_mutation_chunk(
    backend: &dyn Backend,
    envelope: Envelope,
    pending: &mut HashMap<u64, PendingGitPathMutation>,
) -> Option<Response> {
    let id = envelope.id;
    let Request::GitPathMutationChunk {
        operation_id,
        kind,
        paths,
        final_chunk,
    } = envelope.body
    else {
        unreachable!("git mutation chunk dispatcher received another request")
    };

    let mutation = pending
        .entry(operation_id)
        .or_insert_with(|| PendingGitPathMutation {
            kind,
            paths: Vec::new(),
        });
    if mutation.kind != kind {
        pending.remove(&operation_id);
        return Some(Response::Err {
            id,
            code: ErrorCode::Protocol,
            message: format!("git mutation {operation_id} changed kind between chunks"),
        });
    }
    mutation.paths.extend(paths);

    if !final_chunk {
        return Some(Response::Ok {
            id,
            result: serde_json::json!({"accepted": true}),
        });
    }

    let mutation = pending
        .remove(&operation_id)
        .expect("the pending mutation was inserted above");
    let result = match mutation.kind {
        GitPathMutationKindDto::Stage => backend.stage_paths(&mutation.paths),
        GitPathMutationKindDto::Unstage => backend.unstage_paths(&mutation.paths),
    };
    Some(match result {
        Ok(()) => Response::Ok {
            id,
            result: serde_json::json!({"ok": true}),
        },
        Err(error) => {
            let (code, message) = backend_err(error);
            Response::Err { id, code, message }
        }
    })
}

fn spawn_db_cell_worker(
    workdir: PathBuf,
    stdout: Arc<Mutex<BufWriter<Stdout>>>,
    cancellations: Arc<Mutex<HashMap<u64, reef_io::CancellationToken>>>,
) -> io::Result<mpsc::Sender<DbCellTask>> {
    let (tx, rx) = mpsc::channel::<DbCellTask>();
    thread::Builder::new()
        .name("reef-agent-db-cell".into())
        .spawn(move || {
            while let Ok(task) = rx.recv() {
                let request_id = task.id;
                let response = dispatch_db_cell(&workdir, task, Arc::clone(&stdout));
                if let Ok(mut cells) = cancellations.lock() {
                    cells.remove(&request_id);
                }
                let Some(response) = response else {
                    break;
                };
                if write_response(&stdout, response).is_err() {
                    break;
                }
            }
        })?;
    Ok(tx)
}

fn spawn_search_worker(
    backend: Arc<LocalBackend>,
    stdout: Arc<Mutex<BufWriter<Stdout>>>,
    cancellations: Arc<Mutex<HashMap<u64, reef_io::CancellationToken>>>,
) -> io::Result<mpsc::Sender<SearchTask>> {
    let (tx, rx) = mpsc::channel::<SearchTask>();
    thread::Builder::new()
        .name("reef-agent-search".into())
        .spawn(move || {
            while let Ok(task) = rx.recv() {
                let response = dispatch_search_content(
                    &*backend,
                    task.id,
                    task.request,
                    task.cancellation,
                    Arc::clone(&stdout),
                );
                if let Ok(mut searches) = cancellations.lock() {
                    searches.remove(&task.id);
                }
                let Some(response) = response else {
                    break;
                };
                if write_response(&stdout, response).is_err() {
                    break;
                }
            }
        })?;
    Ok(tx)
}

fn write_response(stdout: &Mutex<BufWriter<Stdout>>, response: Response) -> io::Result<()> {
    let mut writer = stdout
        .lock()
        .map_err(|_| io::Error::other("agent stdout lock poisoned"))?;
    encode_frame(&mut *writer, &Frame::Response(response))?;
    writer.flush()
}

fn fs_change_notification(change: reef_io::FsChange, has_repo: bool) -> Notification {
    Notification::FsChanged {
        has_repo,
        workspace_changed: change.workspace_changed,
        workspace_paths: change
            .workspace_paths
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        git_metadata_changed: change.git_metadata_changed,
    }
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
            has_repo: backend.has_repo(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
        })
        .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),

        Request::Shutdown => Ok(serde_json::json!({"shutting_down": true})),

        Request::Subscribe => Ok(serde_json::json!({"subscribed": true})),

        Request::ReadDir { path } => match backend.list_dir(Path::new(&path)) {
            Ok(entries) => serde_json::to_value(
                entries
                    .into_iter()
                    .map(|entry| DirEntryDto {
                        name: entry.name,
                        is_dir: entry.is_dir,
                        has_children: entry.has_children,
                    })
                    .collect::<Vec<_>>(),
            )
            .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),
            Err(e) => Err(backend_err(e)),
        },

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
        Request::GitStatusStats => match backend.git_status_stats() {
            Ok(stats) => serde_json::to_value(GitStatusStatsDto {
                staged: stats.staged,
                unstaged: stats.unstaged,
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
        Request::GitPathMutationChunk { .. } => Err((
            ErrorCode::Protocol,
            "git mutation chunks must be dispatched by the connection state machine".into(),
        )),
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

        Request::ListCommits { limit, scope } => match backend
            .list_commits(&graph_scope_from_dto(scope), limit as usize)
        {
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
        Request::FileSize { rel_path } => match backend.file_size(Path::new(&rel_path)) {
            Ok(size) => Ok(serde_json::json!({ "size": size })),
            Err(e) => Err(backend_err(e)),
        },
        Request::WriteFile { rel_path, content } => {
            match backend.write_file(Path::new(&rel_path), &content) {
                Ok(()) => Ok(serde_json::json!({"ok": true})),
                Err(e) => Err(backend_err(e)),
            }
        }
        Request::ReplaceFile {
            rel_path,
            pattern,
            replacement,
            lines,
            max_file_size,
        } => {
            let request = reef_io::ReplaceFileRequest {
                pattern,
                replacement,
                lines: lines
                    .into_iter()
                    .map(|line| reef_io::ReplaceLineGuard {
                        line_no: line.line_no,
                        expected_revision: line.expected_revision,
                    })
                    .collect(),
                max_file_size,
            };
            match backend.replace_file(Path::new(&rel_path), &request) {
                Ok(outcome) => serde_json::to_value(match outcome {
                    reef_io::ReplaceFileOutcome::Changed {
                        lines_replaced,
                        stale,
                    } => ReplaceFileOutcomeDto::Changed {
                        lines_replaced,
                        stale,
                    },
                    reef_io::ReplaceFileOutcome::NoMatch { stale } => {
                        ReplaceFileOutcomeDto::NoMatch { stale }
                    }
                    reef_io::ReplaceFileOutcome::TooLarge => ReplaceFileOutcomeDto::TooLarge,
                })
                .map_err(|error| (ErrorCode::Protocol, format!("encode: {error}"))),
                Err(error) => Err(backend_err(error)),
            }
        }
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
            let domain = reef_io::WalkOpts {
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
        Request::CancelSearch { .. } => Err((
            ErrorCode::Protocol,
            "CancelSearch must be routed through the connection state machine".to_string(),
        )),

        // ── M5: SQLite preview ────
        Request::LoadDbInitial {
            rel_path,
            page_size,
        } => load_db_initial_handler(workdir, &rel_path, page_size),
        Request::LoadDbPage {
            rel_path,
            table,
            offset,
            limit,
        } => load_db_page_handler(workdir, &rel_path, &table, offset, limit),

        // ── v9: SQLite preview, multi-schema ────
        Request::LoadDbInitialV2 {
            rel_path,
            page_size,
        } => load_db_initial_v2_handler(workdir, &rel_path, page_size),
        Request::LoadDbPageV2 {
            rel_path,
            schema,
            kind,
            name,
            offset,
            limit,
        } => load_db_page_v2_handler(workdir, &rel_path, &schema, kind, &name, offset, limit),
        Request::LoadDbObjectDetail {
            rel_path,
            schema,
            kind,
            name,
        } => load_db_object_detail_handler(workdir, &rel_path, &schema, kind, &name),
        Request::LoadDbCell { .. } => Err((
            ErrorCode::Protocol,
            "LoadDbCell must be routed through dispatch_db_cell".to_string(),
        )),
        Request::CancelDbCell { .. } => Err((
            ErrorCode::Protocol,
            "CancelDbCell must be routed through the connection state machine".to_string(),
        )),
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
    cancellation: reef_io::CancellationToken,
    stdout: Arc<Mutex<BufWriter<Stdout>>>,
) -> Option<Response> {
    let domain = reef_io::ContentSearchRequest {
        pattern: request.pattern,
        fixed_strings: request.fixed_strings,
        case_sensitive: request.case_sensitive,
        max_results: request.max_results,
        max_line_chars: request.max_line_chars,
        cancellation,
    };

    // The closure needs to reach `stdout`; it's an `Arc<Mutex<_>>` so
    // we move a clone in. If the frame write ever fails (broken pipe,
    // the client went away) we flip `broken` and return
    // `ControlFlow::Break` to short-circuit the walker so we don't
    // keep doing work for nobody.
    let mut broken = false;
    let mut sink = |hits: Vec<reef_io::ContentMatchHit>| -> ControlFlow<()> {
        let dto_hits: Vec<MatchHitDto> = hits
            .into_iter()
            .map(|h| MatchHitDto {
                path: h.path.to_string_lossy().to_string(),
                display: h.display,
                line: h.line as u64,
                line_text: h.line_text,
                line_revision: h.line_revision,
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

fn backend_err(e: reef_io::BackendError) -> (ErrorCode, String) {
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
    use std::io::Read;

    use reef_io::BackendError;
    let missing = || {
        serde_json::to_value(ReadFileResponse {
            is_file: false,
            bytes: Vec::new(),
            size: 0,
            resolved_path: None,
        })
        .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}")))
    };
    let abs = match reef_io::local::canonical_child_within(workdir, Path::new(rel)) {
        Ok(p) => p,
        Err(BackendError::NotFound) => return missing(),
        Err(e) => return Err(backend_err(e)),
    };
    if !abs.is_file() {
        return missing();
    }
    let file = std::fs::File::open(&abs).map_err(|e| (ErrorCode::Io, e.to_string()))?;
    let size = file
        .metadata()
        .map_err(|e| (ErrorCode::Io, e.to_string()))?
        .len();
    let mut bytes = Vec::new();
    file.take(max_bytes)
        .read_to_end(&mut bytes)
        .map_err(|e| (ErrorCode::Io, e.to_string()))?;
    let resolved_path = abs
        .strip_prefix(workdir)
        .map_err(|e| {
            (
                ErrorCode::Protocol,
                format!("resolved path escaped workdir: {e}"),
            )
        })?
        .to_string_lossy()
        .into_owned();
    serde_json::to_value(ReadFileResponse {
        is_file: true,
        bytes,
        size,
        resolved_path: Some(resolved_path),
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
    use reef_io::local::resolve_rel_within;
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

fn file_entry_to_dto(e: reef_core::git::FileEntry) -> FileEntryDto {
    FileEntryDto {
        path: e.path,
        status: file_status_to_dto(e.status),
        additions: e.additions,
        deletions: e.deletions,
    }
}

fn file_status_to_dto(s: reef_core::git::FileStatus) -> FileStatusDto {
    use reef_core::git::FileStatus;
    match s {
        FileStatus::Modified => FileStatusDto::Modified,
        FileStatus::Added => FileStatusDto::Added,
        FileStatus::Deleted => FileStatusDto::Deleted,
        FileStatus::Renamed => FileStatusDto::Renamed,
        FileStatus::Untracked => FileStatusDto::Untracked,
    }
}

fn diff_to_dto(d: reef_core::diff::DiffContent) -> DiffContentDto {
    DiffContentDto {
        path: d.path,
        hunks: d.hunks.into_iter().map(diff_hunk_to_dto).collect(),
    }
}

fn diff_hunk_to_dto(h: reef_core::diff::DiffHunk) -> DiffHunkDto {
    DiffHunkDto {
        header: h.header.to_string(),
        lines: h.lines.into_iter().map(diff_line_to_dto).collect(),
    }
}

fn diff_line_to_dto(l: reef_core::diff::DiffLine) -> DiffLineDto {
    DiffLineDto {
        tag: line_tag_to_dto(l.tag),
        content: l.content.to_string(),
        old_lineno: l.old_lineno,
        new_lineno: l.new_lineno,
    }
}

fn line_tag_to_dto(t: reef_core::diff::LineTag) -> LineTagDto {
    use reef_core::diff::LineTag;
    match t {
        LineTag::Context => LineTagDto::Context,
        LineTag::Added => LineTagDto::Added,
        LineTag::Removed => LineTagDto::Removed,
    }
}

fn commit_info_to_dto(c: reef_core::git::CommitInfo) -> CommitInfoDto {
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

fn commit_detail_to_dto(c: reef_core::git::CommitDetail) -> CommitDetailDto {
    CommitDetailDto {
        info: commit_info_to_dto(c.info),
        message: c.message,
        committer_name: c.committer_name,
        committer_time: c.committer_time,
        files: c.files.into_iter().map(file_entry_to_dto).collect(),
    }
}

fn ref_label_to_dto(r: reef_core::git::RefLabel) -> RefLabelDto {
    use reef_core::git::RefLabel;
    match r {
        RefLabel::Head => RefLabelDto::Head,
        RefLabel::Branch(s) => RefLabelDto::Branch(s),
        RefLabel::RemoteBranch(s) => RefLabelDto::RemoteBranch(s),
        RefLabel::Tag(s) => RefLabelDto::Tag(s),
    }
}

fn graph_scope_from_dto(scope: reef_proto::GraphScopeDto) -> reef_core::git::GraphScope {
    match scope {
        reef_proto::GraphScopeDto::AllRefs => reef_core::git::GraphScope::AllRefs,
        reef_proto::GraphScopeDto::Branch(s) => reef_core::git::GraphScope::Branch(s),
    }
}

// ── SQLite preview dispatch + domain → DTO helpers ──────────────────────

/// Agent-side `LoadDbInitial` handler. Resolves the workdir-relative
/// path, applies the same symlink-escape gate as `read_file_response`,
/// then either returns a `DatabaseInfoDto` or `None` (file isn't a
/// SQLite database — client falls back to the binary card path).
///
/// Hard errors (encrypted, corrupt, oversized) collapse into
/// `ErrorCode::Other` with the reader's message verbatim — the client
/// surfaces those as a toast / preview error.
fn load_db_initial_handler(
    workdir: &Path,
    rel: &str,
    page_size: u32,
) -> Result<serde_json::Value, (ErrorCode, String)> {
    use reef_io::BackendError;
    use reef_io::local::canonical_child_within;
    let none_value = || {
        serde_json::to_value(None::<reef_proto::DatabaseInfoDto>)
            .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}")))
    };
    let abs = match canonical_child_within(workdir, Path::new(rel)) {
        Ok(p) => p,
        Err(BackendError::NotFound) => return none_value(),
        Err(e) => return Err(backend_err(e)),
    };
    if !abs.is_file() {
        return none_value();
    }
    if !reef_sqlite_preview::has_sqlite_extension(Path::new(rel)) {
        return none_value();
    }
    match reef_sqlite_preview::probe_magic(&abs) {
        Ok(false) => return none_value(),
        Err(e) => return Err((ErrorCode::Io, e.to_string())),
        Ok(true) => {}
    }
    match reef_sqlite_preview::read_initial(&abs, page_size) {
        Ok(info) => serde_json::to_value(Some(database_info_to_dto(info)))
            .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),
        Err(e) => Err((ErrorCode::Other, format!("sqlite: {e}"))),
    }
}

fn load_db_page_handler(
    workdir: &Path,
    rel: &str,
    table: &str,
    offset: u64,
    limit: u32,
) -> Result<serde_json::Value, (ErrorCode, String)> {
    use reef_io::BackendError;
    use reef_io::local::canonical_child_within;
    let abs = match canonical_child_within(workdir, Path::new(rel)) {
        Ok(p) => p,
        Err(BackendError::NotFound) => return Err((ErrorCode::NotFound, "file not found".into())),
        Err(e) => return Err(backend_err(e)),
    };
    if !abs.is_file() {
        return Err((ErrorCode::NotFound, "not a regular file".into()));
    }
    match reef_sqlite_preview::load_page(&abs, table, offset, limit) {
        Ok(page) => serde_json::to_value(db_page_to_dto(page))
            .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),
        Err(e) => Err((ErrorCode::Other, format!("sqlite: {e}"))),
    }
}

fn database_info_to_dto(d: reef_sqlite_preview::DatabaseInfo) -> reef_proto::DatabaseInfoDto {
    reef_proto::DatabaseInfoDto {
        tables: d.tables.into_iter().map(table_summary_to_dto).collect(),
        selected_table: d.selected_table as u32,
        initial_page: db_page_to_dto(d.initial_page),
        bytes_on_disk: d.bytes_on_disk,
    }
}

fn table_summary_to_dto(t: reef_sqlite_preview::TableSummary) -> reef_proto::TableSummaryDto {
    reef_proto::TableSummaryDto {
        name: t.name,
        columns: t.columns.into_iter().map(column_info_to_dto).collect(),
        row_count: t.row_count,
    }
}

fn column_info_to_dto(c: reef_sqlite_preview::ColumnInfo) -> reef_proto::ColumnInfoDto {
    reef_proto::ColumnInfoDto {
        name: c.name,
        decl_type: c.decl_type,
        notnull: c.notnull,
        pk: c.pk,
    }
}

fn db_page_to_dto(p: reef_sqlite_preview::DbPage) -> reef_proto::DbPageDto {
    reef_proto::DbPageDto {
        rows: p
            .rows
            .into_iter()
            .map(|cells| cells.into_iter().map(sqlite_value_to_dto).collect())
            .collect(),
        row_locators: p
            .row_locators
            .into_iter()
            .map(db_row_locator_to_dto)
            .collect(),
    }
}

fn db_row_locator_to_dto(
    locator: reef_sqlite_preview::DbRowLocator,
) -> reef_proto::DbRowLocatorDto {
    match locator {
        reef_sqlite_preview::DbRowLocator::RowId(value) => {
            reef_proto::DbRowLocatorDto::RowId { value }
        }
        reef_sqlite_preview::DbRowLocator::PrimaryKey(values) => {
            reef_proto::DbRowLocatorDto::PrimaryKey {
                values: values.into_iter().map(db_locator_value_to_dto).collect(),
            }
        }
        reef_sqlite_preview::DbRowLocator::OffsetFingerprint {
            offset,
            fingerprint,
        } => reef_proto::DbRowLocatorDto::OffsetFingerprint {
            offset,
            fingerprint,
        },
    }
}

fn db_locator_value_to_dto(
    value: reef_sqlite_preview::DbLocatorValue,
) -> reef_proto::DbLocatorValueDto {
    match value {
        reef_sqlite_preview::DbLocatorValue::Null => reef_proto::DbLocatorValueDto::Null,
        reef_sqlite_preview::DbLocatorValue::Integer(value) => {
            reef_proto::DbLocatorValueDto::Integer { value }
        }
        reef_sqlite_preview::DbLocatorValue::Real(value) => {
            reef_proto::DbLocatorValueDto::Real { value }
        }
        reef_sqlite_preview::DbLocatorValue::Text(value) => {
            reef_proto::DbLocatorValueDto::Text { value }
        }
        reef_sqlite_preview::DbLocatorValue::Blob(bytes) => {
            reef_proto::DbLocatorValueDto::Blob { bytes }
        }
    }
}

fn db_row_locator_from_dto(
    locator: reef_proto::DbRowLocatorDto,
) -> reef_sqlite_preview::DbRowLocator {
    match locator {
        reef_proto::DbRowLocatorDto::RowId { value } => {
            reef_sqlite_preview::DbRowLocator::RowId(value)
        }
        reef_proto::DbRowLocatorDto::PrimaryKey { values } => {
            reef_sqlite_preview::DbRowLocator::PrimaryKey(
                values.into_iter().map(db_locator_value_from_dto).collect(),
            )
        }
        reef_proto::DbRowLocatorDto::OffsetFingerprint {
            offset,
            fingerprint,
        } => reef_sqlite_preview::DbRowLocator::OffsetFingerprint {
            offset,
            fingerprint,
        },
    }
}

fn db_locator_value_from_dto(
    value: reef_proto::DbLocatorValueDto,
) -> reef_sqlite_preview::DbLocatorValue {
    match value {
        reef_proto::DbLocatorValueDto::Null => reef_sqlite_preview::DbLocatorValue::Null,
        reef_proto::DbLocatorValueDto::Integer { value } => {
            reef_sqlite_preview::DbLocatorValue::Integer(value)
        }
        reef_proto::DbLocatorValueDto::Real { value } => {
            reef_sqlite_preview::DbLocatorValue::Real(value)
        }
        reef_proto::DbLocatorValueDto::Text { value } => {
            reef_sqlite_preview::DbLocatorValue::Text(value)
        }
        reef_proto::DbLocatorValueDto::Blob { bytes } => {
            reef_sqlite_preview::DbLocatorValue::Blob(bytes)
        }
    }
}

fn sqlite_value_to_dto(v: reef_sqlite_preview::SqliteValue) -> reef_proto::SqliteValueDto {
    match v {
        reef_sqlite_preview::SqliteValue::Null => reef_proto::SqliteValueDto::Null,
        reef_sqlite_preview::SqliteValue::Integer(value) => {
            reef_proto::SqliteValueDto::Integer { value }
        }
        reef_sqlite_preview::SqliteValue::Real(value) => reef_proto::SqliteValueDto::Real { value },
        reef_sqlite_preview::SqliteValue::Text { value, truncated } => {
            reef_proto::SqliteValueDto::Text { value, truncated }
        }
        reef_sqlite_preview::SqliteValue::Blob { len } => {
            reef_proto::SqliteValueDto::Blob { len: len as u64 }
        }
    }
}

// ── v9: SQLite preview, multi-schema handlers ─────────────────────────────

/// V2 of [`load_db_initial_handler`]. Walks every schema (main/temp/
/// attached) via `read_initial_v2` and returns the full graph.
fn load_db_initial_v2_handler(
    workdir: &Path,
    rel: &str,
    page_size: u32,
) -> Result<serde_json::Value, (ErrorCode, String)> {
    use reef_io::BackendError;
    use reef_io::local::canonical_child_within;
    let response_value = |info, resolved_path| {
        serde_json::to_value(reef_proto::LoadDbInitialV2ResponseDto {
            info,
            resolved_path,
        })
        .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}")))
    };
    let abs = match canonical_child_within(workdir, Path::new(rel)) {
        Ok(p) => p,
        Err(BackendError::NotFound) => return response_value(None, None),
        Err(e) => return Err(backend_err(e)),
    };
    if !abs.is_file() {
        return response_value(None, None);
    }
    let resolved_path = abs
        .strip_prefix(workdir)
        .map_err(|e| {
            (
                ErrorCode::Protocol,
                format!("resolved path escaped workdir: {e}"),
            )
        })?
        .to_string_lossy()
        .into_owned();
    if !reef_sqlite_preview::has_sqlite_extension(Path::new(rel)) {
        return response_value(None, Some(resolved_path));
    }
    match reef_sqlite_preview::probe_magic(&abs) {
        Ok(false) => return response_value(None, Some(resolved_path)),
        Err(e) => return Err((ErrorCode::Io, e.to_string())),
        Ok(true) => {}
    }
    match reef_sqlite_preview::read_initial_v2(&abs, page_size) {
        Ok(info) => response_value(Some(database_info_v2_to_dto(info)), Some(resolved_path)),
        Err(e) => Err((ErrorCode::Other, format!("sqlite: {e}"))),
    }
}

fn load_db_page_v2_handler(
    workdir: &Path,
    rel: &str,
    schema: &str,
    kind: reef_proto::DbObjectKindDto,
    name: &str,
    offset: u64,
    limit: u32,
) -> Result<serde_json::Value, (ErrorCode, String)> {
    use reef_io::BackendError;
    use reef_io::local::canonical_child_within;
    let abs = match canonical_child_within(workdir, Path::new(rel)) {
        Ok(p) => p,
        Err(BackendError::NotFound) => return Err((ErrorCode::NotFound, "file not found".into())),
        Err(e) => return Err(backend_err(e)),
    };
    if !abs.is_file() {
        return Err((ErrorCode::NotFound, "not a regular file".into()));
    }
    match reef_sqlite_preview::load_page_qualified(
        &abs,
        schema,
        db_object_kind_from_dto(kind),
        name,
        offset,
        limit,
    ) {
        Ok(page) => serde_json::to_value(db_page_to_dto(page))
            .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),
        Err(e) => Err((ErrorCode::Other, format!("sqlite: {e}"))),
    }
}

fn dispatch_db_cell(
    workdir: &Path,
    task: DbCellTask,
    stdout: Arc<Mutex<BufWriter<Stdout>>>,
) -> Option<Response> {
    use reef_io::BackendError;
    use reef_io::local::canonical_child_within;
    let id = task.id;
    let abs = match canonical_child_within(workdir, Path::new(&task.rel_path)) {
        Ok(path) => path,
        Err(BackendError::NotFound) => {
            return Some(Response::Err {
                id,
                code: ErrorCode::NotFound,
                message: "file not found".into(),
            });
        }
        Err(error) => {
            let (code, message) = backend_err(error);
            return Some(Response::Err { id, code, message });
        }
    };
    if !abs.is_file() {
        return Some(Response::Err {
            id,
            code: ErrorCode::NotFound,
            message: "not a regular file".into(),
        });
    }
    let value = match reef_sqlite_preview::load_cell_qualified(
        &abs,
        &task.key.schema,
        task.key.kind,
        &task.key.name,
        &task.locator,
        task.column,
        task.cancellation.shared_flag(),
    ) {
        Ok(value) => value,
        Err(reef_sqlite_preview::PreviewError::Cancelled) => {
            return Some(Response::Err {
                id,
                code: ErrorCode::Cancelled,
                message: "cancelled".into(),
            });
        }
        Err(error) => {
            return Some(Response::Err {
                id,
                code: ErrorCode::Other,
                message: format!("sqlite: {error}"),
            });
        }
    };
    let completed = match value {
        reef_sqlite_preview::SqliteValue::Text { value, .. } => {
            let bytes = value.into_bytes();
            for chunk in bytes.chunks(reef_sqlite_preview::DB_CELL_CHUNK_BYTES) {
                if task.cancellation.is_cancelled() {
                    return Some(Response::Err {
                        id,
                        code: ErrorCode::Cancelled,
                        message: "cancelled".into(),
                    });
                }
                let frame = Frame::Notification(Notification::DbCellChunk {
                    request_id: id,
                    bytes: chunk.to_vec(),
                });
                let mut guard = stdout.lock().ok()?;
                encode_frame(&mut *guard, &frame).ok()?;
                guard.flush().ok()?;
            }
            let revision = reef_sqlite_preview::db_cell_revision(&bytes);
            reef_proto::DbCellCompletedDto::Text {
                byte_len: revision.byte_len,
                content_hash: revision.content_hash,
            }
        }
        value => reef_proto::DbCellCompletedDto::Complete {
            value: sqlite_value_to_dto(value),
        },
    };
    match serde_json::to_value(completed) {
        Ok(result) => Some(Response::Ok { id, result }),
        Err(error) => Some(Response::Err {
            id,
            code: ErrorCode::Protocol,
            message: format!("encode: {error}"),
        }),
    }
}

fn load_db_object_detail_handler(
    workdir: &Path,
    rel: &str,
    schema: &str,
    kind: reef_proto::DbObjectKindDto,
    name: &str,
) -> Result<serde_json::Value, (ErrorCode, String)> {
    use reef_io::BackendError;
    use reef_io::local::canonical_child_within;
    let abs = match canonical_child_within(workdir, Path::new(rel)) {
        Ok(p) => p,
        Err(BackendError::NotFound) => return Err((ErrorCode::NotFound, "file not found".into())),
        Err(e) => return Err(backend_err(e)),
    };
    if !abs.is_file() {
        return Err((ErrorCode::NotFound, "not a regular file".into()));
    }
    match reef_sqlite_preview::read_object_detail_at(
        &abs,
        schema,
        db_object_kind_from_dto(kind),
        name,
    ) {
        Ok(detail) => serde_json::to_value(db_object_detail_to_dto(detail))
            .map_err(|e| (ErrorCode::Protocol, format!("encode: {e}"))),
        Err(e) => Err((ErrorCode::Other, format!("sqlite: {e}"))),
    }
}

fn database_info_v2_to_dto(
    d: reef_sqlite_preview::DatabaseInfoV2,
) -> reef_proto::DatabaseInfoV2Dto {
    reef_proto::DatabaseInfoV2Dto {
        schemas: d.schemas.into_iter().map(schema_summary_to_dto).collect(),
        default_schema: d.default_schema,
        default_object: d.default_object.map(db_object_key_to_dto),
        initial_page: db_page_to_dto(d.initial_page),
        bytes_on_disk: d.bytes_on_disk,
    }
}

fn schema_summary_to_dto(s: reef_sqlite_preview::SchemaSummary) -> reef_proto::SchemaSummaryDto {
    reef_proto::SchemaSummaryDto {
        name: s.name,
        kind: schema_kind_to_dto(s.kind),
        file: s.file,
        objects: s.objects.into_iter().map(db_object_to_dto).collect(),
        truncated: s.truncated,
    }
}

fn db_object_to_dto(o: reef_sqlite_preview::DbObject) -> reef_proto::DbObjectDto {
    reef_proto::DbObjectDto {
        schema: o.schema,
        name: o.name,
        kind: db_object_kind_to_dto(o.kind),
        tbl_name: o.tbl_name,
        row_count: o.row_count,
        columns: o.columns.into_iter().map(column_info_to_dto).collect(),
        is_virtual: o.is_virtual,
        is_without_rowid: o.is_without_rowid,
        is_strict: o.is_strict,
    }
}

fn db_object_key_to_dto(k: reef_sqlite_preview::DbObjectKey) -> reef_proto::DbObjectKeyDto {
    reef_proto::DbObjectKeyDto {
        schema: k.schema,
        name: k.name,
        kind: db_object_kind_to_dto(k.kind),
    }
}

fn db_object_kind_to_dto(k: reef_sqlite_preview::DbObjectKind) -> reef_proto::DbObjectKindDto {
    match k {
        reef_sqlite_preview::DbObjectKind::Table => reef_proto::DbObjectKindDto::Table,
        reef_sqlite_preview::DbObjectKind::View => reef_proto::DbObjectKindDto::View,
        reef_sqlite_preview::DbObjectKind::Index => reef_proto::DbObjectKindDto::Index,
        reef_sqlite_preview::DbObjectKind::Trigger => reef_proto::DbObjectKindDto::Trigger,
    }
}

fn db_object_kind_from_dto(k: reef_proto::DbObjectKindDto) -> reef_sqlite_preview::DbObjectKind {
    match k {
        reef_proto::DbObjectKindDto::Table => reef_sqlite_preview::DbObjectKind::Table,
        reef_proto::DbObjectKindDto::View => reef_sqlite_preview::DbObjectKind::View,
        reef_proto::DbObjectKindDto::Index => reef_sqlite_preview::DbObjectKind::Index,
        reef_proto::DbObjectKindDto::Trigger => reef_sqlite_preview::DbObjectKind::Trigger,
    }
}

fn schema_kind_to_dto(k: reef_sqlite_preview::SchemaKind) -> reef_proto::SchemaKindDto {
    match k {
        reef_sqlite_preview::SchemaKind::Main => reef_proto::SchemaKindDto::Main,
        reef_sqlite_preview::SchemaKind::Temp => reef_proto::SchemaKindDto::Temp,
        reef_sqlite_preview::SchemaKind::Attached => reef_proto::SchemaKindDto::Attached,
    }
}

fn db_object_detail_to_dto(
    d: reef_sqlite_preview::DbObjectDetail,
) -> reef_proto::DbObjectDetailDto {
    use reef_sqlite_preview::DbObjectDetail as D;
    match d {
        D::Table { create_sql } => reef_proto::DbObjectDetailDto::Table { create_sql },
        D::View { create_sql } => reef_proto::DbObjectDetailDto::View { create_sql },
        D::Index {
            unique,
            columns,
            partial_where,
            tbl_name,
            create_sql,
        } => reef_proto::DbObjectDetailDto::Index {
            unique,
            columns,
            partial_where,
            tbl_name,
            create_sql,
        },
        D::Trigger {
            timing,
            event,
            tbl_name,
            sql,
        } => reef_proto::DbObjectDetailDto::Trigger {
            timing: trigger_timing_to_dto(timing),
            event: trigger_event_to_dto(event),
            tbl_name,
            sql,
        },
    }
}

fn trigger_timing_to_dto(t: reef_sqlite_preview::TriggerTiming) -> reef_proto::TriggerTimingDto {
    match t {
        reef_sqlite_preview::TriggerTiming::Before => reef_proto::TriggerTimingDto::Before,
        reef_sqlite_preview::TriggerTiming::After => reef_proto::TriggerTimingDto::After,
        reef_sqlite_preview::TriggerTiming::InsteadOf => reef_proto::TriggerTimingDto::InsteadOf,
        reef_sqlite_preview::TriggerTiming::Unknown => reef_proto::TriggerTimingDto::Unknown,
    }
}

fn trigger_event_to_dto(e: reef_sqlite_preview::TriggerEvent) -> reef_proto::TriggerEventDto {
    match e {
        reef_sqlite_preview::TriggerEvent::Insert => reef_proto::TriggerEventDto::Insert,
        reef_sqlite_preview::TriggerEvent::Update => reef_proto::TriggerEventDto::Update,
        reef_sqlite_preview::TriggerEvent::Delete => reef_proto::TriggerEventDto::Delete,
        reef_sqlite_preview::TriggerEvent::Unknown => reef_proto::TriggerEventDto::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use reef_proto::Notification;

    use super::fs_change_notification;

    #[test]
    fn filesystem_change_notification_preserves_kind_and_paths() {
        let notification = fs_change_notification(
            reef_io::FsChange {
                workspace_changed: true,
                workspace_paths: vec![PathBuf::from("src/main.rs")],
                git_metadata_changed: false,
                repo_presence_changed: false,
            },
            true,
        );

        assert!(matches!(
            notification,
            Notification::FsChanged {
                has_repo: true,
                workspace_changed: true,
                workspace_paths,
                git_metadata_changed: false,
            } if workspace_paths == vec!["src/main.rs"]
        ));
    }
}
