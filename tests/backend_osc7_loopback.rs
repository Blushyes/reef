//! Integration coverage for the OSC 7 / shell-integration module against
//! the real `$HOME`-based API (not the `install_in` test-only variant).
//!
//! Each test redirects `$HOME` to a tempdir via `test_support::HomeGuard`
//! so we don't scribble in the developer's `~/.reef/sessions`. A
//! process-wide lock serialises the tests — `std::env::set_var` is
//! process-global, so tests that touch it must not overlap.

use std::sync::Mutex;

use reef::shell_integration::{
    Session, Shell, SshInfo, sessions_root, snippet_installed_marker, sweep_stale_sessions,
};
use test_support::HomeGuard;

static HOME_LOCK: Mutex<()> = Mutex::new(());

fn sample_info() -> SshInfo<'static> {
    SshInfo {
        host: "root@test",
        workdir: "/srv/app",
        control_path: "/tmp/reef/cm-test",
    }
}

#[test]
fn session_install_writes_ssh_info_under_home_reef_sessions() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let _home = HomeGuard::enter(tmp.path());

    let result = Session::install(&sample_info()).expect("HOME set → Session::install should try");
    let session = result.expect("install should succeed under writable HOME");

    let expected_root = sessions_root().unwrap();
    assert!(
        session.dir().starts_with(&expected_root),
        "session dir {:?} should live under {:?}",
        session.dir(),
        expected_root
    );
    assert_eq!(
        session.dir().file_name().unwrap().to_str().unwrap(),
        std::process::id().to_string(),
        "session dir name should be the reef pid"
    );

    let info = session.dir().join("ssh-info");
    let content = std::fs::read_to_string(&info).unwrap();
    assert!(content.contains("REEF_HOST=root@test"));
    assert!(content.contains("REEF_WORKDIR=/srv/app"));
    assert!(content.contains("REEF_CONTROL_PATH=/tmp/reef/cm-test"));
}

#[test]
fn session_drop_removes_dir_under_home() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let _home = HomeGuard::enter(tmp.path());

    let info = SshInfo {
        host: "u@h",
        workdir: "/w",
        control_path: "cp",
    };
    let dir_path;
    {
        let session = Session::install(&info).unwrap().unwrap();
        dir_path = session.dir().to_path_buf();
        assert!(dir_path.exists());
    }
    assert!(
        !dir_path.exists(),
        "drop should remove {dir_path:?} under HOME"
    );
}

#[test]
fn sweep_cleans_stale_dirs_under_home() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let _home = HomeGuard::enter(tmp.path());

    let root = sessions_root().unwrap();
    std::fs::create_dir_all(&root).unwrap();

    // Plant: one dir for the current (live) pid, one for a plausibly-dead
    // high pid, one with a non-numeric name (should be ignored by sweep).
    let live = root.join(std::process::id().to_string());
    let stale = root.join("4294967295"); // u32::MAX, almost certainly not alive
    let unrelated = root.join("not-a-pid");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::create_dir_all(&unrelated).unwrap();

    sweep_stale_sessions();

    assert!(live.exists(), "live pid dir must survive");
    assert!(!stale.exists(), "stale pid dir must be removed");
    assert!(unrelated.exists(), "non-numeric dir must be left alone");
}

#[test]
fn marker_absent_by_default_returns_hint_trigger() {
    // Sanity check that `snippet_installed_marker` resolves to a path under
    // the test HOME and isn't pre-populated. The main-binary helper
    // `hint_for_ssh_connect` gates on this exact file — if we ever
    // accidentally touch it during install, the hint would never fire.
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let _home = HomeGuard::enter(tmp.path());

    let marker = snippet_installed_marker().expect("HOME set");
    assert!(
        marker.starts_with(tmp.path()),
        "marker should be under HOME"
    );
    assert!(!marker.exists(), "marker should not pre-exist");

    // Installing a Session must not touch the marker — the marker is
    // touched only by the `reef shell-integration …` CLI subcommand.
    let info = SshInfo {
        host: "u@h",
        workdir: "/w",
        control_path: "cp",
    };
    let s = Session::install(&info).unwrap().unwrap();
    drop(s);
    assert!(
        !marker.exists(),
        "Session install/drop should not create the marker"
    );
}

#[test]
fn snippets_are_nonempty_and_shell_specific() {
    // Sanity: the embedded snippets carry the expected prelude — if
    // `include_str!` silently picked up an empty file (build misconfig),
    // `reef shell-integration zsh >> ~/.zshrc` would be a no-op.
    for shell in Shell::ALL {
        let name = shell.name();
        let body = shell.snippet();
        assert!(body.len() > 100, "{name} snippet suspiciously short");
        assert!(
            body.contains("REEF_SESSION_ACTIVE"),
            "{name} snippet must guard on REEF_SESSION_ACTIVE"
        );
        assert!(
            body.contains("ps -p"),
            "{name} snippet should use ps -p for pid liveness"
        );
        assert!(
            body.contains(".reef/sessions"),
            "{name} snippet must anchor on ~/.reef/sessions path"
        );
    }
}
