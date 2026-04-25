//! Read-only SQLite preview reader.
//!
//! Used by both `reef` (LocalBackend) and `reef-agent` (the SSH daemon) to
//! produce a friendly card for `.db` / `.sqlite[3]` files in the Files tab —
//! a list of tables with row counts + the first page of rows for whichever
//! table the user is viewing. The crate is deliberately tiny:
//!
//! - **Read-only.** Connections are opened with
//!   `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX` and the URI flag
//!   `?mode=ro&immutable=1`, so we never take a write lock and we don't
//!   participate in WAL recovery if another process is writing.
//! - **No connection cache.** Every call opens a fresh `Connection` and
//!   drops it before returning. Open is µs-scale on local disk and the
//!   call sites already run on background workers — caching would force
//!   Send/Sync gymnastics for no measurable win.
//! - **No async.** The whole reader is synchronous; the preview worker on
//!   the reef side is already a background thread, and the agent side
//!   handles each request on a fresh thread.
//!
//! Errors collapse into a small enum so the UI can pick a friendly message
//! without parsing rusqlite text. Cell values are typed (`SqliteValue`)
//! rather than pre-stringified so the render layer can italicise NULL and
//! show `<blob N B>` placeholders distinctly.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// Magic bytes at offset 0 of every SQLite database file. SQLite's docs
/// guarantee this header for both rollback-journal and WAL modes —
/// matching it byte-for-byte is the cheapest way to filter out junk
/// `.db` files (LMDB, Berkeley DB, etc.) before handing the path to
/// rusqlite, which would otherwise return a generic "file is not a
/// database" error.
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Hard size cap. SQLite preview opens read-only with `immutable=1` so
/// memory pressure is mostly bounded by the per-page result, but a
/// multi-GB DB still costs us a stat + a page-index scan on first
/// `sqlite_master` query. Beyond this we'd rather show the generic
/// "too large" binary card and bail. 256 MiB matches the order of
/// magnitude of the largest fixture / dev DBs one would reasonably
/// browse — bigger than that and the caller probably wants `sqlite3`,
/// not a preview pane.
pub const MAX_PREVIEW_BYTES: u64 = 256 * 1024 * 1024;

/// Per-cell text length cap. Any TEXT longer than this is truncated
/// at the reader and a trailing `…` glyph is appended; the wire payload
/// never carries the original full string. Bounds the worst-case page
/// payload — without this, a single row containing a 1 MB JSON blob in
/// a TEXT column could exceed `MAX_FRAME_SIZE` over SSH.
pub const MAX_TEXT_CELL_CHARS: usize = 200;

/// File extensions we treat as candidates for SQLite. The check is a
/// fast pre-filter; the actual gate is the magic-bytes match below.
/// `.sqlite-journal`, `.sqlite-wal`, `.sqlite-shm` deliberately are NOT
/// in this list — those are sidecars that live next to a real DB and
/// shouldn't open through the preview path themselves.
const SQLITE_EXTENSIONS: &[&str] = &["db", "sqlite", "sqlite3"];

/// Reader-level errors. Distinguished from generic IO so the call site
/// can pick a friendly preview-card phrasing (encrypted vs corrupt vs
/// not-actually-sqlite).
#[derive(Debug)]
pub enum PreviewError {
    /// File doesn't have a SQLite magic header. Either we were called
    /// on a non-DB file or the file is truncated/corrupt at offset 0.
    NotSqlite,
    /// File is bigger than [`MAX_PREVIEW_BYTES`].
    TooLarge { size: u64 },
    /// `rusqlite::Connection::open_with_flags_and_vfs` failed. Common
    /// real-world causes: encrypted DB (SQLCipher / Chrome cookies),
    /// truncated file, FS errors. The string is the rusqlite-rendered
    /// message — call site shouldn't parse it, just display verbatim
    /// (already truncated to a single line).
    OpenFailed(String),
    /// Query against `sqlite_master` or a user table failed. Same
    /// rationale as `OpenFailed` — string for display only.
    QueryFailed(String),
    /// File IO error at the magic-bytes probe stage (file vanished,
    /// permission denied, etc.).
    Io(String),
}

