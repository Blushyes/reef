use std::path::{Path, PathBuf};
use std::sync::Mutex;

use reef::backend::{
    Backend, LocalBackend, RemoteBackend, RepoDiscoverOpts, normalize_repo_root_rel, repo_key,
};
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

fn init_repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git2::Repository::init(path).unwrap();
}

fn init_bare_repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git2::Repository::init_bare(path).unwrap();
}

fn configure_identity(path: &Path) {
    let repo = git2::Repository::open(path).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Reef Test").unwrap();
    config.set_str("user.email", "reef@example.com").unwrap();
}

fn configure_upstream(path: &Path, remote_path: &Path) -> String {
    let repo = git2::Repository::open(path).unwrap();
    repo.remote("origin", remote_path.to_str().unwrap())
        .unwrap();
    let branch = repo.head().unwrap().shorthand().unwrap().to_string();
    let mut config = repo.config().unwrap();
    config
        .set_str(&format!("branch.{branch}.remote"), "origin")
        .unwrap();
    config
        .set_str(
            &format!("branch.{branch}.merge"),
            &format!("refs/heads/{branch}"),
        )
        .unwrap();
    branch
}

fn create_branch(path: &Path, branch: &str) {
    let repo = git2::Repository::open(path).unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch(branch, &head, false).unwrap();
}

fn push_remote_commit(remote_path: &Path, file: &str, contents: &str, message: &str) {
    let updater = TempDir::new().unwrap();
    std::process::Command::new("git")
        .args([
            "clone",
            remote_path.to_str().unwrap(),
            updater.path().to_str().unwrap(),
        ])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .current_dir(updater.path())
        .args(["config", "user.name", "Reef Test"])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .current_dir(updater.path())
        .args(["config", "user.email", "reef@example.com"])
        .status()
        .unwrap();
    write_file(&updater.path().join(file), contents);
    std::process::Command::new("git")
        .current_dir(updater.path())
        .args(["add", "."])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .current_dir(updater.path())
        .args(["commit", "-m", message])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .current_dir(updater.path())
        .arg("push")
        .status()
        .unwrap();
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn commit_all(path: &Path, message: &str) {
    let repo = git2::Repository::open(path).unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = git2::Signature::now("Reef Test", "reef@example.com").unwrap();
    let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
        .unwrap();
}

fn repo_paths(backend: &dyn Backend, opts: &RepoDiscoverOpts) -> (Vec<String>, bool) {
    let resp = backend.discover_repos(opts).unwrap();
    let paths = resp
        .repos
        .iter()
        .map(|r| repo_key(&r.repo_root_rel))
        .collect();
    (paths, resp.truncated)
}

#[test]
fn normalize_repo_root_rel_contract() {
    assert_eq!(
        normalize_repo_root_rel(Path::new("")).unwrap(),
        PathBuf::from(".")
    );
    assert_eq!(
        normalize_repo_root_rel(Path::new(".")).unwrap(),
        PathBuf::from(".")
    );
    assert_eq!(
        normalize_repo_root_rel(Path::new("./a/b")).unwrap(),
        PathBuf::from("a/b")
    );
    assert_eq!(repo_key(Path::new(".")), ".");
    assert_eq!(repo_key(Path::new("a/b")), "a/b");
    assert!(normalize_repo_root_rel(Path::new("../a")).is_err());
    assert!(normalize_repo_root_rel(Path::new("/tmp/a")).is_err());
}

#[test]
fn discovers_sibling_repos_with_stable_order() {
    let tmp = TempDir::new().unwrap();
    init_repo(&tmp.path().join("zeta"));
    init_repo(&tmp.path().join("alpha"));

    let backend = LocalBackend::open_at(tmp.path().to_path_buf());
    let opts = RepoDiscoverOpts {
        max_depth: 1,
        include_nested: false,
        max_repos: Some(100),
    };

    let (paths, truncated) = repo_paths(&backend, &opts);
    assert_eq!(paths, vec!["alpha", "zeta"]);
    assert!(!truncated);
}

#[test]
fn max_depth_limits_discovery() {
    let tmp = TempDir::new().unwrap();
    init_repo(&tmp.path().join("apps/web"));

    let backend = LocalBackend::open_at(tmp.path().to_path_buf());
    let shallow = RepoDiscoverOpts {
        max_depth: 1,
        include_nested: false,
        max_repos: Some(100),
    };
    let deep = RepoDiscoverOpts {
        max_depth: 2,
        include_nested: false,
        max_repos: Some(100),
    };

    assert_eq!(repo_paths(&backend, &shallow).0, Vec::<String>::new());
    assert_eq!(repo_paths(&backend, &deep).0, vec!["apps/web"]);
}

#[test]
fn nested_repos_are_suppressed_by_default() {
    let tmp = TempDir::new().unwrap();
    init_repo(&tmp.path().join("outer"));
    init_repo(&tmp.path().join("outer/inner"));

    let backend = LocalBackend::open_at(tmp.path().to_path_buf());
    let default_nested = RepoDiscoverOpts {
        max_depth: 2,
        include_nested: false,
        max_repos: Some(100),
    };
    let include_nested = RepoDiscoverOpts {
        max_depth: 2,
        include_nested: true,
        max_repos: Some(100),
    };

    assert_eq!(repo_paths(&backend, &default_nested).0, vec!["outer"]);
    assert_eq!(
        repo_paths(&backend, &include_nested).0,
        vec!["outer", "outer/inner"]
    );
}

#[test]
fn max_repos_truncates() {
    let tmp = TempDir::new().unwrap();
    init_repo(&tmp.path().join("a"));
    init_repo(&tmp.path().join("b"));

    let backend = LocalBackend::open_at(tmp.path().to_path_buf());
    let opts = RepoDiscoverOpts {
        max_depth: 1,
        include_nested: false,
        max_repos: Some(1),
    };

    let (paths, truncated) = repo_paths(&backend, &opts);
    assert_eq!(paths.len(), 1);
    assert!(truncated);
}

#[test]
fn discover_repos_remote_matches_local() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    init_repo(&tmp.path().join("alpha"));
    init_repo(&tmp.path().join("beta"));

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());
    let opts = RepoDiscoverOpts {
        max_depth: 1,
        include_nested: false,
        max_repos: Some(100),
    };

    assert_eq!(repo_paths(&local, &opts), repo_paths(&remote, &opts));
}

