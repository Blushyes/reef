//! Shared test helpers across reef crates.
//!
//! All items here are `pub` and consumed via `[dev-dependencies]`.

use git2::{Repository, Signature};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Locate the `reef-agent` binary across all the target-dir layouts CI runs
/// our tests under:
///
/// - default: `<workspace>/target/{debug,release}/reef-agent`
/// - `cargo llvm-cov`: `<workspace>/target/llvm-cov-target/{debug,release}/reef-agent`
/// - `CARGO_TARGET_DIR=...`: `<dir>/{debug,release}/reef-agent`
///
/// Panics with the searched paths so a missing binary is easy to triage.
/// Used by every `tests/backend_*_loopback.rs` helper that spawns the agent
/// as a subprocess.
pub fn agent_bin() -> PathBuf {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));

    let mut target_dirs: Vec<PathBuf> = Vec::new();
    if let Ok(d) = std::env::var("CARGO_TARGET_DIR") {
        target_dirs.push(PathBuf::from(d));
    }
    target_dirs.push(workspace_root.join("target").join("llvm-cov-target"));
    target_dirs.push(workspace_root.join("target"));

    let mut tried = Vec::new();
    for target in &target_dirs {
        for profile in ["debug", "release"] {
            let candidate = target.join(profile).join("reef-agent");
            if candidate.exists() {
                return candidate;
            }
            tried.push(candidate.display().to_string());
        }
    }
    panic!(
        "reef-agent binary not found; tried:\n  - {}",
        tried.join("\n  - ")
    );
}

/// Process-wide lock for HOME mutations. Multiple `#[cfg(test)] mod tests`
/// in different `src/*.rs` files compile into the SAME `cargo test --lib`
/// binary, so they share the global env-var space. A per-file lock works
/// only when one file owns every HOME-touching test in its binary; the
/// moment a second file is added, the two locks no longer serialise
/// against each other and `HomeGuard` mid-restore can be observed by the
/// other test as a corrupt $HOME.
///
/// All lib-side tests that touch HOME (and any integration test that
/// doesn't already use its own per-file lock) should grab THIS lock for
/// the lifetime of the `HomeGuard`. Each integration test (`tests/*.rs`)
/// is its own binary, so they could in principle keep per-file locks —
/// but pointing them at the shared lock costs nothing and removes a
/// footgun for the next contributor.
pub static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Redirect `$HOME` to a path for the lifetime of the guard, then restore
/// whatever value was there before (or remove it if HOME was unset).
///
/// `std::env::set_var` is process-global, so callers MUST serialise HOME
/// mutations. Use [`HOME_LOCK`] above unless you have a specific reason
/// to keep a per-file lock.
///
/// Typical use:
/// ```no_run
/// use test_support::{HOME_LOCK, HomeGuard, tempdir_repo};
///
/// # fn body() {
/// let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
/// let (tmp, _repo) = tempdir_repo();
/// let _home = HomeGuard::enter(tmp.path());
/// // ... test body — any `std::env::var("HOME")` reads the tempdir
/// # }
/// ```
pub struct HomeGuard {
    original: Option<OsString>,
}