impl std::fmt::Display for PreviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PreviewError::NotSqlite => f.write_str("not a SQLite database"),
            PreviewError::TooLarge { size } => write!(f, "file too large ({size} bytes)"),
            PreviewError::OpenFailed(s) => write!(f, "open failed: {s}"),
            PreviewError::QueryFailed(s) => write!(f, "query failed: {s}"),
            PreviewError::Io(s) => write!(f, "io: {s}"),
        }
    }
}

impl std::error::Error for PreviewError {}

/// One column on a table — name + declared type as it appears in
/// `PRAGMA table_info`. SQLite's type system is "type affinity" not
/// strict, so the declared type may be empty (no declared type) or
/// a non-standard string ("VARCHAR(255)", "DATETIME", …). We pass it
/// through verbatim and let the render layer truncate if needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub decl_type: String,
}

/// One table or view in the database.
#[derive(Debug, Clone)]
pub struct TableSummary {
    pub name: String,
    pub columns: Vec<ColumnInfo>,
    pub row_count: u64,
}

/// One typed cell value. NULL is distinct from an empty TEXT so the
/// renderer can show it differently (italic "NULL" vs "" empty cell).
/// BLOB carries only its length — the bytes are never shipped, both to
/// keep the wire payload small and to dodge the "binary in JSON" issue.
#[derive(Debug, Clone, PartialEq)]
pub enum SqliteValue {
    Null,
    Integer(i64),
    Real(f64),
    /// TEXT, possibly truncated to [`MAX_TEXT_CELL_CHARS`]. The boolean
    /// is `true` if the original value was longer and we appended `…`.
    Text {
        value: String,
        truncated: bool,
    },
    Blob {
        len: usize,
    },
}

impl std::fmt::Display for SqliteValue {
    /// Canonical display form, shared between width measurement and
    /// rendering so the two never disagree about a cell's column
    /// occupancy. Single source of truth — caller wraps the result
    /// in styling separately.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SqliteValue::Null => f.write_str("NULL"),
            SqliteValue::Integer(n) => write!(f, "{n}"),
            SqliteValue::Real(r) => write!(f, "{r}"),
            SqliteValue::Text { value, truncated } => {
                f.write_str(value)?;
                if *truncated {
                    f.write_str("…")?;
                }
                Ok(())
            }
            SqliteValue::Blob { len } => write!(f, "<blob {len} B>"),
        }
    }
}

/// One page of rows from a single table.
#[derive(Debug, Clone)]
pub struct DbPage {
    /// Each inner Vec aligns positionally with the parent table's
    /// `columns`. Length always equals `columns.len()` for every row.
    pub rows: Vec<Vec<SqliteValue>>,
}

/// Top-level preview payload. Built once when the user selects a `.db`
/// file in the tree; navigation (table-switch, page-flip) goes through
/// [`load_page`] returning a smaller `DbPage`.
#[derive(Debug, Clone)]
pub struct DatabaseInfo {
    /// All user tables and views, sorted by name. Excludes
    /// `sqlite_*` internal tables.
    pub tables: Vec<TableSummary>,
    /// Index into `tables` of the table whose rows are sampled into
    /// `initial_page`. We pick the smallest non-empty table; if every
    /// table is empty, picks index 0.
    pub selected_table: usize,
    /// First page of rows from `tables[selected_table]`. Empty when the
    /// selected table has no rows or the DB has no tables at all.
    pub initial_page: DbPage,
    /// Total bytes of the on-disk file at preview time. Used by render
    /// to format the meta line ("sqlite · 12 tables · 4.1 MB"). Carried
    /// here so the reader is the single source of truth for size.
    pub bytes_on_disk: u64,
}

/// `true` when `path` has a SQLite-shaped extension. Cheap pre-filter —
/// the actual gate is [`probe_magic`].
pub fn has_sqlite_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SQLITE_EXTENSIONS.iter().any(|s| s.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Read the first 16 bytes and check the SQLite magic header. Returns
/// `Ok(true)` only when the bytes match exactly. `Ok(false)` covers
/// "file is real but doesn't look like SQLite" (short file, wrong
/// header). `Err(Io)` covers actual IO failures (file gone, permission
/// denied) — caller should bubble up rather than swallow.
pub fn probe_magic(path: &Path) -> Result<bool, PreviewError> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| PreviewError::Io(e.to_string()))?;
    let mut buf = [0u8; 16];
    let n = f
        .read(&mut buf)
        .map_err(|e| PreviewError::Io(e.to_string()))?;
    Ok(n == 16 && &buf == SQLITE_MAGIC)
}

