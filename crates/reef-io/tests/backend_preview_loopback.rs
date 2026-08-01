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

use reef_core::preview::{BinaryReason, PreviewBody};
use reef_io::{Backend, LocalBackend, RemoteBackend};
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
    Markdown,
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
        PreviewBody::Text(_) => BodyShape::Text,
        PreviewBody::Markdown(_) => BodyShape::Markdown,
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
            .load_preview(Path::new(name), true)
            .unwrap_or_else(|| panic!("local preview None for {name}"));
        let r = remote
            .load_preview(Path::new(name), true)
            .unwrap_or_else(|| panic!("remote preview None for {name}"));
        assert_eq!(l.path, r.path, "file_path for {name}");
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

    let l = local.load_preview(Path::new("a.txt"), true).unwrap();
    let r = remote.load_preview(Path::new("a.txt"), true).unwrap();
    let (PreviewBody::Text(lt), PreviewBody::Text(rt)) = (&l.body, &r.body) else {
        panic!("expected both Text, got {:?} / {:?}", l.body, r.body);
    };
    assert_eq!(lt.lines, rt.lines, "text lines diverged");
}

#[cfg(unix)]
#[test]
fn remote_preview_reports_the_canonical_workspace_dependency() {
    use std::os::unix::fs::symlink;

    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = tempdir_repo();
    std::fs::write(tmp.path().join("target.txt"), "hello\n").unwrap();
    symlink("target.txt", tmp.path().join("alias.txt")).unwrap();

    let remote = spawn_remote(tmp.path());
    let preview = remote
        .load_preview(Path::new("alias.txt"), true)
        .expect("remote symlink preview");

    assert_eq!(preview.path, "alias.txt");
    assert_eq!(
        preview.resolved_path.as_deref(),
        Some(Path::new("target.txt"))
    );
}

#[test]
fn load_preview_markdown_model_matches_on_local_and_remote() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = tempdir_repo();
    let body = "# Title\n\n| Name | Count |\n|:---|---:|\n| reef | 1 |\n";
    std::fs::write(tmp.path().join("README.md"), body.as_bytes()).unwrap();

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    let l = local.load_preview(Path::new("README.md"), true).unwrap();
    let r = remote.load_preview(Path::new("README.md"), true).unwrap();
    let (PreviewBody::Markdown(lm), PreviewBody::Markdown(rm)) = (&l.body, &r.body) else {
        panic!(
            "expected both Markdown previews, got {:?} / {:?}",
            l.body, r.body
        );
    };
    assert_eq!(lm, rm);
}

#[test]
fn load_preview_large_markdown_stays_markdown_locally_and_remotely() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = tempdir_repo();
    let body = format!("# Title\n\n{}", "large markdown paragraph ".repeat(24_000));
    std::fs::write(tmp.path().join("README.md"), body.as_bytes()).unwrap();

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    let local_preview = local.load_preview(Path::new("README.md"), true).unwrap();
    let remote_preview = remote.load_preview(Path::new("README.md"), true).unwrap();

    assert_eq!(shape_of(&local_preview.body), BodyShape::Markdown);
    assert_eq!(shape_of(&remote_preview.body), BodyShape::Markdown);
}

#[test]
fn remote_structured_preview_preserves_complete_source_above_ordinary_text_limit() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = tempdir_repo();
    let source = format!(r#"{{"payload":"{}"}}"#, "x".repeat(2 * 1024 * 1024));
    std::fs::write(tmp.path().join("large.json"), source.as_bytes()).unwrap();

    let remote = spawn_remote(tmp.path());
    let preview = remote
        .load_preview(Path::new("large.json"), true)
        .expect("remote structured preview");
    let PreviewBody::Text(text) = preview.body else {
        panic!("expected text preview");
    };

    assert_eq!(text.source.as_deref(), Some(source.as_str()));
}

#[test]
fn oversized_structured_preview_is_too_large_locally_and_remotely() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = tempdir_repo();
    let source = format!(
        r#"{{"payload":"{}"}}"#,
        "x".repeat(reef_core::preview::MAX_TEXT_PREVIEW_BYTES as usize)
    );
    std::fs::write(tmp.path().join("oversized.json"), source.as_bytes()).unwrap();

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());
    let local_preview = local
        .load_preview(Path::new("oversized.json"), true)
        .expect("local structured preview");
    let remote_preview = remote
        .load_preview(Path::new("oversized.json"), true)
        .expect("remote structured preview");

    assert_eq!(shape_of(&local_preview.body), BodyShape::BinaryTooLarge);
    assert_eq!(shape_of(&remote_preview.body), BodyShape::BinaryTooLarge);
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

fn users_key() -> reef_sqlite_preview::DbObjectKey {
    reef_sqlite_preview::DbObjectKey {
        schema: "main".to_string(),
        name: "users".to_string(),
        kind: reef_sqlite_preview::DbObjectKind::Table,
    }
}