#[test]
fn git_status_for_child_repo_remote_matches_local() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    init_repo(&tmp.path().join("alpha"));
    write_file(&tmp.path().join("alpha/new.txt"), "hello\n");

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    let local_status = local.git_status_for(Path::new("alpha")).unwrap();
    let remote_status = remote.git_status_for(Path::new("alpha")).unwrap();

    assert_eq!(local_status.branch_name, remote_status.branch_name);
    assert_eq!(local_status.unstaged.len(), 1);
    assert_eq!(remote_status.unstaged.len(), 1);
    assert_eq!(local_status.unstaged[0].path, "new.txt");
    assert_eq!(remote_status.unstaged[0].path, "new.txt");
}

#[test]
fn unstaged_diff_for_child_repo_remote_matches_local() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    init_repo(&tmp.path().join("alpha"));
    write_file(&tmp.path().join("alpha/new.txt"), "hello\n");

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    let local_diff = local
        .unstaged_diff_for(Path::new("alpha"), "new.txt", 3)
        .unwrap()
        .unwrap();
    let remote_diff = remote
        .unstaged_diff_for(Path::new("alpha"), "new.txt", 3)
        .unwrap()
        .unwrap();

    assert_eq!(local_diff.file_path, "new.txt");
    assert_eq!(remote_diff.file_path, "new.txt");
    assert_eq!(local_diff.hunks.len(), remote_diff.hunks.len());
}

#[test]
fn stage_unstage_for_child_repo_remote_matches_local() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    init_repo(&tmp.path().join("alpha"));
    write_file(&tmp.path().join("alpha/new.txt"), "hello\n");

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    local.stage_for(Path::new("alpha"), "new.txt").unwrap();
    let local_status = local.git_status_for(Path::new("alpha")).unwrap();
    assert_eq!(local_status.staged.len(), 1);

    local.unstage_for(Path::new("alpha"), "new.txt").unwrap();
    let local_status = local.git_status_for(Path::new("alpha")).unwrap();
    assert_eq!(local_status.unstaged.len(), 1);

    remote.stage_for(Path::new("alpha"), "new.txt").unwrap();
    let remote_status = remote.git_status_for(Path::new("alpha")).unwrap();
    assert_eq!(remote_status.staged.len(), 1);

    remote.unstage_for(Path::new("alpha"), "new.txt").unwrap();
    let remote_status = remote.git_status_for(Path::new("alpha")).unwrap();
    assert_eq!(remote_status.unstaged.len(), 1);
}

