//! Integration coverage for the V2 multi-schema API.
//!
//! These tests exercise the path that `reef-agent` will route over the
//! wire for v9 protocol clients: `list_databases` + `list_objects` for
//! each schema, schema-qualified column reads, index/trigger detail
//! parsing, and the `UnsupportedObjectKind` guard on `load_page_qualified`.
//!
//! The crate's lib-level tests cover the V1 read path. We deliberately
//! drive these from `tests/` so the public surface is exercised the
//! way a real downstream caller (the agent, the local backend) would
//! see it — no internal helpers reachable.

use std::path::PathBuf;

use reef_sqlite_preview::{
    DbObjectDetail, DbObjectKind, MAX_OBJECTS_PER_SCHEMA, PreviewError, SchemaKind, TriggerEvent,
    TriggerTiming, count_rows_qualified, list_databases, list_objects, load_page_qualified,
    read_columns_qualified, read_index_detail, read_initial_v2, read_object_detail,
    read_trigger_detail,
};
use rusqlite::Connection;

/// Write a tempfile-backed SQLite database from one batch of DDL and
/// return the path (kept alive by the returned `TempDir`). Mirrors the
/// V1 lib-test helper but at integration scope.
fn setup_file_db(setup_sql: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("fixture.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(setup_sql).unwrap();
    drop(conn);
    (tmp, path)
}

/// Build an in-memory `Connection` populated with every object kind we
/// care about plus an attached in-memory aux database. Returned
/// connection keeps the ATTACH alive as long as the caller holds it.
fn in_memory_with_attach() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        r#"
        ATTACH DATABASE ':memory:' AS aux;

        -- Plain table with PK + NOT NULL column for column-metadata
        -- coverage.
        CREATE TABLE main.users (
            id INTEGER PRIMARY KEY,
            email TEXT NOT NULL,
            display_name TEXT
        );
        INSERT INTO main.users(id, email, display_name)
            VALUES (1, 'a@x.io', 'Alice'),
                   (2, 'b@x.io', 'Bob'),
                   (3, 'c@x.io', NULL);

        -- Empty table — exercises row_count = Some(0) path.
        CREATE TABLE main.orders (id INTEGER, user_id INTEGER);

        -- View — should be listed under Views, columns populated, but
        -- row_count left as None per the policy in list_objects.
        CREATE VIEW main.active_users AS
            SELECT id, email FROM users WHERE display_name IS NOT NULL;

        -- Plain unique index for read_index_detail coverage.
        CREATE UNIQUE INDEX main.users_email_idx ON users(email);

        -- Partial index — read_index_detail must surface the WHERE.
        CREATE INDEX main.users_named_idx ON users(display_name)
            WHERE display_name IS NOT NULL;

        -- Trigger — parse_trigger_header must pick AFTER / INSERT.
        CREATE TRIGGER main.users_audit
            AFTER INSERT ON users
            BEGIN
                SELECT 1;
            END;

        -- Virtual table (fts5 if compiled in; fall back to a sentinel
        -- module name otherwise so the prefix detection still fires).
        CREATE VIRTUAL TABLE main.notes USING fts5(body);

        -- Temp table — appears under the `temp` schema in
        -- PRAGMA database_list.
        CREATE TEMP TABLE temp_scratch (n INTEGER);
        INSERT INTO temp_scratch VALUES (1), (2);

        -- Object in the attached schema — must appear under `aux`.
        CREATE TABLE aux.shadow (id INTEGER PRIMARY KEY, payload TEXT);
        INSERT INTO aux.shadow VALUES (1, 'hi'), (2, 'world');
        "#,
    )
    .unwrap();
    conn
}

