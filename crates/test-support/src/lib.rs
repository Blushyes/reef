//! Shared test helpers across reef crates.
//!
//! All items here are `pub` and consumed via `[dev-dependencies]`.

use git2::{Repository, Signature};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Initialize a real git repository in a temp directory. Sets the required
/// `user.name` and `user.email` config so commits don't depend on the caller's
/// global git config (critical for CI).
pub fn tempdir_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().expect("create tempdir");
    let repo = Repository::init(dir.path()).expect("git init");
    {
        let mut cfg = repo.config().expect("open repo config");
        cfg.set_str("user.name", "Tester").unwrap();
        cfg.set_str("user.email", "tester@example.com").unwrap();
    }
    (dir, repo)
}

/// Make an initial commit in the given repo. Writes `content` to `<workdir>/<path>`,
/// stages it, and commits with the message `subject`. Returns the commit OID.
pub fn commit_file(repo: &Repository, path: &str, content: &str, subject: &str) -> git2::Oid {
    let workdir = repo.workdir().expect("repo has workdir").to_path_buf();
    let full = workdir.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&full, content).unwrap();

    let mut index = repo.index().unwrap();
    index.add_path(Path::new(path)).unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();

    let sig = Signature::now("Tester", "tester@example.com").unwrap();
    let parents: Vec<git2::Commit> = repo
        .head()
        .ok()
        .and_then(|h| h.target())
        .and_then(|oid| repo.find_commit(oid).ok())
        .into_iter()
        .collect();
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();

    repo.commit(Some("HEAD"), &sig, &sig, subject, &tree, &parent_refs)
        .unwrap()
}

/// Write a file in the repo's workdir without staging or committing.
/// Useful for exercising "unstaged" / "untracked" code paths.
pub fn write_file(repo: &Repository, path: &str, content: &str) {
    let workdir = repo.workdir().expect("repo has workdir").to_path_buf();
    let full = workdir.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&full, content).unwrap();
}
