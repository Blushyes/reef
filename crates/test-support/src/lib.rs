//! Shared test helpers across reef crates.
//!
//! All items here are `pub` and consumed via `[dev-dependencies]`.

use git2::{Repository, Signature};
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Redirect `$HOME` to a path for the lifetime of the guard, then restore
/// whatever value was there before (or remove it if HOME was unset).
///
/// `std::env::set_var` is process-global, so callers MUST serialise HOME
/// mutations through a `static Mutex<()>` in their test file. This helper
/// is the "do the unsafe set/restore correctly" part; the lock is yours.
///
/// Typical use:
/// ```no_run
/// use std::sync::Mutex;
/// use test_support::{tempdir_repo, HomeGuard};
/// static HOME_LOCK: Mutex<()> = Mutex::new(());
///
/// # fn body() {
/// let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
/// let (tmp, _repo) = tempdir_repo();
/// let _home = HomeGuard::enter(tmp.path());
/// // ... test body — any `std::env::var("HOME")` reads the tempdir
/// # }
/// ```
pub struct HomeGuard {
    original: Option<OsString>,
}

impl HomeGuard {
    pub fn enter(path: &Path) -> Self {
        let original = std::env::var_os("HOME");
        // SAFETY: caller must hold a process-wide HOME_LOCK for the
        // duration of this guard's lifetime. See the type-level doc.
        unsafe {
            std::env::set_var("HOME", path);
        }
        Self { original }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        // SAFETY: same as `enter`; the lock the caller holds spans the
        // guard's whole lifetime, including this Drop.
        unsafe {
            if let Some(v) = self.original.take() {
                std::env::set_var("HOME", v);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }
}

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
