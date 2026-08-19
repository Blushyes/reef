use std::collections::BTreeSet;

use reef_sqlite_preview::{
    DatabaseInfoV2, DbObject, DbObjectDetail, DbObjectKey, DbRowLocator, SqliteValue,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbNav {
    PrevPage,
    NextPage,
    PrevTable,
    NextTable,
    FirstPage,
    LastPage,
}

#[derive(Debug, Clone)]
pub struct DbPreviewState {
    pub path: String,
    pub source_revision: u64,
    pub selection: DbObjectKey,
    pub expanded: BTreeSet<String>,
    pub page: u64,
    pub last_page: Option<u64>,
    pub current_rows: Vec<Vec<SqliteValue>>,
    pub current_row_locators: Vec<DbRowLocator>,
    pub rows_per_page: u32,
    pub detail: Option<DbObjectDetail>,
    pub cell: Option<DbCellPreviewState>,
}

/// The cell the user opened in the data grid: which cell it is, its
/// complete (untruncated) value once the reader returns it, and where
/// the value pane is scrolled to.
#[derive(Debug, Clone)]
pub struct DbCellPreviewState {
    pub object_key: DbObjectKey,
    /// Index into [`DbPreviewState::current_rows`] — the highlighted
    /// row on the page currently loaded.
    pub row: usize,
    pub row_offset: u64,
    pub row_locator: DbRowLocator,
    pub column: usize,
    /// `None` while the complete value is still in flight.
    pub value: Option<SqliteValue>,
    /// Bumped every time a read lands, so a renderer caching derived
    /// layout can tell a re-read that changed the value from one that
    /// merely re-confirmed it.
    pub value_revision: u64,
    /// First displayed line of the value pane.
    pub scroll: usize,
}

impl DbPreviewState {
    pub fn from_initial(
        path: &str,
        source_revision: u64,
        info: &DatabaseInfoV2,
        rows_per_page: u32,
    ) -> Self {
        let selection =
            info.default_object
                .clone()
                .unwrap_or_else(|| reef_sqlite_preview::DbObjectKey {
                    schema: info.default_schema.clone(),
                    name: String::new(),
                    kind: reef_sqlite_preview::DbObjectKind::Table,
                });
        let mut expanded = BTreeSet::new();
        expanded.insert(info.default_schema.clone());
        let last_page = info
            .lookup(&selection)
            .and_then(|object| max_page_for_object(object, rows_per_page))
            .or_else(|| (info.initial_page.rows.len() < rows_per_page as usize).then_some(0));
        Self {
            path: path.to_string(),
            source_revision,
            selection,
            expanded,
            page: 0,
            last_page,
            current_rows: info.initial_page.rows.clone(),
            current_row_locators: info.initial_page.row_locators.clone(),
            rows_per_page,
            detail: None,
            cell: None,
        }
    }
}

pub fn max_page_for_object(object: &DbObject, page_size: u32) -> Option<u64> {
    if page_size == 0 {
        return Some(0);
    }
    let rows = object.row_count?;
    let pages = rows.div_ceil(page_size as u64);
    Some(pages.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use reef_sqlite_preview::{DatabaseInfoV2, DbObjectKind, DbPage, SchemaKind, SchemaSummary};

    use super::{DbPreviewState, max_page_for_object};

    fn object(kind: DbObjectKind, row_count: Option<u64>) -> reef_sqlite_preview::DbObject {
        reef_sqlite_preview::DbObject {
            schema: "main".into(),
            name: "items".into(),
            kind,
            tbl_name: Some("items".into()),
            row_count,
            columns: Vec::new(),
            is_virtual: false,
            is_without_rowid: false,
            is_strict: false,
        }
    }

    #[test]
    fn unknown_view_row_count_keeps_the_last_page_open() {
        assert_eq!(
            max_page_for_object(&object(DbObjectKind::View, None), 50),
            None
        );
    }

    #[test]
    fn short_initial_view_page_discovers_page_zero_as_the_end() {
        let view = object(DbObjectKind::View, None);
        let key = view.key();
        let info = DatabaseInfoV2 {
            schemas: vec![SchemaSummary {
                name: "main".into(),
                kind: SchemaKind::Main,
                file: None,
                objects: vec![view],
                truncated: false,
            }],
            default_schema: "main".into(),
            default_object: Some(key),
            initial_page: DbPage {
                rows: vec![Vec::new()],
                row_locators: Vec::new(),
            },
            bytes_on_disk: 0,
        };

        let state = DbPreviewState::from_initial("fixture.db", 1, &info, 50);
        assert_eq!(state.last_page, Some(0));
    }

    #[test]
    fn full_initial_view_page_remains_probeable() {
        let view = object(DbObjectKind::View, None);
        let key = view.key();
        let info = DatabaseInfoV2 {
            schemas: vec![SchemaSummary {
                name: "main".into(),
                kind: SchemaKind::Main,
                file: None,
                objects: vec![view],
                truncated: false,
            }],
            default_schema: "main".into(),
            default_object: Some(key),
            initial_page: DbPage {
                rows: vec![Vec::new(); 50],
                row_locators: Vec::new(),
            },
            bytes_on_disk: 0,
        };

        let state = DbPreviewState::from_initial("fixture.db", 1, &info, 50);
        assert_eq!(state.last_page, None);
    }
}
