//! Remote backend — speaks length-prefixed JSON-RPC (see `reef-proto`) to a
//! `reef-agent` subprocess (typically `ssh host reef-agent --stdio …`).
//!
//! Phase 3 of the SSH feature: this file ships the M1 "loopback" story —
//! enough to spawn the agent locally, issue synchronous RPC calls from the
//! main thread and receive debounced fs-change notifications. `launch_editor`
//! is explicitly left `Unimplemented` for M1 (Phase 6 adds SSH -t transparent
//! forwarding). Everything else matches the `Backend` contract byte-for-byte
//! with `LocalBackend`.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use reef_proto::{
    ContentSearchCompletedDto, ContentSearchRequestDto, DatabaseInfoDto, DirEntryDto, Envelope,
    MatchHitDto, Notification, ReadFileResponse, RepoDiscoverOptsDto, RepoDiscoverResponseDto,
    Request, Response, TrashResponseDto, WalkOptsDto, WalkResponseDto, decode_frame, encode_frame,
};

use super::{
    Backend, BackendError, ContentMatchHit, ContentSearchCompleted, ContentSearchRequest,
    EditorLaunchSpec, RepoDiscoverOpts, RepoDiscoverResponse, SearchChunkSink, StatusSnapshot,
    TrashOutcome, WalkOpts, WalkResponse, WorkspaceRepoMeta, normalize_repo_root_rel, repo_key,
};
use crate::file_tree::{PreviewContent, TreeEntry};
use crate::git::{CommitDetail, CommitInfo, DiffContent, FileEntry, RefLabel};
use std::ops::ControlFlow;

/// Default timeout for a single RPC round-trip. Applied to every `request`
/// call so a hung agent can't stall the UI indefinitely.
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Max bytes we ever ask the agent to return for `ReadFile`. Matches the
/// limits applied in `file_tree::load_preview` (512 KB highlight cap + some
/// headroom for un-highlighted previews).
const READ_FILE_MAX_BYTES: u64 = 2 * 1024 * 1024;

type PendingMap = HashMap<u64, mpsc::Sender<Response>>;
/// `request_id` → sender for streaming `SearchChunk` notifications. The
/// read thread consults this map on every `Notification::SearchChunk`
/// frame so hits land at the right in-flight search worker. The map is
/// mutated by `search_content` around the RPC (register before send,
/// drop after the final response is received or on error).
type ChunkSinkMap = HashMap<u64, mpsc::Sender<Vec<MatchHitDto>>>;

/// RAII registration in a `request_id → sender` map. Drop removes the
/// id; the read loop also removes on receipt, so Drop becomes a no-op
/// on the success path. Used for both `pending` and `search_chunks` so
/// the two cleanup paths can't drift.
struct MapGuard<'a, V: Send> {
    map: &'a Arc<Mutex<HashMap<u64, V>>>,
    id: u64,
}

impl<'a, V: Send> MapGuard<'a, V> {
    fn register(
        map: &'a Arc<Mutex<HashMap<u64, V>>>,
        id: u64,
        value: V,
        lock_label: &'static str,
    ) -> Result<Self, BackendError> {
        let mut m = map
            .lock()
            .map_err(|e| BackendError::Rpc(format!("{lock_label} lock poisoned: {e}")))?;
        m.insert(id, value);
        Ok(Self { map, id })
    }
}

impl<'a, V: Send> Drop for MapGuard<'a, V> {
    fn drop(&mut self) {
        if let Ok(mut m) = self.map.lock() {
            m.remove(&self.id);
        }
    }
}

pub struct RemoteBackend {
    // Set once during handshake via interior mutability so the post-spawn
    // mutation doesn't have to move the whole struct out of a Drop type.
    workdir: Mutex<PathBuf>,
    workdir_name: Mutex<String>,
    branch_name_cache: Mutex<String>,
    tx: Mutex<BufWriter<ChildStdin>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<PendingMap>>,
    /// See `ChunkSinkMap`. Shared with the read thread.
    search_chunks: Arc<Mutex<ChunkSinkMap>>,
    fs_rx: Mutex<Option<mpsc::Receiver<()>>>,
    _fs_tx: mpsc::Sender<()>,
    _reader: thread::JoinHandle<()>,
    _stderr_reader: thread::JoinHandle<()>,
    child: Mutex<Child>,
    /// Populated by `connect_ssh` so `editor_launch_spec` can assemble an
    /// `ssh -t` command reusing the ControlMaster socket. `spawn()` (used
    /// by the `--agent-exec` route) leaves this `None` and
    /// `editor_launch_spec` returns `Unimplemented` for that path.
    ssh_launch: Option<SshLaunchInfo>,
}

#[derive(Debug, Clone)]
struct SshLaunchInfo {
    ssh_args: Vec<String>,
    host: String,
    remote_workdir: String,
    /// `RemoteOs` of the remote host, used by `editor_launch_spec` and
    /// `upload_from_local` to pick between POSIX and Windows shell
    /// command layouts. Defaults to `Posix` for the legacy
    /// `RemoteBackend::spawn` code path (no probe available).
    remote_os: crate::agent_deploy::RemoteOs,
}

impl RemoteBackend {
    /// Convenience: spawn `ssh <host> <agent_path> --stdio --workdir <path>`
    /// using an established `SshSession` (so ControlMaster from
    /// `agent_deploy::ensure_agent` is reused — no second auth prompt).
    ///
    /// Both `remote_workdir` and `agent_path` are run through
    /// `shell_escape`, so callers may pass raw paths with spaces.
    /// `remote_os` comes from `ensure_agent_with_session`'s probe and
    /// is threaded through so later `editor_launch_spec` / upload calls
    /// can pick the right shell flavour.
    pub fn connect_ssh(
        session: &crate::agent_deploy::SshSession,
        remote_workdir: &str,
        agent_path: &str,
        remote_os: crate::agent_deploy::RemoteOs,
    ) -> io::Result<Self> {
        use crate::agent_deploy::ssh::shell_escape;

        // Assemble the remote command as a single string so the remote
        // shell splits it; otherwise `ssh host /path/to/bin --stdio --workdir /foo`
        // works but ssh's own word-splitting trips on unescaped spaces
        // in $path.
        let remote_cmd = format!(
            "{} --stdio --workdir {}",
            shell_escape(agent_path),
            shell_escape(remote_workdir),
        );

        let mut argv: Vec<String> = Vec::with_capacity(session.ssh_args().len() + 3);
        argv.push("ssh".to_string());
        argv.extend(session.ssh_args().iter().cloned());
        argv.push(session.host().to_string());
        argv.push(remote_cmd);
        let launch = SshLaunchInfo {
            ssh_args: session.ssh_args().to_vec(),
            host: session.host().to_string(),
            remote_workdir: remote_workdir.to_string(),
            remote_os,
        };
        let mut backend = Self::spawn(&argv)?;
        backend.ssh_launch = Some(launch);
        Ok(backend)
    }

