//! reef-proto — length-prefixed JSON-RPC protocol between `reef` (the TUI)
//! and `reef-agent` (the remote daemon).
//!
//! Wire format: each message is a 4-byte big-endian length prefix followed
//! by a UTF-8 JSON body. The body is either a `Request` envelope
//! (`{"id": u64, "body": {...}}`), a `Response`, or a `Notification`. We
//! pick JSON over msgpack/bincode because reef-agent runs at interactive
//! request rates (a few to a few dozen per second) and the cost of JSON
//! parsing is rounding error next to git / fs syscalls — while the
//! human-readable wire format pays for itself the first time somebody
//! has to `ssh host reef-agent --stdio < fixture.jsonl` to debug a
//! failing connection.
//!
//! The Backend trait on the reef side owns a rich domain type set
//! (`FileEntry`, `DiffContent`, `CommitInfo`, …). The types here are
//! bit-for-bit equivalents but **live in this crate**, because the
//! reef-proto crate can't depend on reef (cyclic) and we want the agent to
//! be able to consume the protocol types without pulling in git2, syntect,
//! ratatui and friends. `src/backend/remote.rs` holds the `From<Dto>` impls
//! that translate the protocol types back into their domain twins.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Read, Write};

// ── Frame codec ────────────────────────────────────────────────────────────

/// Maximum frame size we accept on the wire. 16 MiB — comfortably above the
/// largest `CommitDetail` or single-file diff we'd ever ship, and small
/// enough that a bug that accidentally negotiates a length near `u32::MAX`
/// fails fast instead of attempting a multi-GiB allocation.
pub const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

/// Protocol version spoken by this crate. Bumped whenever request/response
/// DTOs change in a way that would make an older agent misinterpret them
/// (e.g. adding new Request variants, renaming fields, changing semantics).
///
/// `reef-agent --protocol-version` prints this number so the install script
/// can detect a stale agent binary and force a reinstall. The number itself
/// is deliberately small — the client trusts the on-disk agent only when
/// the version string matches byte-for-byte.
///
/// - v1: initial release (M1/M2; status/diff/commit/ReadDir/ReadFile).
/// - v2: M3 adds file mutations (CreateFile/CreateDir/Rename/CopyWithin/
///       UploadFile/RemoveRecursive/Trash) + WalkFiles + SearchContent.
/// - v3: M4 adds `RevertPath` for folder/section discard and extends
///       `FileEntryDto` with `additions`/`deletions` for the Git tab's
///       `+N -M` column.
/// - v4: `SearchContent` becomes truly streaming — the final response is
///       now `ContentSearchCompletedDto { truncated }` and hits arrive
///       asynchronously as `Notification::SearchChunk { request_id, hits }`
///       frames before the Ok response lands. Older agents that return
///       all hits in the response body would be misinterpreted (missing
///       notifications, wrong response shape) so this is a hard bump.
/// - v5: adds `RangeFiles` / `RangeFileDiff` for the Graph tab's
///       multi-commit range mode. v4 agents would error `Unknown op`
///       on these requests, which a v5 client surfaces as a toast — so
///       a hard bump keeps the auto-redeploy path honest.
/// - v6: adds `Commit { message }` so the Git tab can create commits
///       from the staged index. A v5 agent would respond `Unknown op`
///       to the new request, so bumping surfaces the stale agent as a
///       toast rather than a silent no-op.
/// - v7: adds `LoadDbInitial` / `LoadDbPage` for the SQLite preview
///       card in the Files tab. A v6 agent would respond `Unknown op`
///       so we bump to surface the stale agent as a toast rather than
///       leaving the preview pane silently stuck on the loading card.
/// - v8: adds `DiscoverRepos` so a parent workdir can expose multiple
///       child Git repositories before Git operations become repo-scoped.
/// - v9: adds `WriteFile` for global find-and-replace, plus `FileSize`
///       so the worker can skip oversized files without round-tripping
///       their bytes. A v7 agent would respond `Unknown op` to either,
///       so a hard bump surfaces the stale agent as a toast and
///       triggers auto-redeploy.
/// - v10: adds `GitStatusFor` so status runs in the selected repo.
/// - v11: adds repo-scoped diff requests.
/// - v12: adds repo-scoped stage/unstage requests.
/// - v13: adds `RevertPathFor` so discard/restore runs in the selected repo.
/// - v14: adds `PushFor` / `CommitFor` so push/commit run in the selected repo.
/// - v15: adds repo-scoped graph/history requests.
/// - v16: adds repo-scoped branch checkout.
/// - v17: adds repo-scoped pull.
pub const PROTOCOL_VERSION: u32 = 17;

