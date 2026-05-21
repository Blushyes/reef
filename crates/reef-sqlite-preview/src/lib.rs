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
    /// Caller asked for row data on a kind that doesn't have rows
    /// (Index / Trigger). Surfaced as a typed error so the UI can
    /// route to the detail pane instead of querying a bogus page.
    UnsupportedObjectKind,
    /// A specific schema-qualified object wasn't found in `sqlite_master`.
    /// Happens when the cached UI state refers to an object that has
    /// been dropped underneath us (rare with `mode=ro`, but possible
    /// across reconnects).
    ObjectNotFound { schema: String, name: String },
}

impl std::fmt::Display for PreviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PreviewError::NotSqlite => f.write_str("not a SQLite database"),
            PreviewError::TooLarge { size } => write!(f, "file too large ({size} bytes)"),
            PreviewError::OpenFailed(s) => write!(f, "open failed: {s}"),
            PreviewError::QueryFailed(s) => write!(f, "query failed: {s}"),
            PreviewError::Io(s) => write!(f, "io: {s}"),
            PreviewError::UnsupportedObjectKind => {
                f.write_str("object kind has no rows (index/trigger)")
            }
            PreviewError::ObjectNotFound { schema, name } => {
                write!(f, "object not found: {schema}.{name}")
            }
        }
    }
}

impl std::error::Error for PreviewError {}

/// One column on a table — name + declared type as it appears in
/// `PRAGMA table_info`. SQLite's type system is "type affinity" not
/// strict, so the declared type may be empty (no declared type) or
/// a non-standard string ("VARCHAR(255)", "DATETIME", …). We pass it
/// through verbatim and let the render layer truncate if needed.
///
/// `notnull` and `pk` are populated from the same `PRAGMA table_info`
/// row; renderers use them for inline column annotations (PK / NOT NULL
/// chips in detail views) without an extra round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub decl_type: String,
    pub notnull: bool,
    pub pk: bool,
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

// ─── V2 multi-schema types ──────────────────────────────────────────────
//
// The V1 [`DatabaseInfo`] above assumes a single `main` schema with only
// tables + views. V2 widens the model to mirror what SQLite actually
// exposes: `PRAGMA database_list` returns `main` plus `temp` plus any
// ATTACHed files, each with its own `sqlite_master` table covering
// tables, views, indexes, triggers, and virtual tables. The wire DTOs
// in `reef-proto` (V2 variants) mirror these structs 1:1.

/// One kind of database object as recorded in `sqlite_master.type`.
/// Virtual tables share the `Table` variant — the distinction is
/// carried separately on [`DbObject::is_virtual`] so existing
/// kind-based dispatch (has-rows? has-detail-pane?) stays clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DbObjectKind {
    Table,
    View,
    Index,
    Trigger,
}

impl DbObjectKind {
    /// Tables and views have queryable rows; indexes and triggers don't.
    /// The renderer routes the data pane vs the detail pane on this
    /// boolean, and [`load_page_qualified`] rejects non-row kinds.
    pub fn has_rows(self) -> bool {
        matches!(self, Self::Table | Self::View)
    }

    /// Value seen in `sqlite_master.type`.
    pub fn as_master_type(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
            Self::Index => "index",
            Self::Trigger => "trigger",
        }
    }

    /// Inverse of [`Self::as_master_type`]. Returns `None` for unknown
    /// strings so we can gracefully skip any future type SQLite adds.
    pub fn from_master_type(s: &str) -> Option<Self> {
        match s {
            "table" => Some(Self::Table),
            "view" => Some(Self::View),
            "index" => Some(Self::Index),
            "trigger" => Some(Self::Trigger),
            _ => None,
        }
    }

    /// Title-case plural form used by the sidebar's subsection header
    /// (e.g. `Tables (12)`).
    pub fn section_label(self) -> &'static str {
        match self {
            Self::Table => "Tables",
            Self::View => "Views",
            Self::Index => "Indexes",
            Self::Trigger => "Triggers",
        }
    }
}

/// Classification for a row in `PRAGMA database_list`. The renderer
/// uses this to group schemas (Main first, then Temp, then Attached).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SchemaKind {
    /// The default `main` schema — the on-disk file itself.
    Main,
    /// The implicit `temp` schema for CREATE TEMP TABLE / VIEW. Only
    /// populated when the connection has temp objects, but the entry
    /// itself appears in `PRAGMA database_list` whether or not any
    /// temp objects exist.
    Temp,
    /// A schema attached via `ATTACH DATABASE … AS name`. The reader
    /// never issues ATTACH itself; this variant exists so a UI feature
    /// that does (or a connection inherited from an external source)
    /// renders correctly.
    Attached,
}

