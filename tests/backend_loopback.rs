//! Loopback parity test for RemoteBackend.
//!
//! Strategy: spawn `reef-agent --stdio --workdir <tempdir>` in-process,
//! drive it through `RemoteBackend`, and verify the results line up with
//! what `LocalBackend` returns on the exact same tempdir. This is the M1
//! smoke test — a full contract suite belongs in a later milestone, but
//! even this catches the biggest classes of regressions (protocol drift,
//! DTO mis-mapping, path normalization mismatches).

use std::sync::Mutex;

use reef::backend::{Backend, LocalBackend, RemoteBackend};
use test_support::{agent_bin, commit_file, tempdir_repo, write_file};

static BACKEND_LOCK: Mutex<()> = Mutex::new(());

fn spawn_remote(workdir: &std::path::Path) -> RemoteBackend {
    let argv = vec![
        agent_bin().display().to_string(),
        "--stdio".to_string(),
        "--workdir".to_string(),
        workdir.display().to_string(),
    ];
    RemoteBackend::spawn(&argv).expect("spawn remote backend")
}

#[test]
fn remote_git_status_matches_local() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "tracked.txt", "v1\n", "init");
    write_file(&raw, "tracked.txt", "v2\n");
    write_file(&raw, "new.txt", "new\n");

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    let l = local.git_status().expect("local status");
    let r = remote.git_status().expect("remote status");

    assert_eq!(l.branch_name, r.branch_name);
    assert_eq!(l.ahead_behind, r.ahead_behind);
    assert_eq!(l.staged.len(), r.staged.len());
    assert_eq!(l.unstaged.len(), r.unstaged.len());
    let l_unstaged: Vec<_> = l.unstaged.iter().map(|e| (&e.path, e.status)).collect();
    let r_unstaged: Vec<_> = r.unstaged.iter().map(|e| (&e.path, e.status)).collect();
    assert_eq!(l_unstaged, r_unstaged);
}

#[test]
fn remote_staged_diff_matches_local() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "one\n", "init");
    write_file(&raw, "a.txt", "two\n");
    // stage through git2 so both backends see the same starting point
    let mut idx = raw.index().unwrap();
    idx.add_path(std::path::Path::new("a.txt")).unwrap();
    idx.write().unwrap();

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    let l = local.staged_diff("a.txt", 3).expect("local diff");
    let r = remote.staged_diff("a.txt", 3).expect("remote diff");

    assert_eq!(l.is_some(), r.is_some());
    let (l, r) = (l.unwrap(), r.unwrap());
    assert_eq!(l.file_path, r.file_path);
    assert_eq!(l.hunks.len(), r.hunks.len());
    let lines_l: Vec<_> = l
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter().map(|ln| (ln.tag, ln.content.clone())))
        .collect();
    let lines_r: Vec<_> = r
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter().map(|ln| (ln.tag, ln.content.clone())))
        .collect();
    assert_eq!(lines_l, lines_r);
}

#[test]
fn remote_list_commits_matches_local() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "v1", "first");
    commit_file(&raw, "a.txt", "v2", "second");
    commit_file(&raw, "a.txt", "v3", "third");

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    let l = local.list_commits(10).expect("local commits");
    let r = remote.list_commits(10).expect("remote commits");

    assert_eq!(l.len(), r.len());
    for (a, b) in l.iter().zip(r.iter()) {
        assert_eq!(a.oid, b.oid);
        assert_eq!(a.subject, b.subject);
    }
}

#[test]
fn remote_read_file_respects_cap() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "big.txt", "abcdefghij", "init");

    let remote = spawn_remote(tmp.path());
    let bytes = remote
        .read_file(std::path::Path::new("big.txt"), 4)
        .expect("remote read_file");
    assert_eq!(bytes, b"abcd");
}
