//! Backend::editor_launch_spec unit tests.
//!
//! LocalBackend: should resolve $VISUAL/$EDITOR and tack the absolute
//! file path onto the args. RemoteBackend built via `spawn` (no ssh
//! session available) should refuse with Unimplemented.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use reef::backend::{Backend, BackendError, LocalBackend, RemoteBackend};
use tempfile::TempDir;

static BACKEND_LOCK: Mutex<()> = Mutex::new(());

fn agent_bin() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_reef-agent") {
        return PathBuf::from(path);
    }
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let root = PathBuf::from(manifest_dir);
    // cargo-llvm-cov sets CARGO_TARGET_DIR to target/llvm-cov-target;
    // check that first so coverage CI finds the binary.
    let target_dirs: Vec<PathBuf> = std::env::var("CARGO_TARGET_DIR")
        .map(|d| vec![PathBuf::from(d)])
        .unwrap_or_default()
        .into_iter()
        .chain([root.join("target")])
        .collect();
    for target in &target_dirs {
        for profile in ["debug", "release"] {
            let candidate = target.join(profile).join("reef-agent");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    panic!("reef-agent binary not found under target/{{debug,release}}");
}

fn spawn_remote(workdir: &Path) -> RemoteBackend {
    let argv = vec![
        agent_bin().display().to_string(),
        "--stdio".to_string(),
        "--workdir".to_string(),
        workdir.display().to_string(),
    ];
    RemoteBackend::spawn(&argv).expect("spawn remote")
}

/// Env-var reads are process-wide, so this test serialises against
/// anything else that mutates $VISUAL/$EDITOR. The existing loopback
/// tests in the suite don't touch those, so a dedicated lock here is
/// sufficient.
static EDITOR_ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn local_editor_spec_uses_editor_env() {
    let _lock = EDITOR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: single-threaded env mutation protected by EDITOR_ENV_LOCK.
    // Other tests in this binary can't race because cargo runs test cases
    // within one binary sequentially by default (plus the mutex).
    unsafe {
        std::env::set_var("VISUAL", "");
        std::env::set_var("EDITOR", "true");
    }

    let tmp = TempDir::new().unwrap();
    let b = LocalBackend::open_at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("a.txt"), "").unwrap();
    let spec = b.editor_launch_spec(Path::new("a.txt")).expect("spec");
    assert_eq!(spec.program, OsString::from("true"));
    // args ends with the absolute file path.
    let last = spec.args.last().expect("path arg");
    let last = last.to_string_lossy().into_owned();
    assert!(last.ends_with("a.txt"), "got {last}");
    assert!(spec.inherit_tty);

    unsafe {
        std::env::remove_var("VISUAL");
        std::env::remove_var("EDITOR");
    }
}

#[test]
fn remote_spawn_variant_refuses_editor_spec() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let r = spawn_remote(tmp.path());
    // `spawn` (as opposed to `connect_ssh`) doesn't carry an SshSession,
    // so editor_launch_spec must refuse rather than fabricate an ssh
    // command out of thin air.
    let err = r.editor_launch_spec(Path::new("x.txt")).unwrap_err();
    assert!(
        matches!(err, BackendError::Unimplemented(_)),
        "expected Unimplemented, got {err:?}"
    );
}

#[test]
fn local_editor_spec_rejects_path_escape() {
    let _lock = EDITOR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::set_var("EDITOR", "true");
    }
    let tmp = TempDir::new().unwrap();
    let b = LocalBackend::open_at(tmp.path().to_path_buf());
    let err = b
        .editor_launch_spec(Path::new("../escape.txt"))
        .unwrap_err();
    assert!(
        matches!(err, BackendError::PathEscape(_)),
        "expected PathEscape, got {err:?}"
    );
    unsafe {
        std::env::remove_var("EDITOR");
    }
}
