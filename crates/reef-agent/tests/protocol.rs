//! End-to-end protocol test for `reef-agent --stdio`.
//!
//! Spawns the compiled agent as a subprocess with piped stdio, feeds it
//! framed JSON-RPC requests, reads the framed responses back, and asserts
//! the replies round-trip through every Phase-1 request.

use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use reef_proto::{
    CommitInfoDto, DirEntryDto, Envelope, FileStatusDto, Frame, HandshakeResponse, Request,
    Response, StatusSnapshotDto, decode_frame, encode_frame,
};
use test_support::{commit_file, tempdir_repo, write_file};

// These tests spawn a subprocess and don't mutate HOME, but do occasionally
// race for cwd via `tempdir_repo()`'s git config writes on the shared git
// system dir. Serialise them for safety.
static AGENT_LOCK: Mutex<()> = Mutex::new(());

fn agent_bin() -> PathBuf {
    // `CARGO_BIN_EXE_<name>` is set when a test crate declares the binary as
    // a dependency — here it's the same crate's `[[bin]]`, so cargo exposes
    // the path for us.
    PathBuf::from(env!("CARGO_BIN_EXE_reef-agent"))
}

/// Spawn `reef-agent --stdio --workdir <path>`. Returns the child plus
/// buffered reader/writer around stdin/stdout.
struct Agent {
    child: std::process::Child,
    reader: BufReader<std::process::ChildStdout>,
    writer: BufWriter<std::process::ChildStdin>,
    next_id: u64,
}

impl Agent {
    fn spawn(workdir: &std::path::Path) -> Self {
        let mut cmd = Command::new(agent_bin());
        cmd.arg("--stdio")
            .arg("--workdir")
            .arg(workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn reef-agent");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        Agent {
            child,
            reader: BufReader::new(stdout),
            writer: BufWriter::new(stdin),
            next_id: 1,
        }
    }

    fn request(&mut self, body: Request) -> Response {
        let id = self.next_id;
        self.next_id += 1;
        encode_frame(&mut self.writer, &Envelope { id, body }).unwrap();
        self.writer.flush().unwrap();
        loop {
            match decode_frame(&mut self.reader).expect("decode_frame") {
                Frame::Response(resp) => return resp,
                // Agents may emit fs notifications interleaved with
                // responses; ignore them in this test.
                Frame::Notification(_) => continue,
            }
        }
    }

    fn shutdown(&mut self) {
        let _ = encode_frame(
            &mut self.writer,
            &Envelope {
                id: u64::MAX,
                body: Request::Shutdown,
            },
        );
        let _ = self.writer.flush();
        let _ = self.child.wait();
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ok_result(resp: Response) -> serde_json::Value {
    match resp {
        Response::Ok { result, .. } => result,
        Response::Err { message, code, .. } => {
            panic!("expected Ok response, got Err({code:?}): {message}")
        }
    }
}

#[test]
fn handshake_returns_workdir_and_branch() {
    let _lock = AGENT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "hello", "init");

    let mut agent = Agent::spawn(tmp.path());
    let resp = agent.request(Request::Handshake);
    let info: HandshakeResponse = serde_json::from_value(ok_result(resp)).unwrap();
    assert!(!info.workdir.is_empty());
    assert!(info.agent_version.starts_with("0."));
    // Default branch on a freshly-init'd repo is master or main depending
    // on the host's git config — both are acceptable.
    assert!(info.branch_name == "master" || info.branch_name == "main");

    agent.shutdown();
}

#[test]
fn git_status_reports_untracked_and_staged() {
    let _lock = AGENT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "tracked.txt", "v1\n", "init");
    write_file(&raw, "tracked.txt", "v2\n"); // modified unstaged
    write_file(&raw, "new.txt", "new\n"); // untracked

    let mut agent = Agent::spawn(tmp.path());
    let resp = agent.request(Request::GitStatus);
    let snap: StatusSnapshotDto = serde_json::from_value(ok_result(resp)).unwrap();
    assert!(snap.staged.is_empty());
    let by_path: std::collections::HashMap<String, FileStatusDto> = snap
        .unstaged
        .iter()
        .map(|e| (e.path.clone(), e.status))
        .collect();
    assert_eq!(
        by_path.get("tracked.txt").copied(),
        Some(FileStatusDto::Modified)
    );
    assert_eq!(
        by_path.get("new.txt").copied(),
        Some(FileStatusDto::Untracked)
    );

    agent.shutdown();
}

#[test]
fn read_dir_lists_workdir_entries() {
    let _lock = AGENT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "alpha.txt", "", "init");
    write_file(&raw, "beta.txt", "");
    std::fs::create_dir_all(tmp.path().join("sub")).unwrap();

    let mut agent = Agent::spawn(tmp.path());
    let resp = agent.request(Request::ReadDir { path: "".into() });
    let entries: Vec<DirEntryDto> = serde_json::from_value(ok_result(resp)).unwrap();
    let names: std::collections::HashSet<String> = entries
        .iter()
        .filter(|e| e.name != ".git")
        .map(|e| e.name.clone())
        .collect();
    assert!(names.contains("alpha.txt"), "got names: {names:?}");
    assert!(names.contains("beta.txt"), "got names: {names:?}");
    assert!(names.contains("sub"), "got names: {names:?}");
    let sub_is_dir = entries.iter().find(|e| e.name == "sub").unwrap().is_dir;
    assert!(sub_is_dir);

    agent.shutdown();
}

#[test]
fn read_file_returns_bytes_and_respects_cap() {
    let _lock = AGENT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "big.txt", "abcdefghij", "init");

    let mut agent = Agent::spawn(tmp.path());
    let resp = agent.request(Request::ReadFile {
        path: "big.txt".into(),
        max_bytes: 4,
    });
    let payload: reef_proto::ReadFileResponse = serde_json::from_value(ok_result(resp)).unwrap();
    assert!(payload.is_file);
    assert_eq!(payload.size, 10);
    assert_eq!(payload.bytes, b"abcd".to_vec());

    agent.shutdown();
}

#[test]
fn stage_unstage_reflects_in_status() {
    let _lock = AGENT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "v1\n", "init");
    write_file(&raw, "a.txt", "v2\n");

    let mut agent = Agent::spawn(tmp.path());

    // Stage
    let _ = ok_result(agent.request(Request::Stage {
        path: "a.txt".into(),
    }));
    let snap: StatusSnapshotDto =
        serde_json::from_value(ok_result(agent.request(Request::GitStatus))).unwrap();
    assert_eq!(snap.staged.len(), 1);
    assert!(snap.unstaged.is_empty());

    // Unstage
    let _ = ok_result(agent.request(Request::Unstage {
        path: "a.txt".into(),
    }));
    let snap: StatusSnapshotDto =
        serde_json::from_value(ok_result(agent.request(Request::GitStatus))).unwrap();
    assert!(snap.staged.is_empty());
    assert_eq!(snap.unstaged.len(), 1);

    agent.shutdown();
}

#[test]
fn list_commits_returns_history() {
    let _lock = AGENT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "v1", "first");
    commit_file(&raw, "a.txt", "v2", "second");

    let mut agent = Agent::spawn(tmp.path());
    let commits: Vec<CommitInfoDto> =
        serde_json::from_value(ok_result(agent.request(Request::ListCommits { limit: 10 })))
            .unwrap();
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0].subject, "second");
    assert_eq!(commits[1].subject, "first");

    agent.shutdown();
}
