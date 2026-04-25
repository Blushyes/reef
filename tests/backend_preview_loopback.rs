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
    Database,
}

fn shape_of(body: &PreviewBody) -> BodyShape {
    match body {
        PreviewBody::Text { .. } => BodyShape::Text,
        PreviewBody::Image(_) => BodyShape::Image,
        PreviewBody::Database(_) => BodyShape::Database,
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

/// Build a tiny SQLite fixture at `path` with `SETUP_SQL` so both
/// backends have something real to read. Bare-minimum schema —
/// enough to exercise the table list, row count, and first-page
/// sample without making each test setup verbose.
fn seed_sqlite_db(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).expect("open sqlite");
    conn.execute_batch(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, age INTEGER); \
         INSERT INTO users(name, age) VALUES ('alice', 30), ('bob', 25), ('carol', 40); \
         CREATE TABLE posts(id INTEGER PRIMARY KEY, body TEXT); \
         INSERT INTO posts(body) VALUES ('hello'), ('world');",
    )
    .expect("seed sqlite");
    drop(conn);
}

#[test]
fn load_preview_parity_for_sqlite_database() {
    use reef::file_tree::PreviewBody;
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = tempdir_repo();
    let db_path = tmp.path().join("fixture.db");
    seed_sqlite_db(&db_path);

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    let l = local
        .load_preview(Path::new("fixture.db"), true, true)
        .expect("local preview None for fixture.db");
    let r = remote
        .load_preview(Path::new("fixture.db"), true, true)
        .expect("remote preview None for fixture.db");

    assert_eq!(shape_of(&l.body), BodyShape::Database, "local shape");
    assert_eq!(shape_of(&r.body), BodyShape::Database, "remote shape");

    let (PreviewBody::Database(li), PreviewBody::Database(ri)) = (&l.body, &r.body) else {
        unreachable!("shape_of asserted Database above");
    };

    // Table list parity — names + row counts must match. Without
    // this guard a divergence in the agent's `list_tables` filter
    // (e.g. accidentally including `sqlite_sequence`) would slip
    // through silently.
    let l_names: Vec<_> = li
        .tables
        .iter()
        .map(|t| (t.name.clone(), t.row_count))
        .collect();
    let r_names: Vec<_> = ri
        .tables
        .iter()
        .map(|t| (t.name.clone(), t.row_count))
        .collect();
    assert_eq!(l_names, r_names, "table list / counts");

    // Selected table + initial page row count parity.
    assert_eq!(li.selected_table, ri.selected_table, "selected_table");
    assert_eq!(
        li.initial_page.rows.len(),
        ri.initial_page.rows.len(),
        "initial_page row count",
    );
}

#[test]
fn db_load_page_parity_across_backends() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = tempdir_repo();
    let db_path = tmp.path().join("fixture.db");
    seed_sqlite_db(&db_path);

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    // Page from offset 0, limit 2 — should give two rows.
    let lp = local
        .db_load_page(Path::new("fixture.db"), "users", 0, 2)
        .expect("local db_load_page");
    let rp = remote
        .db_load_page(Path::new("fixture.db"), "users", 0, 2)
        .expect("remote db_load_page");

    assert_eq!(lp.rows.len(), 2, "local row count");
    assert_eq!(rp.rows.len(), 2, "remote row count");
    assert_eq!(lp.rows.len(), rp.rows.len(), "row count parity");

    // Cell-level: remote round-trips through DTO, so a serde gap
    // would surface here as a typed mismatch (e.g. Integer arriving
    // as Text). Comparing the entire row vec covers all five
    // SqliteValue variants in the fixture.
    for (i, (l_row, r_row)) in lp.rows.iter().zip(rp.rows.iter()).enumerate() {
        assert_eq!(l_row, r_row, "row {i} cell parity");
    }
}

#[test]
fn db_load_page_offset_works_across_backends() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = tempdir_repo();
    let db_path = tmp.path().join("fixture.db");
    seed_sqlite_db(&db_path);

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    // users has 3 rows; OFFSET 2 LIMIT 2 must return exactly 1 row.
    let lp = local
        .db_load_page(Path::new("fixture.db"), "users", 2, 2)
        .expect("local db_load_page");
    let rp = remote
        .db_load_page(Path::new("fixture.db"), "users", 2, 2)
        .expect("remote db_load_page");

    assert_eq!(lp.rows.len(), 1);
    assert_eq!(rp.rows.len(), 1);
    assert_eq!(lp.rows, rp.rows);
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