/// Identifier for a single schema-qualified object. Hashable + Clone
/// so the UI can use it as a HashMap / BTreeSet key (e.g. for the
/// "currently selected object" pointer in `DbPreviewState`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DbObjectKey {
    pub schema: String,
    pub name: String,
    pub kind: DbObjectKind,
}

/// One object discovered in a schema's `sqlite_master`. Carries every
/// field the UI needs to render its sidebar row + decide whether to
/// load a page (`kind.has_rows()`) or a detail pane.
///
/// `columns` is populated eagerly for Table / View so the initial RPC
/// payload covers the whole schema in one round-trip; Index / Trigger
/// leave it empty.
#[derive(Debug, Clone)]
pub struct DbObject {
    pub schema: String,
    pub name: String,
    pub kind: DbObjectKind,
    /// For Index / Trigger: the table they reference. For Table / View:
    /// `Some(name)` (sqlite_master sets `tbl_name = name` for those).
    pub tbl_name: Option<String>,
    /// `None` for non-row kinds and for views (where COUNT(*) might be
    /// expensive). Tables always populate this eagerly.
    pub row_count: Option<u64>,
    /// Empty for Index / Trigger.
    pub columns: Vec<ColumnInfo>,
    /// `true` when the table was created via `CREATE VIRTUAL TABLE`
    /// (FTS, RTree, etc.). The renderer adds a `ⓥ` glyph to the
    /// sidebar row.
    pub is_virtual: bool,
    /// Best-effort: parsed from the trailing `WITHOUT ROWID` modifier
    /// in `sqlite_master.sql`. Display-only, doesn't affect querying.
    pub is_without_rowid: bool,
    /// Best-effort: parsed from a trailing `STRICT` modifier.
    pub is_strict: bool,
}

impl DbObject {
    pub fn key(&self) -> DbObjectKey {
        DbObjectKey {
            schema: self.schema.clone(),
            name: self.name.clone(),
            kind: self.kind,
        }
    }
}

/// One schema in `PRAGMA database_list`, with its full object inventory.
#[derive(Debug, Clone)]
pub struct SchemaSummary {
    pub name: String,
    pub kind: SchemaKind,
    /// File path SQLite reports for this schema. Empty for `temp`
    /// (in-memory), `Some` for `main` (the opened file) and any
    /// attached schemas. We expose the raw string; the renderer
    /// shortens it for display.
    pub file: Option<String>,
    pub objects: Vec<DbObject>,
    /// `true` when [`MAX_OBJECTS_PER_SCHEMA`] kicked in and trimmed
    /// the list. The renderer shows a "+N more" hint when set.
    pub truncated: bool,
}

/// Top-level V2 payload. Built once per `.db` open; pagination still
/// goes through [`load_page_qualified`].
#[derive(Debug, Clone)]
pub struct DatabaseInfoV2 {
    pub schemas: Vec<SchemaSummary>,
    /// Schema chosen as the initial selection — `main` when present.
    pub default_schema: String,
    /// Object the initial page was read from. `None` only when every
    /// schema is empty.
    pub default_object: Option<DbObjectKey>,
    /// First page of rows for `default_object`, or empty.
    pub initial_page: DbPage,
    pub bytes_on_disk: u64,
}

impl DatabaseInfoV2 {
    /// Look up an object by its schema-qualified key. Returns `None`
    /// when the key references a schema or object that's not in the
    /// graph (stale selection across previews).
    pub fn lookup(&self, key: &DbObjectKey) -> Option<&DbObject> {
        self.schemas
            .iter()
            .find(|s| s.name == key.schema)?
            .objects
            .iter()
            .find(|o| o.name == key.name && o.kind == key.kind)
    }

    /// Flat iterator over every row-bearing object (Tables + Views)
    /// across every schema. Used by the pagination footer to compute
    /// `(i/N)` and by `db_navigate` to walk the navigation cycle.
    pub fn iter_row_bearing(&self) -> impl Iterator<Item = &DbObject> {
        self.schemas
            .iter()
            .flat_map(|s| s.objects.iter())
            .filter(|o| o.kind.has_rows())
    }
}

