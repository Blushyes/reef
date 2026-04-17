//! Integration test for the fs watcher. Spawns the real watcher against a
//! tempdir repo, triggers file events, and verifies the resulting JSON-RPC
//! notifications arrive on the injected `Writer`.

use reef_git::watcher;
use reef_git::writer::Writer;
use reef_protocol::read_message;
use std::io::{self, BufReader, Cursor, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use test_support::{commit_file, tempdir_repo, write_file};

/// In-memory sink shared between the watcher thread and the test thread.
/// Writer's `Box<dyn Write + Send>` consumes it by move, so we wrap the
/// shared buffer in a small adapter that implements `Write`.
#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Poll the shared buffer until at least one full RPC message can be parsed
/// or the timeout expires. Returns the decoded method names.
fn wait_for_notifications(buf: &SharedBuf, timeout: Duration) -> Vec<String> {
    let start = Instant::now();
    loop {
        let snapshot: Vec<u8> = buf.0.lock().unwrap().clone();
        let mut methods = Vec::new();
        let mut reader = BufReader::new(Cursor::new(snapshot));
        while let Ok(msg) = read_message(&mut reader) {
            methods.push(msg.method);
        }
        if !methods.is_empty() {
            return methods;
        }
        if start.elapsed() > timeout {
            return methods;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// On macOS, `TempDir` paths live under `/var/folders/...` which is a symlink
/// to `/private/var/folders/...`. The notify watcher receives events with the
/// canonical path, so the `path.starts_with(workdir)` check in `is_relevant`
/// fails unless we feed it the canonical form.
fn canonical(p: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[test]
fn workdir_write_triggers_status_changed() {
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "existing.txt", "v1", "init");

    let shared = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let writer = Writer::from_writer(shared.clone());

    let workdir = canonical(tmp.path());
    let gitdir = canonical(&workdir.join(".git"));
    watcher::spawn(workdir.clone(), gitdir, writer);

    // Give the notify watcher a moment to register its kernel watch.
    std::thread::sleep(Duration::from_millis(200));
    write_file(&raw, "new.txt", "fresh content");

    // Debounce window inside watcher is 300ms — wait long enough for the
    // notification to be emitted.
    let methods = wait_for_notifications(&shared, Duration::from_secs(3));
    assert!(
        methods.iter().any(|m| m == "reef/statusChanged"),
        "expected statusChanged, got {:?}",
        methods
    );
}

#[test]
fn gitignored_file_does_not_trigger() {
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, ".gitignore", "target/\n", "add gitignore");

    let shared = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let writer = Writer::from_writer(shared.clone());

    let workdir = canonical(tmp.path());
    let gitdir = canonical(&workdir.join(".git"));
    watcher::spawn(workdir.clone(), gitdir, writer);

    std::thread::sleep(Duration::from_millis(200));
    std::fs::create_dir_all(tmp.path().join("target")).unwrap();
    std::fs::write(tmp.path().join("target/build.tmp"), "junk").unwrap();

    // Wait through one full debounce window — we expect NO notifications.
    std::thread::sleep(Duration::from_millis(700));
    let methods = wait_for_notifications(&shared, Duration::from_millis(50));
    assert!(
        methods.is_empty(),
        "gitignored change must not fire notification, got {:?}",
        methods
    );
}