    /// Spawn the given argv as a subprocess (typically `ssh host reef-agent
    /// --stdio …`) and wire up the read/write threads.
    pub fn spawn(argv: &[String]) -> io::Result<Self> {
        if argv.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote backend: empty argv",
            ));
        }
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("child stdin missing"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("child stdout missing"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("child stderr missing"))?;

        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));
        let search_chunks: Arc<Mutex<ChunkSinkMap>> = Arc::new(Mutex::new(HashMap::new()));
        let (fs_tx, fs_rx) = mpsc::channel::<()>();

        let reader_pending = Arc::clone(&pending);
        let reader_chunks = Arc::clone(&search_chunks);
        let reader_fs_tx = fs_tx.clone();
        let reader = thread::Builder::new()
            .name("reef-remote-reader".into())
            .spawn(move || read_loop(stdout, reader_pending, reader_chunks, reader_fs_tx))
            .map_err(io::Error::other)?;

        let stderr_reader = thread::Builder::new()
            .name("reef-remote-stderr".into())
            .spawn(move || drain_stderr(stderr))
            .map_err(io::Error::other)?;

        let backend = Self {
            workdir: Mutex::new(PathBuf::new()),
            workdir_name: Mutex::new(String::new()),
            branch_name_cache: Mutex::new(String::new()),
            tx: Mutex::new(BufWriter::new(stdin)),
            next_id: AtomicU64::new(1),
            pending,
            search_chunks,
            fs_rx: Mutex::new(Some(fs_rx)),
            _fs_tx: fs_tx,
            _reader: reader,
            _stderr_reader: stderr_reader,
            child: Mutex::new(child),
            ssh_launch: None,
        };

        // Handshake: ask the agent for its workdir and name once so the UI
        // can render `workdir_name` / `branch_name` synchronously without
        // round-tripping on every call.
        let info = backend
            .handshake()
            .map_err(|e| io::Error::other(format!("remote backend handshake failed: {e}")))?;
        if let Ok(mut w) = backend.workdir.lock() {
            *w = PathBuf::from(&info.workdir);
        }
        if let Ok(mut n) = backend.workdir_name.lock() {
            *n = info.workdir_name;
        }
        if let Ok(mut b) = backend.branch_name_cache.lock() {
            *b = info.branch_name;
        }

        // Ask the agent to start streaming fs events. If the call fails we
        // still return the backend — fs-change polling simply won't fire.
        let _ = backend.request::<serde_json::Value>(Request::Subscribe);

        Ok(backend)
    }

    fn handshake(&self) -> Result<reef_proto::HandshakeResponse, BackendError> {
        let resp: reef_proto::HandshakeResponse = self.request(Request::Handshake)?;
        if resp.protocol_version != reef_proto::PROTOCOL_VERSION {
            return Err(BackendError::Protocol(format!(
                "agent speaks protocol v{}, client speaks v{}; \
                 re-run `reef --ssh` to trigger auto-update",
                resp.protocol_version,
                reef_proto::PROTOCOL_VERSION,
            )));
        }
        Ok(resp)
    }

    fn send_envelope(&self, envelope: Envelope) -> Result<(), BackendError> {
        let mut guard = self
            .tx
            .lock()
            .map_err(|e| BackendError::Rpc(format!("tx lock poisoned: {e}")))?;
        encode_frame(&mut *guard, &envelope)
            .map_err(|e| BackendError::Rpc(format!("write frame: {e}")))?;
        guard
            .flush()
            .map_err(|e| BackendError::Rpc(format!("flush: {e}")))?;
        Ok(())
    }

    fn request<T: serde::de::DeserializeOwned>(&self, req: Request) -> Result<T, BackendError> {
        self.request_with_timeout(req, DEFAULT_RPC_TIMEOUT)
    }

    fn request_with_timeout<T: serde::de::DeserializeOwned>(
        &self,
        req: Request,
        timeout: Duration,
    ) -> Result<T, BackendError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel::<Response>();
        // Without this guard, RPC timeouts / send errors leak `pending`
        // slots — the read loop only removes on Response receipt.
        let _pending = MapGuard::register(&self.pending, id, tx, "pending")?;
        self.send_envelope(Envelope { id, body: req })?;
        let response = rx
            .recv_timeout(timeout)
            .map_err(|e| BackendError::Rpc(format!("recv: {e}")))?;
        match response {
            Response::Ok { result, .. } => serde_json::from_value::<T>(result)
                .map_err(|e| BackendError::Protocol(format!("response decode: {e}"))),
            Response::Err { code, message, .. } => Err(BackendError::from_wire(code, message)),
        }
    }

    /// Test-only accessor for the in-flight RPC map size. Used by the
    /// leak regression test in `tests/backend_writes_loopback.rs` —
    /// verifies that timed-out / errored requests release their slot.
    #[doc(hidden)]
    pub fn __pending_len_for_tests(&self) -> usize {
        self.pending.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Test-only entry to `request_with_timeout`, so the leak regression
    /// test can drive the failure path (100ms timeout against a killed
    /// agent) without having to wait the full `DEFAULT_RPC_TIMEOUT`.
    #[doc(hidden)]
    pub fn __request_with_timeout_for_tests<T: serde::de::DeserializeOwned>(
        &self,
        req: Request,
        timeout: Duration,
    ) -> Result<T, BackendError> {
        self.request_with_timeout(req, timeout)
    }

    /// Test-only helper that kills the agent subprocess so subsequent
    /// RPCs are guaranteed to fail (send hits BrokenPipe, or
    /// recv_timeout never gets a response). Used by the PendingGuard
    /// failure-path regression test; no production path should call
    /// this.
    #[doc(hidden)]
    pub fn __kill_agent_for_tests(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Shut the agent down cleanly. Called by Drop; exposed for tests that
    /// want an explicit, errorable teardown.
    pub fn shutdown(&self) {
        // Best-effort: tell the agent to exit. Ignore errors; Drop will
        // fall back to killing the subprocess.
        let _ = self.send_envelope(Envelope {
            id: self.next_id.fetch_add(1, Ordering::SeqCst),
            body: Request::Shutdown,
        });
    }
}

impl Drop for RemoteBackend {
    fn drop(&mut self) {
        self.shutdown();
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            // Poll for exit with a hard cap so Drop doesn't block if the
            // agent is unresponsive. A zombie is harmless until reef exits.
            let deadline = std::time::Instant::now() + Duration::from_millis(200);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    _ => break,
                }
            }
        }
    }
}

