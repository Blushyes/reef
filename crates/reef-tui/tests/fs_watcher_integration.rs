//! Integration tests for the host-owned fs watcher. Drives `fs_watcher::spawn`
//! against a real tempdir and asserts the debounced channel contract.

use reef_io::{Backend, FsChange, LocalBackend, fs_watcher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use test_support::{commit_file, tempdir_repo, write_file};

static WATCHER_LOCK: Mutex<()> = Mutex::new(());

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
fn wait_until_ready(workdir: &Path, rx: &mpsc::Receiver<FsChange>) {
    let marker = workdir.join(".reef-watch-ready");
    let start = Instant::now();
    let mut attempt = 0usize;
    loop {
        attempt += 1;
        std::fs::write(&marker, attempt.to_string()).unwrap();
        match rx.recv_timeout(Duration::from_millis(700)) {
            Ok(_) => break,
            Err(mpsc::RecvTimeoutError::Timeout) if start.elapsed() < Duration::from_secs(15) => {
                continue;
            }
            Err(e) => panic!("watcher did not become ready: {e:?}"),
        }
    }
    thread::sleep(Duration::from_millis(500));
    while rx.try_recv().is_ok() {}
}

fn recv_repo_presence_change(rx: &mpsc::Receiver<FsChange>) -> Option<FsChange> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(change) if change.repo_presence_changed => return Some(change),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

#[test]
fn workdir_write_triggers_event() {
    let _lock = WATCHER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "existing.txt", "v1", "init");

    let workdir = canonical(tmp.path());
    let rx = fs_watcher::spawn(workdir);

    wait_until_ready(tmp.path(), &rx);
    write_file(&raw, "new.txt", "fresh content");

    let got = rx.recv_timeout(Duration::from_secs(3));
    assert!(
        got.is_ok(),
        "expected a debounced event within 3s, got {:?}",
        got
    );
}

#[test]
fn gitignored_write_does_not_trigger() {
    let _lock = WATCHER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = WATCHER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = WATCHER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().expect("tempdir");
    let workdir = canonical(tmp.path());
    let rx = fs_watcher::spawn(workdir);

    wait_until_ready(tmp.path(), &rx);
    std::fs::write(tmp.path().join("hello.txt"), "hi").unwrap();

    let got = rx.recv_timeout(Duration::from_secs(3));
    assert!(
        got.is_ok(),
        "non-git workdir should still receive events, got {:?}",
        got
    );
}

#[test]
fn debounce_coalesces_bursts() {
    let _lock = WATCHER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        first.is_ok(),
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

#[test]
fn repository_presence_from_nested_workdir_tracks_ancestor_dotgit() {
    let _lock = WATCHER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "src/keep.txt", "v1", "init");

    let nested_workdir = tmp.path().join("src");
    let backend = LocalBackend::open_at(canonical(&nested_workdir));
    assert!(backend.has_repo());
    let rx = backend.subscribe_fs_events();
    wait_until_ready(&nested_workdir, &rx);

    let git_dir = tmp.path().join(".git");
    let parked_git_dir = tmp.path().join(".git.parked");
    std::fs::rename(&git_dir, &parked_git_dir).unwrap();

    let removed = recv_repo_presence_change(&rx);
    assert!(
        matches!(
            removed,
            Some(FsChange {
                repo_presence_changed: true
            })
        ),
        "removing .git should report a repository capability change, got {removed:?}",
    );
    assert!(!backend.has_repo());

    std::fs::rename(&parked_git_dir, &git_dir).unwrap();

    let restored = recv_repo_presence_change(&rx);
    assert!(
        matches!(
            restored,
            Some(FsChange {
                repo_presence_changed: true
            })
        ),
        "restoring .git should report a repository capability change, got {restored:?}",
    );
    assert!(backend.has_repo());
}

#[test]
fn repository_created_in_ancestor_after_watcher_start_updates_presence() {
    let _lock = WATCHER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().expect("tempdir");
    let nested_workdir = tmp.path().join("src");
    std::fs::create_dir(&nested_workdir).unwrap();

    let backend = LocalBackend::open_at(canonical(&nested_workdir));
    assert!(!backend.has_repo());
    let rx = backend.subscribe_fs_events();
    wait_until_ready(&nested_workdir, &rx);

    let (prepared_repo, raw) = tempdir_repo();
    drop(raw);
    std::fs::rename(prepared_repo.path().join(".git"), tmp.path().join(".git")).unwrap();

    let created = recv_repo_presence_change(&rx);
    assert!(
        matches!(
            created,
            Some(FsChange {
                repo_presence_changed: true
            })
        ),
        "creating .git in an ancestor should report a repository capability change, got {created:?}",
    );
    assert!(backend.has_repo());
}