/// When a trigger fires relative to the row event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerTiming {
    Before,
    After,
    /// `INSTEAD OF` triggers, which only attach to views.
    InsteadOf,
    /// Couldn't parse the timing from the CREATE TRIGGER SQL. Renderer
    /// falls back to displaying the raw SQL.
    Unknown,
}

/// Which DML event the trigger reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerEvent {
    Insert,
    Update,
    Delete,
    Unknown,
}

/// Detail-pane payload for a non-row object (Index / Trigger) or a
/// schema-level description of a Table / View. Indexes contribute
/// uniqueness + column ordering + partial-WHERE clause; triggers
/// contribute parsed timing/event plus the raw SQL body.
#[derive(Debug, Clone)]
pub enum DbObjectDetail {
    Table {
        create_sql: Option<String>,
    },
    View {
        create_sql: Option<String>,
    },
    Index {
        unique: bool,
        /// Indexed column names in key order. Entries are `"<expr>"`
        /// for expression indexes where SQLite can't surface a name.
        columns: Vec<String>,
        /// Tail of `CREATE INDEX … WHERE <expr>` when this is a
        /// partial index. Best-effort string slice.
        partial_where: Option<String>,
        /// Table this index belongs to.
        tbl_name: String,
        create_sql: Option<String>,
    },
    Trigger {
        timing: TriggerTiming,
        event: TriggerEvent,
        tbl_name: String,
        sql: String,
    },
}

/// Per-schema cap on object listing. Most real databases have ≤ a few
/// hundred objects; this is defensive against pathological auto-generated
/// schemas (per-tenant tables, ETL staging fan-out, etc.) so a single
/// browse doesn't allocate a million strings.
pub const MAX_OBJECTS_PER_SCHEMA: usize = 5_000;

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

/// Read column metadata for a table in the default `main` schema. Thin
/// wrapper over [`read_columns_qualified`] kept for the V1 wire path —
/// internal callers (V2) should use the qualified version directly.
fn read_columns(conn: &Connection, table: &str) -> Result<Vec<ColumnInfo>, PreviewError> {
    read_columns_qualified(conn, "main", table)
}

/// Read column metadata via `PRAGMA <schema>.table_info(<table>)`. Order
/// matches the table's declared column order — same as a `SELECT *`
/// would yield. Returns name, declared type, NOT NULL flag, and primary
/// key flag.
///
/// PRAGMA statements don't accept bound parameters in SQLite, so the
/// schema and table identifiers are interpolated through [`quote_ident`]
/// — safe by construction (double-quote wrapping with embedded-quote
/// doubling), and identifiers come from `sqlite_master` / `PRAGMA
/// database_list` rather than user input.
pub fn read_columns_qualified(
    conn: &Connection,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnInfo>, PreviewError> {
    let sql = format!(
        "PRAGMA {}.table_info({})",
        quote_ident(schema),
        quote_ident(table)
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?;
    // PRAGMA table_info returns: cid, name, type, notnull, dflt_value, pk
    let cols: Vec<ColumnInfo> = stmt
        .query_map([], |row| {
            Ok(ColumnInfo {
                name: row.get::<_, String>(1)?,
                decl_type: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                notnull: row.get::<_, i64>(3)? != 0,
                pk: row.get::<_, i64>(5)? != 0,
            })
        })
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?;
    Ok(cols)
}

/// `SELECT COUNT(*) FROM "schema"."table"`. Cheap on small/medium
/// tables; gets pricier on multi-million-row tables but still fast
/// enough for a preview load.
fn count_rows(conn: &Connection, table: &str) -> Result<u64, PreviewError> {
    count_rows_qualified(conn, "main", table)
}

/// Schema-qualified variant of [`count_rows`]. Used by V2 to count rows
/// in `temp` and attached schemas; V1 wraps this with `schema="main"`.
pub fn count_rows_qualified(
    conn: &Connection,
    schema: &str,
    table: &str,
) -> Result<u64, PreviewError> {
    let sql = format!("SELECT COUNT(*) FROM {}", quote_qualified(schema, table));
    conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map(|n| n.max(0) as u64)
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))
}

/// Quote a SQLite identifier by wrapping in double quotes and doubling
/// any embedded `"`. Same rules as
/// `https://sqlite.org/lang_keywords.html`. We can't bind table names
/// as parameters in standard SQL — they're identifiers, not values —
/// so this is the safe-by-construction path.
pub fn quote_ident(ident: &str) -> String {
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

/// Quote a schema-qualified identifier as `"schema"."name"`. Both halves
/// go through [`quote_ident`] so a malicious-looking schema or object
/// name (none in practice — both come from `sqlite_master` / `PRAGMA
/// database_list`) can't escape the quoting.
pub fn quote_qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
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
    load_page_qualified(path, "main", DbObjectKind::Table, table, offset, limit)
}

