//! Track C — `backend.upload_from_local` parity.
//!
//! LocalBackend is exercised as a correctness baseline: external drag-drop
//! from an out-of-tree path into the workdir must land byte-identical
//! content. RemoteBackend (via the loopback agent, no real ssh) can't be
//! scp'd against, so we assert the `Unimplemented` fallback shape — the
//! production upload path goes through `connect_ssh`'s `ssh_launch`,
//! which `spawn()` leaves as `None`.

use std::path::Path;
use std::sync::Mutex;

use reef_io::{Backend, BackendError, LocalBackend, RemoteBackend, copy_local_sources_to_backend};
use tempfile::TempDir;
use test_support::agent_bin;

static BACKEND_LOCK: Mutex<()> = Mutex::new(());

fn spawn_remote(workdir: &Path) -> RemoteBackend {
    let argv = vec![
        agent_bin().display().to_string(),
        "--stdio".to_string(),
        "--workdir".to_string(),
        workdir.display().to_string(),
    ];
    RemoteBackend::spawn(&argv).expect("spawn remote")
}

#[test]
fn local_upload_file_copies_bytes() {
    let workdir = TempDir::new().unwrap();
    let src_tmp = TempDir::new().unwrap();
    let src = src_tmp.path().join("payload.txt");
    std::fs::write(&src, b"hello upload").unwrap();

    let b = LocalBackend::open_at(workdir.path().to_path_buf());
    b.upload_from_local(&src, Path::new("payload.txt"))
        .expect("local upload");
    assert_eq!(
        std::fs::read(workdir.path().join("payload.txt")).unwrap(),
        b"hello upload"
    );
}

#[test]
fn local_upload_dir_copies_recursively() {
    let workdir = TempDir::new().unwrap();
    let src_tmp = TempDir::new().unwrap();
    let src = src_tmp.path().join("pkg");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("one.txt"), "1").unwrap();
    std::fs::create_dir(src.join("nested")).unwrap();
    std::fs::write(src.join("nested/two.txt"), "2").unwrap();

    let b = LocalBackend::open_at(workdir.path().to_path_buf());
    b.upload_from_local(&src, Path::new("pkg"))
        .expect("upload dir");

    assert_eq!(
        std::fs::read_to_string(workdir.path().join("pkg/one.txt")).unwrap(),
        "1"
    );
    assert_eq!(
        std::fs::read_to_string(workdir.path().join("pkg/nested/two.txt")).unwrap(),
        "2"
    );
}

#[test]
fn local_upload_rejects_path_escape() {
    let workdir = TempDir::new().unwrap();
    let src_tmp = TempDir::new().unwrap();
    let src = src_tmp.path().join("x.txt");
    std::fs::write(&src, "").unwrap();
    let b = LocalBackend::open_at(workdir.path().to_path_buf());
    let err = b
        .upload_from_local(&src, Path::new("../escape.txt"))
        .unwrap_err();
    assert!(matches!(err, BackendError::PathEscape(_)), "got {err:?}");
}

#[test]
fn remote_spawn_variant_refuses_upload() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let r = spawn_remote(tmp.path());
    let src_tmp = TempDir::new().unwrap();
    let src = src_tmp.path().join("x.txt");
    std::fs::write(&src, "").unwrap();
    // `spawn()` (i.e. `--agent-exec` path) carries no `SshSession`, so
    // upload must refuse with Unimplemented rather than fabricate an
    // scp argv.
    let err = r.upload_from_local(&src, Path::new("x.txt")).unwrap_err();
    assert!(
        matches!(err, BackendError::Unimplemented(_)),
        "expected Unimplemented for --agent-exec remote, got {err:?}"
    );
}

#[test]
fn remote_copy_files_does_not_treat_matching_local_path_as_remote_source() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workdir = TempDir::new().unwrap();
    let destination = workdir.path().join("destination");
    std::fs::create_dir(&destination).unwrap();
    let source = workdir.path().join("source.txt");
    std::fs::write(&source, "host-local content").unwrap();
    let remote = spawn_remote(workdir.path());

    let error = copy_local_sources_to_backend(&remote, &[source], &destination).unwrap_err();

    assert!(
        error.contains("remote upload requires an ssh session"),
        "expected the host-local upload path, got: {error}"
    );
    assert!(!destination.join("source.txt").exists());
}
