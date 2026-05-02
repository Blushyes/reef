//! Backend::editor_launch_spec unit tests.
//!
//! LocalBackend: should resolve $VISUAL/$EDITOR and tack the absolute
//! file path onto the args. RemoteBackend built via `spawn` (no ssh
//! session available) should refuse with Unimplemented.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Mutex;

use reef::backend::{Backend, BackendError, LocalBackend, RemoteBackend};
use tempfile::TempDir;
use test_support::{HOME_LOCK, HomeGuard, agent_bin};

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
fn local_editor_spec_uses_editor_env() {
    // `resolve_editor` now reads the `editor.command` pref before
    // $VISUAL / $EDITOR (so the in-app Settings page can override the
    // shell environment). Redirect $HOME to an empty tempdir so the
    // developer's real `~/.config/reef/prefs` doesn't override the
    // env-var contract this test is asserting. Shares the
    // workspace-wide HOME_LOCK so we serialise against any future
    // test in this binary that touches HOME.
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home_tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(home_tmp.path());
    // SAFETY: single-threaded env mutation protected by HOME_LOCK.
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
    // Same HOME isolation rationale as `local_editor_spec_uses_editor_env`
    // — without it the dev's real `editor.command` pref leaks in. The
    // PathEscape check happens before editor resolution though, so this
    // is mostly defensive.
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home_tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(home_tmp.path());
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
