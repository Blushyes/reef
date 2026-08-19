//! Local vs Remote parity — asynchronously loaded Git numstat.
//!
//! Status classification stays cheap; a separate request carries content
//! statistics. This test pins that request across the remote boundary.

use std::path::Path;
use std::sync::Mutex;

use reef_io::{Backend, LocalBackend, RemoteBackend};
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

#[test]
fn unstaged_numstat_crosses_wire() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, repo) = tempdir_repo();
    // 3 lines committed, first two lines replaced in workdir:
    //   +2 added (replacements), -2 removed.
    commit_file(&repo, "a.txt", "one\ntwo\nthree\n", "init");
    write_file(&repo, "a.txt", "ONE\nTWO\nthree\n");
    // Also add a wholly-new file — untracked numstat counts the full
    // line count as additions.
    write_file(&repo, "b.txt", "x\ny\nz\n");

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    let local_stats = local.git_status_stats().expect("local status stats");
    let remote_stats = remote.git_status_stats().expect("remote status stats");

    assert_eq!(local_stats, remote_stats);
    assert_eq!(remote_stats.unstaged.get("a.txt"), Some(&(2, 2)));
    assert_eq!(remote_stats.unstaged.get("b.txt"), Some(&(3, 0)));
}