fn read_loop(
    stdout: ChildStdout,
    pending: Arc<Mutex<PendingMap>>,
    search_chunks: Arc<Mutex<ChunkSinkMap>>,
    fs_tx: mpsc::Sender<()>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let frame = match decode_frame(&mut reader) {
            Ok(env) => env,
            Err(e) => {
                // EOF or truncated — exit cleanly. Callers observe the
                // disconnect via pending `recv_timeout` firing.
                if e.kind() != io::ErrorKind::UnexpectedEof {
                    eprintln!("[reef] remote reader: {e}");
                }
                return;
            }
        };
        match frame {
            reef_proto::Frame::Response(resp) => {
                let id = resp.id();
                if let Ok(mut map) = pending.lock() {
                    if let Some(tx) = map.remove(&id) {
                        let _ = tx.send(resp);
                    }
                }
            }
            reef_proto::Frame::Notification(note) => match note {
                Notification::FsChanged => {
                    let _ = fs_tx.send(());
                }
                Notification::AgentLog { level, message } => {
                    eprintln!("[reef-agent:{level}] {message}");
                }
                Notification::SearchChunk { request_id, hits } => {
                    // Forward to the per-request sink if one is
                    // registered. A missing entry means the client
                    // already tore down the search (timeout, backend
                    // error) — silently drop in that case; the agent
                    // will still eventually send a terminal response
                    // which the pending map will deal with.
                    let sender = match search_chunks.lock() {
                        Ok(map) => map.get(&request_id).cloned(),
                        Err(_) => None,
                    };
                    if let Some(tx) = sender {
                        let _ = tx.send(hits);
                    }
                }
            },
        }
    }
}

fn drain_stderr(stderr: std::process::ChildStderr) {
    use std::io::BufRead;
    let reader = BufReader::new(stderr);
    for line in reader.lines().map_while(Result::ok) {
        eprintln!("[reef-agent] {line}");
    }
}

impl Backend for RemoteBackend {
    fn workdir_path(&self) -> PathBuf {
        match self.workdir.lock() {
            Ok(w) => w.clone(),
            Err(e) => {
                eprintln!("[reef] workdir lock poisoned: {e}");
                PathBuf::new()
            }
        }
    }

    fn workdir_name(&self) -> String {
        match self.workdir_name.lock() {
            Ok(n) => n.clone(),
            Err(e) => {
                eprintln!("[reef] workdir_name lock poisoned: {e}");
                String::new()
            }
        }
    }

    fn branch_name(&self) -> String {
        // Cached value from handshake. Refreshed opportunistically by
        // callers that fetch a full StatusSnapshot.
        match self.branch_name_cache.lock() {
            Ok(b) => b.clone(),
            Err(e) => {
                eprintln!("[reef] branch_name lock poisoned: {e}");
                String::new()
            }
        }
    }

    fn has_repo(&self) -> bool {
        !self.branch_name().is_empty()
    }

    fn discover_repos(
        &self,
        opts: &RepoDiscoverOpts,
    ) -> Result<RepoDiscoverResponse, BackendError> {
        let dto = RepoDiscoverOptsDto {
            max_depth: opts.max_depth as u64,
            include_nested: opts.include_nested,
            max_repos: opts.max_repos.map(|n| n as u64),
        };
        let resp: RepoDiscoverResponseDto = self.request(Request::DiscoverRepos { opts: dto })?;
        Ok(RepoDiscoverResponse {
            repos: resp
                .repos
                .into_iter()
                .map(|r| WorkspaceRepoMeta {
                    repo_root_rel: PathBuf::from(r.repo_root_rel),
                    display_name: r.display_name,
                })
                .collect(),
            truncated: resp.truncated,
        })
    }

    fn build_file_tree(
        &self,
        expanded: &HashSet<PathBuf>,
        git_statuses: &HashMap<String, char>,
    ) -> Result<Vec<TreeEntry>, String> {
        let mut out = Vec::new();
        walk_remote(self, Path::new(""), expanded, git_statuses, &mut out, 0)?;
        Ok(out)
    }

    fn load_preview(
        &self,
        rel_path: &Path,
        dark: bool,
        _wants_decoded_image: bool,
    ) -> Option<PreviewContent> {
        // Fetch bytes over RPC and rebuild a `PreviewContent`. `PreviewBody`'s
        // `Image` variant carries a decoded `DynamicImage` that isn't serde-
        // shippable, so for now we surface every binary (image or otherwise)
        // as the generic Binary metadata card regardless of the decode
        // hint. Image rendering over SSH would need raw bytes +
        // client-side decode; tracked in issue #31.
        use crate::file_tree::{BinaryInfo, BinaryReason, PreviewBody};
        let rel_str = rel_path.to_string_lossy().to_string();

        // SQLite branch — client-side extension check, agent does the
        // magic-bytes probe and the actual reading. Critical: a `.db`
        // file can be hundreds of MB, and slurping it through ReadFile
        // would blow `MAX_FRAME_SIZE` and stall the SSH pipe. The
        // agent-side path opens the DB read-only and returns just the
        // schema + first page of rows. On RPC failure or agent's
        // "not actually sqlite" reply, fall through to the standard
        // ReadFile path so the file still gets a binary card.
        if reef_sqlite_preview::has_sqlite_extension(rel_path) {
            if let Ok(Some(dto)) = self.request::<Option<DatabaseInfoDto>>(Request::LoadDbInitial {
                rel_path: rel_str.clone(),
                page_size: crate::file_tree::INITIAL_DB_PAGE_ROWS,
            }) {
                return Some(PreviewContent {
                    file_path: rel_str,
                    body: PreviewBody::Database(database_info_from_dto(dto)),
                });
            }
        }
        let resp: ReadFileResponse = self
            .request(Request::ReadFile {
                path: rel_str.clone(),
                max_bytes: READ_FILE_MAX_BYTES,
            })
            .ok()?;
        if !resp.is_file {
            return None;
        }
        let raw = resp.bytes;
        let bytes_on_disk = resp.size;

        if raw.is_empty() {
            return Some(PreviewContent {
                file_path: rel_str,
                body: PreviewBody::Binary(BinaryInfo::new(0, None, BinaryReason::Empty)),
            });
        }

        let check_len = raw.len().min(8192);
        if raw[..check_len].contains(&0) {
            return Some(PreviewContent {
                file_path: rel_str,
                body: PreviewBody::Binary(BinaryInfo::new(
                    bytes_on_disk,
                    None,
                    BinaryReason::NullBytes,
                )),
            });
        }

        let content = String::from_utf8_lossy(&raw);
        let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
        let lines = if lines.len() > 10_000 {
            lines[..10_000].to_vec()
        } else {
            lines
        };

        let highlighted = if raw.len() <= 512 * 1024 && lines.len() <= 5_000 {
            crate::ui::highlight::highlight_file(&rel_str, &lines, dark)
        } else {
            None
        };

        Some(PreviewContent {
            file_path: rel_str,
            body: PreviewBody::Text { lines, highlighted },
        })
    }