#[test]
fn list_databases_returns_main_temp_and_attached() {
    let conn = in_memory_with_attach();
    let dbs = list_databases(&conn).unwrap();

    let names: Vec<&str> = dbs.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(names.contains(&"main"), "main missing: {names:?}");
    assert!(names.contains(&"temp"), "temp missing: {names:?}");
    assert!(names.contains(&"aux"), "aux missing: {names:?}");

    // SchemaKind classification.
    let kinds: std::collections::HashMap<_, _> =
        dbs.iter().map(|(n, k, _)| (n.as_str(), *k)).collect();
    assert_eq!(kinds["main"], SchemaKind::Main);
    assert_eq!(kinds["temp"], SchemaKind::Temp);
    assert_eq!(kinds["aux"], SchemaKind::Attached);
}

#[test]
fn list_objects_main_covers_all_four_kinds_plus_virtual() {
    let conn = in_memory_with_attach();
    let (objects, truncated) = list_objects(&conn, "main", MAX_OBJECTS_PER_SCHEMA).unwrap();

    assert!(!truncated, "truncation must not fire on a tiny fixture");

    // Bucket by kind for readable assertions.
    let by_kind: std::collections::HashMap<DbObjectKind, Vec<&str>> = {
        let mut m: std::collections::HashMap<DbObjectKind, Vec<&str>> =
            std::collections::HashMap::new();
        for o in &objects {
            m.entry(o.kind).or_default().push(o.name.as_str());
        }
        m
    };

    assert!(by_kind[&DbObjectKind::Table].contains(&"users"));
    assert!(by_kind[&DbObjectKind::Table].contains(&"orders"));
    assert!(by_kind[&DbObjectKind::Table].contains(&"notes")); // virtual
    assert!(by_kind[&DbObjectKind::View].contains(&"active_users"));
    assert!(by_kind[&DbObjectKind::Index].contains(&"users_email_idx"));
    assert!(by_kind[&DbObjectKind::Index].contains(&"users_named_idx"));
    assert!(by_kind[&DbObjectKind::Trigger].contains(&"users_audit"));

    // Virtual table must flag `is_virtual`.
    let notes = objects.iter().find(|o| o.name == "notes").unwrap();
    assert!(notes.is_virtual, "notes must be flagged as virtual");

    // Plain table must NOT flag `is_virtual`.
    let users = objects.iter().find(|o| o.name == "users").unwrap();
    assert!(!users.is_virtual);

    // row_count is populated for tables, None for views / indexes /
    // triggers.
    assert_eq!(users.row_count, Some(3));
    let view = objects.iter().find(|o| o.name == "active_users").unwrap();
    assert_eq!(view.row_count, None);
    let idx = objects
        .iter()
        .find(|o| o.name == "users_email_idx")
        .unwrap();
    assert_eq!(idx.row_count, None);
    let trig = objects.iter().find(|o| o.name == "users_audit").unwrap();
    assert_eq!(trig.row_count, None);

    // Columns eagerly populated for table; NOT NULL + PK fields
    // surface for the email + id columns respectively.
    let id_col = users.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(id_col.pk);
    let email_col = users.columns.iter().find(|c| c.name == "email").unwrap();
    assert!(email_col.notnull);
    assert!(!email_col.pk);
}

#[test]
fn list_objects_excludes_sqlite_internal_and_fts_shadows() {
    let conn = in_memory_with_attach();
    let (objects, _) = list_objects(&conn, "main", MAX_OBJECTS_PER_SCHEMA).unwrap();
    let names: Vec<&str> = objects.iter().map(|o| o.name.as_str()).collect();

    // No internal bookkeeping leaks through.
    assert!(
        !names.iter().any(|n| n.starts_with("sqlite_")),
        "found sqlite_* leak: {names:?}"
    );

    // FTS5 creates shadow tables `notes_data`, `notes_idx`,
    // `notes_content`, `notes_docsize`, `notes_config`. All have
    // sql=NULL in sqlite_master and must be filtered.
    for shadow_suffix in &["_data", "_idx", "_content", "_docsize", "_config"] {
        let leaked = format!("notes{shadow_suffix}");
        assert!(
            !names.contains(&leaked.as_str()),
            "fts shadow {leaked} leaked into list"
        );
    }
}

