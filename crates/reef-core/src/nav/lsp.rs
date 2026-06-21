//! LSP client: long-lived child subprocess, dedicated reader thread,
//! request/response pending map keyed by JSON-RPC IDs.
//!
//! **Scope of v1:** PATH-detected language servers only. Auto-download is
//! deferred; this file keeps the minimal protocol core.
//!
//! **SSH:** Never starts in remote mode. The nav worker gates on
//! `Backend::is_remote()` before dispatching `LspRefine` tasks.
//!
//! **Quality contract:** LSP results never move the user's cursor.
//! They write to the refine cache (`App::nav_refine_cache`); the next
//! `gd` on the same identifier consults the cache first.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};

use super::NavLang;

/// Upper bound on a single JSON-RPC frame body. A well-formed
/// definition response is a few KB; this cap stops a malformed or
/// corrupted `Content-Length` from triggering a multi-GB allocation in
/// `read_frame`. 64 MiB is far above any real LSP message.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Cap on `LspClient::doc_versions` before it's flushed. Bounds the
/// per-URI version map for long sessions (see `goto_definition`).
const MAX_TRACKED_DOCS: usize = 1024;

/// LSP supervisor state — exposed to the UI for the status-bar badge.
/// `Off` is the resting state when no LSP has been requested yet for
/// this language; `Ready` after `initialized`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspBadge {
    /// No supervisor instance exists for this language yet, or the
    /// binary couldn't be located.
    Off,
    /// Child spawned, `initialize` sent, awaiting `initialized`
    /// notification.
    Booting,
    /// Up and answering requests.
    Ready,
    /// Last request crashed the child. The supervisor restarts on the
    /// next call.
    Crashed,
}

/// JSON-RPC over stdio. Owns the child + the reader thread + the
/// response pending map. Cloneable handles aren't useful — exactly one
/// of these per `(lang, workspace)` pair, held by the nav worker.
pub struct LspClient {
    #[allow(dead_code)] // We don't read it after spawn but the join handle keeps the thread alive.
    child: Mutex<Child>,
    tx: Mutex<BufWriter<ChildStdin>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, std::sync::mpsc::Sender<Value>>>>,
    /// Monotonic `textDocument` version per URI. We `didClose` after
    /// every request, then `didOpen` again on the next `gd`; replaying
    /// `version: 1` each time makes strict servers treat the re-open as
    /// a stale no-op and return empty. A never-resetting counter keeps
    /// the version strictly increasing across re-opens.
    doc_versions: Mutex<HashMap<String, i64>>,
    _reader: thread::JoinHandle<()>,
    _stderr_reader: thread::JoinHandle<()>,
}