/// Schema + kind-aware variant of [`load_page`]. Used by the V2 wire
/// path so the UI can browse `temp` schema tables, attached databases,
/// and views (which are read identically to tables — `SELECT * FROM
/// view_name`).
///
/// Returns [`PreviewError::UnsupportedObjectKind`] when called with
/// `kind = Index | Trigger` — those don't have rows and should be
/// routed through [`read_object_detail`] instead.
pub fn load_page_qualified(
    path: &Path,
    schema: &str,
    kind: DbObjectKind,
    name: &str,
    offset: u64,
    limit: u32,
) -> Result<DbPage, PreviewError> {
    if !kind.has_rows() {
        return Err(PreviewError::UnsupportedObjectKind);
    }
    if !probe_magic(path)? {
        return Err(PreviewError::NotSqlite);
    }
    let conn = open_readonly(path)?;
    read_page_qualified(&conn, schema, name, offset, limit)
}

fn read_page_qualified(
    conn: &Connection,
    schema: &str,
    name: &str,
    offset: u64,
    limit: u32,
) -> Result<DbPage, PreviewError> {
    let columns = read_columns_qualified(conn, schema, name)?;
    if columns.is_empty() {
        // Either the object doesn't exist or is genuinely zero-column —
        // both rare and both handled identically (empty page).
        return Ok(DbPage { rows: Vec::new() });
    }
    let sql = format!(
        "SELECT * FROM {} LIMIT ?1 OFFSET ?2",
        quote_qualified(schema, name)
    );
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

    let initial_page = read_page_qualified(
        &conn,
        "main",
        &tables[selected_table].name,
        0,
        initial_page_size,
    )?;

    Ok(DatabaseInfo {
        tables,
        selected_table,
        initial_page,
        bytes_on_disk,
    })
}

// ─── V2 multi-schema reader ─────────────────────────────────────────────

/// List schemas attached to this connection. Always returns at least one
/// entry (`main`) — `temp` is also always listed even when empty, and
/// any user-issued `ATTACH DATABASE` shows up here too.
pub fn list_databases(
    conn: &Connection,
) -> Result<Vec<(String, SchemaKind, Option<String>)>, PreviewError> {
    let mut stmt = conn
        .prepare("PRAGMA database_list")
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?;
    // Columns: seq (int), name (text), file (text — may be empty for
    // in-memory / temp). Map empty-string file to `None`.
    let rows = stmt
        .query_map([], |row| {
            let name: String = row.get(1)?;
            let file: Option<String> = row.get::<_, Option<String>>(2)?;
            let kind = match name.as_str() {
                "main" => SchemaKind::Main,
                "temp" => SchemaKind::Temp,
                _ => SchemaKind::Attached,
            };
            let file_opt = file.filter(|s| !s.is_empty());
            Ok((name, kind, file_opt))
        })
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))
}