    fn read_file(&self, rel_path: &Path, max_bytes: u64) -> Result<Vec<u8>, BackendError> {
        let rel_str = rel_path.to_string_lossy().to_string();
        let resp: ReadFileResponse = self.request(Request::ReadFile {
            path: rel_str,
            max_bytes,
        })?;
        if !resp.is_file {
            return Err(BackendError::NotFound);
        }
        Ok(resp.bytes)
    }

    fn db_load_page(
        &self,
        rel_path: &Path,
        table: &str,
        offset: u64,
        limit: u32,
    ) -> Result<reef_sqlite_preview::DbPage, BackendError> {
        let rel_str = rel_path.to_string_lossy().to_string();
        let dto: reef_proto::DbPageDto = self.request(Request::LoadDbPage {
            rel_path: rel_str,
            table: table.to_string(),
            offset,
            limit,
        })?;
        Ok(db_page_from_dto(dto))
    }

    fn git_status(&self) -> Result<StatusSnapshot, BackendError> {
        let snap: reef_proto::StatusSnapshotDto = self.request(Request::GitStatus)?;
        if let Ok(mut cache) = self.branch_name_cache.lock() {
            *cache = snap.branch_name.clone();
        }
        Ok(StatusSnapshot {
            staged: snap.staged.into_iter().map(Into::into).collect(),
            unstaged: snap.unstaged.into_iter().map(Into::into).collect(),
            branch_name: snap.branch_name,
            ahead_behind: snap.ahead_behind,
        })
    }

