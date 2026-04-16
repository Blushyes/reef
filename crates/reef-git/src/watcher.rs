use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use reef_protocol::RpcMessage;

use crate::Writer;

const DEBOUNCE: Duration = Duration::from_millis(300);

pub fn spawn(workdir: PathBuf, gitdir: PathBuf, writer: Writer) {
    let _ = thread::Builder::new()
        .name("reef-git-watcher".into())
        .spawn(move || run(workdir, gitdir, writer));
}

fn run(workdir: PathBuf, gitdir: PathBuf, writer: Writer) {
    // Open a separate repo handle in this thread for gitignore checks.
    // git2::Repository is !Send, so it must be created here, not captured.
    let ignore_repo = git2::Repository::open(&workdir).ok();

    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher: RecommendedWatcher =
        match notify::recommended_watcher(move |res: notify::Result<Event>| {
            let _ = tx.send(res);
        }) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[reef-git] watcher init failed: {e}");
                return;
            }
        };

    if let Err(e) = watcher.watch(&workdir, RecursiveMode::Recursive) {
        eprintln!("[reef-git] watcher.watch failed: {e}");
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
                if is_relevant(&ev, &gitdir, &workdir, ignore_repo.as_ref()) {
                    pending = true;
                }
            }
            Ok(Err(_)) => {} // notify backend hiccup, ignore
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if pending {
                    pending = false;
                    writer.send(&RpcMessage::notification(
                        "reef/statusChanged",
                        serde_json::json!({}),
                    ));
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn is_relevant(
    ev: &Event,
    gitdir: &Path,
    workdir: &Path,
    repo: Option<&git2::Repository>,
) -> bool {
    for path in &ev.paths {
        if let Ok(rel) = path.strip_prefix(gitdir) {
            if is_relevant_gitdir_path(rel) {
                return true;
            }
        } else if path.starts_with(workdir) {
            if !is_ignored(repo, path, workdir) {
                return true;
            }
        }
    }
    false
}

fn is_relevant_gitdir_path(rel: &Path) -> bool {
    // Lock files are transient writes during git operations — the real change
    // arrives via the rename to the unlocked name, so skip *.lock.
    if rel.extension().and_then(|e| e.to_str()) == Some("lock") {
        return false;
    }
    let Some(first) = rel.components().next() else {
        // event on .git/ itself — be conservative and refresh
        return true;
    };
    let name = first.as_os_str().to_str().unwrap_or("");
    matches!(
        name,
        "HEAD"
            | "ORIG_HEAD"
            | "MERGE_HEAD"
            | "FETCH_HEAD"
            | "CHERRY_PICK_HEAD"
            | "REBASE_HEAD"
            | "index"
            | "packed-refs"
            | "refs"
    )
}

fn is_ignored(repo: Option<&git2::Repository>, path: &Path, workdir: &Path) -> bool {
    let Some(repo) = repo else { return false };
    let rel = path.strip_prefix(workdir).unwrap_or(path);
    repo.status_should_ignore(rel).unwrap_or(false)
}