/// List every user-visible object in one schema, with eagerly-populated
/// columns + row counts for tables. `max_count` caps the returned `Vec`;
/// the second element of the returned tuple is `true` when the cap
/// kicked in.
///
/// Filtering rules:
/// - `name NOT LIKE 'sqlite_%'` excludes internal bookkeeping
///   (`sqlite_sequence`, `sqlite_autoindex_*`, etc.).
/// - FTS5 shadow tables (`<vt>_data`, `<vt>_idx`, `<vt>_content`,
///   `<vt>_docsize`, `<vt>_config`) are hidden when their parent
///   virtual table appears in the same schema. Modern SQLite populates
///   `sqlite_master.sql` for these shadows so we can't rely on
///   `sql IS NULL` as the discriminator.
/// - Rows with NULL sql that we still can't classify are dropped — they
///   represent auto-created bookkeeping the user almost never wants.
/// - Ordering: tables, then views, then indexes, then triggers; within
///   each, alphabetically.
pub fn list_objects(
    conn: &Connection,
    schema: &str,
    max_count: usize,
) -> Result<(Vec<DbObject>, bool), PreviewError> {
    let master = format!("{}.sqlite_master", quote_ident(schema));
    let sql = format!(
        "SELECT type, name, tbl_name, sql FROM {master} \
         WHERE name NOT LIKE 'sqlite_%' \
         ORDER BY \
           CASE type WHEN 'table' THEN 0 WHEN 'view' THEN 1 \
                     WHEN 'index' THEN 2 WHEN 'trigger' THEN 3 \
                     ELSE 4 END, \
           name"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?;
    let raw: Vec<(String, String, Option<String>, Option<String>)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?;

    // Collect virtual-table names so we can recognise and drop their
    // FTS shadow tables in the post-filter below. Modern FTS5 records
    // shadow tables in sqlite_master with non-NULL sql, so we can't
    // filter them out in the SQL WHERE clause alone.
    let virtual_tables: std::collections::HashSet<String> = raw
        .iter()
        .filter(|(t, _, _, sql)| {
            t == "table" && sql.as_deref().is_some_and(is_create_virtual_table)
        })
        .map(|(_, name, _, _)| name.clone())
        .collect();

    let is_fts_shadow = |name: &str| -> bool {
        const SHADOW_SUFFIXES: &[&str] = &["_data", "_idx", "_content", "_docsize", "_config"];
        for suffix in SHADOW_SUFFIXES {
            if let Some(stripped) = name.strip_suffix(suffix)
                && virtual_tables.contains(stripped)
            {
                return true;
            }
        }
        false
    };

    let filtered: Vec<_> = raw
        .into_iter()
        .filter(|(t, name, _, sql)| {
            // Drop FTS shadows (regardless of sql presence).
            if t == "table" && is_fts_shadow(name) {
                return false;
            }
            // Drop rows with NULL sql except for genuine virtual tables
            // (their sql IS populated). NULL-sql rows that survive past
            // the FTS shadow filter are auto-indexes / other internal
            // helpers the user can't act on.
            sql.is_some()
        })
        .collect();

    let truncated = filtered.len() > max_count;
    let mut out = Vec::with_capacity(filtered.len().min(max_count));
    for (type_str, name, tbl_name, sql) in filtered.into_iter().take(max_count) {
        let Some(kind) = DbObjectKind::from_master_type(&type_str) else {
            continue;
        };
        let sql_ref = sql.as_deref();
        let is_virtual = sql_ref.is_some_and(is_create_virtual_table);
        let is_without_rowid = sql_ref.is_some_and(sql_has_without_rowid_suffix);
        let is_strict = sql_ref.is_some_and(sql_has_strict_modifier);

        // Eagerly populate columns for table / view so the UI can size
        // its data grid without a follow-up RPC. For indexes / triggers
        // we leave the vec empty — they don't have a row schema.
        let columns = if matches!(kind, DbObjectKind::Table | DbObjectKind::View) {
            read_columns_qualified(conn, schema, &name).unwrap_or_default()
        } else {
            Vec::new()
        };

        // Row counts: tables only. Views might be expensive (arbitrary
        // SELECT); indexes / triggers don't have rows. Failures here
        // are soft — we'd rather show the object with `—` count than
        // hide it.
        let row_count = if matches!(kind, DbObjectKind::Table) {
            count_rows_qualified(conn, schema, &name).ok()
        } else {
            None
        };

        out.push(DbObject {
            schema: schema.to_string(),
            name,
            kind,
            tbl_name,
            row_count,
            columns,
            is_virtual,
            is_without_rowid,
            is_strict,
        });
    }
    Ok((out, truncated))
}

/// Top-level V2 entry. Walks every schema, reads its objects, picks an
/// initial selection (smallest non-empty table in `main`), and reads
/// one page of rows. The returned [`DatabaseInfoV2`] is everything the
/// UI needs to render its first frame.
pub fn read_initial_v2(
    path: &Path,
    initial_page_size: u32,
) -> Result<DatabaseInfoV2, PreviewError> {
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

    let dbs = list_databases(&conn)?;
    let mut schemas = Vec::with_capacity(dbs.len());
    for (db_name, kind, file) in dbs {
        let (objects, truncated) = list_objects(&conn, &db_name, MAX_OBJECTS_PER_SCHEMA)?;
        schemas.push(SchemaSummary {
            name: db_name,
            kind,
            file,
            objects,
            truncated,
        });
    }

    // Default schema: prefer `main`, then first schema with objects,
    // then just the first schema.
    let default_schema = schemas
        .iter()
        .find(|s| s.kind == SchemaKind::Main)
        .map(|s| s.name.clone())
        .or_else(|| {
            schemas
                .iter()
                .find(|s| !s.objects.is_empty())
                .map(|s| s.name.clone())
        })
        .or_else(|| schemas.first().map(|s| s.name.clone()))
        .unwrap_or_else(|| "main".to_string());

    // Default object: smallest non-empty table in default schema, else
    // first table-or-view in default schema. Indexes / triggers never
    // chosen as initial selection.
    let default_object = schemas
        .iter()
        .find(|s| s.name == default_schema)
        .and_then(pick_default_object);

    let initial_page = match &default_object {
        Some(key) if key.kind.has_rows() => {
            read_page_qualified(&conn, &key.schema, &key.name, 0, initial_page_size)?
        }
        _ => DbPage { rows: Vec::new() },
    };

    Ok(DatabaseInfoV2 {
        schemas,
        default_schema,
        default_object,
        initial_page,
        bytes_on_disk,
    })
}

/// Path-level convenience: open a read-only connection on the file and
/// dispatch to [`read_object_detail`]. Mirrors the V1 [`load_page`]
/// shape so call sites (agent handlers, local backend) don't have to
/// open the connection themselves.
pub fn read_object_detail_at(
    path: &Path,
    schema: &str,
    kind: DbObjectKind,
    name: &str,
) -> Result<DbObjectDetail, PreviewError> {
    if !probe_magic(path)? {
        return Err(PreviewError::NotSqlite);
    }
    let conn = open_readonly(path)?;
    read_object_detail(&conn, schema, kind, name)
}

/// Dispatch detail-pane reads by object kind. Tables / Views return
/// their `CREATE` SQL; Indexes / Triggers return parsed structural
/// detail (see [`DbObjectDetail`]).
pub fn read_object_detail(
    conn: &Connection,
    schema: &str,
    kind: DbObjectKind,
    name: &str,
) -> Result<DbObjectDetail, PreviewError> {
    match kind {
        DbObjectKind::Index => read_index_detail(conn, schema, name),
        DbObjectKind::Trigger => read_trigger_detail(conn, schema, name),
        DbObjectKind::Table => Ok(DbObjectDetail::Table {
            create_sql: read_create_sql(conn, schema, name)?,
        }),
        DbObjectKind::View => Ok(DbObjectDetail::View {
            create_sql: read_create_sql(conn, schema, name)?,
        }),
    }
}

/// Read an index's structural detail: unique flag (from `index_list`),
/// column ordering (from `index_info`), partial-WHERE expression
/// (sliced from `sqlite_master.sql`).
pub fn read_index_detail(
    conn: &Connection,
    schema: &str,
    name: &str,
) -> Result<DbObjectDetail, PreviewError> {
    let master = format!("{}.sqlite_master", quote_ident(schema));
    let master_sql = format!("SELECT tbl_name, sql FROM {master} WHERE type='index' AND name=?1");
    let (tbl_name, create_sql): (String, Option<String>) = conn
        .query_row(&master_sql, rusqlite::params![name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => PreviewError::ObjectNotFound {
                schema: schema.to_string(),
                name: name.to_string(),
            },
            other => PreviewError::QueryFailed(other.to_string()),
        })?;

    // PRAGMA index_info returns: seqno, cid, name (NULL for expressions).
    let info_sql = format!(
        "PRAGMA {}.index_info({})",
        quote_ident(schema),
        quote_ident(name)
    );
    let mut stmt = conn
        .prepare(&info_sql)
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?;
    let columns: Vec<String> = stmt
        .query_map([], |row| {
            row.get::<_, Option<String>>(2)
                .map(|n| n.unwrap_or_else(|| "<expr>".to_string()))
        })
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?;

    // PRAGMA index_list returns: seq, name, unique, origin, partial.
    // We could pull `partial` directly but the WHERE clause itself
    // only lives in `sqlite_master.sql`, so parse it from there.
    let list_sql = format!(
        "PRAGMA {}.index_list({})",
        quote_ident(schema),
        quote_ident(&tbl_name)
    );
    let mut stmt = conn
        .prepare(&list_sql)
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?;
    let unique = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)? != 0))
        })
        .map_err(|e| PreviewError::QueryFailed(e.to_string()))?
        .filter_map(|r| r.ok())
        .find(|(n, _)| n == name)
        .map(|(_, u)| u)
        .unwrap_or(false);

    let partial_where = create_sql.as_deref().and_then(extract_partial_where);

    Ok(DbObjectDetail::Index {
        unique,
        columns,
        partial_where,
        tbl_name,
        create_sql,
    })
}