#[test]
fn revert_path_for_child_repo_does_not_touch_root_same_path() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    write_file(&tmp.path().join("same.txt"), "root base\n");
    commit_all(tmp.path(), "root base");

    init_repo(&tmp.path().join("alpha"));
    write_file(&tmp.path().join("alpha/same.txt"), "child base\n");
    commit_all(&tmp.path().join("alpha"), "child base");

    write_file(&tmp.path().join("same.txt"), "root modified\n");
    write_file(&tmp.path().join("alpha/same.txt"), "child modified\n");

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    local
        .revert_path_for(Path::new("alpha"), "same.txt", false)
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(tmp.path().join("same.txt")).unwrap(),
        "root modified\n"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("alpha/same.txt")).unwrap(),
        "child base\n"
    );

    write_file(
        &tmp.path().join("alpha/same.txt"),
        "child remote modified\n",
    );
    let remote = spawn_remote(tmp.path());
    remote
        .revert_path_for(Path::new("alpha"), "same.txt", false)
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(tmp.path().join("same.txt")).unwrap(),
        "root modified\n"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("alpha/same.txt")).unwrap(),
        "child base\n"
    );
}

#[test]
fn commit_for_child_repo_does_not_commit_root_same_path() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    configure_identity(tmp.path());
    init_repo(&tmp.path().join("alpha"));
    configure_identity(&tmp.path().join("alpha"));

    write_file(&tmp.path().join("same.txt"), "root staged\n");
    write_file(&tmp.path().join("alpha/same.txt"), "child staged\n");
    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    local.stage_for(Path::new("."), "same.txt").unwrap();
    local.stage_for(Path::new("alpha"), "same.txt").unwrap();

    local
        .commit_for(Path::new("alpha"), "child commit")
        .unwrap();

    assert!(git2::Repository::open(tmp.path()).unwrap().head().is_err());
    assert!(
        git2::Repository::open(tmp.path().join("alpha"))
            .unwrap()
            .head()
            .is_ok()
    );
    assert_eq!(
        local.git_status_for(Path::new(".")).unwrap().staged.len(),
        1
    );

    write_file(&tmp.path().join("alpha/remote.txt"), "remote staged\n");
    let remote = spawn_remote(tmp.path());
    remote.stage_for(Path::new("alpha"), "remote.txt").unwrap();
    remote
        .commit_for(Path::new("alpha"), "child remote commit")
        .unwrap();

    assert!(git2::Repository::open(tmp.path()).unwrap().head().is_err());
    assert_eq!(
        remote.git_status_for(Path::new(".")).unwrap().staged.len(),
        1
    );
}

#[test]
fn push_for_child_repo_pushes_child_upstream() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    init_bare_repo(&tmp.path().join("remote.git"));
    init_repo(&tmp.path().join("alpha"));
    configure_identity(&tmp.path().join("alpha"));
    write_file(&tmp.path().join("alpha/same.txt"), "child staged\n");
    commit_all(&tmp.path().join("alpha"), "child base");
    let branch = configure_upstream(&tmp.path().join("alpha"), &tmp.path().join("remote.git"));

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    local.push_for(Path::new("alpha"), false).unwrap();
    let first_oid = git2::Repository::open_bare(tmp.path().join("remote.git"))
        .unwrap()
        .find_reference(&format!("refs/heads/{branch}"))
        .unwrap()
        .target()
        .unwrap();

    write_file(&tmp.path().join("alpha/remote.txt"), "remote staged\n");
    local.stage_for(Path::new("alpha"), "remote.txt").unwrap();
    local
        .commit_for(Path::new("alpha"), "child second")
        .unwrap();
    let remote = spawn_remote(tmp.path());
    remote.push_for(Path::new("alpha"), false).unwrap();

    let second_oid = git2::Repository::open_bare(tmp.path().join("remote.git"))
        .unwrap()
        .find_reference(&format!("refs/heads/{branch}"))
        .unwrap()
        .target()
        .unwrap();
    assert_ne!(first_oid, second_oid);
}

