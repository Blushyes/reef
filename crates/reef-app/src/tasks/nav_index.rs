//! Incremental navigation indexing. IO and parsing stay on the navigation worker.

use std::path::PathBuf;
use std::sync::Arc;

use reef_core::nav::workspace::MAX_FILE_BYTES_INDEX;
use reef_core::nav::{WorkspaceIndex, WorkspaceIndexFile, build_workspace_index};
use reef_io::{Backend, CancellationToken, FsChange, WalkOpts};

pub(super) fn update(
    backend: &dyn Backend,
    previous: Option<Arc<WorkspaceIndex>>,
    change: FsChange,
    cancellation: &CancellationToken,
) -> Option<Arc<WorkspaceIndex>> {
    if cancellation.is_cancelled() {
        return None;
    }
    let root = backend.workdir_path();
    let full =
        previous.is_none() || change.repo_presence_changed || change.workspace_paths.is_empty();
    let scopes: Vec<PathBuf> = if full {
        Vec::new()
    } else {
        change
            .workspace_paths
            .into_iter()
            .map(|path| {
                // Ignore-rule edits may add or remove any file in their subtree.
                if matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(".gitignore" | ".ignore")
                ) {
                    path.parent().unwrap_or(&path).to_path_buf()
                } else {
                    path
                }
            })
            .collect()
    };
    let walk = reef_io::local::walk_paths_in(&root, &WalkOpts::default(), scopes.clone());
    let paths: Vec<PathBuf> = walk
        .paths
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| reef_core::nav::NavLang::from_path(path).is_some())
        .collect();
    if !full
        && paths.is_empty()
        && let Some(previous) = previous.as_ref()
        && !scopes.iter().any(|path| previous.contains_path(path))
    {
        return Some(Arc::clone(previous));
    }
    let files = paths
        .into_iter()
        .take_while(|_| !cancellation.is_cancelled())
        .filter_map(|path| {
            let bytes = backend.read_file(&path, MAX_FILE_BYTES_INDEX + 1).ok()?;
            if bytes.len() as u64 > MAX_FILE_BYTES_INDEX {
                return None;
            }
            Some(WorkspaceIndexFile {
                path,
                source: Arc::from(bytes.into_boxed_slice()),
            })
        });
    let index = if !full && let Some(previous) = previous {
        let mut index = (*previous).clone();
        index.update_files(&scopes, files);
        index
    } else {
        build_workspace_index(root, files)
    };
    (!cancellation.is_cancelled()).then(|| Arc::new(index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reef_core::nav::NavLang;
    use reef_io::LocalBackend;
    use test_support::{tempdir_repo, write_file};

    fn update(
        backend: &dyn Backend,
        previous: Option<Arc<WorkspaceIndex>>,
        change: FsChange,
    ) -> Option<Arc<WorkspaceIndex>> {
        super::update(backend, previous, change, &CancellationToken::default())
    }

    #[test]
    fn cancelled_workspace_does_not_start_indexing() {
        let (temp, _) = tempdir_repo();
        let backend = LocalBackend::open_at(temp.path().to_path_buf());
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        assert!(super::update(&backend, None, FsChange::default(), &cancellation).is_none());
    }

    fn changes(paths: &[&str]) -> FsChange {
        FsChange {
            workspace_changed: true,
            workspace_paths: paths.iter().map(PathBuf::from).collect(),
            ..FsChange::default()
        }
    }

    fn definitions(index: &WorkspaceIndex, name: &str) -> Vec<reef_core::nav::Location> {
        index.definitions_for(name, NavLang::Rust, None)
    }

    #[test]
    fn precise_edits_leave_unreported_files_and_previous_snapshot_untouched() {
        let (temp, repo) = tempdir_repo();
        write_file(&repo, "a.rs", "fn before() {}");
        write_file(&repo, "b.rs", "fn retained() {}");
        let backend = LocalBackend::open_at(temp.path().to_path_buf());
        let before = update(&backend, None, FsChange::default()).unwrap();
        write_file(&repo, "a.rs", "fn after() {}");
        write_file(&repo, "b.rs", "fn unreported() {}");
        let after = update(&backend, Some(Arc::clone(&before)), changes(&["a.rs"])).unwrap();
        assert_eq!(definitions(&after, "after").len(), 1);
        assert!(definitions(&after, "before").is_empty());
        assert_eq!(definitions(&after, "retained").len(), 1);
        assert!(definitions(&after, "unreported").is_empty());
        assert_eq!(definitions(&before, "before").len(), 1);
        std::fs::remove_file(temp.path().join("a.rs")).unwrap();
        let removed = update(&backend, Some(after), changes(&["a.rs"])).unwrap();
        assert!(definitions(&removed, "after").is_empty());
        assert_eq!(removed.file_count(), 1);
    }

    #[test]
    fn non_code_and_ignored_changes_reuse_the_entire_snapshot() {
        let (temp, repo) = tempdir_repo();
        write_file(&repo, ".gitignore", "ignored/\n");
        write_file(&repo, "src/a.rs", "fn visible() {}");
        write_file(&repo, "ignored/b.rs", "fn hidden() {}");
        write_file(&repo, "movie.json", "{}");
        let backend = LocalBackend::open_at(temp.path().to_path_buf());
        let before = update(&backend, None, FsChange::default()).unwrap();
        let after = update(
            &backend,
            Some(Arc::clone(&before)),
            changes(&["movie.json", "ignored/b.rs"]),
        )
        .unwrap();
        assert!(Arc::ptr_eq(&before, &after));
    }

    #[test]
    fn directory_rename_replaces_descendants_and_respects_nested_ignore_rules() {
        let (temp, repo) = tempdir_repo();
        write_file(&repo, "old.data/.gitignore", "ignored.rs\n");
        write_file(&repo, "old.data/visible.rs", "fn visible() {}");
        write_file(&repo, "old.data/ignored.rs", "fn hidden() {}");
        let backend = LocalBackend::open_at(temp.path().to_path_buf());
        let before = update(&backend, None, FsChange::default()).unwrap();
        std::fs::rename(temp.path().join("old.data"), temp.path().join("new.data")).unwrap();
        let after = update(&backend, Some(before), changes(&["old.data", "new.data"])).unwrap();
        assert_eq!(after.file_count(), 1);
        assert_eq!(
            definitions(&after, "visible")[0].path.as_deref(),
            Some(std::path::Path::new("new.data/visible.rs"))
        );
        assert!(definitions(&after, "hidden").is_empty());
    }

    #[test]
    fn ignore_rule_edits_add_and_remove_indexed_files() {
        let (temp, repo) = tempdir_repo();
        write_file(&repo, "src/.gitignore", "hidden.rs\n");
        write_file(&repo, "src/hidden.rs", "fn hidden() {}");
        write_file(&repo, "other.rs", "fn retained() {}");
        let backend = LocalBackend::open_at(temp.path().to_path_buf());
        let before = update(&backend, None, FsChange::default()).unwrap();
        assert!(definitions(&before, "hidden").is_empty());
        write_file(&repo, "src/.gitignore", "");
        let shown = update(&backend, Some(before), changes(&["src/.gitignore"])).unwrap();
        assert_eq!(definitions(&shown, "hidden").len(), 1);
        write_file(&repo, "src/.gitignore", "hidden.rs\n");
        let hidden = update(&backend, Some(shown), changes(&["src/.gitignore"])).unwrap();
        assert!(definitions(&hidden, "hidden").is_empty());
        assert_eq!(definitions(&hidden, "retained").len(), 1);
    }

    #[test]
    fn unknown_paths_reconcile_the_full_workspace() {
        let (temp, repo) = tempdir_repo();
        write_file(&repo, "a.rs", "fn before() {}");
        let backend = LocalBackend::open_at(temp.path().to_path_buf());
        let before = update(&backend, None, FsChange::default()).unwrap();
        write_file(&repo, "a.rs", "fn after() {}");
        write_file(&repo, "b.rs", "fn added() {}");
        let after = update(&backend, Some(before), changes(&[])).unwrap();
        assert!(definitions(&after, "before").is_empty());
        assert_eq!(definitions(&after, "after").len(), 1);
        assert_eq!(definitions(&after, "added").len(), 1);
    }

    #[test]
    fn newly_oversized_or_binary_files_lose_their_old_symbols() {
        let (temp, repo) = tempdir_repo();
        write_file(&repo, "a.rs", "fn old_a() {}");
        write_file(&repo, "b.rs", "fn old_b() {}");
        let backend = LocalBackend::open_at(temp.path().to_path_buf());
        let before = update(&backend, None, FsChange::default()).unwrap();
        std::fs::write(
            temp.path().join("a.rs"),
            vec![b' '; MAX_FILE_BYTES_INDEX as usize + 1],
        )
        .unwrap();
        std::fs::write(temp.path().join("b.rs"), b"\0binary").unwrap();
        let after = update(&backend, Some(before), changes(&["a.rs", "b.rs"])).unwrap();
        assert_eq!(after.file_count(), 0);
    }
}