/// Read a trigger's structural detail. Timing + event are parsed from
/// the header tokens of the CREATE TRIGGER SQL (best-effort) and the
/// raw body is preserved for display.
pub fn read_trigger_detail(
    conn: &Connection,
    schema: &str,
    name: &str,
) -> Result<DbObjectDetail, PreviewError> {
    let master = format!("{}.sqlite_master", quote_ident(schema));
    let master_sql = format!("SELECT tbl_name, sql FROM {master} WHERE type='trigger' AND name=?1");
    let (tbl_name, sql): (String, String) = conn
        .query_row(&master_sql, rusqlite::params![name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => PreviewError::ObjectNotFound {
                schema: schema.to_string(),
                name: name.to_string(),
            },
            other => PreviewError::QueryFailed(other.to_string()),
        })?;

    let (timing, event) = parse_trigger_header(&sql);
    Ok(DbObjectDetail::Trigger {
        timing,
        event,
        tbl_name,
        sql,
    })
}

/// Helper: read `sqlite_master.sql` for an object, regardless of type.
/// Used by [`read_object_detail`] for Table / View.
fn read_create_sql(
    conn: &Connection,
    schema: &str,
    name: &str,
) -> Result<Option<String>, PreviewError> {
    let master = format!("{}.sqlite_master", quote_ident(schema));
    let sql = format!("SELECT sql FROM {master} WHERE name=?1");
    match conn.query_row(&sql, rusqlite::params![name], |row| {
        row.get::<_, Option<String>>(0)
    }) {
        Ok(s) => Ok(s),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(PreviewError::QueryFailed(e.to_string())),
    }
}