/// `true` when `bytes` starts with the SQLite magic header. Useful for
/// callers that already have a probe buffer in memory (the file-tree
/// preview loader reads 8 KB up front for `infer`-based MIME sniffing
/// — checking magic against that buffer avoids a second `read` syscall
/// per `.db` file).
pub fn has_sqlite_magic(bytes: &[u8]) -> bool {
    bytes.len() >= 16 && &bytes[..16] == SQLITE_MAGIC
}

/// `true` when `path` should be routed through the SQLite preview
/// reader. Combines the extension pre-filter with a magic-bytes probe
/// so a `.db` file that happens to be LMDB / Berkeley DB stays out of
/// the SQLite path. Errors from the probe are silently dropped here —
/// the caller already has a fall-back binary card for any file that
/// can't be opened or doesn't sniff as SQLite.
pub fn is_sqlite_file(path: &Path) -> bool {
    has_sqlite_extension(path) && probe_magic(path).unwrap_or(false)
}

/// Build a read-only Connection. Path is rendered through SQLite's
/// URI form so spaces and other shell-meaningful characters survive
/// intact.
///
/// We deliberately do **not** pass `immutable=1`: that flag tells
/// SQLite "the file will never change", which it uses to skip WAL
/// recovery and locking. For live databases (those with a `-wal` /
/// `-shm` sidecar from active writers), `immutable=1` either refuses
/// to open or returns a stale view that doesn't include un-checkpointed
/// commits. Plain `mode=ro` is the right default — SQLite still maps
/// `-shm` read-only and reads consistent snapshots.
fn open_readonly(path: &Path) -> Result<Connection, PreviewError> {
    // SQLite URIs are RFC 3986 — `?` separates the query string,
    // `#` the fragment, and `%` introduces a percent-encoded byte.
    // We escape all three so a workdir path containing literal
    // `?`, `#`, or a `%XX` sequence (e.g. an URL-encoded filename
    // a user dragged in) doesn't get reinterpreted by SQLite's URI
    // parser. Order matters: `%` must run first or the `%3F` we
    // emit for `?` would itself get re-encoded to `%253F`.
    let path_str = path.to_string_lossy();
    let mut escaped = String::with_capacity(path_str.len());
    for c in path_str.chars() {
        match c {
            '%' => escaped.push_str("%25"),
            '?' => escaped.push_str("%3F"),
            '#' => escaped.push_str("%23"),
            other => escaped.push(other),
        }
    }
    let uri = format!("file:{escaped}?mode=ro");
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    Connection::open_with_flags(&uri, flags).map_err(|e| PreviewError::OpenFailed(e.to_string()))
}

/// List user tables + views, with column metadata and row counts.
/// Excludes the SQLite-internal `sqlite_*` tables (`sqlite_sequence`,
/// `sqlite_schema`, etc.) — those are an implementation detail the
/// user almost never wants to browse.
fn list_tables(conn: &Connection) -> Result<Vec<TableSummary>, PreviewError> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type IN ('table','view') \
                 AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?;
    drop(stmt);

    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let columns = read_columns(conn, &name)?;
        let row_count = count_rows(conn, &name)?;
        out.push(TableSummary {
            name,
            columns,
            row_count,
        });
    }
    Ok(out)
}

/// Read column metadata via `PRAGMA table_info`. Order matches the
/// table's declared column order — same as a `SELECT *` would yield.
fn read_columns(conn: &Connection, table: &str) -> Result<Vec<ColumnInfo>, PreviewError> {
    // `PRAGMA table_info` doesn't accept bound parameters in older
    // SQLite, but `pragma_table_info` (table-valued function) does.
    // Use the latter so a malicious table name (none, here, but the
    // habit is cheap) can't escape its quoting.
    let mut stmt = conn
        .prepare("SELECT name, type FROM pragma_table_info(?1)")
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?;
    let cols: Vec<ColumnInfo> = stmt
        .query_map([table], |row| {
            Ok(ColumnInfo {
                name: row.get::<_, String>(0)?,
                decl_type: row.get::<_, String>(1).unwrap_or_default(),
            })
        })
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?;
    Ok(cols)
}

