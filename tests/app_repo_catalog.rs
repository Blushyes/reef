use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reef::app::{App, BranchCreateStep, Tab};
use reef::backend::{Backend, LocalBackend, repo_key};
use reef::input;
use reef::ui::git_status_panel;
use reef::ui::theme::Theme;
use tempfile::TempDir;
use test_support::HomeGuard;

static APP_LOCK: Mutex<()> = Mutex::new(());

fn init_repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git2::Repository::init(path).unwrap();
}

fn configure_identity(path: &Path) {
    let repo = git2::Repository::open(path).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Reef Test").unwrap();
    config.set_str("user.email", "reef@example.com").unwrap();
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

fn create_branch(path: &Path, branch: &str) {
    let repo = git2::Repository::open(path).unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch(branch, &head, false).unwrap();
}

fn init_bare_repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git2::Repository::init_bare(path).unwrap();
}

fn configure_upstream(path: &Path, remote_path: &Path) {
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
}

fn configure_origin(path: &Path, remote_path: &Path) {
    let repo = git2::Repository::open(path).unwrap();
    repo.remote("origin", remote_path.to_str().unwrap())
        .unwrap();
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

fn app_for(root: &Path) -> App {
    let backend = Arc::new(LocalBackend::open_at(root.to_path_buf()));
    App::new_with_backend(Theme::dark(), backend, None)
}

fn wait_for_repo_catalog(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.repo_catalog.discover_load.loading {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for repo catalog");
}

fn wait_for_git_status(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.git_status_load.loading {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for git status");
}

fn wait_for_pull(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.pull_in_flight {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for pull");
}

fn wait_for_push(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.push_in_flight {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for push");
}

fn wait_for_diff(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.diff_load.loading {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for diff");
}

fn wait_for_graph(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.graph_load.loading {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for graph");
}

fn wait_for_commit(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.commit_in_flight {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for commit");
}

fn catalog_paths(app: &App) -> Vec<String> {
    app.repo_catalog
        .repos
        .iter()
        .map(|r| repo_key(&r.repo_root_rel))
        .collect()
}

#[test]
fn app_discovers_child_repos_on_startup() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_repo(&tmp.path().join("alpha"));
    init_repo(&tmp.path().join("beta"));

    let mut app = app_for(tmp.path());
    wait_for_repo_catalog(&mut app);

    assert_eq!(catalog_paths(&app), vec!["alpha", "beta"]);
    assert_eq!(app.repo_catalog.selected_git_repo, None);
}

#[test]
fn app_auto_selects_single_discovered_repo() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_repo(&tmp.path().join("only"));

    let mut app = app_for(tmp.path());
    wait_for_repo_catalog(&mut app);

    assert_eq!(catalog_paths(&app), vec!["only"]);
    assert_eq!(
        app.repo_catalog.selected_git_repo.as_deref(),
        Some(Path::new("only"))
    );
}

#[test]
fn app_auto_selects_root_repo() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_repo(tmp.path());

    let mut app = app_for(tmp.path());
    wait_for_repo_catalog(&mut app);

    assert_eq!(catalog_paths(&app), vec!["."]);
    assert_eq!(
        app.repo_catalog.selected_git_repo.as_deref(),
        Some(Path::new("."))
    );
}

#[test]
fn git_status_command_selects_discovered_repo() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_repo(&tmp.path().join("alpha"));
    init_repo(&tmp.path().join("beta"));

    let mut app = app_for(tmp.path());
    wait_for_repo_catalog(&mut app);

    assert_eq!(app.repo_catalog.selected_git_repo, None);
    assert!(git_status_panel::handle_command(
        &mut app,
        "git.selectRepo",
        &serde_json::json!({ "repo": "beta" }),
    ));
    assert_eq!(
        app.repo_catalog.selected_git_repo.as_deref(),
        Some(Path::new("beta"))
    );
}

#[test]
fn app_loads_status_for_auto_selected_child_repo() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_repo(&tmp.path().join("only"));
    write_file(&tmp.path().join("only/new.txt"), "hello\n");

    let mut app = app_for(tmp.path());
    wait_for_repo_catalog(&mut app);
    wait_for_git_status(&mut app);

    assert_eq!(
        app.repo_catalog.selected_git_repo.as_deref(),
        Some(Path::new("only"))
    );
    assert_eq!(app.unstaged_files.len(), 1);
    assert_eq!(app.unstaged_files[0].path, "new.txt");
}

#[test]
fn app_loads_diff_for_selected_child_repo_file() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_repo(&tmp.path().join("only"));
    write_file(&tmp.path().join("only/new.txt"), "hello\n");

    let mut app = app_for(tmp.path());
    wait_for_repo_catalog(&mut app);
    wait_for_git_status(&mut app);

    assert!(git_status_panel::handle_command(
        &mut app,
        "git.selectFile",
        &serde_json::json!({ "path": "new.txt", "staged": false }),
    ));
    wait_for_diff(&mut app);

    let diff = app.diff_content.as_ref().expect("diff loaded");
    assert_eq!(diff.diff.file_path, "new.txt");
    assert!(
        diff.diff
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .any(|line| line.content == "hello")
    );
}

#[test]
fn app_stages_and_unstages_selected_child_repo_file() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_repo(&tmp.path().join("only"));
    write_file(&tmp.path().join("only/new.txt"), "hello\n");

    let mut app = app_for(tmp.path());
    wait_for_repo_catalog(&mut app);
    wait_for_git_status(&mut app);

    app.stage_file("new.txt");
    wait_for_git_status(&mut app);
    assert_eq!(app.staged_files.len(), 1);
    assert_eq!(app.staged_files[0].path, "new.txt");
    assert!(app.unstaged_files.is_empty());

    app.unstage_file("new.txt");
    wait_for_git_status(&mut app);
    assert!(app.staged_files.is_empty());
    assert_eq!(app.unstaged_files.len(), 1);
    assert_eq!(app.unstaged_files[0].path, "new.txt");
}

#[test]
fn app_discards_selected_child_repo_file() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_repo(&tmp.path().join("only"));
    write_file(&tmp.path().join("only/same.txt"), "child base\n");
    commit_all(&tmp.path().join("only"), "child base");
    write_file(&tmp.path().join("only/same.txt"), "child modified\n");

    let mut app = app_for(tmp.path());
    wait_for_repo_catalog(&mut app);
    wait_for_git_status(&mut app);

    assert_eq!(app.unstaged_files.len(), 1);
    assert!(git_status_panel::handle_command(
        &mut app,
        "git.discardPrompt",
        &serde_json::json!({ "path": "same.txt" }),
    ));
    assert!(git_status_panel::handle_command(
        &mut app,
        "git.discardConfirm",
        &serde_json::Value::Null,
    ));
    wait_for_git_status(&mut app);

    assert!(app.unstaged_files.is_empty());
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("only/same.txt")).unwrap(),
        "child base\n"
    );
}

