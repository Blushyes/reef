//! Integration tests for the host-owned fs watcher. Drives `fs_watcher::spawn`
//! against a real tempdir and asserts the debounced channel contract.

use reef_io::fs_watcher;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use test_support::{commit_file, tempdir_repo, write_file};

/// macOS tempdirs live under `/var/folders/...` which symlinks to
/// `/private/var/folders/...`. notify delivers canonical paths, so prefix
/// checks in the watcher would fail without canonicalizing here too.
fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Wait until the notify backend has actually delivered at least one event.
/// macOS FSEvents can take longer than a fixed warmup to register a recursive
/// watch, especially under CI-like load; repeatedly touching a harmless marker
/// turns that registration race into a real readiness handshake.
fn wait_until_ready(workdir: &Path, rx: &mpsc::Receiver<()>) {
    let marker = workdir.join(".reef-watch-ready");
    let start = Instant::now();
    let mut attempt = 0usize;
    loop {
        attempt += 1;
        std::fs::write(&marker, attempt.to_string()).unwrap();
        match rx.recv_timeout(Duration::from_millis(700)) {
            Ok(()) => break,
            Err(mpsc::RecvTimeoutError::Timeout) if start.elapsed() < Duration::from_secs(5) => {
                continue;
            }
            Err(e) => panic!("watcher did not become ready: {e:?}"),
        }
    }
    thread::sleep(Duration::from_millis(500));
    while rx.try_recv().is_ok() {}
}

#[test]
fn workdir_write_triggers_event() {
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "existing.txt", "v1", "init");

    let workdir = canonical(tmp.path());
    let rx = fs_watcher::spawn(workdir);

    wait_until_ready(tmp.path(), &rx);
    write_file(&raw, "new.txt", "fresh content");

    let got = rx.recv_timeout(Duration::from_secs(3));
    assert!(
        matches!(got, Ok(())),
        "expected a debounced event within 3s, got {:?}",
        got
    );
}

#[test]
fn gitignored_write_does_not_trigger() {
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, ".gitignore", "target/\n", "add gitignore");

    let workdir = canonical(tmp.path());
    let rx = fs_watcher::spawn(workdir);

    wait_until_ready(tmp.path(), &rx);
    std::fs::create_dir_all(tmp.path().join("target")).unwrap();
    std::fs::write(tmp.path().join("target/build.tmp"), "junk").unwrap();

    // One full debounce window + margin. No event should arrive.
    thread::sleep(Duration::from_millis(700));
    assert_eq!(
        rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty),
        "gitignored write must not emit an event",
    );
}

#[test]
fn dotgit_internal_write_does_not_trigger() {
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "keep.txt", "v1", "init");

    let workdir = canonical(tmp.path());
    let rx = fs_watcher::spawn(workdir);

    wait_until_ready(tmp.path(), &rx);
    // Simulate a git-internal write. .git/ must be skipped outright so that
    // repeated index churn during git operations never wakes the host.
    std::fs::write(tmp.path().join(".git/custom-marker"), "x").unwrap();

    thread::sleep(Duration::from_millis(700));
    assert_eq!(
        rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty),
        ".git/ write must not emit an event",
    );
}

#[test]
fn non_git_dir_still_triggers() {
    let tmp = TempDir::new().expect("tempdir");
    let workdir = canonical(tmp.path());
    let rx = fs_watcher::spawn(workdir);

    wait_until_ready(tmp.path(), &rx);
    std::fs::write(tmp.path().join("hello.txt"), "hi").unwrap();

    let got = rx.recv_timeout(Duration::from_secs(3));
    assert!(
        matches!(got, Ok(())),
        "non-git workdir should still receive events, got {:?}",
        got
    );
}

#[test]
fn debounce_coalesces_bursts() {
    let (tmp, _raw) = tempdir_repo();

    let workdir = canonical(tmp.path());
    let rx = fs_watcher::spawn(workdir);

    wait_until_ready(tmp.path(), &rx);
    // Fire five writes back-to-back, well inside the 300ms debounce window.
    for i in 0..5 {
        std::fs::write(tmp.path().join(format!("f{i}.txt")), "x").unwrap();
    }

    // First, wait for the debounce to fire at least once.
    let first = rx.recv_timeout(Duration::from_secs(3));
    assert!(
        matches!(first, Ok(())),
        "expected at least one event after burst, got {:?}",
        first
    );

    // Then wait out another window to allow any stragglers to land.
    thread::sleep(Duration::from_millis(500));
    let extra = rx.try_iter().count();

    // A tight burst must collapse — we tolerate at most one extra notification
    // in case the OS splits the burst across the first debounce window.
    assert!(
        extra <= 1,
        "burst should coalesce into 1-2 events total, got 1 + {} extras",
        extra
    );
    drop(tmp);
}
