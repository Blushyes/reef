//! Local vs Remote parity — graph range mode (`range_files` / `range_file_diff`).
//!
//! v0.17.0 (origin/main) added Shift-extended graph selection that asks for
//! the union of files changed across `oldest..=newest` and per-file diffs
//! for that same range. The Backend trait carries `range_files` and
//! `range_file_diff` for this; v5 of reef-proto wires them through the
//! agent. This test pins the Local vs Remote contract so a future protocol
//! drift doesn't silently regress range-mode in SSH sessions.

use std::path::Path;
use std::sync::Mutex;

use reef::backend::{Backend, LocalBackend, RemoteBackend};
use test_support::{agent_bin, commit_file, tempdir_repo, write_file};

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

/// Seed a 3-commit history so range mode has something to span:
///   c1: add a.txt
///   c2: add b.txt
///   c3: modify a.txt
/// `range_files(c1, c3)` should return both `a.txt` and `b.txt`.
fn seed_three_commits(repo: &git2::Repository) -> (String, String, String) {
    let oid1 = commit_file(repo, "a.txt", "v1\n", "c1: add a");
    let oid2 = commit_file(repo, "b.txt", "v1\n", "c2: add b");
    write_file(repo, "a.txt", "v2\n");
    let mut idx = repo.index().unwrap();
    idx.add_path(Path::new("a.txt")).unwrap();
    idx.write().unwrap();
    let tree_oid = idx.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = git2::Signature::now("Tester", "tester@example.com").unwrap();
    let parent = repo
        .find_commit(repo.head().unwrap().target().unwrap())
        .unwrap();
    let oid3 = repo
        .commit(Some("HEAD"), &sig, &sig, "c3: modify a", &tree, &[&parent])
        .unwrap();
    (oid1.to_string(), oid2.to_string(), oid3.to_string())
}

#[test]
fn range_files_parity() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, repo) = tempdir_repo();
    let (c1, _c2, c3) = seed_three_commits(&repo);

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    let mut l = local.range_files(&c1, &c3).expect("local range_files");
    let mut r = remote.range_files(&c1, &c3).expect("remote range_files");
    l.sort_by(|a, b| a.path.cmp(&b.path));
    r.sort_by(|a, b| a.path.cmp(&b.path));

    assert_eq!(l.len(), r.len(), "file count mismatch");
    for (a, b) in l.iter().zip(r.iter()) {
        assert_eq!(a.path, b.path);
        assert_eq!(a.status, b.status, "status mismatch on {}", a.path);
        assert_eq!(a.additions, b.additions, "additions on {}", a.path);
        assert_eq!(a.deletions, b.deletions, "deletions on {}", a.path);
    }
    // Sanity: the range really did include both files.
    let paths: Vec<&str> = l.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&"a.txt"), "a.txt missing: {paths:?}");
    assert!(paths.contains(&"b.txt"), "b.txt missing: {paths:?}");
}

#[test]
fn range_file_diff_parity() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, repo) = tempdir_repo();
    let (c1, _c2, c3) = seed_three_commits(&repo);

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    let l = local
        .range_file_diff(&c1, &c3, "a.txt", 3)
        .expect("local range_file_diff");
    let r = remote
        .range_file_diff(&c1, &c3, "a.txt", 3)
        .expect("remote range_file_diff");

    let l = l.expect("local diff present");
    let r = r.expect("remote diff present");
    assert_eq!(l.file_path, r.file_path);
    assert_eq!(l.hunks.len(), r.hunks.len(), "hunk count mismatch");
    for (lh, rh) in l.hunks.iter().zip(r.hunks.iter()) {
        assert_eq!(lh.header, rh.header);
        assert_eq!(lh.lines.len(), rh.lines.len());
        for (ll, rl) in lh.lines.iter().zip(rh.lines.iter()) {
            assert_eq!(ll.tag, rl.tag);
            assert_eq!(ll.content, rl.content);
            assert_eq!(ll.old_lineno, rl.old_lineno);
            assert_eq!(ll.new_lineno, rl.new_lineno);
        }
    }
}

#[test]
fn range_file_diff_unrelated_path_returns_none() {
    // Asking for a file the range never touched should yield None on
    // both sides, not an error.
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, repo) = tempdir_repo();
    let (c1, _c2, c3) = seed_three_commits(&repo);

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    let l = local
        .range_file_diff(&c1, &c3, "no-such-file.txt", 3)
        .expect("local rpc");
    let r = remote
        .range_file_diff(&c1, &c3, "no-such-file.txt", 3)
        .expect("remote rpc");
    assert!(l.is_none(), "local should be None, got {l:?}");
    assert!(r.is_none(), "remote should be None, got {r:?}");
}