    fn git_status_for(&self, repo_root_rel: &Path) -> Result<StatusSnapshot, BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.git_status();
        }
        let snap: reef_proto::StatusSnapshotDto = self.request(Request::GitStatusFor {
            repo_root_rel: repo_key(&repo_root_rel),
        })?;
        Ok(StatusSnapshot {
            staged: snap.staged.into_iter().map(Into::into).collect(),
            unstaged: snap.unstaged.into_iter().map(Into::into).collect(),
            branch_name: snap.branch_name,
            ahead_behind: snap.ahead_behind,
        })
    }

    fn staged_diff(
        &self,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError> {
        let resp: Option<reef_proto::DiffContentDto> = self.request(Request::StagedDiff {
            path: path.to_string(),
            context_lines,
        })?;
        Ok(resp.map(Into::into))
    }

    fn staged_diff_for(
        &self,
        repo_root_rel: &Path,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.staged_diff(path, context_lines);
        }
        let resp: Option<reef_proto::DiffContentDto> = self.request(Request::StagedDiffFor {
            repo_root_rel: repo_key(&repo_root_rel),
            path: path.to_string(),
            context_lines,
        })?;
        Ok(resp.map(Into::into))
    }

    fn unstaged_diff(
        &self,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError> {
        let resp: Option<reef_proto::DiffContentDto> = self.request(Request::UnstagedDiff {
            path: path.to_string(),
            context_lines,
        })?;
        Ok(resp.map(Into::into))
    }

    fn unstaged_diff_for(
        &self,
        repo_root_rel: &Path,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.unstaged_diff(path, context_lines);
        }
        let resp: Option<reef_proto::DiffContentDto> = self.request(Request::UnstagedDiffFor {
            repo_root_rel: repo_key(&repo_root_rel),
            path: path.to_string(),
            context_lines,
        })?;
        Ok(resp.map(Into::into))
    }

    fn untracked_diff(&self, path: &str) -> Result<Option<DiffContent>, BackendError> {
        let resp: Option<reef_proto::DiffContentDto> = self.request(Request::UntrackedDiff {
            path: path.to_string(),
        })?;
        Ok(resp.map(Into::into))
    }

    fn untracked_diff_for(
        &self,
        repo_root_rel: &Path,
        path: &str,
    ) -> Result<Option<DiffContent>, BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.untracked_diff(path);
        }
        let resp: Option<reef_proto::DiffContentDto> = self.request(Request::UntrackedDiffFor {
            repo_root_rel: repo_key(&repo_root_rel),
            path: path.to_string(),
        })?;
        Ok(resp.map(Into::into))
    }

    fn stage(&self, path: &str) -> Result<(), BackendError> {
        let _: serde_json::Value = self.request(Request::Stage {
            path: path.to_string(),
        })?;
        Ok(())
    }

    fn stage_for(&self, repo_root_rel: &Path, path: &str) -> Result<(), BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.stage(path);
        }
        let _: serde_json::Value = self.request(Request::StageFor {
            repo_root_rel: repo_key(&repo_root_rel),
            path: path.to_string(),
        })?;
        Ok(())
    }

    fn unstage(&self, path: &str) -> Result<(), BackendError> {
        let _: serde_json::Value = self.request(Request::Unstage {
            path: path.to_string(),
        })?;
        Ok(())
    }

    fn unstage_for(&self, repo_root_rel: &Path, path: &str) -> Result<(), BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.unstage(path);
        }
        let _: serde_json::Value = self.request(Request::UnstageFor {
            repo_root_rel: repo_key(&repo_root_rel),
            path: path.to_string(),
        })?;
        Ok(())
    }

    fn restore(&self, path: &str) -> Result<(), BackendError> {
        let _: serde_json::Value = self.request(Request::Restore {
            path: path.to_string(),
        })?;
        Ok(())
    }

    fn revert_path(&self, path: &str, is_staged: bool) -> Result<(), BackendError> {
        let _: serde_json::Value = self.request(Request::RevertPath {
            path: path.to_string(),
            is_staged,
        })?;
        Ok(())
    }

    fn revert_path_for(
        &self,
        repo_root_rel: &Path,
        path: &str,
        is_staged: bool,
    ) -> Result<(), BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.revert_path(path, is_staged);
        }
        let _: serde_json::Value = self.request(Request::RevertPathFor {
            repo_root_rel: repo_key(&repo_root_rel),
            path: path.to_string(),
            is_staged,
        })?;
        Ok(())
    }

    fn push(&self, force: bool) -> Result<(), BackendError> {
        let _: serde_json::Value = self.request(Request::Push { force })?;
        Ok(())
    }

    fn push_for(&self, repo_root_rel: &Path, force: bool) -> Result<(), BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.push(force);
        }
        let _: serde_json::Value = self.request(Request::PushFor {
            repo_root_rel: repo_key(&repo_root_rel),
            force,
        })?;
        Ok(())
    }

    fn commit(&self, message: &str) -> Result<(), BackendError> {
        let _: serde_json::Value = self.request(Request::Commit {
            message: message.to_string(),
        })?;
        Ok(())
    }

    fn commit_for(&self, repo_root_rel: &Path, message: &str) -> Result<(), BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.commit(message);
        }
        let _: serde_json::Value = self.request(Request::CommitFor {
            repo_root_rel: repo_key(&repo_root_rel),
            message: message.to_string(),
        })?;
        Ok(())
    }

    fn list_commits(&self, limit: usize) -> Result<Vec<CommitInfo>, BackendError> {
        let list: Vec<reef_proto::CommitInfoDto> = self.request(Request::ListCommits {
            limit: limit as u64,
        })?;
        Ok(list.into_iter().map(Into::into).collect())
    }

    fn list_commits_for(
        &self,
        repo_root_rel: &Path,
        limit: usize,
    ) -> Result<Vec<CommitInfo>, BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.list_commits(limit);
        }
        let list: Vec<reef_proto::CommitInfoDto> = self.request(Request::ListCommitsFor {
            repo_root_rel: repo_key(&repo_root_rel),
            limit: limit as u64,
        })?;
        Ok(list.into_iter().map(Into::into).collect())
    }

    fn list_refs(&self) -> Result<HashMap<String, Vec<RefLabel>>, BackendError> {
        let map: HashMap<String, Vec<reef_proto::RefLabelDto>> = self.request(Request::ListRefs)?;
        Ok(map
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().map(Into::into).collect()))
            .collect())
    }

    fn list_refs_for(
        &self,
        repo_root_rel: &Path,
    ) -> Result<HashMap<String, Vec<RefLabel>>, BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.list_refs();
        }
        let map: HashMap<String, Vec<reef_proto::RefLabelDto>> =
            self.request(Request::ListRefsFor {
                repo_root_rel: repo_key(&repo_root_rel),
            })?;
        Ok(map
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().map(Into::into).collect()))
            .collect())
    }

    fn head_oid(&self) -> Result<Option<String>, BackendError> {
        self.request(Request::HeadOid)
    }

    fn head_oid_for(&self, repo_root_rel: &Path) -> Result<Option<String>, BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.head_oid();
        }
        self.request(Request::HeadOidFor {
            repo_root_rel: repo_key(&repo_root_rel),
        })
    }

    fn commit_detail(&self, oid: &str) -> Result<Option<CommitDetail>, BackendError> {
        let resp: Option<reef_proto::CommitDetailDto> = self.request(Request::CommitDetail {
            oid: oid.to_string(),
        })?;
        Ok(resp.map(Into::into))
    }

    fn commit_detail_for(
        &self,
        repo_root_rel: &Path,
        oid: &str,
    ) -> Result<Option<CommitDetail>, BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.commit_detail(oid);
        }
        let resp: Option<reef_proto::CommitDetailDto> = self.request(Request::CommitDetailFor {
            repo_root_rel: repo_key(&repo_root_rel),
            oid: oid.to_string(),
        })?;
        Ok(resp.map(Into::into))
    }

    fn commit_file_diff(
        &self,
        oid: &str,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError> {
        let resp: Option<reef_proto::DiffContentDto> = self.request(Request::CommitFileDiff {
            oid: oid.to_string(),
            path: path.to_string(),
            context_lines,
        })?;
        Ok(resp.map(Into::into))
    }

    fn commit_file_diff_for(
        &self,
        repo_root_rel: &Path,
        oid: &str,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.commit_file_diff(oid, path, context_lines);
        }
        let resp: Option<reef_proto::DiffContentDto> =
            self.request(Request::CommitFileDiffFor {
                repo_root_rel: repo_key(&repo_root_rel),
                oid: oid.to_string(),
                path: path.to_string(),
                context_lines,
            })?;
        Ok(resp.map(Into::into))
    }

    fn range_files(
        &self,
        oldest_oid: &str,
        newest_oid: &str,
    ) -> Result<Vec<FileEntry>, BackendError> {
        let resp: Vec<reef_proto::FileEntryDto> = self.request(Request::RangeFiles {
            oldest_oid: oldest_oid.to_string(),
            newest_oid: newest_oid.to_string(),
        })?;
        Ok(resp.into_iter().map(Into::into).collect())
    }

    fn range_files_for(
        &self,
        repo_root_rel: &Path,
        oldest_oid: &str,
        newest_oid: &str,
    ) -> Result<Vec<FileEntry>, BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.range_files(oldest_oid, newest_oid);
        }
        let resp: Vec<reef_proto::FileEntryDto> = self.request(Request::RangeFilesFor {
            repo_root_rel: repo_key(&repo_root_rel),
            oldest_oid: oldest_oid.to_string(),
            newest_oid: newest_oid.to_string(),
        })?;
        Ok(resp.into_iter().map(Into::into).collect())
    }

    fn range_file_diff(
        &self,
        oldest_oid: &str,
        newest_oid: &str,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError> {
        let resp: Option<reef_proto::DiffContentDto> = self.request(Request::RangeFileDiff {
            oldest_oid: oldest_oid.to_string(),
            newest_oid: newest_oid.to_string(),
            path: path.to_string(),
            context_lines,
        })?;
        Ok(resp.map(Into::into))
    }

    fn range_file_diff_for(
        &self,
        repo_root_rel: &Path,
        oldest_oid: &str,
        newest_oid: &str,
        path: &str,
        context_lines: u32,
    ) -> Result<Option<DiffContent>, BackendError> {
        let repo_root_rel = normalize_repo_root_rel(repo_root_rel)?;
        if repo_root_rel == Path::new(".") {
            return self.range_file_diff(oldest_oid, newest_oid, path, context_lines);
        }
        let resp: Option<reef_proto::DiffContentDto> = self.request(Request::RangeFileDiffFor {
            repo_root_rel: repo_key(&repo_root_rel),
            oldest_oid: oldest_oid.to_string(),
            newest_oid: newest_oid.to_string(),
            path: path.to_string(),
            context_lines,
        })?;
        Ok(resp.map(Into::into))
    }

    fn subscribe_fs_events(&self) -> mpsc::Receiver<()> {
        // First subscriber gets the channel created in `spawn`. Subsequent
        // calls would lose events — but the App only calls this once.
        // For safety we hand out a fresh disconnected receiver rather than
        // panicking on repeat subscription.
        if let Ok(mut slot) = self.fs_rx.lock() {
            if let Some(rx) = slot.take() {
                return rx;
            }
        }
        let (_tx, rx) = mpsc::channel::<()>();
        rx
    }

    fn launch_editor(&self, _rel_path: &Path) -> Result<(), BackendError> {
        Err(BackendError::Unimplemented(
            "remote editor launch (ssh -t forwarding) is not part of M1".into(),
        ))
    }

    fn is_remote(&self) -> bool {
        true
    }

    fn editor_launch_spec(&self, rel_path: &Path) -> Result<EditorLaunchSpec, BackendError> {
        use crate::agent_deploy::{
            RemoteOs,
            ssh::{powershell_escape, shell_escape},
        };
        use std::ffi::OsString;

        let launch = self.ssh_launch.as_ref().ok_or_else(|| {
            BackendError::Unimplemented(
                "remote editor launch requires an ssh session (use --ssh, not --agent-exec)".into(),
            )
        })?;
        // Coerce `rel_path` to a clean workdir-relative string. Absolute
        // paths are refused — the ssh shell wouldn't know what to do with
        // the reef-host path anyway.
        if rel_path.is_absolute() {
            return Err(BackendError::PathEscape(format!(
                "absolute path not allowed for remote editor: {}",
                rel_path.display()
            )));
        }
        let rel_str = normalize_remote_path(rel_path);
        // Pick the editor on the *remote* end. We don't call
        // `resolve_editor()` here — that reads the caller's env, which is
        // the wrong side. Use the standard shell fallback chain that matches
        // what users expect when ssh'ing in manually.
        let remote_cmd = match launch.remote_os {
            RemoteOs::Posix => format!(
                "cd {} && ${{VISUAL:-${{EDITOR:-vi}}}} {}",
                shell_escape(&launch.remote_workdir),
                shell_escape(&rel_str),
            ),
            RemoteOs::Windows => {
                // PowerShell: Set-Location, then resolve VISUAL/EDITOR
                // env vars with notepad as a safe default. Path
                // separators on Windows happen to accept `/` too but we
                // flip to `\` to match what Windows users expect.
                let win_rel = rel_str.replace('/', r"\");
                format!(
                    r###"powershell -NoProfile -NonInteractive -Command "Set-Location '{workdir}'; $editor = if ($env:VISUAL) {{ $env:VISUAL }} elseif ($env:EDITOR) {{ $env:EDITOR }} else {{ 'notepad' }}; & $editor '{path}'""###,
                    workdir = powershell_escape(&launch.remote_workdir),
                    path = powershell_escape(&win_rel),
                )
            }
        };
        let mut args: Vec<OsString> = Vec::with_capacity(launch.ssh_args.len() + 3);
        args.push(OsString::from("-t"));
        for a in &launch.ssh_args {
            args.push(OsString::from(a));
        }
        args.push(OsString::from(&launch.host));
        args.push(OsString::from(remote_cmd));
        Ok(EditorLaunchSpec {
            program: OsString::from("ssh"),
            args,
            inherit_tty: true,
        })
    }

    fn create_file(&self, rel_path: &Path) -> Result<(), BackendError> {
        let _: serde_json::Value = self.request(Request::CreateFile {
            rel_path: rel_path.to_string_lossy().to_string(),
        })?;
        Ok(())
    }

    fn create_dir_all(&self, rel_path: &Path) -> Result<(), BackendError> {
        let _: serde_json::Value = self.request(Request::CreateDirAll {
            rel_path: rel_path.to_string_lossy().to_string(),
        })?;
        Ok(())
    }

    fn rename(&self, from_rel: &Path, to_rel: &Path) -> Result<(), BackendError> {
        let _: serde_json::Value = self.request(Request::Rename {
            from_rel: from_rel.to_string_lossy().to_string(),
            to_rel: to_rel.to_string_lossy().to_string(),
        })?;
        Ok(())
    }

    fn copy_file(&self, from_rel: &Path, to_rel: &Path) -> Result<(), BackendError> {
        let _: serde_json::Value = self.request(Request::CopyFile {
            from_rel: from_rel.to_string_lossy().to_string(),
            to_rel: to_rel.to_string_lossy().to_string(),
        })?;
        Ok(())
    }

    fn copy_dir_recursive(&self, from_rel: &Path, to_rel: &Path) -> Result<(), BackendError> {
        let _: serde_json::Value = self.request(Request::CopyDirRecursive {
            from_rel: from_rel.to_string_lossy().to_string(),
            to_rel: to_rel.to_string_lossy().to_string(),
        })?;
        Ok(())
    }

    fn upload_from_local(
        &self,
        local_src: &Path,
        remote_dst_rel: &Path,
    ) -> Result<(), BackendError> {
        use std::process::Command;
        let launch = self.ssh_launch.as_ref().ok_or_else(|| {
            BackendError::Unimplemented(
                "remote upload requires an ssh session (use --ssh, not --agent-exec)".into(),
            )
        })?;
        // Compose `<host>:<remote_workdir>/<rel>` as a single argv entry.
        // `scp` splits host from path on the first `:` — the right side is
        // a *remote-shell* path expression, so we pre-expand the rel
        // separator to `/` and rely on the agent's workdir being absolute
        // (it is: the install script canonicalises to
        // `/home/user/...`). Shell metacharacters in the target get the
        // same single-quote escape as the install command.
        use crate::agent_deploy::ssh::shell_escape;
        let rel_str = normalize_remote_path(remote_dst_rel);
        let remote_abs = format!(
            "{}/{}",
            launch.remote_workdir.trim_end_matches('/'),
            rel_str
        );
        let target = format!("{}:{}", launch.host, shell_escape(&remote_abs));

        let mut cmd = Command::new("scp");
        for a in &launch.ssh_args {
            cmd.arg(a);
        }
        if local_src.is_dir() {
            cmd.arg("-r");
        }
        cmd.arg(local_src).arg(&target);
        let status = cmd
            .status()
            .map_err(|e| BackendError::Io(format!("spawn scp: {e}")))?;
        if !status.success() {
            return Err(BackendError::Io(format!(
                "scp {} → {} failed ({status})",
                local_src.display(),
                target
            )));
        }
        Ok(())
    }

    fn remove_file(&self, rel_path: &Path) -> Result<(), BackendError> {
        let _: serde_json::Value = self.request(Request::RemoveFile {
            rel_path: rel_path.to_string_lossy().to_string(),
        })?;
        Ok(())
    }

    fn remove_dir_all(&self, rel_path: &Path) -> Result<(), BackendError> {
        let _: serde_json::Value = self.request(Request::RemoveDirAll {
            rel_path: rel_path.to_string_lossy().to_string(),
        })?;
        Ok(())
    }

    fn trash(&self, rel_paths: &[PathBuf]) -> Result<TrashOutcome, BackendError> {
        let paths = rel_paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let resp: TrashResponseDto = self.request(Request::Trash { rel_paths: paths })?;
        Ok(TrashOutcome {
            used_trash: resp.used_trash,
        })
    }

    fn hard_delete(&self, rel_paths: &[PathBuf]) -> Result<(), BackendError> {
        let paths = rel_paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let _: serde_json::Value = self.request(Request::HardDelete { rel_paths: paths })?;
        Ok(())
    }

    fn walk_repo_paths(&self, opts: &WalkOpts) -> Result<WalkResponse, BackendError> {
        let dto = WalkOptsDto {
            include_hidden: opts.include_hidden,
            respect_gitignore: opts.respect_gitignore,
            max_files: opts.max_files,
        };
        let resp: WalkResponseDto = self.request(Request::WalkRepoPaths { opts: dto })?;
        Ok(WalkResponse {
            paths: resp.paths,
            truncated: resp.truncated,
        })
    }

    fn search_content(
        &self,
        request: &ContentSearchRequest,
        on_chunk: &mut SearchChunkSink<'_>,
    ) -> Result<ContentSearchCompleted, BackendError> {
        let dto = ContentSearchRequestDto {
            pattern: request.pattern.clone(),
            fixed_strings: request.fixed_strings,
            case_sensitive: request.case_sensitive,
            max_results: request.max_results,
            max_line_chars: request.max_line_chars,
        };

        // Allocate the envelope id up-front so we can register the chunk
        // sink *before* the frame reaches the agent — otherwise the
        // agent can legally race ahead and emit a `SearchChunk` between
        // send and registration which would be silently dropped.
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<MatchHitDto>>();
        let (resp_tx, resp_rx) = mpsc::channel::<Response>();
        let _pending = MapGuard::register(&self.pending, id, resp_tx, "pending")?;
        let _chunks = MapGuard::register(&self.search_chunks, id, chunk_tx, "chunk")?;

        self.send_envelope(Envelope {
            id,
            body: Request::SearchContent { request: dto },
        })?;

        // Concurrent drain: prioritise forwarding chunks promptly. We
        // poll the response channel with a short timeout so a slow
        // first hit doesn't starve the UI, and between polls we
        // non-blockingly drain any already-buffered chunk frames.
        // `DEFAULT_RPC_TIMEOUT` is re-interpreted as a *gap* limit —
        // the wall-clock deadline for receiving *something* from the
        // agent — since a long search can legitimately take minutes on
        // a huge workdir and we still want it to work.
        let mut aborted = false;
        // 100 ms poll cap: large enough to amortize syscall overhead over
        // long searches without being perceptible as UI latency.
        let poll = Duration::from_millis(100);
        loop {
            // Drain every chunk that's already waiting — this is the
            // hot path when the agent is producing matches faster than
            // the UI consumes them.
            while let Ok(hits) = chunk_rx.try_recv() {
                if aborted {
                    continue;
                }
                let domain: Vec<ContentMatchHit> =
                    hits.into_iter().map(match_hit_dto_to_domain).collect();
                if matches!(on_chunk(domain), ControlFlow::Break(())) {
                    aborted = true;
                }
            }
            match resp_rx.recv_timeout(poll) {
                Ok(Response::Ok { result, .. }) => {
                    // Final drain: chunks may still be queued behind
                    // the response frame because the reader thread
                    // processes frames serially.
                    while let Ok(hits) = chunk_rx.try_recv() {
                        if aborted {
                            continue;
                        }
                        let domain: Vec<ContentMatchHit> =
                            hits.into_iter().map(match_hit_dto_to_domain).collect();
                        if matches!(on_chunk(domain), ControlFlow::Break(())) {
                            aborted = true;
                        }
                    }
                    let completed: ContentSearchCompletedDto = serde_json::from_value(result)
                        .map_err(|e| BackendError::Protocol(format!("response decode: {e}")))?;
                    return Ok(ContentSearchCompleted {
                        truncated: completed.truncated,
                    });
                }
                Ok(Response::Err { code, message, .. }) => {
                    return Err(BackendError::from_wire(code, message));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Loop back: drain more chunks, then try the
                    // response again. No wall-clock deadline here —
                    // SearchContent is allowed to run as long as the
                    // walker needs. Callers that need to bound it flip
                    // `ControlFlow::Break` via the sink.
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if let Ok(mut pending) = self.pending.lock() {
                        pending.remove(&id);
                    }
                    return Err(BackendError::Rpc("agent closed connection".into()));
                }
            }
        }
    }
}

/// Wire-format path: always forward-slash so the agent's shell receives a
/// consistent separator regardless of which OS the *client* runs on.
fn normalize_remote_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn match_hit_dto_to_domain(h: MatchHitDto) -> ContentMatchHit {
    ContentMatchHit {
        path: PathBuf::from(&h.path),
        display: h.display,
        line: h.line as usize,
        line_text: h.line_text,
        byte_range: (h.byte_range_start as usize)..(h.byte_range_end as usize),
    }
}

fn walk_remote(
    backend: &RemoteBackend,
    dir: &Path,
    expanded: &HashSet<PathBuf>,
    git_statuses: &HashMap<String, char>,
    out: &mut Vec<TreeEntry>,
    depth: usize,
) -> Result<(), String> {
    let dir_str = dir.to_string_lossy().to_string();
    let entries: Vec<DirEntryDto> = backend
        .request(Request::ReadDir {
            path: dir_str.clone(),
        })
        .map_err(|e| format!("ReadDir {dir_str:?}: {e}"))?;
    let mut entries: Vec<DirEntryDto> = entries.into_iter().filter(|e| e.name != ".git").collect();
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    for entry in entries {
        let rel = if dir.as_os_str().is_empty() {
            PathBuf::from(&entry.name)
        } else {
            dir.join(&entry.name)
        };
        let rel_str = rel.to_string_lossy().to_string();
        let is_expanded = entry.is_dir && expanded.contains(&rel);
        let git_status = git_statuses.get(&rel_str).copied();

        out.push(TreeEntry {
            path: rel.clone(),
            name: entry.name,
            depth,
            is_dir: entry.is_dir,
            is_expanded,
            git_status,
        });

        if entry.is_dir && is_expanded {
            walk_remote(backend, &rel, expanded, git_statuses, out, depth + 1)?;
        }
    }
    Ok(())
}

// ── DTO -> domain conversions ─────────────────────────────────────────────

impl From<reef_proto::FileEntryDto> for FileEntry {
    fn from(v: reef_proto::FileEntryDto) -> Self {
        FileEntry {
            path: v.path,
            status: v.status.into(),
            additions: v.additions,
            deletions: v.deletions,
        }
    }
}

impl From<reef_proto::FileStatusDto> for crate::git::FileStatus {
    fn from(v: reef_proto::FileStatusDto) -> Self {
        use crate::git::FileStatus;
        match v {
            reef_proto::FileStatusDto::Modified => FileStatus::Modified,
            reef_proto::FileStatusDto::Added => FileStatus::Added,
            reef_proto::FileStatusDto::Deleted => FileStatus::Deleted,
            reef_proto::FileStatusDto::Renamed => FileStatus::Renamed,
            reef_proto::FileStatusDto::Untracked => FileStatus::Untracked,
        }
    }
}

impl From<reef_proto::DiffContentDto> for DiffContent {
    fn from(v: reef_proto::DiffContentDto) -> Self {
        DiffContent {
            file_path: v.file_path,
            hunks: v.hunks.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<reef_proto::DiffHunkDto> for crate::git::DiffHunk {
    fn from(v: reef_proto::DiffHunkDto) -> Self {
        crate::git::DiffHunk {
            header: v.header,
            lines: v.lines.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<reef_proto::DiffLineDto> for crate::git::DiffLine {
    fn from(v: reef_proto::DiffLineDto) -> Self {
        crate::git::DiffLine {
            tag: v.tag.into(),
            content: v.content,
            old_lineno: v.old_lineno,
            new_lineno: v.new_lineno,
        }
    }
}

impl From<reef_proto::LineTagDto> for crate::git::LineTag {
    fn from(v: reef_proto::LineTagDto) -> Self {
        match v {
            reef_proto::LineTagDto::Context => crate::git::LineTag::Context,
            reef_proto::LineTagDto::Added => crate::git::LineTag::Added,
            reef_proto::LineTagDto::Removed => crate::git::LineTag::Removed,
        }
    }
}

impl From<reef_proto::CommitInfoDto> for CommitInfo {
    fn from(v: reef_proto::CommitInfoDto) -> Self {
        CommitInfo {
            oid: v.oid,
            short_oid: v.short_oid,
            parents: v.parents,
            author_name: v.author_name,
            author_email: v.author_email,
            time: v.time,
            subject: v.subject,
        }
    }
}

impl From<reef_proto::CommitDetailDto> for CommitDetail {
    fn from(v: reef_proto::CommitDetailDto) -> Self {
        CommitDetail {
            info: v.info.into(),
            message: v.message,
            committer_name: v.committer_name,
            committer_time: v.committer_time,
            files: v.files.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<reef_proto::RefLabelDto> for RefLabel {
    fn from(v: reef_proto::RefLabelDto) -> Self {
        match v {
            reef_proto::RefLabelDto::Head => RefLabel::Head,
            reef_proto::RefLabelDto::Branch(s) => RefLabel::Branch(s),
            reef_proto::RefLabelDto::RemoteBranch(s) => RefLabel::RemoteBranch(s),
            reef_proto::RefLabelDto::Tag(s) => RefLabel::Tag(s),
        }
    }
}

// ── SQLite preview DTO -> domain conversions ────────────────────────────
//
// Both source and destination types are foreign to this crate (reef_proto
// and reef_sqlite_preview), so the orphan rule blocks `impl From<...>`
// here. Free functions instead — slightly noisier at the call site but
// keeps reef-sqlite-preview wire-agnostic (no reef-proto dep).

fn database_info_from_dto(v: reef_proto::DatabaseInfoDto) -> reef_sqlite_preview::DatabaseInfo {
    reef_sqlite_preview::DatabaseInfo {
        tables: v.tables.into_iter().map(table_summary_from_dto).collect(),
        selected_table: v.selected_table as usize,
        initial_page: db_page_from_dto(v.initial_page),
        bytes_on_disk: v.bytes_on_disk,
    }
}

fn table_summary_from_dto(v: reef_proto::TableSummaryDto) -> reef_sqlite_preview::TableSummary {
    reef_sqlite_preview::TableSummary {
        name: v.name,
        columns: v.columns.into_iter().map(column_info_from_dto).collect(),
        row_count: v.row_count,
    }
}

fn column_info_from_dto(v: reef_proto::ColumnInfoDto) -> reef_sqlite_preview::ColumnInfo {
    reef_sqlite_preview::ColumnInfo {
        name: v.name,
        decl_type: v.decl_type,
    }
}

fn db_page_from_dto(v: reef_proto::DbPageDto) -> reef_sqlite_preview::DbPage {
    reef_sqlite_preview::DbPage {
        rows: v
            .rows
            .into_iter()
            .map(|cells| cells.into_iter().map(sqlite_value_from_dto).collect())
            .collect(),
    }
}

fn sqlite_value_from_dto(v: reef_proto::SqliteValueDto) -> reef_sqlite_preview::SqliteValue {
    match v {
        reef_proto::SqliteValueDto::Null => reef_sqlite_preview::SqliteValue::Null,
        reef_proto::SqliteValueDto::Integer { value } => {
            reef_sqlite_preview::SqliteValue::Integer(value)
        }
        reef_proto::SqliteValueDto::Real { value } => reef_sqlite_preview::SqliteValue::Real(value),
        reef_proto::SqliteValueDto::Text { value, truncated } => {
            reef_sqlite_preview::SqliteValue::Text { value, truncated }
        }
        reef_proto::SqliteValueDto::Blob { len } => {
            reef_sqlite_preview::SqliteValue::Blob { len: len as usize }
        }
    }
}