// ─── V2 parsing helpers ─────────────────────────────────────────────────

fn pick_default_object(schema: &SchemaSummary) -> Option<DbObjectKey> {
    // Smallest non-empty table wins. Tied counts → alphabetical (the
    // object list is already sorted that way, so min_by_key is stable
    // on ties via insertion order).
    let smallest_table = schema
        .objects
        .iter()
        .filter(|o| matches!(o.kind, DbObjectKind::Table) && o.row_count.unwrap_or(0) > 0)
        .min_by_key(|o| o.row_count.unwrap_or(u64::MAX));
    if let Some(o) = smallest_table {
        return Some(o.key());
    }
    // No non-empty tables → first table-or-view, so the user lands on
    // something with a (possibly empty) data grid rather than a blank
    // pane.
    schema
        .objects
        .iter()
        .find(|o| matches!(o.kind, DbObjectKind::Table | DbObjectKind::View))
        .map(DbObject::key)
}

fn is_create_virtual_table(sql: &str) -> bool {
    // "CREATE VIRTUAL TABLE" is 20 ASCII bytes; compare via byte slice
    // to avoid the String allocation that `chars().take(20).collect()`
    // would impose on every list_objects row.
    const PREFIX: &[u8; 20] = b"CREATE VIRTUAL TABLE";
    let bytes = sql.trim_start().as_bytes();
    bytes.len() >= PREFIX.len() && bytes[..PREFIX.len()].eq_ignore_ascii_case(PREFIX)
}

fn sql_has_without_rowid_suffix(sql: &str) -> bool {
    let trimmed = sql.trim_end_matches(|c: char| c.is_whitespace() || c == ';');
    // Comparison target is pure ASCII, so we can byte-compare the tail
    // without touching the UTF-8 boundaries elsewhere in the string —
    // important because user table comments / column names commonly
    // contain multi-byte characters (CJK, emoji, accented Latin) and
    // a naive `&trimmed[trimmed.len()-N..]` slice can land mid-char
    // and panic.
    let tail = b"without rowid";
    let bytes = trimmed.as_bytes();
    bytes.len() >= tail.len() && bytes[bytes.len() - tail.len()..].eq_ignore_ascii_case(tail)
}

fn sql_has_strict_modifier(sql: &str) -> bool {
    // STRICT can appear either at the trailing end (after the closing
    // paren) or alongside WITHOUT ROWID separated by a comma. Cheap
    // best-effort scan — display-only, no querying behavior depends on
    // this.
    let trimmed = sql.trim_end_matches(|c: char| c.is_whitespace() || c == ';');
    let lower = trimmed.to_lowercase();
    lower.ends_with("strict")
        || lower.contains(", strict")
        || lower.contains(",strict")
        || lower.contains(" strict,")
        || lower.contains(" strict ")
}