#[test]
fn list_objects_in_attached_schema() {
    let conn = in_memory_with_attach();
    let (objects, _) = list_objects(&conn, "aux", MAX_OBJECTS_PER_SCHEMA).unwrap();
    let names: Vec<&str> = objects.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, vec!["shadow"]);

    let shadow = &objects[0];
    assert_eq!(shadow.kind, DbObjectKind::Table);
    assert_eq!(shadow.row_count, Some(2));
    assert_eq!(shadow.schema, "aux");
}

#[test]
fn list_objects_in_temp_schema() {
    let conn = in_memory_with_attach();
    let (objects, _) = list_objects(&conn, "temp", MAX_OBJECTS_PER_SCHEMA).unwrap();
    let names: Vec<&str> = objects.iter().map(|o| o.name.as_str()).collect();
    assert!(names.contains(&"temp_scratch"), "{names:?}");
}

#[test]
fn read_columns_qualified_round_trips_pk_and_notnull() {
    let conn = in_memory_with_attach();
    let cols = read_columns_qualified(&conn, "main", "users").unwrap();
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["id", "email", "display_name"]);
    assert!(cols[0].pk);
    assert!(cols[1].notnull);
    assert!(!cols[1].pk);
    assert!(!cols[2].notnull);
}

#[test]
fn count_rows_qualified_works_across_schemas() {
    let conn = in_memory_with_attach();
    assert_eq!(count_rows_qualified(&conn, "main", "users").unwrap(), 3);
    assert_eq!(count_rows_qualified(&conn, "main", "orders").unwrap(), 0);
    assert_eq!(count_rows_qualified(&conn, "aux", "shadow").unwrap(), 2);
    assert_eq!(
        count_rows_qualified(&conn, "temp", "temp_scratch").unwrap(),
        2
    );
}

#[test]
fn read_index_detail_surfaces_unique_and_partial() {
    let conn = in_memory_with_attach();

    let detail = read_index_detail(&conn, "main", "users_email_idx").unwrap();
    match detail {
        DbObjectDetail::Index {
            unique,
            columns,
            partial_where,
            tbl_name,
            ..
        } => {
            assert!(unique, "users_email_idx must be unique");
            assert_eq!(columns, vec!["email"]);
            assert!(partial_where.is_none(), "not a partial index");
            assert_eq!(tbl_name, "users");
        }
        other => panic!("expected Index, got {other:?}"),
    }

    let detail = read_index_detail(&conn, "main", "users_named_idx").unwrap();
    match detail {
        DbObjectDetail::Index {
            unique,
            partial_where,
            ..
        } => {
            assert!(!unique);
            let where_ = partial_where.expect("partial index must surface WHERE");
            // Case from the CREATE INDEX is preserved.
            assert!(
                where_.to_lowercase().contains("display_name is not null"),
                "where clause was: {where_:?}"
            );
        }
        other => panic!("expected Index, got {other:?}"),
    }
}

#[test]
fn read_trigger_detail_parses_timing_and_event() {
    let conn = in_memory_with_attach();
    let detail = read_trigger_detail(&conn, "main", "users_audit").unwrap();
    match detail {
        DbObjectDetail::Trigger {
            timing,
            event,
            tbl_name,
            sql,
        } => {
            assert_eq!(timing, TriggerTiming::After);
            assert_eq!(event, TriggerEvent::Insert);
            assert_eq!(tbl_name, "users");
            assert!(sql.to_uppercase().contains("BEGIN"));
        }
        other => panic!("expected Trigger, got {other:?}"),
    }
}