/// Encode a single envelope-level value to `writer` using the
/// length-prefixed framing. The caller is expected to flush.
pub fn encode_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    if body.len() as u64 > u64::from(MAX_FRAME_SIZE) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "frame body {} exceeds MAX_FRAME_SIZE {}",
                body.len(),
                MAX_FRAME_SIZE
            ),
        ));
    }
    let len = body.len() as u32;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&body)?;
    Ok(())
}

/// One framed message read from the wire. `decode_frame` never blocks
/// longer than the underlying reader does on a single `read_exact` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Frame {
    Response(Response),
    Notification(Notification),
}

/// Read one framed message. Returns `UnexpectedEof` when the stream closes
/// at a frame boundary, `InvalidData` when the length prefix is absurdly
/// large or the body is not valid JSON.
pub fn decode_frame<R: Read>(reader: &mut R) -> io::Result<Frame> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds MAX_FRAME_SIZE {MAX_FRAME_SIZE}"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body)?;
    serde_json::from_slice::<Frame>(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("frame decode: {e}")))
}

/// Agent-side convenience: read one envelope (`{id, body}`). Agents treat
/// every client-bound message as a `Request`; the client side instead
/// decodes `Frame` values because responses and notifications are both
/// server→client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub id: u64,
    pub body: Request,
}

/// Read one envelope from the wire (agent-side).
pub fn read_envelope<R: Read>(reader: &mut R) -> io::Result<Envelope> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds MAX_FRAME_SIZE {MAX_FRAME_SIZE}"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body)?;
    serde_json::from_slice::<Envelope>(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("envelope decode: {e}")))
}

// ── Messages ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "snake_case")]
pub enum Request {
    /// First call after spawning — returns workdir + repo identity.
    Handshake,
    /// Ask the agent to exit after replying. The agent is free to terminate
    /// without a reply.
    Shutdown,

    // ── Filesystem ────
    ReadDir {
        path: String,
    },
    ReadFile {
        path: String,
        max_bytes: u64,
    },

    // ── Workspace Git repositories ────
    DiscoverRepos {
        opts: RepoDiscoverOptsDto,
    },

    // ── Git: status / diff ────
    GitStatus,
    GitStatusFor {
        repo_root_rel: String,
    },
    StagedDiff {
        path: String,
        context_lines: u32,
    },
    StagedDiffFor {
        repo_root_rel: String,
        path: String,
        context_lines: u32,
    },
    UnstagedDiff {
        path: String,
        context_lines: u32,
    },
    UnstagedDiffFor {
        repo_root_rel: String,
        path: String,
        context_lines: u32,
    },
    UntrackedDiff {
        path: String,
    },
    UntrackedDiffFor {
        repo_root_rel: String,
        path: String,
    },

    Stage {
        path: String,
    },
    StageFor {
        repo_root_rel: String,
        path: String,
    },
    Unstage {
        path: String,
    },
    UnstageFor {
        repo_root_rel: String,
        path: String,
    },
    Restore {
        path: String,
    },

    /// "Discard all" / folder-level discard backing op. Collapses the
    /// `unstage(path)` + `restore(path)` sequence the client used to
    /// issue locally (pre-M4) into a single RPC so `RemoteBackend` can
    /// reach the agent-side `git2::Repository` handle — otherwise the
    /// Folder/Section discard branches would silently no-op on remote
    /// (see `App::apply_discard_target`).
    RevertPath {
        path: String,
        is_staged: bool,
    },
    RevertPathFor {
        repo_root_rel: String,
        path: String,
        is_staged: bool,
    },

    Push {
        force: bool,
    },
    PushFor {
        repo_root_rel: String,
        force: bool,
    },
    Pull,
    PullFor {
        repo_root_rel: String,
    },
    CheckoutBranch {
        branch: String,
    },
    CheckoutBranchFor {
        repo_root_rel: String,
        branch: String,
    },

