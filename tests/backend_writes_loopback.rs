//! Local vs Remote parity — write operations.
//!
//! Each test drives the same sequence of mutations through two backends on
//! two parallel tempdir trees, then diffs the resulting filesystem state.
//! The spawn/shutdown cost is paid once per test; the in-test work is
//! bounded so the whole suite stays under a handful of seconds.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use reef::backend::{Backend, LocalBackend, RemoteBackend};
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

/// Snapshot a workdir tree into `{rel_path: Option<bytes>}`. `None` marks
/// a directory (we don't hash contents, just existence). `.git` is pruned
/// because our backends filter it the same way.
fn snapshot(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if prefix.is_empty() && name == ".git" {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                out.insert(rel.clone(), None);
                walk(&entry.path(), &rel, out);
            } else if ft.is_file() {
                let bytes = std::fs::read(entry.path()).unwrap_or_default();
                out.insert(rel, Some(bytes));
            }
        }
    }
    let mut map = BTreeMap::new();
    walk(root, "", &mut map);
    map
}

#[test]
fn create_file_parity() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let l_tmp = TempDir::new().unwrap();
    let r_tmp = TempDir::new().unwrap();
    let l = LocalBackend::open_at(l_tmp.path().to_path_buf());
    let r = spawn_remote(r_tmp.path());

    l.create_file(Path::new("alpha.txt")).unwrap();
    r.create_file(Path::new("alpha.txt")).unwrap();

    // Second call must fail identically (PathExists).
    assert!(l.create_file(Path::new("alpha.txt")).is_err());
    assert!(r.create_file(Path::new("alpha.txt")).is_err());

    assert_eq!(snapshot(l_tmp.path()), snapshot(r_tmp.path()));
}

#[test]
fn create_dir_and_rename_parity() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let l_tmp = TempDir::new().unwrap();
    let r_tmp = TempDir::new().unwrap();
    let l = LocalBackend::open_at(l_tmp.path().to_path_buf());
    let r = spawn_remote(r_tmp.path());

    l.create_dir_all(Path::new("nested/dir")).unwrap();
    r.create_dir_all(Path::new("nested/dir")).unwrap();
    l.create_file(Path::new("nested/dir/file.txt")).unwrap();
    r.create_file(Path::new("nested/dir/file.txt")).unwrap();

    l.rename(
        Path::new("nested/dir/file.txt"),
        Path::new("nested/dir/renamed.txt"),
    )
    .unwrap();
    r.rename(
        Path::new("nested/dir/file.txt"),
        Path::new("nested/dir/renamed.txt"),
    )
    .unwrap();

    assert_eq!(snapshot(l_tmp.path()), snapshot(r_tmp.path()));
}

#[test]
fn copy_and_delete_parity() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let l_tmp = TempDir::new().unwrap();
    let r_tmp = TempDir::new().unwrap();
    std::fs::write(l_tmp.path().join("src.txt"), b"hello").unwrap();
    std::fs::write(r_tmp.path().join("src.txt"), b"hello").unwrap();
    std::fs::create_dir(l_tmp.path().join("pkg")).unwrap();
    std::fs::create_dir(r_tmp.path().join("pkg")).unwrap();
    std::fs::write(l_tmp.path().join("pkg/inner.txt"), b"deep").unwrap();
    std::fs::write(r_tmp.path().join("pkg/inner.txt"), b"deep").unwrap();

    let l = LocalBackend::open_at(l_tmp.path().to_path_buf());
    let r = spawn_remote(r_tmp.path());

    l.copy_file(Path::new("src.txt"), Path::new("dst.txt"))
        .unwrap();
    r.copy_file(Path::new("src.txt"), Path::new("dst.txt"))
        .unwrap();

    l.copy_dir_recursive(Path::new("pkg"), Path::new("pkg-copy"))
        .unwrap();
    r.copy_dir_recursive(Path::new("pkg"), Path::new("pkg-copy"))
        .unwrap();

    l.remove_file(Path::new("dst.txt")).unwrap();
    r.remove_file(Path::new("dst.txt")).unwrap();

    l.remove_dir_all(Path::new("pkg-copy")).unwrap();
    r.remove_dir_all(Path::new("pkg-copy")).unwrap();

    assert_eq!(snapshot(l_tmp.path()), snapshot(r_tmp.path()));
}

