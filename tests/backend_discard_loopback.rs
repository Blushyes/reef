//! Local vs Remote parity — `revert_path` (a.k.a. discard).
//!
//! This is the M4 Track A-0.1 regression guard: pre-M4 `RemoteBackend`
//! silently no-op'd folder/section discard because `app.rs::apply_discard_target`
//! went through the `self.repo` field (always `None` on remote). After
//! the switch to `backend.revert_path`, both backends must flip the same
//! file from "modified" back to clean / from staged back to HEAD.

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

/// Run `git status --porcelain=v1 -uall` and return its stdout as a
/// `Vec<String>` for a byte-level diff. We read the raw porcelain so the
/// assertion catches differences in staging state that a derived
/// `status.staged` / `status.unstaged` might hide.
fn porcelain(workdir: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .current_dir(workdir)
        .args(["status", "--porcelain=v1", "-uall"])
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git status failed: {out:?}");
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines: Vec<String> = s.lines().map(String::from).collect();
    lines.sort();
    lines
}

/// Seed two identical repos so parity comparisons have a common starting
/// point. Each repo has two tracked files (`a.txt`, `b.txt`), both
/// modified in the workdir, plus one new file (`c.txt`) untracked. We
/// also stage `a.txt` so tests can exercise both the staged- and
/// unstaged-side revert paths.
fn seed_pair() -> (tempfile::TempDir, tempfile::TempDir) {
    let (l_tmp, l_repo) = tempdir_repo();
    let (r_tmp, r_repo) = tempdir_repo();
    for (tmp, repo) in [(&l_tmp, &l_repo), (&r_tmp, &r_repo)] {
        commit_file(repo, "a.txt", "v1\n", "seed a");
        commit_file(repo, "b.txt", "v1\n", "seed b");
        write_file(repo, "a.txt", "v2\n");
        write_file(repo, "b.txt", "v2\n");
        write_file(repo, "c.txt", "new\n");
        // Stage a.txt so is_staged=true branches have something to revert.
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("a.txt")).unwrap();
        idx.write().unwrap();
        // Touch `tmp` so the compiler doesn't drop it before we leave seed.
        let _ = tmp;
    }
    (l_tmp, r_tmp)
}

#[test]
fn revert_unstaged_file_parity() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (l_tmp, r_tmp) = seed_pair();
    let l = LocalBackend::open_at(l_tmp.path().to_path_buf());
    let r = spawn_remote(r_tmp.path());

    l.revert_path("b.txt", false).unwrap();
    r.revert_path("b.txt", false).unwrap();

    assert_eq!(porcelain(l_tmp.path()), porcelain(r_tmp.path()));
}

#[test]
fn revert_staged_file_parity() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (l_tmp, r_tmp) = seed_pair();
    let l = LocalBackend::open_at(l_tmp.path().to_path_buf());
    let r = spawn_remote(r_tmp.path());

    l.revert_path("a.txt", true).unwrap();
    r.revert_path("a.txt", true).unwrap();

    assert_eq!(porcelain(l_tmp.path()), porcelain(r_tmp.path()));
}

#[test]
fn folder_and_section_discard_parity() {
    // Drive the *app-level* multi-path sequence that
    // `apply_discard_target` issues for a Section discard: revert every
    // listed path in turn. This is the scenario that used to silently
    // no-op on remote.
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (l_tmp, r_tmp) = seed_pair();
    let l = LocalBackend::open_at(l_tmp.path().to_path_buf());
    let r = spawn_remote(r_tmp.path());

    for path in ["a.txt", "b.txt"] {
        // Treat both as staged for the test — `a.txt` actually is,
        // `b.txt` isn't, and `revert_path(is_staged=true)` on an
        // unstaged file is a no-op followed by restore. Either backend
        // should handle both shapes identically.
        l.revert_path(path, true).unwrap();
        r.revert_path(path, true).unwrap();
    }

    assert_eq!(porcelain(l_tmp.path()), porcelain(r_tmp.path()));
}