/// `SELECT COUNT(*) FROM "table"`. Cheap on small/medium tables; gets
/// pricier on multi-million-row tables but still fast enough for a
/// preview load.
fn count_rows(conn: &Connection, table: &str) -> Result<u64, PreviewError> {
    let sql = format!("SELECT COUNT(*) FROM {}", quote_ident(table));
    conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map(|n| n.max(0) as u64)
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))
}

/// Quote a SQLite identifier by wrapping in double quotes and doubling
/// any embedded `"`. Same rules as
/// `https://sqlite.org/lang_keywords.html`. We can't bind table names
/// as parameters in standard SQL — they're identifiers, not values —
/// so this is the safe-by-construction path.
fn quote_ident(ident: &str) -> String {
    let mut out = String::with_capacity(ident.len() + 2);
    out.push('"');
    for ch in ident.chars() {
        if ch == '"' {
            out.push('"');
            out.push('"');
        } else {
            out.push(ch);
        }
    }
    out.push('"');
    out
}

/// Read one page of rows from `table`. `offset` and `limit` map
/// directly to the SQL clauses — note that SQLite's `LIMIT N OFFSET M`
/// is O(M) for tables without a usable index, so high page numbers
/// on multi-million-row tables get gradually slower. Acceptable for a
/// preview pane; if it becomes a problem in practice we'd switch to
/// keyset pagination on rowid (`WHERE rowid > ?`).
pub fn load_page(
    path: &Path,
    table: &str,
    offset: u64,
    limit: u32,
) -> Result<DbPage, PreviewError> {
    if !probe_magic(path)? {
        return Err(PreviewError::NotSqlite);
    }
    let conn = open_readonly(path)?;
    read_page(&conn, table, offset, limit)
}

fn read_page(
    conn: &Connection,
    table: &str,
    offset: u64,
    limit: u32,
) -> Result<DbPage, PreviewError> {
    let columns = read_columns(conn, table)?;
    if columns.is_empty() {
        // Either the table doesn't exist or is genuinely zero-column —
        // both rare and both handled identically (empty page).
        return Ok(DbPage { rows: Vec::new() });
    }
    let sql = format!("SELECT * FROM {} LIMIT ?1 OFFSET ?2", quote_ident(table));
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?;
    let col_count = columns.len();
    let rows = stmt
        .query_map([limit as i64, offset as i64], |row| {
            let mut cells = Vec::with_capacity(col_count);
            for i in 0..col_count {
                let v = match row.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => SqliteValue::Null,
                    rusqlite::types::ValueRef::Integer(n) => SqliteValue::Integer(n),
                    rusqlite::types::ValueRef::Real(f) => SqliteValue::Real(f),
                    rusqlite::types::ValueRef::Text(bytes) => text_cell(bytes),
                    rusqlite::types::ValueRef::Blob(bytes) => {
                        SqliteValue::Blob { len: bytes.len() }
                    }
                };
                cells.push(v);
            }
            Ok(cells)
        })
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?;
    Ok(DbPage { rows })
}

/// Build a `Text` cell, truncating to [`MAX_TEXT_CELL_CHARS`] graphemes
/// (well — chars, since SQLite doesn't promise grapheme boundaries and
/// we'd need an extra dep for it). The truncation flag is what tells
/// the render layer to draw the trailing `…`.
fn text_cell(bytes: &[u8]) -> SqliteValue {
    let s = String::from_utf8_lossy(bytes);
    let mut chars = s.chars();
    let mut head = String::new();
    let mut taken = 0;
    for ch in chars.by_ref() {
        if taken >= MAX_TEXT_CELL_CHARS {
            return SqliteValue::Text {
                value: head,
                truncated: true,
            };
        }
        head.push(ch);
        taken += 1;
    }
    SqliteValue::Text {
        value: head,
        truncated: false,
    }
}

