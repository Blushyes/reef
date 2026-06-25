use std::collections::BTreeSet;

use reef_sqlite_preview::{DatabaseInfoV2, DbObject, DbObjectDetail, DbObjectKey, SqliteValue};

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
    pub selection: DbObjectKey,
    pub expanded: BTreeSet<String>,
    pub page: u64,
    pub current_rows: Vec<Vec<SqliteValue>>,
    pub rows_per_page: u32,
    pub detail: Option<DbObjectDetail>,
}

impl DbPreviewState {
    pub fn from_initial(path: &str, info: &DatabaseInfoV2, rows_per_page: u32) -> Self {
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
        Self {
            path: path.to_string(),
            selection,
            expanded,
            page: 0,
            current_rows: info.initial_page.rows.clone(),
            rows_per_page,
            detail: None,
        }
    }
}

pub fn max_page_for_object(object: &DbObject, page_size: u32) -> u64 {
    if page_size == 0 {
        return 0;
    }
    let rows = object.row_count.unwrap_or(0);
    let pages = rows.div_ceil(page_size as u64);
    pages.saturating_sub(1)
}
