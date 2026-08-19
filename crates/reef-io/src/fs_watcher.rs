use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use reef_core::git::GitRepo;

use crate::{FsChange, FsChangeCoalescer};

const DEBOUNCE: Duration = Duration::from_millis(300);
// A missing repository may later be created at any ancestor of `workdir`.
// Probing here avoids broad ancestor watches, which are recursive at the
// FSEvents stream level even when notify exposes them as non-recursive.
const REPO_DISCOVERY_INTERVAL: Duration = Duration::from_secs(1);

/// Watch `workdir` recursively and emit a change on the returned receiver
/// whenever a debounced non-ignored event fires. When the watcher can't start,
/// the sender is dropped so callers observe `Disconnected` and stop polling.
pub fn spawn(workdir: PathBuf) -> Receiver<FsChange> {
    let has_repo = Arc::new(AtomicBool::new(GitRepo::open_at(&workdir).is_ok()));
    spawn_with_repo_state(workdir, has_repo, Arc::new(AtomicBool::new(false)))
}

pub(crate) fn spawn_with_repo_state(
    workdir: PathBuf,
    has_repo: Arc<AtomicBool>,
    repo_monitor_active: Arc<AtomicBool>,
) -> Receiver<FsChange> {
    let (out_tx, out_rx) = crossbeam_channel::unbounded::<FsChange>();
    let _ = thread::Builder::new()
        .name("reef-fs-watcher".into())
        .spawn(move || run(workdir, has_repo, repo_monitor_active, out_tx));
    out_rx
}