impl LspClient {
    /// Try to spawn the LSP and run `initialize`. Returns `Ok` after
    /// the server responds to `initialize` (we send `initialized`
    /// right after). Returns `Err` for the common failure modes:
    ///   - binary not found in PATH,
    ///   - child spawn failed,
    ///   - initialize response timed out / didn't parse.
    pub fn spawn(lang: NavLang, workspace: PathBuf) -> Result<Self, String> {
        let profile_lsp = lang
            .profile()
            .lsp
            .as_ref()
            .ok_or_else(|| format!("no LSP profile for {}", lang.name()))?;
        let bin = locate_binary(profile_lsp.bin)
            .ok_or_else(|| format!("no LSP binary `{}` on PATH", profile_lsp.bin))?;

        let mut child = Command::new(&bin)
            .args(profile_lsp.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn {bin:?}: {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "child has no stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "child has no stdout".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "child has no stderr".to_string())?;

        let pending: Arc<Mutex<HashMap<u64, std::sync::mpsc::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = Arc::clone(&pending);
        let reader = thread::Builder::new()
            .name(format!("reef-lsp-{}-reader", lang.name().to_lowercase()))
            .spawn(move || read_loop(stdout, reader_pending))
            .map_err(|e| format!("spawn reader thread: {e}"))?;
        let stderr_reader = thread::Builder::new()
            .name(format!("reef-lsp-{}-stderr", lang.name().to_lowercase()))
            .spawn(move || drain_stderr(stderr))
            .map_err(|e| format!("spawn stderr thread: {e}"))?;

        let client = Self {
            child: Mutex::new(child),
            tx: Mutex::new(BufWriter::new(stdin)),
            next_id: AtomicU64::new(1),
            pending,
            doc_versions: Mutex::new(HashMap::new()),
            _reader: reader,
            _stderr_reader: stderr_reader,
        };

        // Initialize handshake — LSP requires this before any feature
        // request. Use minimal client capabilities: we only do
        // textDocument/definition for now; everything else is
        // negotiated as "we don't support it".
        let workspace_uri = workspace_uri(&workspace);
        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": workspace_uri,
            "capabilities": {
                "textDocument": {
                    "definition": { "linkSupport": false },
                    "synchronization": {
                        "didSave": false,
                        "willSave": false,
                        "willSaveWaitUntil": false,
                    },
                }
            },
            "workspaceFolders": [{
                "uri": workspace_uri,
                "name": workspace.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("workspace"),
            }],
        });
        // `initialize` returns capabilities quickly — the slow part
        // (project indexing) happens asynchronously after the
        // `initialized` notification and does NOT block definition
        // requests (they just return empty until indexed). 15s is
        // ample for the handshake; the old 60s only mattered for a
        // hung server, which the nav worker's spawn-backoff now
        // handles instead of parking the thread for a full minute.
        client.request(
            "initialize",
            init_params,
            std::time::Duration::from_secs(15),
        )?;
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    /// `textDocument/definition` — the only request we send today.
    /// `source` is the in-memory file content; we `didOpen` it before
    /// asking so rust-analyzer doesn't refuse to answer based on disk
    /// content.
    pub fn goto_definition(
        &self,
        path: &std::path::Path,
        source: &[u8],
        line: u32,
        character: u32,
        lang: NavLang,
    ) -> Result<Option<LspLocation>, String> {
        let uri = file_uri(path);
        let lang_id = lsp_language_id(lang);
        let text = String::from_utf8_lossy(source).into_owned();
        // Strictly-increasing version per URI — see `doc_versions`.
        let version = {
            let mut versions = self.doc_versions.lock().unwrap();
            // Bound the map: it retains one entry per distinct URI ever
            // opened (we never prune on didClose, to keep versions
            // monotonic across re-opens). A long session browsing a huge
            // repo would otherwise grow it without limit. Past the cap,
            // drop everything and restart numbering — the affected docs
            // are all closed, so a fresh `version: 1` is valid for them.
            if versions.len() > MAX_TRACKED_DOCS {
                versions.clear();
            }
            let v = versions.entry(uri.clone()).or_insert(0);
            *v += 1;
            *v
        };
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": lang_id,
                    "version": version,
                    "text": text,
                }
            }),
        )?;
        // Capture the result WITHOUT `?` so didClose always runs — a
        // timeout/error must not leak an open document server-side
        // (the next click would re-didOpen an already-open URI,
        // which strict servers reject). `character` is a UTF-16
        // column (converted by the caller); see `byte_col_to_utf16`.
        let result = self.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            }),
            std::time::Duration::from_secs(10),
        );
        // Close the document so the server doesn't accumulate stale
        // buffers for every file the user views — even on the error
        // path above.
        let _ = self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        );
        let resp = result?;
        Ok(parse_definition_response(&resp))
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write_frame(&msg)
    }

    fn request(
        &self,
        method: &str,
        params: Value,
        timeout: std::time::Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel::<Value>();
        self.pending.lock().unwrap().insert(id, tx);
        // Ensures the pending slot is freed even if the wait times out
        // — without this, a flaky server would leak Senders forever.
        let _guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            id,
        };
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_frame(&msg)?;
        let resp = rx
            .recv_timeout(timeout)
            .map_err(|e| format!("LSP {method} timed out: {e}"))?;
        Ok(resp)
    }

    fn write_frame(&self, msg: &Value) -> Result<(), String> {
        let body = serde_json::to_string(msg).map_err(|e| format!("serialize: {e}"))?;
        let mut w = self.tx.lock().unwrap();
        write!(w, "Content-Length: {}\r\n\r\n{}", body.len(), body)
            .map_err(|e| format!("write: {e}"))?;
        w.flush().map_err(|e| format!("flush: {e}"))?;
        Ok(())
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Best-effort shutdown — send `shutdown` + `exit`, then kill
        // if the child outlives us.
        let _ = self.notify("exit", json!({}));
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// RAII guard that removes our entry from the pending map on
/// timeout/error so reusing the same `id` later doesn't pick up the
/// stale Sender.
struct PendingGuard {
    pending: Arc<Mutex<HashMap<u64, std::sync::mpsc::Sender<Value>>>>,
    id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let _ = self.pending.lock().unwrap().remove(&self.id);
    }
}

/// LSP response location, normalized to reef's coordinate system
/// (1-based line is server-side; we keep server's 0-based row).
#[derive(Debug, Clone)]
pub struct LspLocation {
    pub path: PathBuf,
    pub line: u32,
    /// Start column of the definition's range, in UTF-16 code units (the
    /// LSP default encoding). Converted to a byte column via
    /// `nav::utf16_range_to_byte` before it can index reef's per-line byte
    /// ranges.
    pub character: u32,
    /// End column of the range (UTF-16 units) when the server returns a
    /// single-line range, else equal to `character`. Lets the reveal
    /// highlight span the whole identifier rather than collapsing to a
    /// zero-width band at the start column.
    pub character_end: u32,
}

