//! Local vs Remote parity — walk_repo_paths + search_content.
//!
//! Runs the same walk/search against identically-seeded tempdirs on both
//! backends and asserts sorted equality. Any drift here means the agent
//! side and the local side disagree on gitignore / binary-detection /
//! match-range semantics — worth catching immediately.
//!
//! Post-v4 `SearchContent` is streaming: both backends deliver hits via
//! a `FnMut(Vec<ContentMatchHit>)` sink rather than a single collected
//! response. The parity test accumulates all chunks on both sides and
//! compares the flattened hit lists; a second test asserts both
//! backends actually *do* stream (more than one chunk) when the match
//! set is large enough to cross `CHUNK_TARGET_HITS`.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use reef::backend::{
    Backend, ContentMatchHit, ContentSearchRequest, LocalBackend, RemoteBackend, WalkOpts,
};
use tempfile::TempDir;

static BACKEND_LOCK: Mutex<()> = Mutex::new(());

fn agent_bin() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_reef-agent") {
        return PathBuf::from(path);
    }
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let root = PathBuf::from(manifest_dir);
    for profile in ["debug", "release"] {
        let candidate = root.join("target").join(profile).join("reef-agent");
        if candidate.exists() {
            return candidate;
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

fn seed_tree(root: &Path) {
    // Minimal fake .git so ignore::WalkBuilder honours the .gitignore.
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
    std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();

    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn foo() { bar() }\nfn baz() {}").unwrap();
    std::fs::write(root.join("src/other.rs"), "fn other() { foo() }").unwrap();
    std::fs::write(root.join("README.md"), "Use foo() for the thing\n").unwrap();
    std::fs::create_dir(root.join("ignored")).unwrap();
    std::fs::write(root.join("ignored/secret.rs"), "fn secret() { foo() }").unwrap();
}

#[test]
fn walk_repo_paths_parity() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let l_tmp = TempDir::new().unwrap();
    let r_tmp = TempDir::new().unwrap();
    seed_tree(l_tmp.path());
    seed_tree(r_tmp.path());

    let l = LocalBackend::open_at(l_tmp.path().to_path_buf());
    let r = spawn_remote(r_tmp.path());

    let opts = WalkOpts::default();
    let l_resp = l.walk_repo_paths(&opts).unwrap();
    let r_resp = r.walk_repo_paths(&opts).unwrap();
    assert_eq!(l_resp.paths, r_resp.paths);
    assert_eq!(l_resp.truncated, r_resp.truncated);
    assert!(l_resp.paths.iter().any(|p| p == "src/lib.rs"));
    // Gitignore honoured on both sides.
    assert!(!l_resp.paths.iter().any(|p| p.starts_with("ignored/")));
    assert!(!r_resp.paths.iter().any(|p| p.starts_with("ignored/")));
}

/// Helper: drive `search_content` to completion, accumulating every
/// streamed chunk into `(all_hits, chunk_count)`. `chunk_count` is
/// what the streaming-parity test asserts is > 1.
fn drain_search(
    backend: &dyn Backend,
    req: &ContentSearchRequest,
) -> (Vec<ContentMatchHit>, usize, bool) {
    let mut all: Vec<ContentMatchHit> = Vec::new();
    let mut chunks = 0usize;
    let mut sink = |hits: Vec<ContentMatchHit>| -> ControlFlow<()> {
        chunks += 1;
        all.extend(hits);
        ControlFlow::Continue(())
    };
    let completed = backend.search_content(req, &mut sink).unwrap();
    (all, chunks, completed.truncated)
}

#[test]
fn search_content_parity() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let l_tmp = TempDir::new().unwrap();
    let r_tmp = TempDir::new().unwrap();
    seed_tree(l_tmp.path());
    seed_tree(r_tmp.path());

    let l = LocalBackend::open_at(l_tmp.path().to_path_buf());
    let r = spawn_remote(r_tmp.path());

    let req = ContentSearchRequest {
        pattern: "foo".to_string(),
        fixed_strings: true,
        case_sensitive: None,
        max_results: 1000,
        max_line_chars: 250,
    };
    let (mut l_hits, _l_chunks, l_trunc) = drain_search(&l, &req);
    let (mut r_hits, _r_chunks, r_trunc) = drain_search(&r, &req);

    let cmp = |a: &ContentMatchHit, b: &ContentMatchHit| {
        a.display.cmp(&b.display).then(a.line.cmp(&b.line))
    };
    l_hits.sort_by(cmp);
    r_hits.sort_by(cmp);

    assert_eq!(l_trunc, r_trunc);
    assert_eq!(l_hits.len(), r_hits.len());
    for (a, b) in l_hits.iter().zip(r_hits.iter()) {
        assert_eq!(a.display, b.display);
        assert_eq!(a.line, b.line);
        assert_eq!(a.line_text, b.line_text);
        assert_eq!(a.byte_range, b.byte_range);
    }
    assert!(!l_hits.is_empty(), "expected at least one 'foo' hit");
}

/// Seed a workdir with many matches so both backends cross the
/// `CHUNK_TARGET_HITS` (64) threshold at least once — the streaming
/// contract promises "chunk fires while walker is still running", and
/// the simplest way to observe that is to check `chunk_count >= 2`.
fn seed_big_tree(root: &Path, files: usize, hits_per_file: usize) {
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
    for i in 0..files {
        let mut body = String::new();
        for _ in 0..hits_per_file {
            body.push_str("fn foo() { /* hit */ }\n");
        }
        std::fs::write(root.join(format!("file_{i:04}.rs")), body).unwrap();
    }
}

#[test]
fn search_content_streams_multiple_chunks() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 40 files × 4 hits each = 160 hits, comfortably > 64
    // (`CHUNK_TARGET_HITS`) so both backends must emit at least two
    // chunks. We still bound `max_results` so the cap path doesn't
    // kick in and collapse everything to one chunk.
    let l_tmp = TempDir::new().unwrap();
    let r_tmp = TempDir::new().unwrap();
    seed_big_tree(l_tmp.path(), 40, 4);
    seed_big_tree(r_tmp.path(), 40, 4);

    let l = LocalBackend::open_at(l_tmp.path().to_path_buf());
    let r = spawn_remote(r_tmp.path());

    let req = ContentSearchRequest {
        pattern: "foo".to_string(),
        fixed_strings: true,
        case_sensitive: None,
        max_results: 1000,
        max_line_chars: 250,
    };
    let (l_hits, l_chunks, _) = drain_search(&l, &req);
    let (r_hits, r_chunks, _) = drain_search(&r, &req);
    assert_eq!(l_hits.len(), r_hits.len(), "same hit count on both sides");
    assert!(
        l_chunks >= 2,
        "local backend should stream ≥2 chunks, got {l_chunks}"
    );
    assert!(
        r_chunks >= 2,
        "remote backend should stream ≥2 chunks, got {r_chunks}"
    );
}