    /// Create a commit from the staged index with `message`. Agent-side
    /// dispatches to `Backend::commit` which shells out to `git commit -F -`;
    /// wire format on success is `{"ok": true}`.
    Commit {
        message: String,
    },
    CommitFor {
        repo_root_rel: String,
        message: String,
    },

    // ── Git: history ────
    ListCommits {
        limit: u64,
    },
    ListCommitsFor {
        repo_root_rel: String,
        limit: u64,
    },
    ListRefs,
    ListRefsFor {
        repo_root_rel: String,
    },
    HeadOid,
    HeadOidFor {
        repo_root_rel: String,
    },
    CommitDetail {
        oid: String,
    },
    CommitDetailFor {
        repo_root_rel: String,
        oid: String,
    },
    CommitFileDiff {
        oid: String,
        path: String,
        context_lines: u32,
    },
    CommitFileDiffFor {
        repo_root_rel: String,
        oid: String,
        path: String,
        context_lines: u32,
    },
    /// Union of files changed across `oldest..=newest`. Mirrors
    /// `Backend::range_files` — wire format is `Vec<FileEntryDto>`.
    RangeFiles {
        oldest_oid: String,
        newest_oid: String,
    },
    RangeFilesFor {
        repo_root_rel: String,
        oldest_oid: String,
        newest_oid: String,
    },
    /// Single-file diff for the same range as `RangeFiles`. Wire format
    /// is `Option<DiffContentDto>` (None when the path isn't part of the
    /// range diff).
    RangeFileDiff {
        oldest_oid: String,
        newest_oid: String,
        path: String,
        context_lines: u32,
    },
    RangeFileDiffFor {
        repo_root_rel: String,
        oldest_oid: String,
        newest_oid: String,
        path: String,
        context_lines: u32,
    },

    // ── Watcher ────
    /// Tell the agent to start streaming `Notification::FsChanged` events.
    Subscribe,

    // ── M3 Track 1: write operations (all paths workdir-relative) ────
    /// Create an empty file at `rel_path`. Fails `PathExists` if it already
    /// exists (`create_new` semantics — no truncation race).
    CreateFile {
        rel_path: String,
    },
    /// Idempotent `mkdir -p` at `rel_path`.
    CreateDirAll {
        rel_path: String,
    },
    /// Rename within workdir; `fs::rename` semantics.
    Rename {
        from_rel: String,
        to_rel: String,
    },
    /// Copy a single file (not a directory) within the workdir.
    CopyFile {
        from_rel: String,
        to_rel: String,
    },
    /// Recursively copy a directory within the workdir. Symlinks are
    /// skipped — matches `src/tasks.rs::copy_dir_recursive`.
    CopyDirRecursive {
        from_rel: String,
        to_rel: String,
    },
    /// Remove a single file or symlink (no dereference).
    RemoveFile {
        rel_path: String,
    },
    /// Recursively remove a directory tree.
    RemoveDirAll {
        rel_path: String,
    },
    /// Size in bytes of an existing regular file at `rel_path`.
    /// Cheap probe used by callers (notably the global-replace worker)
    /// that need to decide whether to bother fetching the bytes —
    /// avoids round-tripping a truncated 50 MB copy of a file the
    /// caller is about to skip anyway.
    FileSize {
        rel_path: String,
    },

    /// Atomically overwrite an existing regular file with `content`.
    /// Used by global find-and-replace. Path validation rejects
    /// absolute paths, `..` traversal, and symlinks whose canonical
    /// target falls outside the workdir. Fails `NotFound` if the
    /// target doesn't already exist — write-file is replace-only,
    /// not create.
    ///
    /// `content` rides through the same `serde_bytes` base64 path as
    /// `ReadFileResponse.bytes` — without it, serde_json would
    /// expand each byte into a separate integer array element and a
    /// 50 MB replacement would blow up to ~350 MB of JSON.
    WriteFile {
        rel_path: String,
        #[serde(with = "serde_bytes")]
        content: Vec<u8>,
    },
    /// Move one or more paths to the OS Trash, or fall back to permanent
    /// deletion when no trash is configured on the remote host.
    /// Paths are processed in order; the first failure short-circuits.
    Trash {
        rel_paths: Vec<String>,
    },
    /// Permanent delete of one or more paths. Unlike `Trash`, never
    /// attempts a recycle-bin detour.
    HardDelete {
        rel_paths: Vec<String>,
    },

