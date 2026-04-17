//! End-to-end tests for `GitRepo` exercising real `git2::Repository` instances
//! in `TempDir` workdirs. Exercises the entire public API surface.

use reef_git::git::{FileStatus, GitRepo, LineTag, RefLabel};
use std::fs;
use test_support::{commit_file, tempdir_repo, write_file};

/// `GitRepo::open()` uses `Repository::discover()` from cwd. Helper switches
/// cwd to a temp repo, opens, then restores cwd so tests don't bleed.
struct CwdGuard {
    original: std::path::PathBuf,
}

impl CwdGuard {
    fn enter(path: &std::path::Path) -> Self {
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self { original }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

// All tests that change cwd must run serially — std::env::set_current_dir is
// process-global. Guard via a mutex.
use std::sync::Mutex;
static CWD_LOCK: Mutex<()> = Mutex::new(());

fn open_in(path: &std::path::Path) -> (CwdGuard, GitRepo) {
    let guard = CwdGuard::enter(path);
    let repo = GitRepo::open().expect("GitRepo::open succeeds in real repo");
    (guard, repo)
}

#[test]
fn open_succeeds_on_real_repo() {
    let _lock = CWD_LOCK.lock().unwrap();
    let (tmp, _raw) = tempdir_repo();
    let (_g, repo) = open_in(tmp.path());
    assert!(repo.workdir().is_some());
}

#[test]
fn branch_name_reports_initial_branch() {
    let _lock = CWD_LOCK.lock().unwrap();
    let (tmp, raw) = tempdir_repo();
    // Before any commit, HEAD points to an unborn branch — be tolerant.
    commit_file(&raw, "a.txt", "hello", "init");
    let (_g, repo) = open_in(tmp.path());
    let name = repo.branch_name();
    assert!(
        name == "master" || name == "main",
        "expected master/main, got {:?}",
        name
    );
}

#[test]
fn workdir_name_extracts_dirname() {
    let _lock = CWD_LOCK.lock().unwrap();
    let (tmp, _raw) = tempdir_repo();
    let (_g, repo) = open_in(tmp.path());
    let name = repo.workdir_name();
    assert!(!name.is_empty());
}

#[test]
fn get_status_detects_untracked_file() {
    let _lock = CWD_LOCK.lock().unwrap();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "tracked.txt", "content", "init");
    write_file(&raw, "new.txt", "new content");

    let (_g, repo) = open_in(tmp.path());
    let (staged, unstaged) = repo.get_status();
    assert!(staged.is_empty());
    assert_eq!(unstaged.len(), 1);
    assert_eq!(unstaged[0].path, "new.txt");
    assert_eq!(unstaged[0].status, FileStatus::Untracked);
}

#[test]
fn get_status_detects_modified_file() {
    let _lock = CWD_LOCK.lock().unwrap();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "v1", "init");
    write_file(&raw, "a.txt", "v2"); // modify

    let (_g, repo) = open_in(tmp.path());
    let (staged, unstaged) = repo.get_status();
    assert!(staged.is_empty());
    assert_eq!(unstaged.len(), 1);
    assert_eq!(unstaged[0].status, FileStatus::Modified);
}

#[test]
fn stage_file_moves_from_unstaged_to_staged() {
    let _lock = CWD_LOCK.lock().unwrap();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "v1", "init");
    write_file(&raw, "a.txt", "v2");

    let (_g, repo) = open_in(tmp.path());
    repo.stage_file("a.txt").expect("stage succeeds");
    let (staged, unstaged) = repo.get_status();
    assert_eq!(staged.len(), 1);
    assert!(unstaged.is_empty());
}

#[test]
fn stage_then_unstage_roundtrip() {
    let _lock = CWD_LOCK.lock().unwrap();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "v1", "init");
    write_file(&raw, "a.txt", "v2");

    let (_g, repo) = open_in(tmp.path());
    repo.stage_file("a.txt").unwrap();
    repo.unstage_file("a.txt").unwrap();
    let (staged, unstaged) = repo.get_status();
    assert!(staged.is_empty());
    assert_eq!(unstaged.len(), 1);
}

