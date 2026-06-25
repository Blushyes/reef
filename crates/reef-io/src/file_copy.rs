use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::Backend;

/// Copy local filesystem sources into a backend directory.
///
/// Sources may already live under the backend workdir or may be external
/// host-local paths, such as files dropped from Finder. Workdir-local sources
/// use backend-native copy operations; external sources use
/// [`Backend::upload_from_local`] so remote backends can transfer them across
/// the SSH boundary. Local destinations auto-rename top-level collisions
/// (`foo.txt` -> `foo (1).txt`); remote destinations let the agent report a
/// path-exists error because the client cannot probe remote filenames without
/// adding another round trip per candidate.
pub fn copy_local_sources_to_backend(
    backend: &dyn Backend,
    sources: &[PathBuf],
    dest_dir: &Path,
) -> Result<usize, String> {
    let workdir = backend.workdir_path();
    let dest_rel = dest_dir
        .strip_prefix(&workdir)
        .map(PathBuf::from)
        .unwrap_or_default();
    let is_remote = backend.is_remote();

    let canon_dest = if is_remote {
        None
    } else {
        if !dest_dir.is_dir() {
            return Err(format!("destination is not a directory: {:?}", dest_dir));
        }
        Some(
            dest_dir
                .canonicalize()
                .map_err(|e| format!("cannot canonicalize destination {:?}: {}", dest_dir, e))?,
        )
    };

    let mut count = 0;
    for source in sources {
        let basename = source
            .file_name()
            .ok_or_else(|| format!("source has no basename: {:?}", source))?;

        if !is_remote && source.is_dir() {
            let canon_src = source
                .canonicalize()
                .map_err(|e| format!("cannot canonicalize source {:?}: {}", source, e))?;
            let canon_dest = canon_dest.as_ref().unwrap();
            if canon_dest == &canon_src || canon_dest.starts_with(&canon_src) {
                return Err(format!(
                    "cannot copy {:?} into itself or a descendant {:?}",
                    source, dest_dir
                ));
            }
        }

        let final_dest = if is_remote {
            dest_dir.join(basename)
        } else {
            resolve_name_conflict(dest_dir, basename)
        };
        let final_rel = final_dest
            .strip_prefix(&workdir)
            .map(PathBuf::from)
            .unwrap_or_else(|_| dest_rel.join(basename));

        match (source.strip_prefix(&workdir).ok(), source.is_dir()) {
            (Some(src_rel), true) => backend
                .copy_dir_recursive(src_rel, &final_rel)
                .map_err(|e| format!("copy {:?} -> {:?}: {}", source, final_dest, e))?,
            (Some(src_rel), false) => backend
                .copy_file(src_rel, &final_rel)
                .map_err(|e| format!("copy {:?} -> {:?}: {}", source, final_dest, e))?,
            (None, _) => backend
                .upload_from_local(source, &final_rel)
                .map_err(|e| format!("upload {:?} -> {:?}: {}", source, final_dest, e))?,
        }
        count += 1;
    }
    Ok(count)
}

fn resolve_name_conflict(dest_dir: &Path, basename: &OsStr) -> PathBuf {
    let candidate = dest_dir.join(basename);
    if !candidate.exists() {
        return candidate;
    }
    let name = basename.to_string_lossy().into_owned();
    let (stem, ext) = split_stem_ext(&name);
    for n in 1..u32::MAX {
        let renamed = match ext {
            Some(e) => format!("{} ({}).{}", stem, n, e),
            None => format!("{} ({})", stem, n),
        };
        let candidate = dest_dir.join(&renamed);
        if !candidate.exists() {
            return candidate;
        }
    }
    dest_dir.join(format!("{} copy", name))
}

fn split_stem_ext(name: &str) -> (&str, Option<&str>) {
    let trimmed = name.trim_start_matches('.');
    let leading_dots = name.len() - trimmed.len();
    match trimmed.rfind('.') {
        Some(rel) => {
            let abs = leading_dots + rel;
            let (stem, ext) = name.split_at(abs);
            (stem, Some(&ext[1..]))
        }
        None => (name, None),
    }
}

#[cfg(test)]
mod tests {
    use super::{copy_local_sources_to_backend, resolve_name_conflict, split_stem_ext};
    use crate::LocalBackend;
    use std::fs;

    #[test]
    fn split_stem_ext_basic() {
        assert_eq!(split_stem_ext("foo.txt"), ("foo", Some("txt")));
        assert_eq!(
            split_stem_ext("archive.tar.gz"),
            ("archive.tar", Some("gz"))
        );
        assert_eq!(split_stem_ext("README"), ("README", None));
        assert_eq!(split_stem_ext(".env"), (".env", None));
        assert_eq!(split_stem_ext(".env.local"), (".env", Some("local")));
    }

    #[test]
    fn resolve_name_conflict_increments() {
        let tmp = tempfile::TempDir::new().unwrap();
        let basename = std::ffi::OsString::from("foo.txt");

        let p0 = resolve_name_conflict(tmp.path(), &basename);
        assert_eq!(p0.file_name().unwrap(), "foo.txt");

        fs::write(&p0, "").unwrap();
        let p1 = resolve_name_conflict(tmp.path(), &basename);
        assert_eq!(p1.file_name().unwrap(), "foo (1).txt");

        fs::write(&p1, "").unwrap();
        let p2 = resolve_name_conflict(tmp.path(), &basename);
        assert_eq!(p2.file_name().unwrap(), "foo (2).txt");
    }