/// Build a [`DatabaseInfo`] for an on-disk SQLite file. Walks the
/// schema, reads row counts, samples the first page of the smallest
/// non-empty table.
///
/// `initial_page_size` is the row count for the sampled table — not a
/// hard cap on render. The UI layer can re-request a different page
/// size via [`load_page`] once it knows the panel height. We return
/// some rows here so the very first frame after preview-select isn't
/// empty.
pub fn read_initial(path: &Path, initial_page_size: u32) -> Result<DatabaseInfo, PreviewError> {
    let meta = std::fs::metadata(path).map_err(|e| PreviewError::Io(e.to_string()))?;
    let bytes_on_disk = meta.len();
    if bytes_on_disk > MAX_PREVIEW_BYTES {
        return Err(PreviewError::TooLarge {
            size: bytes_on_disk,
        });
    }
    if !probe_magic(path)? {
        return Err(PreviewError::NotSqlite);
    }

    let conn = open_readonly(path)?;
    let tables = list_tables(&conn)?;

    if tables.is_empty() {
        return Ok(DatabaseInfo {
            tables,
            selected_table: 0,
            initial_page: DbPage { rows: Vec::new() },
            bytes_on_disk,
        });
    }

    // Pick the smallest non-empty table for the initial sample so a DB
    // with one giant table and several empty fixtures doesn't surface
    // an empty page on first view. Falls back to index 0 when every
    // table is empty — the page will still be empty, but at least the
    // user lands on a sensible default.
    let selected_table = tables
        .iter()
        .enumerate()
        .filter(|(_, t)| t.row_count > 0)
        .min_by_key(|(_, t)| t.row_count)
        .map(|(i, _)| i)
        .unwrap_or(0);

    let initial_page = read_page(&conn, &tables[selected_table].name, 0, initial_page_size)?;

    Ok(DatabaseInfo {
        tables,
        selected_table,
        initial_page,
        bytes_on_disk,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Build a SQLite database with arbitrary user-supplied SQL setup
    /// and return its on-disk path (kept alive by the returned
    /// `TempDir`).
    fn setup_db(setup_sql: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.db");
        let conn = Connection::open(&path).unwrap();
        for stmt in setup_sql {
            conn.execute_batch(stmt).unwrap();
        }
        drop(conn);
        (tmp, path)
    }

    #[test]
    fn extension_check_accepts_canonical_suffixes() {
        for ext in &["db", "sqlite", "sqlite3", "DB", "SQLite"] {
            let p = PathBuf::from(format!("foo.{ext}"));
            assert!(has_sqlite_extension(&p), "should accept .{ext}");
        }
    }

    #[test]
    fn extension_check_rejects_other_suffixes() {
        for ext in &["sqlite-journal", "sqlite-wal", "sqlite-shm", "sql", "txt"] {
            let p = PathBuf::from(format!("foo.{ext}"));
            assert!(!has_sqlite_extension(&p), "should reject .{ext}");
        }
    }

    #[test]
    fn probe_magic_accepts_real_sqlite() {
        let (_tmp, path) = setup_db(&["CREATE TABLE x(a)"]);
        assert!(probe_magic(&path).unwrap());
    }

    #[test]
    fn probe_magic_rejects_non_sqlite() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fake.db");
        std::fs::write(&path, b"not a sqlite database, just bytes").unwrap();
        assert!(!probe_magic(&path).unwrap());
    }

    #[test]
    fn probe_magic_rejects_short_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("short.db");
        std::fs::write(&path, b"SQLite").unwrap();
        assert!(!probe_magic(&path).unwrap());
    }

    #[test]
    fn read_initial_lists_user_tables_excluding_sqlite_internal() {
        let (_tmp, path) = setup_db(&[
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT)",
            "CREATE TABLE posts(id INTEGER PRIMARY KEY, body TEXT)",
            "INSERT INTO users(name) VALUES ('alice'),('bob')",
            // sqlite_sequence appears once an AUTOINCREMENT is used —
            // by triggering it we verify the filter still excludes it.
            "CREATE TABLE counters(id INTEGER PRIMARY KEY AUTOINCREMENT, n INTEGER)",
            "INSERT INTO counters(n) VALUES (1)",
        ]);

        let info = read_initial(&path, 10).unwrap();
        let names: Vec<_> = info.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["counters", "posts", "users"]);
        assert!(
            !names.iter().any(|n| n.starts_with("sqlite_")),
            "sqlite_* tables should be filtered"
        );
    }

    /// SQL fragment that fills a single-column table with `n` rows
    /// numbered 1..=n. `generate_series` isn't compiled into the
    /// bundled SQLite by default, so we use a recursive CTE — same
    /// shape, portable.
    fn seq_insert(table: &str, col: &str, n: u32) -> String {
        format!(
            "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n < {n}) \
             INSERT INTO {table}({col}) SELECT n FROM seq"
        )
    }

    #[test]
    fn read_initial_picks_smallest_nonempty_table() {
        let big_seed = seq_insert("big", "a", 50);
        let (_tmp, path) = setup_db(&[
            "CREATE TABLE big(a)",
            "CREATE TABLE small(a)",
            "CREATE TABLE empty(a)",
            &big_seed,
            "INSERT INTO small VALUES (1),(2)",
        ]);

        let info = read_initial(&path, 10).unwrap();
        let sel = &info.tables[info.selected_table];
        assert_eq!(sel.name, "small");
        assert_eq!(info.initial_page.rows.len(), 2);
    }

    #[test]
    fn read_initial_caps_page_size() {
        let seed = seq_insert("numbers", "n", 100);
        let (_tmp, path) = setup_db(&["CREATE TABLE numbers(n INTEGER)", &seed]);

        let info = read_initial(&path, 10).unwrap();
        assert_eq!(info.initial_page.rows.len(), 10);
        assert_eq!(info.tables[0].row_count, 100);
    }

    #[test]
    fn load_page_paginates() {
        let seed = seq_insert("numbers", "n", 25);
        let (_tmp, path) = setup_db(&["CREATE TABLE numbers(n INTEGER)", &seed]);

        let p1 = load_page(&path, "numbers", 0, 10).unwrap();
        let p2 = load_page(&path, "numbers", 10, 10).unwrap();
        let p3 = load_page(&path, "numbers", 20, 10).unwrap();

        assert_eq!(p1.rows.len(), 10);
        assert_eq!(p2.rows.len(), 10);
        assert_eq!(p3.rows.len(), 5);
        // First cell of first row of page 1 is 1; of page 2 is 11.
        assert_eq!(p1.rows[0][0], SqliteValue::Integer(1));
        assert_eq!(p2.rows[0][0], SqliteValue::Integer(11));
    }

    #[test]
    fn cell_types_round_trip() {
        let (_tmp, path) = setup_db(&[
            "CREATE TABLE t(i INTEGER, r REAL, s TEXT, b BLOB, n INTEGER)",
            // explicit NULL in `n` so we cover all five ValueRef variants.
            // Real value is `1.5` (exactly representable in IEEE 754) rather
            // than something like `3.14` — clippy's approx_constant lint
            // otherwise nags about it being too close to π.
            "INSERT INTO t VALUES (42, 1.5, 'hello', x'deadbeef', NULL)",
        ]);

        let page = load_page(&path, "t", 0, 10).unwrap();
        let row = &page.rows[0];
        assert_eq!(row[0], SqliteValue::Integer(42));
        assert_eq!(row[1], SqliteValue::Real(1.5));
        assert_eq!(
            row[2],
            SqliteValue::Text {
                value: "hello".into(),
                truncated: false
            }
        );
        assert_eq!(row[3], SqliteValue::Blob { len: 4 });
        assert_eq!(row[4], SqliteValue::Null);
    }

    #[test]
    fn long_text_is_truncated_with_marker() {
        let big = "x".repeat(MAX_TEXT_CELL_CHARS + 50);
        let (_tmp, path) = setup_db(&[
            "CREATE TABLE notes(body TEXT)",
            &format!("INSERT INTO notes VALUES ('{big}')"),
        ]);

        let page = load_page(&path, "notes", 0, 1).unwrap();
        match &page.rows[0][0] {
            SqliteValue::Text { value, truncated } => {
                assert!(*truncated, "long text should be flagged truncated");
                assert_eq!(value.chars().count(), MAX_TEXT_CELL_CHARS);
            }
            other => panic!("expected truncated Text, got {other:?}"),
        }
    }

    #[test]
    fn empty_database_returns_empty_tables_and_page() {
        // `Connection::open` doesn't write the SQLite header until the
        // first schema-modifying statement, so a no-setup DB is 0 bytes
        // on disk and fails the magic-bytes probe. `PRAGMA user_version`
        // forces a single-page write without creating any user tables —
        // gives us a real SQLite file with zero user-visible schema.
        let (_tmp, path) = setup_db(&["PRAGMA user_version = 0"]);
        let info = read_initial(&path, 10).unwrap();
        assert!(info.tables.is_empty());
        assert!(info.initial_page.rows.is_empty());
        assert_eq!(info.selected_table, 0);
    }

    #[test]
    fn database_with_only_empty_tables_lands_on_first_table() {
        let (_tmp, path) = setup_db(&["CREATE TABLE a(x)", "CREATE TABLE b(x)"]);
        let info = read_initial(&path, 10).unwrap();
        // Both empty → selected_table falls back to 0, initial_page empty.
        assert_eq!(info.selected_table, 0);
        assert!(info.initial_page.rows.is_empty());
        assert_eq!(info.tables[0].row_count, 0);
        assert_eq!(info.tables[1].row_count, 0);
    }

    #[test]
    fn not_sqlite_returns_specific_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fake.db");
        std::fs::write(&path, b"this is just text, not a real sqlite database").unwrap();
        match read_initial(&path, 10) {
            Err(PreviewError::NotSqlite) => {}
            other => panic!("expected NotSqlite, got {other:?}"),
        }
    }

    #[test]
    fn quote_ident_doubles_embedded_quotes() {
        assert_eq!(quote_ident("simple"), r#""simple""#);
        assert_eq!(quote_ident(r#"a"b"#), r#""a""b""#);
        assert_eq!(quote_ident(r#"with space"#), r#""with space""#);
    }

    #[test]
    fn views_appear_in_table_list() {
        let (_tmp, path) = setup_db(&[
            "CREATE TABLE t(a)",
            "INSERT INTO t VALUES (1),(2),(3)",
            "CREATE VIEW v AS SELECT a FROM t",
        ]);
        let info = read_initial(&path, 10).unwrap();
        let names: Vec<_> = info.tables.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"t"));
        assert!(names.contains(&"v"));
    }

    #[test]
    fn open_handles_path_with_percent_question_hash() {
        // Workdir paths with `%`, `?`, `#` must round-trip through
        // SQLite's URI parser without getting reinterpreted as
        // percent-encoded bytes / query separators / fragment
        // markers. Pre-fix, a `%41` in a path would silently turn
        // into `A` and open the wrong file (or fail).
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a%41b ?#dir");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fixture.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES (1);")
            .unwrap();
        drop(conn);

        let info = read_initial(&path, 10).unwrap();
        assert_eq!(info.tables.len(), 1);
        assert_eq!(info.tables[0].name, "t");
    }

    #[test]
    fn read_initial_works_on_wal_database() {
        // Real-world SQLite databases that are actively being written
        // run in WAL journaling mode; their `-wal` / `-shm` sidecar
        // files are present alongside the main `.db`. Earlier the
        // reader opened with `?mode=ro&immutable=1`, which SQLite
        // refuses to honour when a WAL is active. This test creates a
        // WAL-mode DB and checks the reader handles it.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wal.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; \
             CREATE TABLE t(a INTEGER); \
             INSERT INTO t VALUES (1), (2), (3);",
        )
        .unwrap();
        // Keep the writer connection alive across the read so the
        // `-wal` / `-shm` sidecars stay un-checkpointed when the
        // reader opens. Closing `conn` first would trigger an
        // implicit checkpoint and delete the sidecars, weakening the
        // test to "regular DB after close".
        let info = read_initial(&path, 10).unwrap();
        assert!(!info.tables.is_empty(), "should find table 't'");
        assert_eq!(info.tables[0].name, "t");
        assert_eq!(info.tables[0].row_count, 3);
        drop(conn);
    }

    #[test]
    fn is_sqlite_file_combines_extension_and_magic() {
        // Real SQLite with .db extension → true.
        let (_tmp1, real) = setup_db(&["CREATE TABLE x(a)"]);
        assert!(is_sqlite_file(&real));

        // Real SQLite renamed without recognised extension → false.
        let renamed = real.with_extension("bin");
        std::fs::copy(&real, &renamed).unwrap();
        assert!(!is_sqlite_file(&renamed));

        // Wrong content with .db extension → false.
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake.db");
        std::fs::write(&fake, b"definitely not a database").unwrap();
        assert!(!is_sqlite_file(&fake));
    }
}