#[test]
fn pull_for_child_repo_remote_matches_local() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    init_bare_repo(&tmp.path().join("remote-alpha.git"));
    init_bare_repo(&tmp.path().join("remote-beta.git"));

    init_repo(&tmp.path().join("alpha"));
    configure_identity(&tmp.path().join("alpha"));
    write_file(&tmp.path().join("alpha/base.txt"), "base\n");
    commit_all(&tmp.path().join("alpha"), "alpha base");
    configure_upstream(
        &tmp.path().join("alpha"),
        &tmp.path().join("remote-alpha.git"),
    );

    init_repo(&tmp.path().join("beta"));
    configure_identity(&tmp.path().join("beta"));
    write_file(&tmp.path().join("beta/base.txt"), "base\n");
    commit_all(&tmp.path().join("beta"), "beta base");
    configure_upstream(
        &tmp.path().join("beta"),
        &tmp.path().join("remote-beta.git"),
    );

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    local.push_for(Path::new("alpha"), false).unwrap();
    local.push_for(Path::new("beta"), false).unwrap();

    push_remote_commit(
        &tmp.path().join("remote-alpha.git"),
        "remote.txt",
        "alpha remote\n",
        "alpha remote",
    );
    local.pull_for(Path::new("alpha")).unwrap();
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("alpha/remote.txt")).unwrap(),
        "alpha remote\n"
    );
    assert!(!tmp.path().join("beta/remote.txt").exists());

    push_remote_commit(
        &tmp.path().join("remote-beta.git"),
        "remote.txt",
        "beta remote\n",
        "beta remote",
    );
    let remote = spawn_remote(tmp.path());
    remote.pull_for(Path::new("beta")).unwrap();
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("beta/remote.txt")).unwrap(),
        "beta remote\n"
    );
}

#[test]
fn graph_history_for_child_repo_remote_matches_local() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    init_repo(&tmp.path().join("alpha"));
    write_file(&tmp.path().join("alpha/a.txt"), "alpha\n");
    commit_all(&tmp.path().join("alpha"), "alpha commit");
    init_repo(&tmp.path().join("beta"));
    write_file(&tmp.path().join("beta/b.txt"), "beta\n");
    commit_all(&tmp.path().join("beta"), "beta commit");

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    assert_eq!(
        local.head_oid_for(Path::new("beta")).unwrap(),
        remote.head_oid_for(Path::new("beta")).unwrap()
    );
    let local_commits = local.list_commits_for(Path::new("beta"), 10).unwrap();
    let remote_commits = remote.list_commits_for(Path::new("beta"), 10).unwrap();
    assert_eq!(local_commits.len(), 1);
    assert_eq!(remote_commits.len(), 1);
    assert_eq!(local_commits[0].subject, "beta commit");
    assert_eq!(remote_commits[0].subject, "beta commit");
}

#[test]
fn checkout_branch_for_child_repo_remote_matches_local() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    init_repo(&tmp.path().join("alpha"));
    write_file(&tmp.path().join("alpha/a.txt"), "alpha\n");
    commit_all(&tmp.path().join("alpha"), "alpha commit");
    create_branch(&tmp.path().join("alpha"), "feature");

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    local
        .checkout_branch_for(Path::new("alpha"), "feature")
        .unwrap();
    assert_eq!(
        local
            .git_status_for(Path::new("alpha"))
            .unwrap()
            .branch_name,
        "feature"
    );

    local
        .checkout_branch_for(Path::new("alpha"), "master")
        .unwrap();
    let remote = spawn_remote(tmp.path());
    remote
        .checkout_branch_for(Path::new("alpha"), "feature")
        .unwrap();
    assert_eq!(
        remote
            .git_status_for(Path::new("alpha"))
            .unwrap()
            .branch_name,
        "feature"
    );
}

#[test]
fn create_branch_for_child_repo_remote_matches_local() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    init_repo(&tmp.path().join("alpha"));
    write_file(&tmp.path().join("alpha/a.txt"), "alpha\n");
    commit_all(&tmp.path().join("alpha"), "alpha commit");
    create_branch(&tmp.path().join("alpha"), "base-feature");

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    local
        .create_branch_for(Path::new("alpha"), "local-new", None)
        .unwrap();
    assert_eq!(
        local
            .git_status_for(Path::new("alpha"))
            .unwrap()
            .branch_name,
        "local-new"
    );

    local
        .checkout_branch_for(Path::new("alpha"), "master")
        .unwrap();
    let remote = spawn_remote(tmp.path());
    remote
        .create_branch_for(Path::new("alpha"), "remote-new", Some("base-feature"))
        .unwrap();
    assert_eq!(
        remote
            .git_status_for(Path::new("alpha"))
            .unwrap()
            .branch_name,
        "remote-new"
    );
    assert!(git2::Repository::open(tmp.path()).unwrap().head().is_err());
}
