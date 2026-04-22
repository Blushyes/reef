//! Local vs Remote parity — numstat fields on `FileEntry`.
//!
//! v0.12.0 added `additions` / `deletions` to `FileEntry` for the Git tab
//! `+N -M` column. Pre-M4 those never crossed the wire (RemoteBackend
//! always returned zero). This test checks both backends see the same
//! per-file numbers after a modification.

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

    let l = local.git_status().expect("local status");
    let r = remote.git_status().expect("remote status");

    // Index + sort by path so the zip pairs the same file on both sides.
    let mut l_unstaged = l.unstaged.clone();
    let mut r_unstaged = r.unstaged.clone();
    l_unstaged.sort_by(|a, b| a.path.cmp(&b.path));
    r_unstaged.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(l_unstaged.len(), r_unstaged.len());
    for (a, b) in l_unstaged.iter().zip(r_unstaged.iter()) {
        assert_eq!(a.path, b.path);
        assert_eq!(a.additions, b.additions, "additions mismatch on {}", a.path);
        assert_eq!(a.deletions, b.deletions, "deletions mismatch on {}", a.path);
        // Also assert remote non-zero so we catch the pre-M4 "always 0"
        // regression if the numstat wiring breaks again.
        assert!(
            b.additions > 0 || b.deletions > 0,
            "remote lost numstat for {}",
            b.path
        );
    }
}