#[test]
fn path_escape_is_rejected_by_both_backends() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let l_tmp = TempDir::new().unwrap();
    let r_tmp = TempDir::new().unwrap();
    let l = LocalBackend::open_at(l_tmp.path().to_path_buf());
    let r = spawn_remote(r_tmp.path());

    let escaped = Path::new("../escape.txt");
    assert!(l.create_file(escaped).is_err());
    assert!(r.create_file(escaped).is_err());
}

#[test]
fn pending_map_does_not_leak_on_repeated_requests() {
    // Locks down that `MapGuard::Drop` is a no-op after the read loop
    // already removed via Response receipt — i.e. doesn't double-free
    // or grow the map on the success path.
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let r_tmp = TempDir::new().unwrap();
    let r = spawn_remote(r_tmp.path());

    for _ in 0..10 {
        // `head_oid` round-trips a real RPC; `workdir_name` would
        // hit the client-side cache and skip the pending machinery.
        let _ = r.head_oid();
    }
    assert_eq!(r.__pending_len_for_tests(), 0);
}

#[test]
fn pending_map_cleaned_on_rpc_failure() {
    // The whole point of `PendingGuard` is the *failure* path: if the
    // read loop never delivers a Response (agent died, socket dropped,
    // RPC timed out), the guard's `Drop` has to remove the in-flight
    // slot — otherwise the map grows unbounded across transient SSH
    // blips. The success-path test above wouldn't catch a regression
    // where the guard only fires on Ok(Response).
    //
    // Setup: spawn a real agent, complete the handshake, kill it. The
    // stdin pipe closes, so `send_envelope` may or may not fail
    // depending on buffering — either way we get an Err. If the write
    // buffers, `recv_timeout(100ms)` fires and PendingGuard::Drop
    // cleans up. We assert both: Err surfaces, and `pending_len == 0`.
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let r_tmp = TempDir::new().unwrap();
    let r = spawn_remote(r_tmp.path());

    r.__kill_agent_for_tests();

    let res: Result<reef_proto::HandshakeResponse, _> = r
        .__request_with_timeout_for_tests(
            reef_proto::Request::Handshake,
            std::time::Duration::from_millis(100),
        );
    assert!(res.is_err(), "expected failure after agent kill, got Ok");
    assert_eq!(
        r.__pending_len_for_tests(),
        0,
        "pending map must be empty after failed RPC",
    );
}

#[test]
fn read_file_rejects_path_escape() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let l_tmp = TempDir::new().unwrap();
    let r_tmp = TempDir::new().unwrap();
    // Plant a real file outside the workdir so a successful boundary
    // breach would actually return bytes — without this, `is_err` could
    // mean either "rejected" or "no such file".
    let outside = l_tmp.path().parent().unwrap().join("outside.txt");
    std::fs::write(&outside, b"secret").unwrap();

    let l = LocalBackend::open_at(l_tmp.path().to_path_buf());
    let r = spawn_remote(r_tmp.path());

    let escaped = Path::new("../outside.txt");
    assert!(l.read_file(escaped, 1024).is_err());
    assert!(r.read_file(escaped, 1024).is_err());

    let _ = std::fs::remove_file(&outside);
}

#[test]
fn hard_delete_parity() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let l_tmp = TempDir::new().unwrap();
    let r_tmp = TempDir::new().unwrap();
    std::fs::write(l_tmp.path().join("a.txt"), b"").unwrap();
    std::fs::write(r_tmp.path().join("a.txt"), b"").unwrap();
    std::fs::create_dir(l_tmp.path().join("d")).unwrap();
    std::fs::create_dir(r_tmp.path().join("d")).unwrap();
    std::fs::write(l_tmp.path().join("d/nested.txt"), b"").unwrap();
    std::fs::write(r_tmp.path().join("d/nested.txt"), b"").unwrap();

    let l = LocalBackend::open_at(l_tmp.path().to_path_buf());
    let r = spawn_remote(r_tmp.path());

    let paths = vec![PathBuf::from("a.txt"), PathBuf::from("d")];
    l.hard_delete(&paths).unwrap();
    r.hard_delete(&paths).unwrap();

    assert_eq!(snapshot(l_tmp.path()), snapshot(r_tmp.path()));
}