#[test]
fn app_commits_selected_child_repo_file() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_repo(&tmp.path().join("only"));
    configure_identity(&tmp.path().join("only"));
    write_file(&tmp.path().join("only/new.txt"), "hello\n");

    let mut app = app_for(tmp.path());
    wait_for_repo_catalog(&mut app);
    wait_for_git_status(&mut app);

    app.stage_file("new.txt");
    wait_for_git_status(&mut app);
    app.git_status.commit_message = "child commit".to_string();
    app.git_status.commit_cursor = app.git_status.commit_message.len();
    app.run_commit();
    wait_for_commit(&mut app);
    app.refresh_status();
    wait_for_git_status(&mut app);

    assert!(app.git_status.commit_error.is_none());
    assert!(app.git_status.commit_message.is_empty());
    assert!(app.staged_files.is_empty());
    assert!(
        git2::Repository::open(tmp.path().join("only"))
            .unwrap()
            .head()
            .is_ok()
    );
}

#[test]
fn app_persists_selected_repo() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_repo(&tmp.path().join("alpha"));
    init_repo(&tmp.path().join("beta"));

    {
        let mut app = app_for(tmp.path());
        wait_for_repo_catalog(&mut app);
        assert!(git_status_panel::handle_command(
            &mut app,
            "git.selectRepo",
            &serde_json::json!({ "repo": "beta" }),
        ));
        assert_eq!(
            app.repo_catalog.selected_git_repo.as_deref(),
            Some(Path::new("beta"))
        );
    }

    let mut app = app_for(tmp.path());
    wait_for_repo_catalog(&mut app);
    assert_eq!(
        app.repo_catalog.selected_git_repo.as_deref(),
        Some(Path::new("beta"))
    );
}

#[test]
fn app_graph_loads_selected_child_repo() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_repo(&tmp.path().join("alpha"));
    write_file(&tmp.path().join("alpha/a.txt"), "alpha\n");
    commit_all(&tmp.path().join("alpha"), "alpha commit");
    init_repo(&tmp.path().join("beta"));
    write_file(&tmp.path().join("beta/b.txt"), "beta\n");
    commit_all(&tmp.path().join("beta"), "beta commit");

    let mut app = app_for(tmp.path());
    wait_for_repo_catalog(&mut app);
    assert!(git_status_panel::handle_command(
        &mut app,
        "git.selectRepo",
        &serde_json::json!({ "repo": "beta" }),
    ));
    app.active_tab = Tab::Graph;
    app.refresh_graph();
    wait_for_graph(&mut app);

    assert_eq!(app.git_graph.rows.len(), 1);
    assert_eq!(app.git_graph.rows[0].commit.subject, "beta commit");
}