fn extract_partial_where(sql: &str) -> Option<String> {
    // Last `WHERE` token in the CREATE INDEX is the partial predicate.
    // Case-insensitive search by lowercasing only the ASCII letters
    // in-place at the byte level, which preserves byte positions
    // identical to `sql`'s — so the position from `rfind` can safely
    // slice back into `sql`. (Calling `str::to_lowercase` would shift
    // byte indices for non-ASCII chars like 'İ' or 'ß'.)
    let mut lower_bytes = sql.as_bytes().to_vec();
    for b in lower_bytes.iter_mut() {
        b.make_ascii_lowercase();
    }
    let key = b" where ";
    let pos = lower_bytes.windows(key.len()).rposition(|w| w == key)?;
    let start = pos + key.len();
    // `start` is guaranteed to be at a UTF-8 char boundary because
    // it follows the ASCII space character — the byte after a single
    // ASCII byte is always a boundary.
    let rest = sql[start..].trim().trim_end_matches(';').trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

fn parse_trigger_header(sql: &str) -> (TriggerTiming, TriggerEvent) {
    // Header = everything before the `BEGIN` keyword. Splitting at a
    // word boundary avoids matching `BEGIN` inside an identifier (rare,
    // but possible since SQLite tolerates `BEGIN` quoted as an
    // identifier).
    let lower = sql.to_lowercase();
    let header_end = lower
        .find(" begin ")
        .or_else(|| lower.find("\nbegin"))
        .or_else(|| lower.find("\tbegin"))
        .unwrap_or(lower.len());
    let header: &str = &lower[..header_end];
    let tokens: Vec<&str> = header.split_whitespace().collect();

    let timing = if tokens.contains(&"before") {
        TriggerTiming::Before
    } else if tokens.contains(&"after") {
        TriggerTiming::After
    } else if tokens.windows(2).any(|w| w[0] == "instead" && w[1] == "of") {
        TriggerTiming::InsteadOf
    } else {
        TriggerTiming::Unknown
    };

    let event = if tokens.contains(&"insert") {
        TriggerEvent::Insert
    } else if tokens.contains(&"update") {
        TriggerEvent::Update
    } else if tokens.contains(&"delete") {
        TriggerEvent::Delete
    } else {
        TriggerEvent::Unknown
    };

    (timing, event)
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

    #[test]
    fn read_initial_v2_handles_multibyte_comments_in_ddl() {
        // Regression: SQL helpers were byte-slicing the trailing bytes
        // of `sqlite_master.sql` to detect `WITHOUT ROWID`, which
        // panicked when the boundary landed inside a multi-byte
        // character. Real-world DBs frequently keep CJK column
        // comments in their stored DDL (the user's `sofast.db`
        // surfaced this).
        let (_tmp, path) = setup_db(&[
            "CREATE TABLE stars (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL, -- 收藏项的标题
                description TEXT,    -- 收藏项的描述
                type TEXT NOT NULL,  -- 内容类型，例如 'link', 'image', 'text'
                content TEXT,         -- 实际内容
                created_at INTEGER
            )",
            "INSERT INTO stars(id, title, type) VALUES \
                ('1','示例','link'),('2','another','image')",
            "CREATE INDEX idx_stars_type ON stars(type) WHERE type = 'link'",
        ]);
        let info = read_initial_v2(&path, 5).expect("read_initial_v2 must not panic on CJK DDL");
        let main = info.schemas.iter().find(|s| s.name == "main").unwrap();
        let stars = main.objects.iter().find(|o| o.name == "stars").unwrap();
        assert!(!stars.is_without_rowid);
        let idx = main
            .objects
            .iter()
            .find(|o| o.name == "idx_stars_type")
            .expect("partial index should be listed");
        assert_eq!(idx.kind, DbObjectKind::Index);
    }

    #[test]
    fn extract_partial_where_works_with_multibyte_surrounding_text() {
        // Partial WHERE extraction used to slice the original sql at
        // a byte index derived from `to_lowercase()` — which can
        // shift indices for non-ASCII letters. Now we lowercase
        // bytes in-place to keep positions stable.
        let sql = "CREATE INDEX i ON stars(type) -- 注释 \n WHERE type = 'link'";
        let got = extract_partial_where(sql).unwrap();
        assert!(got.contains("type = 'link'"), "extracted WHERE was {got:?}");
    }
}