impl HomeGuard {
    pub fn enter(path: &Path) -> Self {
        let original = std::env::var_os("HOME");
        // SAFETY: caller must hold a process-wide HOME_LOCK for the
        // duration of this guard's lifetime. See the type-level doc.
        unsafe {
            std::env::set_var("HOME", path);
        }
        Self { original }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        // SAFETY: same as `enter`; the lock the caller holds spans the
        // guard's whole lifetime, including this Drop.
        unsafe {
            if let Some(v) = self.original.take() {
                std::env::set_var("HOME", v);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }
}

/// Swap the process-wide current working directory for the duration of a
/// test, and restore it on drop. Same serialisation warning as
/// `HomeGuard` — `set_current_dir` is process-global, so callers must
/// hold a test-file-local `Mutex<()>` for the guard's whole lifetime.
pub struct CwdGuard {
    original: std::path::PathBuf,
}

impl CwdGuard {
    pub fn enter(path: &Path) -> Self {
        let original = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(path).expect("set_current_dir");
        Self { original }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

/// Pin the UI language to English so test output stays deterministic
/// across developer locales. i18n's `lang()` caches the choice in a
/// process-wide `OnceLock` on first call, so callers should invoke this
/// before any code path that reads `lang()`. Idempotent across calls
/// within the same test binary.
///
/// Same serialisation warning as `HomeGuard`/`CwdGuard`: `set_var` is
/// process-global, so callers must hold a test-file-local `Mutex<()>`.
pub fn force_en_lang() {
    if std::env::var_os("REEF_LANG").as_deref() == Some(std::ffi::OsStr::new("en")) {
        return;
    }
    // SAFETY: callers hold a test-file-local mutex, and no other test
    // touches `REEF_LANG` concurrently. Must precede any `t(Msg::*)` so
    // the OnceLock seats with the English variant.
    unsafe {
        std::env::set_var("REEF_LANG", "en");
    }
}

/// Initialize a real git repository in a temp directory. Sets the required
/// `user.name` and `user.email` config so commits don't depend on the caller's
/// global git config (critical for CI).
pub fn tempdir_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().expect("create tempdir");
    let repo = Repository::init(dir.path()).expect("git init");
    {
        let mut cfg = repo.config().expect("open repo config");
        cfg.set_str("user.name", "Tester").unwrap();
        cfg.set_str("user.email", "tester@example.com").unwrap();
    }
    (dir, repo)
}

/// Make an initial commit in the given repo. Writes `content` to `<workdir>/<path>`,
/// stages it, and commits with the message `subject`. Returns the commit OID.
pub fn commit_file(repo: &Repository, path: &str, content: &str, subject: &str) -> git2::Oid {
    let workdir = repo.workdir().expect("repo has workdir").to_path_buf();
    let full = workdir.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&full, content).unwrap();

    let mut index = repo.index().unwrap();
    index.add_path(Path::new(path)).unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();

    let sig = Signature::now("Tester", "tester@example.com").unwrap();
    let parents: Vec<git2::Commit> = repo
        .head()
        .ok()
        .and_then(|h| h.target())
        .and_then(|oid| repo.find_commit(oid).ok())
        .into_iter()
        .collect();
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();

    repo.commit(Some("HEAD"), &sig, &sig, subject, &tree, &parent_refs)
        .unwrap()
}

/// Write a file in the repo's workdir without staging or committing.
/// Useful for exercising "unstaged" / "untracked" code paths.
pub fn write_file(repo: &Repository, path: &str, content: &str) {
    let workdir = repo.workdir().expect("repo has workdir").to_path_buf();
    let full = workdir.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&full, content).unwrap();
}

/// Write a small solid-colour PNG at `<dir>/<name>`. Used by image-preview
/// tests — keeping the fixture generated at runtime means the repo stays
/// free of binary blobs (which are a pain to review and diff).
///
/// The returned buffer is the same bytes written to disk, so tests that
/// want to sniff magic bytes without hitting the filesystem can use it
/// directly.
pub fn write_png(dir: &Path, name: &str, width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    use image::{ImageBuffer, Rgb};
    let img = ImageBuffer::from_pixel(width, height, Rgb(rgb));
    let full = dir.join(name);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    img.save_with_format(&full, image::ImageFormat::Png)
        .expect("write PNG fixture");
    fs::read(&full).expect("read back PNG fixture")
}

/// In-memory PNG bytes for tests that want to feed buffers to a parser
/// without touching disk (MIME sniffing, magic-byte detection).
pub fn png_bytes(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    use image::{ImageBuffer, ImageFormat, Rgb};
    use std::io::Cursor;
    let img = ImageBuffer::from_pixel(width, height, Rgb(rgb));
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
        .expect("encode PNG");
    buf
}

/// Write a striped PNG (alternating horizontal rows of `rgb_a` and `rgb_b`)
/// at `<dir>/<name>`. Stripes are required for halfblocks snapshot tests:
/// the halfblocks encoder collapses cells whose upper and lower halves
/// share a color to a literal space, so a uniform solid-colour PNG
/// produces no visible characters in a text dump. Alternating rows force
/// different upper/lower cells and light up the `▀` glyph.
pub fn write_striped_png(
    dir: &Path,
    name: &str,
    width: u32,
    height: u32,
    rgb_a: [u8; 3],
    rgb_b: [u8; 3],
) {
    use image::{ImageBuffer, Rgb};
    let img = ImageBuffer::from_fn(width, height, |_, y| {
        if y % 2 == 0 { Rgb(rgb_a) } else { Rgb(rgb_b) }
    });
    let full = dir.join(name);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    img.save_with_format(&full, image::ImageFormat::Png)
        .expect("write striped PNG fixture");
}