/// Reader loop — runs in its own thread. Demuxes responses by JSON-RPC
/// ID into the pending map; ignores notifications and server-side
/// requests (we don't handle them in v1).
fn read_loop(
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<u64, std::sync::mpsc::Sender<Value>>>>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let Some(body) = read_frame(&mut reader) else {
            // EOF: server died, drain leaves the pending senders
            // dropped (clients see RecvError on rx).
            break;
        };
        let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
            continue;
        };
        if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
            let Some(tx) = pending.lock().unwrap().remove(&id) else {
                continue;
            };
            let _ = tx.send(msg);
        }
        // Server-side notifications (window/logMessage,
        // $/progress, etc.) are ignored — we don't surface them in
        // v1. Phase 4 polish: forward window/showMessage to the
        // toast layer.
    }
}

fn read_frame<R: BufRead>(reader: &mut R) -> Option<Vec<u8>> {
    let mut content_length: Option<usize> = None;
    let mut header_line = String::new();
    loop {
        header_line.clear();
        let n = reader.read_line(&mut header_line).ok()?;
        if n == 0 {
            return None;
        }
        let trimmed = header_line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        // Header names are case-insensitive (LSP frames them HTTP-style).
        // A server or stdio shim emitting `content-length:` must not kill
        // the reader loop — match the name case-insensitively rather than
        // with an exact `strip_prefix`.
        if let Some((name, rest)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("Content-Length") {
                content_length = rest.trim().parse().ok();
            }
        }
    }
    let len = content_length?;
    // Reject an implausible length before allocating — a malformed or
    // corrupted header must not drive a multi-GB `vec![0u8; len]`. The
    // reader returns None (treated as EOF) so the supervisor recovers
    // via spawn-backoff rather than OOM-ing the process.
    if len > MAX_FRAME_BYTES {
        return None;
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    Some(body)
}

fn drain_stderr(stderr: std::process::ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
        // Discarded for now. rust-analyzer is chatty on stderr
        // (progress, logging); piping it into reef's status bar would
        // need a per-line filter. Phase 4 follow-up.
        line.clear();
    }
}

fn parse_definition_response(resp: &Value) -> Option<LspLocation> {
    let result = resp.get("result")?;
    // The server returns either a Location, a Location[], or null.
    let first = if result.is_null() {
        return None;
    } else if result.is_array() {
        result.as_array()?.first()?.clone()
    } else {
        result.clone()
    };
    // `Location` carries `uri`; `LocationLink` carries `targetUri` +
    // `targetSelectionRange`/`targetRange`. A server that ignores our
    // `linkSupport: false` and returns `LocationLink[]` only has
    // `targetUri` — fall back to it, otherwise the `targetRange` branch
    // below would be permanently unreachable (we'd bail on the missing
    // `uri` first).
    let uri = first
        .get("uri")
        .or_else(|| first.get("targetUri"))
        .and_then(|v| v.as_str())?;
    let path = uri_to_path(uri)?;
    let range = first
        .get("range")
        .or_else(|| first.get("targetSelectionRange"))
        .or_else(|| first.get("targetRange"))?;
    let start = range.get("start")?;
    let line = start.get("line")?.as_u64()? as u32;
    let character = start.get("character")?.as_u64()? as u32;
    // End column, but only when the range stays on `line` (definitions
    // are single-token, so this holds in practice). A multi-line range
    // falls back to a zero-width band at the start.
    let character_end = range
        .get("end")
        .filter(|end| end.get("line").and_then(|l| l.as_u64()) == Some(line as u64))
        .and_then(|end| end.get("character"))
        .and_then(|c| c.as_u64())
        .map(|c| c as u32)
        .unwrap_or(character);
    Some(LspLocation {
        path,
        line,
        character,
        character_end,
    })
}

/// Walk `PATH` looking for `name`. Single source of truth for "is
/// this binary installed?" — used by the supervisor on every lazy
/// spawn, and by `App::lang_lsp_available` for surface UX (e.g. the
/// status-bar badge can pre-render `RA?` when the binary is
/// installable). Phase 4 will layer a `~/.cache/reef/lsp/` lookup on
/// top of this same path.
pub fn locate_binary(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = candidate.with_extension("exe");
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

fn lsp_language_id(lang: NavLang) -> &'static str {
    lang.profile()
        .lsp
        .as_ref()
        .expect("caller gates on profile.lsp")
        .language_id
}

fn workspace_uri(path: &std::path::Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    file_uri(&abs)
}

fn file_uri(path: &std::path::Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    url::Url::from_file_path(&abs)
        .expect("canonical file path")
        .to_string()
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_round_trips_unsafe_and_non_ascii_paths() {
        for path in ["/Users/名前/proj/src.rs", "/has space/a#b%c", "/plain/path"] {
            let uri = file_uri(std::path::Path::new(path));
            assert_eq!(uri_to_path(&uri).unwrap(), PathBuf::from(path));
        }
    }
}
