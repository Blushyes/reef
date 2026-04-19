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
    let workdir = std::fs::canonicalize(&workdir).unwrap_or(workdir);
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

    if let Err(e) = watcher.watch(&workdir, RecursiveMode::Recursive) {
        eprintln!("[reef] fs watcher watch({:?}) failed: {e}", workdir);
        return;
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
            .matched_path_or_any_parents(path, is_dir)
            .is_ignore()
        {
            continue;
        }
        return true;
    }
    false
}