    #[test]
    fn resolve_name_conflict_dotfile() {
        let tmp = tempfile::TempDir::new().unwrap();
        let basename = std::ffi::OsString::from(".env");
        fs::write(tmp.path().join(".env"), "").unwrap();
        let path = resolve_name_conflict(tmp.path(), &basename);
        assert_eq!(path.file_name().unwrap(), ".env (1)");
    }

    #[test]
    fn copy_local_sources_file_into_dir() {
        let src_tmp = tempfile::TempDir::new().unwrap();
        let dst_tmp = tempfile::TempDir::new().unwrap();
        let src = src_tmp.path().join("alpha.txt");
        fs::write(&src, "hello").unwrap();

        let backend = LocalBackend::open_at(dst_tmp.path().to_path_buf());
        let count = copy_local_sources_to_backend(&backend, &[src], dst_tmp.path()).unwrap();

        assert_eq!(count, 1);
        assert_eq!(
            fs::read_to_string(dst_tmp.path().join("alpha.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn copy_local_sources_recurses_into_directories() {
        let src_tmp = tempfile::TempDir::new().unwrap();
        let dst_tmp = tempfile::TempDir::new().unwrap();

        let pkg = src_tmp.path().join("pkg");
        fs::create_dir(&pkg).unwrap();
        fs::write(pkg.join("one.txt"), "1").unwrap();
        fs::create_dir(pkg.join("nested")).unwrap();
        fs::write(pkg.join("nested").join("two.txt"), "2").unwrap();

        let backend = LocalBackend::open_at(dst_tmp.path().to_path_buf());
        let count =
            copy_local_sources_to_backend(&backend, std::slice::from_ref(&pkg), dst_tmp.path())
                .unwrap();

        assert_eq!(count, 1);
        assert_eq!(
            fs::read_to_string(dst_tmp.path().join("pkg").join("one.txt")).unwrap(),
            "1"
        );
        assert_eq!(
            fs::read_to_string(dst_tmp.path().join("pkg").join("nested").join("two.txt")).unwrap(),
            "2"
        );
    }

    #[test]
    fn copy_local_sources_auto_renames_on_collision() {
        let src_tmp = tempfile::TempDir::new().unwrap();
        let dst_tmp = tempfile::TempDir::new().unwrap();

        let src = src_tmp.path().join("dup.txt");
        fs::write(&src, "new").unwrap();
        fs::write(dst_tmp.path().join("dup.txt"), "old").unwrap();

        let backend = LocalBackend::open_at(dst_tmp.path().to_path_buf());
        copy_local_sources_to_backend(&backend, &[src], dst_tmp.path()).unwrap();

        assert_eq!(
            fs::read_to_string(dst_tmp.path().join("dup.txt")).unwrap(),
            "old"
        );
        assert_eq!(
            fs::read_to_string(dst_tmp.path().join("dup (1).txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn copy_local_sources_rejects_non_directory_dest() {
        let dst_tmp = tempfile::TempDir::new().unwrap();
        let not_a_dir = dst_tmp.path().join("nope.txt");
        fs::write(&not_a_dir, "").unwrap();
        let src_tmp = tempfile::TempDir::new().unwrap();
        let src = src_tmp.path().join("x");
        fs::write(&src, "").unwrap();

        let backend = LocalBackend::open_at(dst_tmp.path().to_path_buf());
        assert!(copy_local_sources_to_backend(&backend, &[src], &not_a_dir).is_err());
    }

    #[test]
    fn copy_local_sources_blocks_copy_into_self() {
        let tmp = tempfile::TempDir::new().unwrap();
        let pkg = tmp.path().join("pkg");
        fs::create_dir(&pkg).unwrap();
        fs::write(pkg.join("a.txt"), "").unwrap();

        let backend = LocalBackend::open_at(tmp.path().to_path_buf());
        let err =
            copy_local_sources_to_backend(&backend, std::slice::from_ref(&pkg), &pkg).unwrap_err();

        assert!(
            err.contains("into itself") || err.contains("descendant"),
            "expected self-copy rejection, got: {err}"
        );
    }

    #[test]
    fn copy_local_sources_blocks_copy_into_descendant() {
        let tmp = tempfile::TempDir::new().unwrap();
        let pkg = tmp.path().join("pkg");
        let nested = pkg.join("nested");
        fs::create_dir_all(&nested).unwrap();

        let backend = LocalBackend::open_at(tmp.path().to_path_buf());
        let err = copy_local_sources_to_backend(&backend, std::slice::from_ref(&pkg), &nested)
            .unwrap_err();

        assert!(err.contains("into itself") || err.contains("descendant"));
    }

    #[test]
    fn copy_local_sources_allows_sibling_dest_same_parent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let pkg = tmp.path().join("pkg");
        fs::create_dir(&pkg).unwrap();
        fs::write(pkg.join("a.txt"), "hello").unwrap();

        let backend = LocalBackend::open_at(tmp.path().to_path_buf());
        let count = copy_local_sources_to_backend(&backend, std::slice::from_ref(&pkg), tmp.path())
            .unwrap();

        assert_eq!(count, 1);
        assert_eq!(
            fs::read_to_string(tmp.path().join("pkg (1)").join("a.txt")).unwrap(),
            "hello"
        );
    }
}