fn run(
    workdir: PathBuf,
    has_repo: Arc<AtomicBool>,
    repo_monitor_active: Arc<AtomicBool>,
    out_tx: Sender<FsChange>,
) {
    // macOS tempdirs and symlinked workdirs: notify delivers canonical paths,
    // so prefix checks would fail without canonicalizing up front.
    let original_workdir = workdir;
    let workdir = std::fs::canonicalize(&original_workdir).unwrap_or(original_workdir.clone());
    let repo_gi = build_repo_gitignore(&workdir);

    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher: RecommendedWatcher =
        match notify::recommended_watcher(move |res: notify::Result<Event>| {
            let _ = tx.send(res);
        }) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[reef] fs watcher init failed: {e}");
                return;
            }
        };

    let mut watch_roots = vec![(original_workdir, RecursiveMode::Recursive)];
    if watch_roots[0].0 != workdir {
        watch_roots.push((workdir.clone(), RecursiveMode::Recursive));
    }
    for (root, mode) in &watch_roots {
        if let Err(e) = watcher.watch(root, *mode) {
            eprintln!("[reef] fs watcher watch({:?}) failed: {e}", root);
            return;
        }
    }

    let mut watched_gitdirs = Vec::new();
    let mut gitdirs = gitdirs_for(&workdir);
    for gitdir in &gitdirs {
        watch_external_gitdir(&mut watcher, gitdir, &workdir, &mut watched_gitdirs);
    }

    let current_has_repo = GitRepo::open_at(&workdir).is_ok();
    has_repo.store(current_has_repo, Ordering::Release);
    repo_monitor_active.store(true, Ordering::Release);

    let mut debounce_deadline: Option<Instant> = None;
    let mut pending_change = FsChangeCoalescer::default();
    let mut next_repo_check = Instant::now() + REPO_DISCOVERY_INTERVAL;
    let mut previous_has_repo = current_has_repo;
    loop {
        let now = Instant::now();
        let fs_change_ready = debounce_deadline.is_some_and(|deadline| now >= deadline);
        let repo_check_due = now >= next_repo_check;
        if fs_change_ready || repo_check_due {
            if fs_change_ready {
                debounce_deadline = None;
            }
            if repo_check_due {
                next_repo_check = now + REPO_DISCOVERY_INTERVAL;
            }

            let current_has_repo = GitRepo::open_at(&workdir).is_ok();
            let repo_presence_changed = current_has_repo != previous_has_repo;
            previous_has_repo = current_has_repo;
            has_repo.store(current_has_repo, Ordering::Release);

            let next_gitdirs = gitdirs_for(&workdir);
            if next_gitdirs != gitdirs {
                gitdirs = next_gitdirs;
                for gitdir in &gitdirs {
                    watch_external_gitdir(&mut watcher, gitdir, &workdir, &mut watched_gitdirs);
                }
            }

            if fs_change_ready || repo_presence_changed {
                let change = FsChange {
                    repo_presence_changed,
                    ..pending_change.take()
                };
                if out_tx.send(change).is_err() {
                    break;
                }
            }
            continue;
        }

        let next_deadline = debounce_deadline
            .map(|deadline| deadline.min(next_repo_check))
            .unwrap_or(next_repo_check);
        let timeout = next_deadline.saturating_duration_since(now);
        match rx.recv_timeout(timeout) {
            Ok(Ok(ev)) => {
                let change = classify_event(&ev, &gitdirs, &workdir, &repo_gi);
                if change.workspace_changed || change.git_metadata_changed {
                    pending_change.push(change);
                    debounce_deadline = Some(Instant::now() + DEBOUNCE);
                }
            }
            Ok(Err(_)) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    repo_monitor_active.store(false, Ordering::Release);
}

fn build_repo_gitignore(workdir: &Path) -> Gitignore {
    let mut b = GitignoreBuilder::new(workdir);
    let _ = b.add(workdir.join(".gitignore"));
    let _ = b.add(workdir.join(".git").join("info").join("exclude"));
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

fn gitdirs_for(workdir: &Path) -> Vec<PathBuf> {
    let Ok(repo) = GitRepo::open_at(workdir) else {
        return Vec::new();
    };
    let gitdir = normalize_event_path(repo.gitdir());
    let commondir = normalize_event_path(repo.commondir());
    if gitdir.starts_with(&commondir) {
        vec![commondir]
    } else if commondir.starts_with(&gitdir) {
        vec![gitdir]
    } else {
        vec![gitdir, commondir]
    }
}

fn watch_external_gitdir(
    watcher: &mut RecommendedWatcher,
    gitdir: &Path,
    workdir: &Path,
    watched_gitdirs: &mut Vec<PathBuf>,
) {
    if gitdir.starts_with(workdir) || watched_gitdirs.iter().any(|path| path == gitdir) {
        return;
    }
    if let Err(error) = watcher.watch(gitdir, RecursiveMode::Recursive) {
        eprintln!("[reef] fs watcher watch gitdir({gitdir:?}) failed: {error}");
        return;
    }
    watched_gitdirs.push(gitdir.to_path_buf());
}

fn classify_event(
    ev: &Event,
    gitdirs: &[PathBuf],
    workdir: &Path,
    repo_gi: &Gitignore,
) -> FsChange {
    let mut change = FsChangeCoalescer::default();
    for path in &ev.paths {
        let path = normalize_event_path(path);
        if gitdirs
            .iter()
            .any(|gitdir| path == *gitdir || path.starts_with(gitdir))
        {
            change.mark_git_metadata_changed();
            continue;
        }
        // matched_path_or_any_parents panics if path is not under the matcher
        // root, so we must bail out before the gitignore check when notify
        // surfaces a sibling or transient path.
        if !path.starts_with(workdir) {
            continue;
        }
        let is_dir = path.is_dir();
        if repo_gi
            .matched_path_or_any_parents(&path, is_dir)
            .is_ignore()
        {
            continue;
        }
        if is_sqlite_sidecar(&path) {
            continue;
        }
        if let Ok(relative) = path.strip_prefix(workdir) {
            change.push_workspace_path(relative.to_path_buf());
        }
    }
    change.into_change()
}

/// `true` for a SQLite journal / WAL sidecar sitting next to its
/// database. SQLite rewrites the `-shm` index whenever a WAL database
/// is opened — including the read-only opens a preview does — so
/// treating these as workspace changes makes a reader trigger the
/// reload that makes it read again, without end. Recognised only when
/// the database they belong to is actually there, so an unrelated file
/// that happens to end in `-journal` still counts as a change.
fn is_sqlite_sidecar(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    ["-wal", "-shm", "-journal"].iter().any(|suffix| {
        name.len() > suffix.len()
            && name.ends_with(suffix)
            && path
                .with_file_name(&name[..name.len() - suffix.len()])
                .is_file()
    })
}

fn normalize_event_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    let Ok(parent) = std::fs::canonicalize(parent) else {
        return path.to_path_buf();
    };
    match path.file_name() {
        Some(name) => parent.join(name),
        None => parent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use test_support::{commit_file, tempdir_repo};

    #[test]
    fn sqlite_sidecars_next_to_their_database_are_not_workspace_changes() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("qb.db"), b"").unwrap();
        for sidecar in ["qb.db-wal", "qb.db-shm", "qb.db-journal"] {
            assert!(
                super::is_sqlite_sidecar(&tmp.path().join(sidecar)),
                "{sidecar} should be ignored"
            );
        }
        // The database itself is a real change.
        assert!(!super::is_sqlite_sidecar(&tmp.path().join("qb.db")));
        // A file that merely ends in one of the suffixes, with no
        // database beside it, stays a real change.
        assert!(!super::is_sqlite_sidecar(&tmp.path().join("notes-journal")));
        assert!(!super::is_sqlite_sidecar(&tmp.path().join("-wal")));
    }

    #[test]
    fn linked_worktree_uses_common_gitdir_as_metadata_root() {
        let (_tmp, repo) = tempdir_repo();
        commit_file(&repo, "keep.txt", "v1", "init");
        let linked_parent = TempDir::new().expect("linked worktree parent");
        let linked_workdir = linked_parent.path().join("linked");
        let worktree = repo
            .worktree("linked", &linked_workdir, None)
            .expect("create linked worktree");
        drop(worktree);

        assert_eq!(
            gitdirs_for(&linked_workdir),
            vec![normalize_event_path(repo.commondir())]
        );
    }
}