#[test]
fn load_preview_parity_for_sqlite_database() {
    use reef_core::preview::PreviewBody;
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = tempdir_repo();
    let db_path = tmp.path().join("fixture.db");
    seed_sqlite_db(&db_path);

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    let l = local
        .load_preview(Path::new("fixture.db"), true)
        .expect("local preview None for fixture.db");
    let r = remote
        .load_preview(Path::new("fixture.db"), true)
        .expect("remote preview None for fixture.db");

    assert_eq!(shape_of(&l.body), BodyShape::Database, "local shape");
    assert_eq!(shape_of(&r.body), BodyShape::Database, "remote shape");

    let (PreviewBody::Database(li), PreviewBody::Database(ri)) = (&l.body, &r.body) else {
        unreachable!("shape_of asserted Database above");
    };

    // Object list parity for the default (main) schema — names +
    // kinds + row counts must match. Without this guard a divergence
    // in the agent's `list_objects` filter (e.g. accidentally including
    // `sqlite_sequence`) would slip through silently.
    fn main_objects(
        info: &reef_sqlite_preview::DatabaseInfoV2,
    ) -> Vec<(String, reef_sqlite_preview::DbObjectKind, Option<u64>)> {
        info.schemas
            .iter()
            .find(|s| s.name == "main")
            .map(|s| {
                s.objects
                    .iter()
                    .map(|o| (o.name.clone(), o.kind, o.row_count))
                    .collect()
            })
            .unwrap_or_default()
    }
    assert_eq!(
        main_objects(li),
        main_objects(ri),
        "main schema object list / counts",
    );

    // Default schema + selected object + initial page row count parity.
    assert_eq!(li.default_schema, ri.default_schema, "default_schema");
    assert_eq!(li.default_object, ri.default_object, "default_object");
    assert_eq!(
        li.initial_page.rows.len(),
        ri.initial_page.rows.len(),
        "initial_page row count",
    );
}

#[cfg(unix)]
#[test]
fn remote_sqlite_preview_reports_the_canonical_workspace_dependency() {
    use std::os::unix::fs::symlink;

    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = tempdir_repo();
    seed_sqlite_db(&tmp.path().join("target.db"));
    symlink("target.db", tmp.path().join("alias.db")).unwrap();

    let remote = spawn_remote(tmp.path());
    let preview = remote
        .load_preview(Path::new("alias.db"), true)
        .expect("remote sqlite symlink preview");

    assert_eq!(shape_of(&preview.body), BodyShape::Database);
    assert_eq!(
        preview.resolved_path.as_deref(),
        Some(Path::new("target.db"))
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
        .db_load_page(Path::new("fixture.db"), &users_key(), 0, 2)
        .expect("local db_load_page");
    let rp = remote
        .db_load_page(Path::new("fixture.db"), &users_key(), 0, 2)
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
        .db_load_page(Path::new("fixture.db"), &users_key(), 2, 2)
        .expect("local db_load_page");
    let rp = remote
        .db_load_page(Path::new("fixture.db"), &users_key(), 2, 2)
        .expect("remote db_load_page");

    assert_eq!(lp.rows.len(), 1);
    assert_eq!(rp.rows.len(), 1);
    assert_eq!(lp.rows, rp.rows);
}

#[test]
fn db_load_cell_returns_complete_text_across_backends() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = tempdir_repo();
    let db_path = tmp.path().join("fixture.db");
    seed_sqlite_db(&db_path);
    let unit = "完整单元格";
    let full_text = unit.repeat(reef_sqlite_preview::DB_CELL_CHUNK_BYTES / unit.len() + 100);
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute("UPDATE posts SET body = ?1 WHERE id = 1", [&full_text])
        .expect("seed long text");
    drop(connection);
    let key = reef_sqlite_preview::DbObjectKey {
        schema: "main".to_string(),
        name: "posts".to_string(),
        kind: reef_sqlite_preview::DbObjectKind::Table,
    };

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());
    let page = local
        .db_load_page(Path::new("fixture.db"), &key, 0, 1)
        .expect("local db_load_page");
    let locator = &page.row_locators[0];
    let local_value = local
        .db_load_cell(
            Path::new("fixture.db"),
            &key,
            locator,
            1,
            &reef_io::CancellationToken::default(),
        )
        .expect("local db_load_cell");
    let remote_value = remote
        .db_load_cell(
            Path::new("fixture.db"),
            &key,
            locator,
            1,
            &reef_io::CancellationToken::default(),
        )
        .expect("remote db_load_cell");

    assert_eq!(local_value, remote_value);
    assert_eq!(
        local_value,
        reef_sqlite_preview::SqliteValue::Text {
            value: full_text,
            truncated: false,
        }
    );
}

#[test]
fn load_preview_missing_file_returns_none_on_both() {
    let _lock = BACKEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = tempdir_repo();

    let local = LocalBackend::open_at(tmp.path().to_path_buf());
    let remote = spawn_remote(tmp.path());

    assert!(local.load_preview(Path::new("no-such.txt"), true).is_none());
    assert!(
        remote
            .load_preview(Path::new("no-such.txt"), true)
            .is_none()
    );
}