#[test]
fn read_object_detail_dispatches_by_kind() {
    let conn = in_memory_with_attach();

    let t = read_object_detail(&conn, "main", DbObjectKind::Table, "users").unwrap();
    assert!(matches!(t, DbObjectDetail::Table { .. }));

    let v = read_object_detail(&conn, "main", DbObjectKind::View, "active_users").unwrap();
    assert!(matches!(v, DbObjectDetail::View { .. }));

    let i = read_object_detail(&conn, "main", DbObjectKind::Index, "users_email_idx").unwrap();
    assert!(matches!(i, DbObjectDetail::Index { .. }));

    let trg = read_object_detail(&conn, "main", DbObjectKind::Trigger, "users_audit").unwrap();
    assert!(matches!(trg, DbObjectDetail::Trigger { .. }));
}

#[test]
fn read_object_detail_missing_object_is_typed_error() {
    let conn = in_memory_with_attach();
    let err = read_object_detail(&conn, "main", DbObjectKind::Index, "no_such_index").unwrap_err();
    match err {
        PreviewError::ObjectNotFound { schema, name } => {
            assert_eq!(schema, "main");
            assert_eq!(name, "no_such_index");
        }
        other => panic!("expected ObjectNotFound, got {other:?}"),
    }
}

#[test]
fn load_page_qualified_rejects_index_and_trigger() {
    let (_tmp, path) = setup_file_db(
        r#"
        CREATE TABLE t(a INTEGER);
        INSERT INTO t VALUES (1), (2), (3);
        CREATE INDEX t_idx ON t(a);
        CREATE TRIGGER t_trg AFTER INSERT ON t BEGIN SELECT 1; END;
        "#,
    );

    for kind in [DbObjectKind::Index, DbObjectKind::Trigger] {
        let err = load_page_qualified(&path, "main", kind, "t_idx", 0, 10).unwrap_err();
        assert!(
            matches!(err, PreviewError::UnsupportedObjectKind),
            "expected UnsupportedObjectKind for {kind:?}, got {err:?}"
        );
    }

    // Table / view still page successfully.
    let page = load_page_qualified(&path, "main", DbObjectKind::Table, "t", 0, 10).unwrap();
    assert_eq!(page.rows.len(), 3);
}

#[test]
fn read_initial_v2_walks_real_file_main_schema() {
    // `read_initial_v2` opens its own RO connection so it can't see
    // ATTACH or temp objects from another writer. SQLite only lists
    // `temp` in `PRAGMA database_list` once something on the connection
    // touches it, so a freshly-opened RO connection on a file fixture
    // returns just `main`.
    let (_tmp, path) = setup_file_db(
        r#"
        CREATE TABLE rooms(id INTEGER PRIMARY KEY, name TEXT);
        INSERT INTO rooms(name) VALUES ('alpha'), ('beta');
        CREATE VIEW room_names AS SELECT name FROM rooms;
        CREATE INDEX rooms_name_idx ON rooms(name);
        "#,
    );

    let info = read_initial_v2(&path, 5).unwrap();

    // At least main is present; temp/aux are connection-state-dependent.
    let schema_names: Vec<&str> = info.schemas.iter().map(|s| s.name.as_str()).collect();
    assert!(schema_names.contains(&"main"), "{schema_names:?}");

    // Default schema is main.
    assert_eq!(info.default_schema, "main");

    // Default object is the smallest non-empty table (only one here).
    let default = info.default_object.expect("rooms table should be picked");
    assert_eq!(default.schema, "main");
    assert_eq!(default.name, "rooms");
    assert_eq!(default.kind, DbObjectKind::Table);

    // Initial page sampled the rows.
    assert_eq!(info.initial_page.rows.len(), 2);

    // main schema's object list covers tables / views / indexes.
    let main = info.schemas.iter().find(|s| s.name == "main").unwrap();
    let names: Vec<&str> = main.objects.iter().map(|o| o.name.as_str()).collect();
    assert!(names.contains(&"rooms"));
    assert!(names.contains(&"room_names"));
    assert!(names.contains(&"rooms_name_idx"));
}