#[test]
fn app_switches_branch_for_selected_child_repo() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_repo(&tmp.path().join("only"));
    write_file(&tmp.path().join("only/a.txt"), "alpha\n");
    commit_all(&tmp.path().join("only"), "base commit");
    create_branch(&tmp.path().join("only"), "feature");

    let mut app = app_for(tmp.path());
    wait_for_repo_catalog(&mut app);
    wait_for_git_status(&mut app);
    assert_eq!(app.branch_name, "master");
    assert!(app.git_status.branches.iter().any(|b| b == "feature"));

    assert!(git_status_panel::handle_command(
        &mut app,
        "git.checkoutBranch",
        &serde_json::json!({ "branch": "feature" }),
    ));
    wait_for_git_status(&mut app);

    assert_eq!(app.branch_name, "feature");
}

#[test]
fn app_creates_branch_for_selected_child_repo() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_repo(&tmp.path().join("only"));
    write_file(&tmp.path().join("only/a.txt"), "alpha\n");
    commit_all(&tmp.path().join("only"), "base commit");
    create_branch(&tmp.path().join("only"), "base-feature");

    let mut app = app_for(tmp.path());
    wait_for_repo_catalog(&mut app);
    wait_for_git_status(&mut app);

    app.open_branch_create_dialog();
    app.start_branch_create_choose_base();
    let base_idx = app
        .branch_create_base_choices()
        .iter()
        .position(|branch| branch == "base-feature")
        .unwrap();
    app.select_branch_create_base(base_idx);
    let dialog = app.git_status.branch_create_dialog.as_mut().unwrap();
    dialog.input = "new-child".to_string();
    dialog.cursor = dialog.input.len();
    app.submit_branch_create_dialog();
    wait_for_git_status(&mut app);

    assert_eq!(app.branch_name, "new-child");
    assert!(app.git_status.branch_create_dialog.is_none());
}

#[test]
fn branch_create_mode_picker_moves_selection() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_repo(&tmp.path().join("only"));
    write_file(&tmp.path().join("only/a.txt"), "alpha\n");
    commit_all(&tmp.path().join("only"), "base commit");

    let mut app = app_for(tmp.path());
    wait_for_repo_catalog(&mut app);
    wait_for_git_status(&mut app);
    app.open_branch_create_dialog();

    assert_eq!(
        app.git_status
            .branch_create_dialog
            .as_ref()
            .unwrap()
            .selected_base_idx,
        0
    );
    input::handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut app);
    assert_eq!(
        app.git_status
            .branch_create_dialog
            .as_ref()
            .unwrap()
            .selected_base_idx,
        1
    );
    input::handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut app);
    assert!(matches!(
        app.git_status.branch_create_dialog.as_ref().unwrap().step,
        BranchCreateStep::ChooseBase
    ));
}

#[test]
fn app_pulls_selected_child_repo() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_bare_repo(&tmp.path().join("remote.git"));
    init_repo(&tmp.path().join("only"));
    configure_identity(&tmp.path().join("only"));
    write_file(&tmp.path().join("only/base.txt"), "base\n");
    commit_all(&tmp.path().join("only"), "base commit");
    configure_upstream(&tmp.path().join("only"), &tmp.path().join("remote.git"));
    LocalBackend::open_at(tmp.path().to_path_buf())
        .push_for(Path::new("only"), false)
        .unwrap();

    push_remote_commit(
        &tmp.path().join("remote.git"),
        "remote.txt",
        "remote\n",
        "remote commit",
    );

    let mut app = app_for(tmp.path());
    wait_for_repo_catalog(&mut app);
    wait_for_git_status(&mut app);
    assert!(git_status_panel::handle_command(
        &mut app,
        "git.pull",
        &serde_json::json!({})
    ));
    wait_for_pull(&mut app);

    assert_eq!(
        std::fs::read_to_string(tmp.path().join("only/remote.txt")).unwrap(),
        "remote\n"
    );
}

#[test]
fn app_publishes_selected_child_branch() {
    let _lock = APP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _home = HomeGuard::enter(tmp.path());
    init_bare_repo(&tmp.path().join("remote.git"));
    init_repo(&tmp.path().join("only"));
    configure_identity(&tmp.path().join("only"));
    write_file(&tmp.path().join("only/base.txt"), "base\n");
    commit_all(&tmp.path().join("only"), "base commit");
    configure_origin(&tmp.path().join("only"), &tmp.path().join("remote.git"));

    let mut app = app_for(tmp.path());
    app.set_active_tab(Tab::Git);
    wait_for_repo_catalog(&mut app);
    wait_for_git_status(&mut app);
    assert!(app.should_offer_publish_branch());
    assert!(git_status_panel::handle_command(
        &mut app,
        "git.publishBranch",
        &serde_json::json!({})
    ));
    wait_for_push(&mut app);

    let repo = git2::Repository::open(tmp.path().join("only")).unwrap();
    let branch = repo.head().unwrap().shorthand().unwrap().to_string();
    assert_eq!(
        repo.config()
            .unwrap()
            .get_string(&format!("branch.{branch}.remote"))
            .unwrap(),
        "origin"
    );
    assert!(
        git2::Repository::open_bare(tmp.path().join("remote.git"))
            .unwrap()
            .find_reference(&format!("refs/heads/{branch}"))
            .is_ok()
    );
}