    // ── M3 Track 2: walk + search ────
    /// List every file in the workdir (respecting `.gitignore` /
    /// `.git/info/exclude`), capped at `max_files` to protect huge
    /// monorepos. Returns workdir-relative display strings plus a
    /// `truncated` flag when the cap was hit.
    WalkRepoPaths {
        opts: WalkOptsDto,
    },
    /// Content search: stream grep-equivalent hits back. Response body
    /// caps at `MAX_SEARCH_HITS` on the agent side — over the cap we
    /// stop walking and set `truncated = true`.
    SearchContent {
        request: ContentSearchRequestDto,
    },

    // ── M5: SQLite preview ────
    /// Build the initial preview card for a SQLite file at `rel_path`:
    /// list of tables (name + columns + row counts) and the first page
    /// of rows for the smallest non-empty table. Response is
    /// `Option<DatabaseInfoDto>` — `None` when the agent's magic-bytes
    /// probe rejects the file as non-SQLite (so the client can fall
    /// back to the standard binary card without a second round-trip).
    /// Hard errors (encrypted DB, corrupt header, file too large)
    /// propagate as `ErrorCode::Other` with a short reason string.
    LoadDbInitial {
        rel_path: String,
        page_size: u32,
    },
    /// Page-flip / table-switch on an already-previewed SQLite file.
    /// `offset` and `limit` map directly to `LIMIT N OFFSET M`. The
    /// response is `DbPageDto`. Note that offset cost grows with M for
    /// tables without a usable index — see the equivalent comment on
    /// `reef_sqlite_preview::load_page` for the keyset-pagination
    /// follow-up if this becomes a hotspot.
    LoadDbPage {
        rel_path: String,
        table: String,
        offset: u64,
        limit: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok {
        id: u64,
        result: serde_json::Value,
    },
    Err {
        id: u64,
        code: ErrorCode,
        message: String,
    },
}

impl Response {
    pub fn id(&self) -> u64 {
        match self {
            Response::Ok { id, .. } => *id,
            Response::Err { id, .. } => *id,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    Io,
    Git,
    Protocol,
    Unimplemented,
    /// Write op refused because the destination already exists.
    PathExists,
    /// Path escapes the workdir (relative path with leading `..`, or an
    /// absolute path). The agent always rejects these to keep the
    /// workdir the security boundary.
    PathEscape,
    /// `Trash` fell through to nothing — neither a trash tool nor a
    /// successful permanent delete. Distinct from `Io` so the client can
    /// phrase the toast differently.
    TrashUnavailable,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Notification {
    FsChanged,
    AgentLog {
        level: String,
        message: String,
    },
    /// Streaming `SearchContent` payload. The agent emits 1..N of these
    /// frames, tagged with the originating envelope `request_id`, before
    /// the final `Response::Ok { result: ContentSearchCompletedDto }`
    /// lands for that id. Chunk size is governed by
    /// [`CHUNK_TARGET_HITS`] on the agent side so users see the first
    /// matches as soon as they're found instead of waiting for the whole
    /// walk.
    SearchChunk {
        request_id: u64,
        hits: Vec<MatchHitDto>,
    },
}

// ── DTOs ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeResponse {
    pub workdir: String,
    pub workdir_name: String,
    pub branch_name: String,
    pub agent_version: String,
    /// Protocol version spoken by this agent binary. The client validates
    /// this against `PROTOCOL_VERSION` during handshake and rejects
    /// mismatches with a clear error rather than letting DTO decode failures
    /// surface as confusing `Protocol` errors later.
    pub protocol_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntryDto {
    pub name: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileResponse {
    pub is_file: bool,
    #[serde(with = "serde_bytes")]
    pub bytes: Vec<u8>,
    pub size: u64,
}

/// Bare minimum replacement for the `serde_bytes` crate. Base64-encodes raw
/// bytes so they round-trip through JSON without exploding into a per-byte
/// array. We roll our own rather than pulling in another dep.
mod serde_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], ser: S) -> Result<S::Ok, S::Error> {
        let encoded = encode(bytes);
        ser.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(de)?;
        decode(&s).map_err(serde::de::Error::custom)
    }

    // ── tiny base64 (standard alphabet, padded) ────────────────────────────

    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        let mut i = 0;
        while i + 3 <= input.len() {
            let n =
                ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | (input[i + 2] as u32);
            out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
            out.push(ALPHABET[(n & 0x3F) as usize] as char);
            i += 3;
        }
        match input.len() - i {
            2 => {
                let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
                out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
                out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
                out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
                out.push('=');
            }
            1 => {
                let n = (input[i] as u32) << 16;
                out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
                out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
                out.push('=');
                out.push('=');
            }
            _ => {}
        }
        out
    }

