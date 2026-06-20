use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

const DEBOUNCE: Duration = Duration::from_millis(300);

/// Watch `workdir` recursively and emit `()` on the returned receiver whenever a
/// debounced non-ignored event fires. When the watcher can't start, the sender
/// is dropped so callers observe `Disconnected` and simply stop polling.
pub fn spawn(workdir: PathBuf) -> mpsc::Receiver<()> {
    let (out_tx, out_rx) = mpsc::channel::<()>();
    let _ = thread::Builder::new()
        .name("reef-fs-watcher".into())
        .spawn(move || run(workdir, out_tx));
    out_rx
}

fn run(workdir: PathBuf, out_tx: mpsc::Sender<()>) {
    // macOS tempdirs and symlinked workdirs: notify delivers canonical paths,
    // so prefix checks would fail without canonicalizing up front.
    let original_workdir = workdir;
    let workdir = std::fs::canonicalize(&original_workdir).unwrap_or(original_workdir.clone());
    let gitdir = workdir.join(".git");

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

    let mut watch_roots = vec![original_workdir];
    if watch_roots[0] != workdir {
        watch_roots.push(workdir.clone());
    }
    for root in &watch_roots {
        if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
            eprintln!("[reef] fs watcher watch({:?}) failed: {e}", root);
            return;
        }
    }

    let mut pending = false;
    loop {
        let timeout = if pending {
            DEBOUNCE
        } else {
            Duration::from_secs(3600)
        };
        match rx.recv_timeout(timeout) {
            Ok(Ok(ev)) => {
                if is_relevant(&ev, &gitdir, &workdir, &repo_gi) {
                    pending = true;
                }
            }
            Ok(Err(_)) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if pending {
                    pending = false;
                    if out_tx.send(()).is_err() {
                        return;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn build_repo_gitignore(workdir: &Path) -> Gitignore {
    let mut b = GitignoreBuilder::new(workdir);
    let _ = b.add(workdir.join(".gitignore"));
    let _ = b.add(workdir.join(".git").join("info").join("exclude"));
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

fn is_relevant(ev: &Event, gitdir: &Path, workdir: &Path, repo_gi: &Gitignore) -> bool {
    for path in &ev.paths {
        let path = normalize_event_path(path);
        if path.starts_with(gitdir) {
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
        return true;
    }
    false
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
