use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedLocalTarget {
    Dir(PathBuf),
    File { workdir: PathBuf, rel: PathBuf },
}

pub fn expand_tilde_path(raw: &str) -> Result<PathBuf, String> {
    if raw == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set; cannot expand `~`".to_string());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set; cannot expand `~/`".to_string())?;
        return Ok(home.join(rest));
    }
    Ok(PathBuf::from(raw))
}

pub fn resolve_local_target(raw: &str) -> Result<ResolvedLocalTarget, String> {
    let expanded = expand_tilde_path(raw)?;
    let canonical =
        std::fs::canonicalize(&expanded).map_err(|e| format!("cannot open `{raw}`: {e}"))?;
    if canonical.is_dir() {
        return Ok(ResolvedLocalTarget::Dir(canonical));
    }
    if canonical.is_file() {
        let parent = canonical
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| format!("`{raw}` has no parent directory"))?;
        let workdir = match git2::Repository::discover(&parent) {
            Ok(repo) => repo.workdir().map(PathBuf::from).unwrap_or(parent.clone()),
            Err(_) => parent.clone(),
        };
        let rel = match canonical.strip_prefix(&workdir) {
            Ok(rel) => PathBuf::from(rel),
            Err(_) => canonical
                .file_name()
                .map(PathBuf::from)
                .ok_or_else(|| format!("cannot derive file name for `{raw}`"))?,
        };
        return Ok(ResolvedLocalTarget::File { workdir, rel });
    }
    Err(format!("`{raw}` is neither a file nor a directory"))
}

pub fn workdir_relative_path(root: &Path, abs: &Path) -> Option<PathBuf> {
    let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let canon_abs =
        std::fs::canonicalize(abs).unwrap_or_else(|_| match (abs.parent(), abs.file_name()) {
            (Some(parent), Some(name)) => std::fs::canonicalize(parent)
                .map(|canonical_parent| canonical_parent.join(name))
                .unwrap_or_else(|_| abs.to_path_buf()),
            _ => abs.to_path_buf(),
        });

    canon_abs
        .strip_prefix(&canon_root)
        .or_else(|_| canon_abs.strip_prefix(root))
        .ok()
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::{ResolvedLocalTarget, resolve_local_target, workdir_relative_path};

    #[test]
    fn resolve_local_target_file_without_repo_uses_parent_workdir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("scratch.txt");
        std::fs::write(&file, "hi").unwrap();

        let resolved = resolve_local_target(file.to_str().unwrap()).unwrap();

        assert_eq!(
            resolved,
            ResolvedLocalTarget::File {
                workdir: std::fs::canonicalize(tmp.path()).unwrap(),
                rel: std::path::PathBuf::from("scratch.txt"),
            }
        );
    }

    #[test]
    fn resolve_local_target_file_inside_repo_uses_repo_root() {
        let (tmp, _repo) = test_support::tempdir_repo();
        let dir = tmp.path().join("src");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let resolved = resolve_local_target(file.to_str().unwrap()).unwrap();

        assert_eq!(
            resolved,
            ResolvedLocalTarget::File {
                workdir: std::fs::canonicalize(tmp.path()).unwrap(),
                rel: std::path::PathBuf::from("src/main.rs"),
            }
        );
    }

    #[test]
    fn workdir_relative_path_accepts_existing_child() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("src").join("main.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "fn main() {}").unwrap();

        assert_eq!(
            workdir_relative_path(tmp.path(), &file),
            Some(std::path::PathBuf::from("src/main.rs"))
        );
    }

    #[test]
    fn workdir_relative_path_accepts_missing_child_when_parent_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let parent = tmp.path().join("src");
        std::fs::create_dir_all(&parent).unwrap();

        assert_eq!(
            workdir_relative_path(tmp.path(), &parent.join("generated.rs")),
            Some(std::path::PathBuf::from("src/generated.rs"))
        );
    }

    #[test]
    fn workdir_relative_path_rejects_outside_path() {
        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let file = outside.path().join("dep.rs");
        std::fs::write(&file, "fn dep() {}").unwrap();

        assert_eq!(workdir_relative_path(root.path(), &file), None);
    }
}
