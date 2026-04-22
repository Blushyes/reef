//! Local vs Remote parity — numstat fields on `FileEntry`.
//!
//! v0.12.0 added `additions` / `deletions` to `FileEntry` for the Git tab
//! `+N -M` column. Pre-M4 those never crossed the wire (RemoteBackend
//! always returned zero). This test checks both backends see the same
//! per-file numbers after a modification.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use reef::backend::{Backend, LocalBackend, RemoteBackend};
use test_support::{commit_file, tempdir_repo, write_file};

static BACKEND_LOCK: Mutex<()> = Mutex::new(());

fn agent_bin() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_reef-agent") {
        return PathBuf::from(path);
    }
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let root = PathBuf::from(manifest_dir);
    // cargo-llvm-cov sets CARGO_TARGET_DIR to target/llvm-cov-target;
    // check that first so coverage CI finds the binary.
    let target_dirs: Vec<PathBuf> = std::env::var("CARGO_TARGET_DIR")
        .map(|d| vec![PathBuf::from(d)])
        .unwrap_or_default()
        .into_iter()
        .chain([root.join("target")])
        .collect();
    for target in &target_dirs {
        for profile in ["debug", "release"] {
            let candidate = target.join(profile).join("reef-agent");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    panic!("reef-agent binary not found under target/{{debug,release}}");
}

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
