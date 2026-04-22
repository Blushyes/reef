//! End-to-end tests for `GitRepo` exercising real `git2::Repository` instances
//! in `TempDir` workdirs. Exercises the entire public API surface.

use reef::git::{FileStatus, GitRepo, LineTag, RefLabel};
use std::fs;
use test_support::{CwdGuard, commit_file, tempdir_repo, write_file};

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
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _raw) = tempdir_repo();
    let (_g, repo) = open_in(tmp.path());
    assert!(repo.workdir().is_some());
}

#[test]
fn branch_name_reports_initial_branch() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _raw) = tempdir_repo();
    let (_g, repo) = open_in(tmp.path());
    let name = repo.workdir_name();
    assert!(!name.is_empty());
}

#[test]
fn get_status_detects_untracked_file() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    let oid = commit_file(&raw, "a.txt", "v1", "init");

    let (_g, repo) = open_in(tmp.path());
    assert_eq!(repo.head_oid().as_deref(), Some(oid.to_string().as_str()));
}

#[test]
fn list_commits_returns_topological_order() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    let oid = commit_file(&raw, "a.txt", "v1", "init");

    let (_g, repo) = open_in(tmp.path());
    let refs = repo.list_refs();
    let labels = refs.get(&oid.to_string()).expect("HEAD commit has refs");
    assert!(labels.iter().any(|l| matches!(l, RefLabel::Head)));
}

#[test]
fn get_commit_returns_metadata_and_files() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

// ── Remote sync state (ahead_behind / push) ─────────────────────────────────

/// Seeds a local repo, registers a fake `origin` remote (bogus URL — we never
/// actually talk to it), manually creates `refs/remotes/origin/<branch>` at
/// the current HEAD, and binds the local branch's upstream to it. This
/// simulates "just-fetched" state without needing a real remote on disk,
/// sidestepping Git's file:// protocol restrictions.
fn setup_repo_with_fake_upstream() -> (tempfile::TempDir, git2::Repository) {
    let (tmp, raw) = tempdir_repo();
    raw.remote("origin", "file:///nonexistent/bogus")
        .expect("add fake remote");
    let oid = commit_file(&raw, "a.txt", "v1", "init");

    let branch_name = raw
        .head()
        .unwrap()
        .shorthand()
        .unwrap_or("master")
        .to_string();
    raw.reference(
        &format!("refs/remotes/origin/{}", branch_name),
        oid,
        false,
        "fake upstream",
    )
    .expect("create remote-tracking ref");

    {
        // Branch already exists from the commit_file above — just bind its
        // upstream. Re-creating with `branch(force=true)` fails because HEAD
        // is checked out on it.
        let mut branch = raw
            .find_branch(&branch_name, git2::BranchType::Local)
            .expect("find local branch");
        branch
            .set_upstream(Some(&format!("origin/{}", branch_name)))
            .expect("bind upstream");
    }
    let _ = oid; // silence unused when the branch code path above was refactored

    (tmp, raw)
}

#[test]
fn ahead_behind_no_upstream_returns_none() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "v1", "init");
    let (_g, repo) = open_in(tmp.path());
    assert!(repo.ahead_behind().is_none(), "no upstream configured");
}

#[test]
fn ahead_behind_zero_when_in_sync() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _raw) = setup_repo_with_fake_upstream();
    let (_g, repo) = open_in(tmp.path());
    assert_eq!(repo.ahead_behind(), Some((0, 0)));
}

#[test]
fn ahead_behind_detects_local_ahead() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = setup_repo_with_fake_upstream();
    // Two commits past upstream.
    commit_file(&raw, "b.txt", "v2", "second");
    commit_file(&raw, "c.txt", "v3", "third");

    let (_g, repo) = open_in(tmp.path());
    assert_eq!(repo.ahead_behind(), Some((2, 0)));
}

#[test]
fn ahead_behind_detects_divergence() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = setup_repo_with_fake_upstream();
    let initial_oid = raw.head().unwrap().target().unwrap();
    let initial = raw.find_commit(initial_oid).unwrap();
    let sig = git2::Signature::now("Tester", "tester@example.com").unwrap();
    let tree = initial.tree().unwrap();

    // Advance origin/<branch> by one commit we don't have locally.
    let branch_name = raw
        .head()
        .unwrap()
        .shorthand()
        .unwrap_or("master")
        .to_string();
    raw.commit(
        Some(&format!("refs/remotes/origin/{}", branch_name)),
        &sig,
        &sig,
        "remote-only commit",
        &tree,
        &[&initial],
    )
    .expect("commit on remote-tracking ref");

    // Then advance local by a different commit on top of the shared base.
    commit_file(&raw, "local.txt", "local", "local-only commit");

    let (_g, repo) = open_in(tmp.path());
    assert_eq!(repo.ahead_behind(), Some((1, 1)));
}

#[test]
fn push_without_upstream_returns_error_message() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "v1", "init");
    let (_g, repo) = open_in(tmp.path());
    // No remote → git push can't find one → exits non-zero with stderr.
    let err = repo
        .push(false)
        .expect_err("push must fail without a remote");
    assert!(!err.is_empty(), "error message should be non-empty");
}

// ─── numstat + rename detection ─────────────────────────────────────────────

#[test]
fn get_status_fills_unstaged_line_counts() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "line1\nline2\n", "init");
    // Delete line2, add line3 → 1 addition, 1 deletion.
    write_file(&raw, "a.txt", "line1\nline3\n");

    let (_g, repo) = open_in(tmp.path());
    let (_staged, unstaged) = repo.get_status();
    let entry = unstaged
        .iter()
        .find(|f| f.path == "a.txt")
        .expect("a.txt in unstaged");
    assert_eq!(entry.additions, 1);
    assert_eq!(entry.deletions, 1);
}

#[test]
fn get_status_counts_untracked_file_lines() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    // Need at least one commit so `get_status` has a valid HEAD.
    commit_file(&raw, "seed.txt", "seed\n", "init");
    write_file(&raw, "new.txt", "a\nb\nc\n");

    let (_g, repo) = open_in(tmp.path());
    let (_staged, unstaged) = repo.get_status();
    let entry = unstaged
        .iter()
        .find(|f| f.path == "new.txt")
        .expect("new.txt in unstaged");
    assert_eq!(entry.status, FileStatus::Untracked);
    assert_eq!(entry.additions, 3);
    assert_eq!(entry.deletions, 0);
}

#[test]
fn get_status_detects_staged_rename_with_line_counts() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, raw) = tempdir_repo();
    // Commit a.txt then stage a "rename to b.txt, plus one added line".
    // libgit2's find_similar should collapse (delete a.txt, add b.txt) into
    // a single Renamed delta keyed on b.txt, and merge_renames carries that
    // through so the sidebar's FileEntry has the right +1 count.
    commit_file(&raw, "a.txt", "line1\nline2\nline3\n", "init");
    fs::remove_file(tmp.path().join("a.txt")).unwrap();
    write_file(&raw, "b.txt", "line1\nline2\nline3\nline4\n");

    let (_g, repo) = open_in(tmp.path());
    repo.stage_file("a.txt").expect("stage deletion");
    repo.stage_file("b.txt").expect("stage addition");

    let (staged, _unstaged) = repo.get_status();
    let entry = staged
        .iter()
        .find(|f| f.status == FileStatus::Renamed)
        .expect("a renamed entry exists in staged");
    assert_eq!(entry.path, "b.txt", "Renamed entry keys on new path");
    assert_eq!(
        entry.additions, 1,
        "exactly one line added on top of rename"
    );
    assert_eq!(entry.deletions, 0);
}