#[test]
fn restore_file_reverts_workdir_to_head() {
    let _lock = CWD_LOCK.lock().unwrap();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "v1", "init");
    write_file(&raw, "a.txt", "v2");

    let (_g, repo) = open_in(tmp.path());
    repo.restore_file("a.txt").unwrap();
    let content = fs::read_to_string(tmp.path().join("a.txt")).unwrap();
    assert_eq!(content, "v1");
    let (staged, unstaged) = repo.get_status();
    assert!(staged.is_empty());
    assert!(unstaged.is_empty());
}

#[test]
fn get_diff_unstaged_shows_additions() {
    let _lock = CWD_LOCK.lock().unwrap();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "line1\n", "init");
    write_file(&raw, "a.txt", "line1\nline2\n");

    let (_g, repo) = open_in(tmp.path());
    let diff = repo.get_diff("a.txt", false, 3).expect("diff available");
    assert_eq!(diff.file_path, "a.txt");
    let added: Vec<&str> = diff
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter(|l| l.tag == LineTag::Added)
        .map(|l| l.content.as_str())
        .collect();
    assert!(added.iter().any(|c| c.contains("line2")));
}

#[test]
fn get_diff_staged_compares_index_to_head() {
    let _lock = CWD_LOCK.lock().unwrap();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "v1\n", "init");
    write_file(&raw, "a.txt", "v2\n");

    let (_g, repo) = open_in(tmp.path());
    repo.stage_file("a.txt").unwrap();
    let diff = repo.get_diff("a.txt", true, 3).expect("staged diff");
    let has_removed = diff
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .any(|l| l.tag == LineTag::Removed);
    let has_added = diff
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .any(|l| l.tag == LineTag::Added);
    assert!(has_removed && has_added);
}

#[test]
fn head_oid_matches_latest_commit() {
    let _lock = CWD_LOCK.lock().unwrap();
    let (tmp, raw) = tempdir_repo();
    let oid = commit_file(&raw, "a.txt", "v1", "init");

    let (_g, repo) = open_in(tmp.path());
    assert_eq!(repo.head_oid().as_deref(), Some(oid.to_string().as_str()));
}

#[test]
fn list_commits_returns_topological_order() {
    let _lock = CWD_LOCK.lock().unwrap();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "v1", "first");
    commit_file(&raw, "a.txt", "v2", "second");
    commit_file(&raw, "a.txt", "v3", "third");

    let (_g, repo) = open_in(tmp.path());
    let commits = repo.list_commits(10);
    assert_eq!(commits.len(), 3);
    assert_eq!(commits[0].subject, "third");
    assert_eq!(commits[2].subject, "first");
}

#[test]
fn list_refs_contains_head() {
    let _lock = CWD_LOCK.lock().unwrap();
    let (tmp, raw) = tempdir_repo();
    let oid = commit_file(&raw, "a.txt", "v1", "init");

    let (_g, repo) = open_in(tmp.path());
    let refs = repo.list_refs();
    let labels = refs.get(&oid.to_string()).expect("HEAD commit has refs");
    assert!(labels.iter().any(|l| matches!(l, RefLabel::Head)));
}

#[test]
fn get_commit_returns_metadata_and_files() {
    let _lock = CWD_LOCK.lock().unwrap();
    let (tmp, raw) = tempdir_repo();
    let oid = commit_file(&raw, "a.txt", "v1", "first");

    let (_g, repo) = open_in(tmp.path());
    let detail = repo
        .get_commit(&oid.to_string())
        .expect("lookup commit by oid");
    assert_eq!(detail.info.subject, "first");
    assert_eq!(detail.info.author_name, "Tester");
    assert_eq!(detail.files.len(), 1);
    assert_eq!(detail.files[0].path, "a.txt");
}

#[test]
fn get_commit_file_diff_shows_additions() {
    let _lock = CWD_LOCK.lock().unwrap();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "v1\n", "first");
    let oid = commit_file(&raw, "a.txt", "v1\nv2\n", "second");

    let (_g, repo) = open_in(tmp.path());
    let diff = repo
        .get_commit_file_diff(&oid.to_string(), "a.txt", 3)
        .expect("diff available");
    let added = diff
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .any(|l| l.tag == LineTag::Added && l.content.contains("v2"));
    assert!(added);
}
