//! Local vs Remote parity — `Backend::load_preview`.
//!
//! Locks down that text / empty / null-byte binary files all classify
//! the same way regardless of whether the workdir is local or behind
//! a `reef-agent` SSH bridge. Pre-fix the remote returned a wrong
//! `PreviewBody` variant for empty files (Text vs Binary(Empty)) and
//! the wrong `BinaryReason` for null-byte detected binaries.
//!
//! Image rendering / MIME detection over RPC is tracked separately
//! (issue #31); this test stays at the variant + meta_line level so
//! it doesn't depend on either.

use std::path::Path;
use std::sync::Mutex;

use reef::backend::{Backend, LocalBackend, RemoteBackend};
use reef::file_tree::{BinaryReason, PreviewBody};
use test_support::{agent_bin, tempdir_repo};

static BACKEND_LOCK: Mutex<()> = Mutex::new(());

fn spawn_remote(workdir: &Path) -> RemoteBackend {
    let argv = vec![
        agent_bin().display().to_string(),
        "--stdio".to_string(),
        "--workdir".to_string(),
        workdir.display().to_string(),
    ];
    RemoteBackend::spawn(&argv).expect("spawn remote")
}

/// Tag the body shape down to the `BinaryReason` so two runs can
/// compare without referencing the inner bytes / line vec / mime
/// (those carry intentional Local-vs-Remote gaps — MIME is None on
/// remote until #31 lands).
#[derive(Debug, PartialEq, Eq)]
enum BodyShape {
    Text,
    BinaryEmpty,
    BinaryNullBytes,
    BinaryNonImage,
    BinaryUnsupportedImage,
    BinaryTooLarge,
    BinaryDecodeError,
    Image,
}

fn shape_of(body: &PreviewBody) -> BodyShape {
    match body {
        PreviewBody::Text { .. } => BodyShape::Text,
        PreviewBody::Image(_) => BodyShape::Image,
        PreviewBody::Binary(info) => match &info.reason {
            BinaryReason::Empty => BodyShape::BinaryEmpty,
            BinaryReason::NullBytes => BodyShape::BinaryNullBytes,
            BinaryReason::NonImage => BodyShape::BinaryNonImage,
            BinaryReason::UnsupportedImage => BodyShape::BinaryUnsupportedImage,
            BinaryReason::TooLarge => BodyShape::BinaryTooLarge,
            BinaryReason::DecodeError(_) => BodyShape::BinaryDecodeError,
        },
    }
}

#[test]
fn load_preview_parity_for_text_empty_and_nullbyte() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = tempdir_repo();

    // text — utf-8, no null bytes
    std::fs::write(tmp.path().join("text.txt"), b"hello\nworld\n").unwrap();
    // empty — zero bytes; both backends should say Binary(Empty)
    std::fs::write(tmp.path().join("empty.txt"), b"").unwrap();
    // null-byte binary (no recognisable MIME header) — both should say
    // Binary(NullBytes), NOT NonImage (NonImage is reserved for files
    // where infer recognised a non-image MIME type).
    std::fs::write(tmp.path().join("garbage.bin"), b"abc\0def\0\0\0").unwrap();

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    for name in ["text.txt", "empty.txt", "garbage.bin"] {
        let l = local
            .load_preview(Path::new(name), true, true)
            .unwrap_or_else(|| panic!("local preview None for {name}"));
        let r = remote
            .load_preview(Path::new(name), true, true)
            .unwrap_or_else(|| panic!("remote preview None for {name}"));
        assert_eq!(l.file_path, r.file_path, "file_path for {name}");
        assert_eq!(
            shape_of(&l.body),
            shape_of(&r.body),
            "body shape for {name}"
        );
    }
}

#[test]
fn load_preview_text_lines_match() {
    // Spot-check that Text bodies land with the same line count on both
    // sides — guards against the remote backend silently truncating /
    // re-encoding differently than the file_tree util.
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = tempdir_repo();
    let body = "alpha\nbeta\ngamma\ndelta\n";
    std::fs::write(tmp.path().join("a.txt"), body.as_bytes()).unwrap();

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    let l = local.load_preview(Path::new("a.txt"), true, true).unwrap();
    let r = remote.load_preview(Path::new("a.txt"), true, true).unwrap();
    let (PreviewBody::Text { lines: ll, .. }, PreviewBody::Text { lines: rl, .. }) =
        (&l.body, &r.body)
    else {
        panic!("expected both Text, got {:?} / {:?}", l.body, r.body);
    };
    assert_eq!(ll, rl, "text lines diverged");
}

#[test]
fn load_preview_missing_file_returns_none_on_both() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = tempdir_repo();

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    assert!(
        local
            .load_preview(Path::new("no-such.txt"), true, true)
            .is_none()
    );
    assert!(
        remote
            .load_preview(Path::new("no-such.txt"), true, true)
            .is_none()
    );
}
