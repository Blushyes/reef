//! Read-path symlink-escape guard. Regression for #31.
//!
//! Without `canonical_child_within`, a workdir containing
//! `link → /outside/...` would let `read_file` / `Request::ReadFile` /
//! `load_preview` exfiltrate any file the backend user can read — the
//! concrete threat is a malicious workdir opened in remote/agent mode,
//! where the SSH user is trusted to the host filesystem but the
//! caller is not. All read entry points must reject it.

#![cfg(unix)]

use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::Mutex;

use reef_io::{Backend, BackendError, LocalBackend, RemoteBackend};
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

/// Build a workdir with `link → <secret_dir>/secret.txt`. Both tempdirs
/// are returned so they stay alive for the test's duration; the secret
/// dir is intentionally unrelated to the workdir root.
fn with_escape_layout() -> (TempDir, TempDir) {
    let secret_dir = TempDir::new().expect("secret tempdir");
    let secret = secret_dir.path().join("secret.txt");
    std::fs::write(&secret, b"TOPSECRET").unwrap();

    let workdir = TempDir::new().expect("workdir tempdir");
    symlink(&secret, workdir.path().join("link")).unwrap();
    (workdir, secret_dir)
}

fn with_directory_escape_layout() -> (TempDir, TempDir) {
    let outside = TempDir::new().expect("outside tempdir");
    let workdir = TempDir::new().expect("workdir tempdir");
    symlink(outside.path(), workdir.path().join("link")).unwrap();
    (workdir, outside)
}

#[test]
fn local_read_file_rejects_symlink_escape() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (workdir, _secret_dir) = with_escape_layout();
    let b = LocalBackend::open_at(workdir.path().to_path_buf());

    let err = b.read_file(Path::new("link"), 1024).unwrap_err();
    assert!(
        matches!(err, BackendError::PathEscape(_)),
        "expected PathEscape, got {err:?}"
    );
}

#[test]
fn local_read_file_allows_internal_symlink() {
    // A symlink that stays inside the workdir (`alias → target.txt`) is
    // legitimate and must still resolve — the guard only rejects
    // *escapes*, not all symlinks.
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workdir = TempDir::new().unwrap();
    std::fs::write(workdir.path().join("target.txt"), b"inside").unwrap();
    symlink("target.txt", workdir.path().join("alias")).unwrap();

    let b = LocalBackend::open_at(workdir.path().to_path_buf());
    let bytes = b.read_file(Path::new("alias"), 1024).unwrap();
    assert_eq!(bytes, b"inside");
}

#[test]
fn local_load_preview_refuses_symlink_escape() {
    // `load_preview` returns Option — a symlink escape degrades to
    // "not previewable" rather than surfacing an error to the UI.
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (workdir, _secret_dir) = with_escape_layout();
    let b = LocalBackend::open_at(workdir.path().to_path_buf());

    assert!(b.load_preview(Path::new("link"), false).is_none());
}

#[test]
fn trusted_local_preview_follows_external_video_symlink() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let outside = TempDir::new().expect("outside tempdir");
    let video = outside.path().join("clip.mp4");
    std::fs::write(&video, b"\x00\x00\x00\x18ftypmp42").unwrap();
    let workdir = TempDir::new().expect("workdir tempdir");
    symlink(&video, workdir.path().join("clip.mp4")).unwrap();
    let video = std::fs::canonicalize(video).unwrap();

    let backend = LocalBackend::open_at_with_external_previews(workdir.path().to_path_buf());
    let preview = backend
        .load_preview(Path::new("clip.mp4"), false)
        .expect("external video symlink preview");

    assert_eq!(
        (preview.mime.as_deref(), preview.local_path.as_deref()),
        (Some("video/mp4"), Some(video.as_path()))
    );
}

#[test]
fn remote_read_file_rejects_symlink_escape() {
    // Same guarantee, but over the RPC boundary — an escape on the
    // agent side must surface as `PathEscape` at the RemoteBackend,
    // not silently return the symlink target's bytes.
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (workdir, _secret_dir) = with_escape_layout();
    let r = spawn_remote(workdir.path());

    let err = r.read_file(Path::new("link"), 1024).unwrap_err();
    assert!(
        matches!(err, BackendError::PathEscape(_)),
        "expected PathEscape over RPC, got {err:?}"
    );
}

#[test]
fn local_create_file_rejects_symlinked_parent_escape() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (workdir, outside) = with_directory_escape_layout();
    let backend = LocalBackend::open_at(workdir.path().to_path_buf());

    let error = backend
        .create_file(Path::new("link/created.txt"))
        .unwrap_err();

    assert!(
        matches!(error, BackendError::PathEscape(_))
            && !outside.path().join("created.txt").exists(),
        "expected PathEscape without an outside write, got {error:?}"
    );
}

#[test]
fn remote_create_file_rejects_symlinked_parent_escape() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (workdir, outside) = with_directory_escape_layout();
    let backend = spawn_remote(workdir.path());

    let error = backend
        .create_file(Path::new("link/created.txt"))
        .unwrap_err();

    assert!(
        matches!(error, BackendError::PathEscape(_))
            && !outside.path().join("created.txt").exists(),
        "expected PathEscape over RPC without an outside write, got {error:?}"
    );
}

#[test]
fn local_create_dir_all_rejects_symlinked_ancestor_escape() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (workdir, outside) = with_directory_escape_layout();
    let backend = LocalBackend::open_at(workdir.path().to_path_buf());

    let error = backend
        .create_dir_all(Path::new("link/nested/dir"))
        .unwrap_err();

    assert!(
        matches!(error, BackendError::PathEscape(_)) && !outside.path().join("nested").exists(),
        "expected PathEscape without an outside directory, got {error:?}"
    );
}

#[test]
fn remote_create_dir_all_rejects_symlinked_ancestor_escape() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (workdir, outside) = with_directory_escape_layout();
    let backend = spawn_remote(workdir.path());

    let error = backend
        .create_dir_all(Path::new("link/nested/dir"))
        .unwrap_err();

    assert!(
        matches!(error, BackendError::PathEscape(_)) && !outside.path().join("nested").exists(),
        "expected PathEscape over RPC without an outside directory, got {error:?}"
    );
}

#[test]
fn remote_copy_file_rejects_symlinked_destination_escape() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (workdir, outside) = with_directory_escape_layout();
    std::fs::write(workdir.path().join("source.txt"), "inside").unwrap();
    std::fs::write(outside.path().join("target.txt"), "outside").unwrap();
    symlink(
        outside.path().join("target.txt"),
        workdir.path().join("target.txt"),
    )
    .unwrap();
    let backend = spawn_remote(workdir.path());

    let error = backend
        .copy_file(Path::new("source.txt"), Path::new("target.txt"))
        .unwrap_err();
    let outside_content =
        std::fs::read_to_string(outside.path().join("target.txt")).expect("read outside target");

    assert!(
        matches!(error, BackendError::PathEscape(_)) && outside_content == "outside",
        "expected PathEscape without overwriting the outside file, got {error:?}"
    );
}