    fn decode(input: &str) -> Result<Vec<u8>, String> {
        let bytes = input.as_bytes();
        if !bytes.len().is_multiple_of(4) {
            return Err(format!("base64 length {} not a multiple of 4", bytes.len()));
        }
        let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
        let mut buf = [0u8; 4];
        let mut i = 0;
        while i < bytes.len() {
            let mut pad = 0;
            for j in 0..4 {
                let c = bytes[i + j];
                if c == b'=' {
                    pad += 1;
                    buf[j] = 0;
                    continue;
                }
                if pad > 0 {
                    return Err("base64: non-pad after pad".into());
                }
                buf[j] = decode_char(c)?;
            }
            let n = ((buf[0] as u32) << 18)
                | ((buf[1] as u32) << 12)
                | ((buf[2] as u32) << 6)
                | (buf[3] as u32);
            out.push(((n >> 16) & 0xFF) as u8);
            if pad < 2 {
                out.push(((n >> 8) & 0xFF) as u8);
            }
            if pad < 1 {
                out.push((n & 0xFF) as u8);
            }
            i += 4;
        }
        Ok(out)
    }

    fn decode_char(c: u8) -> Result<u8, String> {
        match c {
            b'A'..=b'Z' => Ok(c - b'A'),
            b'a'..=b'z' => Ok(c - b'a' + 26),
            b'0'..=b'9' => Ok(c - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            other => Err(format!("invalid base64 char {other:#x}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshotDto {
    pub staged: Vec<FileEntryDto>,
    pub unstaged: Vec<FileEntryDto>,
    pub branch_name: String,
    pub ahead_behind: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntryDto {
    pub path: String,
    pub status: FileStatusDto,
    /// v3: lines added in this file's diff (HEAD→index for staged,
    /// index→workdir for unstaged; whole-file line count for untracked).
    /// `default` so a v2 agent talking to a v3 client (shouldn't happen
    /// post-install-script check, but paranoid) deserialises as zero
    /// rather than failing the whole envelope.
    #[serde(default)]
    pub additions: u32,
    #[serde(default)]
    pub deletions: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileStatusDto {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffContentDto {
    pub file_path: String,
    pub hunks: Vec<DiffHunkDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffHunkDto {
    pub header: String,
    pub lines: Vec<DiffLineDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffLineDto {
    pub tag: LineTagDto,
    pub content: String,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LineTagDto {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfoDto {
    pub oid: String,
    pub short_oid: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub time: i64,
    pub subject: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitDetailDto {
    pub info: CommitInfoDto,
    pub message: String,
    pub committer_name: String,
    pub committer_time: i64,
    pub files: Vec<FileEntryDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum RefLabelDto {
    Head,
    Branch(String),
    RemoteBranch(String),
    Tag(String),
}

/// Convenience alias — the ref map is keyed by OID (hex).
pub type RefMapDto = HashMap<String, Vec<RefLabelDto>>;

// ── M3 Track 1: write-op responses ─────────────────────────────────────────

/// Outcome of a `Trash` request. The agent may or may not have a real
/// recycle bin available; either way the files are gone after a success,
/// but the client picks a different toast depending on `used_trash`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrashResponseDto {
    /// `true` if the agent found a system trash tool (`gio trash` etc.);
    /// `false` if it had to fall through to `fs::remove_*`.
    pub used_trash: bool,
}

// ── Workspace Git repositories ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoDiscoverOptsDto {
    pub max_depth: u64,
    pub include_nested: bool,
    pub max_repos: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRepoMetaDto {
    pub repo_root_rel: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoDiscoverResponseDto {
    pub repos: Vec<WorkspaceRepoMetaDto>,
    pub truncated: bool,
}

// ── M3 Track 2: walk + search ──────────────────────────────────────────────

/// Knobs for `WalkRepoPaths`. Mirrors what `quick_open.rs`'s `WalkBuilder`
/// wants, but spelled out on the wire so the agent can implement it
/// identically. Fields kept small — this DTO is constructed per keystroke
/// in the open palette, JSON-serialised, and shipped over ssh stdio.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalkOptsDto {
    /// Include dotfiles (default `true` — matches VSCode's Ctrl+P).
    pub include_hidden: bool,
    /// Honour `.gitignore` + `.git/info/exclude` + global excludes.
    pub respect_gitignore: bool,
    /// Hard cap on returned paths. `None` means "no cap" but the agent
    /// still applies `MAX_WALK_PATHS` to keep responses bounded.
    pub max_files: Option<u64>,
}

impl Default for WalkOptsDto {
    fn default() -> Self {
        Self {
            include_hidden: true,
            respect_gitignore: true,
            max_files: None,
        }
    }
}

/// Walk response: workdir-relative display strings + a truncation marker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalkResponseDto {
    /// Sorted list of workdir-relative file paths.
    pub paths: Vec<String>,
    pub truncated: bool,
}

/// One hit returned by `SearchContent`. Shape-compatible with
/// `src/global_search.rs::MatchHit` so the client can convert with no
/// data loss — the only difference is `path` is a plain `String` here
/// (wire format) vs `PathBuf` on the domain side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchHitDto {
    /// Workdir-relative path.
    pub path: String,
    /// Display string for the UI — usually equal to `path`, precomputed.
    pub display: String,
    /// 0-indexed line number.
    pub line: u64,
    /// Matched line text, already truncated to the client's
    /// `MAX_LINE_CHARS` cap (passed through in the request).
    pub line_text: String,
    /// Byte range of the match within `line_text`. Half-open.
    pub byte_range_start: u32,
    pub byte_range_end: u32,
}

/// Content search knobs. `max_results` and `max_line_chars` default on
/// the client side; the agent also enforces `MAX_SEARCH_HITS` as a hard
/// ceiling regardless of what the client requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentSearchRequestDto {
    pub pattern: String,
    /// `true` → exact substring (case-smart); `false` → treat as regex.
    pub fixed_strings: bool,
    /// `None` means smart-case (matches ripgrep's `-S`).
    pub case_sensitive: Option<bool>,
    /// Client-visible cap. Agent enforces `min(this, MAX_SEARCH_HITS)`.
    pub max_results: u32,
    /// Client-visible cap on `line_text` — matches `global_search::
    /// MAX_LINE_CHARS` on the reef side.
    pub max_line_chars: u32,
}

/// Final `SearchContent` response body. In v4+ hits are streamed via
/// [`Notification::SearchChunk`] frames keyed by the originating
/// envelope id; the `Ok` response carries only the terminal marker so
/// clients know the walker is done and whether the hit cap tripped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentSearchCompletedDto {
    pub truncated: bool,
}

/// Hard ceiling the agent enforces regardless of client request. Keeps
/// cumulative search output bounded so ssh stdout doesn't balloon on a
/// runaway pattern.
pub const MAX_SEARCH_HITS: u32 = 10_000;

/// Target batch size for `Notification::SearchChunk` frames. The agent
/// accumulates this many hits and then flushes — small enough that the
/// user sees the first frame almost immediately when matches are
/// plentiful, large enough that we're not one-frame-per-hit (which would
/// dominate JSON/RPC overhead on a ripgrep-like "lots of matches"
/// query). 64 lines is one terminal screenful on a typical display.
pub const CHUNK_TARGET_HITS: usize = 64;
/// Hard ceiling on `WalkRepoPaths` response. Larger monorepos can have
/// more files than this — the UI's quick-open index simply truncates,
/// which is the same behaviour as bumping into the walker's own limit.
pub const MAX_WALK_PATHS: u64 = 100_000;

// ── M5: SQLite preview DTOs ────────────────────────────────────────────────

/// Wire shape for `reef_sqlite_preview::DatabaseInfo`. Carries the table
/// list (with columns + row counts), the index of the table whose first
/// page is bundled in `initial_page`, and the on-disk byte size for the
/// preview card's meta line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseInfoDto {
    pub tables: Vec<TableSummaryDto>,
    pub selected_table: u32,
    pub initial_page: DbPageDto,
    pub bytes_on_disk: u64,
}

/// One table or view in the database. `columns` order matches a
/// `SELECT *` against this table; `row_count` is `SELECT COUNT(*)`
/// captured at preview-build time and not refreshed on page-flips.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableSummaryDto {
    pub name: String,
    pub columns: Vec<ColumnInfoDto>,
    pub row_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnInfoDto {
    pub name: String,
    /// Declared type as it appears in `PRAGMA table_info` (may be
    /// empty for typeless columns or non-standard like
    /// `"VARCHAR(255)"`).
    pub decl_type: String,
}

/// One page of rows from a table. Each inner Vec aligns positionally
/// with the parent table's `columns` (length always equals
/// `columns.len()` for every row).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbPageDto {
    pub rows: Vec<Vec<SqliteValueDto>>,
}

/// One typed cell value. NULL is distinct from an empty TEXT so the
/// renderer can italicise NULL. BLOB carries only its byte length —
/// the bytes themselves are never shipped.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SqliteValueDto {
    Null,
    Integer {
        value: i64,
    },
    Real {
        value: f64,
    },
    /// `value` is the (possibly truncated) UTF-8 string. `truncated`
    /// is `true` when the original cell was longer than
    /// `MAX_TEXT_CELL_CHARS` and the renderer should append `…`.
    Text {
        value: String,
        truncated: bool,
    },
    Blob {
        len: u64,
    },
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn envelope_roundtrip() {
        let env = Envelope {
            id: 42,
            body: Request::ReadFile {
                path: "src/main.rs".into(),
                max_bytes: 1024,
            },
        };
        let mut buf = Vec::new();
        encode_frame(&mut buf, &env).unwrap();
        let mut cursor = Cursor::new(&buf);
        let got = read_envelope(&mut cursor).unwrap();
        assert_eq!(got.id, 42);
        match got.body {
            Request::ReadFile { path, max_bytes } => {
                assert_eq!(path, "src/main.rs");
                assert_eq!(max_bytes, 1024);
            }
            other => panic!("unexpected body: {other:?}"),
        }
    }

    #[test]
    fn response_frame_roundtrip() {
        let resp = Frame::Response(Response::Ok {
            id: 7,
            result: serde_json::json!({"ok": true}),
        });
        let mut buf = Vec::new();
        encode_frame(&mut buf, &resp).unwrap();
        let mut cursor = Cursor::new(&buf);
        let got = decode_frame(&mut cursor).unwrap();
        match got {
            Frame::Response(Response::Ok { id, result }) => {
                assert_eq!(id, 7);
                assert_eq!(result, serde_json::json!({"ok": true}));
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[test]
    fn notification_frame_roundtrip() {
        let note = Frame::Notification(Notification::FsChanged);
        let mut buf = Vec::new();
        encode_frame(&mut buf, &note).unwrap();
        let mut cursor = Cursor::new(&buf);
        let got = decode_frame(&mut cursor).unwrap();
        assert!(matches!(got, Frame::Notification(Notification::FsChanged)));
    }

    #[test]
    fn truncated_length_prefix_is_unexpected_eof() {
        let mut cursor = Cursor::new(vec![0u8, 0, 1]); // only 3 bytes
        let err = decode_frame(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn oversized_length_rejected_as_invalid_data() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME_SIZE + 1).to_be_bytes());
        let mut cursor = Cursor::new(buf);
        let err = decode_frame(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn truncated_body_is_unexpected_eof() {
        // Length says 10 bytes but only 4 follow
        let mut buf = Vec::new();
        buf.extend_from_slice(&10u32.to_be_bytes());
        buf.extend_from_slice(b"oops");
        let mut cursor = Cursor::new(buf);
        let err = decode_frame(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn base64_roundtrip_all_lengths() {
        for n in 0..=64 {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 7) as u8).collect();
            let dto = ReadFileResponse {
                is_file: true,
                bytes: bytes.clone(),
                size: n as u64,
            };
            let json = serde_json::to_vec(&dto).unwrap();
            let decoded: ReadFileResponse = serde_json::from_slice(&json).unwrap();
            assert_eq!(decoded.bytes, bytes, "n={n}");
        }
    }
}
