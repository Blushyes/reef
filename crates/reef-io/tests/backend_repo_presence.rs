use reef_io::{Backend, LocalBackend};
use test_support::tempdir_repo;

#[test]
fn local_repo_presence_refreshes_without_fs_subscription() {
    let (tmp, repo) = tempdir_repo();
    drop(repo);
    let backend = LocalBackend::open_at(tmp.path().to_path_buf());
    let git_dir = tmp.path().join(".git");
    let parked_git_dir = tmp.path().join(".git.parked");

    assert!(backend.has_repo());

    std::fs::rename(&git_dir, &parked_git_dir).unwrap();
    assert!(!backend.has_repo());

    std::fs::rename(&parked_git_dir, &git_dir).unwrap();
    assert!(backend.has_repo());
}
