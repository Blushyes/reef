use std::io::Cursor;
use std::path::Path;

use calamine::{Cell, Data, DataRef, Dimensions, Reader, Sheets};
use serde::Serialize;

use super::{BinaryInfo, BinaryReason, PreviewBody};

pub const MAX_WORKBOOK_BYTES: u64 = 10 * 1024 * 1024;
const MAX_ROWS: usize = 10_000;
const MAX_COLUMNS: usize = 256;
const MAX_CELLS: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkbookPreview {
    pub sheets: Vec<WorksheetPreview>,
}

/// Coordinates are zero-based absolute worksheet coordinates. Empty leading
/// rows/columns are omitted without changing the addresses shown by hosts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorksheetPreview {
    pub name: String,
    pub start_row: u32,
    pub start_column: u32,
    pub total_rows: usize,
    pub total_columns: usize,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub truncated: bool,
}

pub fn is_spreadsheet(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "xlsx" | "xls" | "xlsm" | "xlsb" | "ods"
            )
        })
}

pub fn preview_body(bytes: &[u8], bytes_on_disk: u64) -> PreviewBody {
    if bytes_on_disk > MAX_WORKBOOK_BYTES {
        return PreviewBody::Binary(BinaryInfo::with_head_bytes(
            bytes_on_disk,
            None,
            BinaryReason::TooLarge,
            bytes,
        ));
    }
    match read_workbook(bytes) {
        Ok(workbook) => PreviewBody::Spreadsheet(workbook),
        Err(error) => PreviewBody::Binary(BinaryInfo::with_head_bytes(
            bytes_on_disk,
            None,
            super::binary::decode_error(format!("spreadsheet: {error}")),
            bytes,
        )),
    }
}

fn read_workbook(bytes: &[u8]) -> Result<WorkbookPreview, calamine::Error> {
    let mut workbook = calamine::open_workbook_auto_from_rs(Cursor::new(bytes))?;
    let mut sheets = Vec::new();
    let names: Vec<_> = workbook
        .sheets_metadata()
        .iter()
        .filter(|sheet| sheet.typ == calamine::SheetType::WorkSheet)
        .map(|sheet| sheet.name.clone())
        .collect();
    for name in names {
        let sheet = match &mut workbook {
            Sheets::Xlsx(reader) => {
                let bounds = {
                    let mut cells = reader.worksheet_cells_reader(&name)?;
                    worksheet_bounds(|| cells.next_cell())?
                };
                let mut cells = reader.worksheet_cells_reader(&name)?;
                streamed_worksheet(name, bounds, || cells.next_cell())?
            }
            Sheets::Xlsb(reader) => {
                let bounds = {
                    let mut cells = reader.worksheet_cells_reader(&name)?;
                    worksheet_bounds(|| cells.next_cell())?
                };
                let mut cells = reader.worksheet_cells_reader(&name)?;
                streamed_worksheet(name, bounds, || cells.next_cell())?
            }
            // These readers eagerly parse their worksheets on open. Their input
            // and output limits do not impose a bound on parser allocations.
            Sheets::Xls(_) | Sheets::Ods(_) => {
                let range = workbook.worksheet_range(&name)?;
                worksheet_preview(name, &range)
            }
        };
        sheets.push(sheet);
    }
    Ok(WorkbookPreview { sheets })
}

// Scan actual nonempty cells instead of trusting the optional file dimensions.
// A second streaming pass fills only the bounded projection. This preserves
// origins even when a later row contains a cell to the left of the first row,
// without ever materializing the potentially enormous dense used range.
fn worksheet_bounds<'a, E>(
    mut next: impl FnMut() -> Result<Option<Cell<DataRef<'a>>>, E>,
) -> Result<Option<Dimensions>, E> {
    let mut bounds: Option<Dimensions> = None;
    while let Some(cell) = next()? {
        if matches!(cell.get_value(), DataRef::Empty) {
            continue;
        }
        let (row, column) = cell.get_position();
        match &mut bounds {
            Some(bounds) => {
                bounds.start.0 = bounds.start.0.min(row);
                bounds.start.1 = bounds.start.1.min(column);
                bounds.end.0 = bounds.end.0.max(row);
                bounds.end.1 = bounds.end.1.max(column);
            }
            None => bounds = Some(Dimensions::new((row, column), (row, column))),
        }
    }
    Ok(bounds)
}

fn streamed_worksheet<'a, E>(
    name: String,
    bounds: Option<Dimensions>,
    mut next: impl FnMut() -> Result<Option<Cell<DataRef<'a>>>, E>,
) -> Result<WorksheetPreview, E> {
    let mut sheet = worksheet_projection(name, bounds);
    while let Some(cell) = next()? {
        let (row, column) = cell.get_position();
        // Empty formatting cells can precede the nonempty used range.
        let (Some(row), Some(column)) = (
            row.checked_sub(sheet.start_row),
            column.checked_sub(sheet.start_column),
        ) else {
            continue;
        };
        if let Some(value) = sheet
            .rows
            .get_mut(row as usize)
            .and_then(|row| row.get_mut(column as usize))
        {
            *value = display_value(&Data::from(cell.get_value().clone()));
        }
    }
    Ok(sheet)
}

fn worksheet_preview(name: String, range: &calamine::Range<Data>) -> WorksheetPreview {
    let bounds = range
        .start()
        .zip(range.end())
        .map(|(start, end)| Dimensions::new(start, end));
    let mut sheet = worksheet_projection(name, bounds);
    for (source, target) in range.rows().zip(&mut sheet.rows) {
        for (source, target) in source.iter().zip(target) {
            *target = display_value(source);
        }
    }
    sheet
}

fn worksheet_projection(name: String, bounds: Option<Dimensions>) -> WorksheetPreview {
    let ((start_row, start_column), total_rows, total_columns) = match bounds {
        Some(bounds) => (
            bounds.start,
            bounds.end.0 as usize - bounds.start.0 as usize + 1,
            bounds.end.1 as usize - bounds.start.1 as usize + 1,
        ),
        None => ((0, 0), 0, 0),
    };
    let column_count = total_columns.min(MAX_COLUMNS);
    let row_count = total_rows
        .min(MAX_ROWS)
        .min(MAX_CELLS / column_count.max(1));
    WorksheetPreview {
        name,
        start_row,
        start_column,
        total_rows,
        total_columns,
        columns: (0..column_count)
            .map(|i| column_label(start_column + i as u32))
            .collect(),
        rows: vec![vec![String::new(); column_count]; row_count],
        truncated: row_count < total_rows || column_count < total_columns,
    }
}

fn display_value(value: &Data) -> String {
    match value {
        Data::DateTime(date) if date.is_datetime() => {
            let (y, m, d, h, min, s, ms) = date.to_ymd_hms_milli();
            if (h, min, s, ms) == (0, 0, 0, 0) {
                format!("{y:04}-{m:02}-{d:02}")
            } else {
                format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}.{ms:03}")
            }
        }
        Data::DateTime(date) => {
            let millis = (date.as_f64() * 86_400_000.0).round() as i64;
            let sign = if millis < 0 { "-" } else { "" };
            let millis = millis.unsigned_abs();
            format!(
                "{sign}{}:{:02}:{:02}.{:03}",
                millis / 3_600_000,
                millis / 60_000 % 60,
                millis / 1_000 % 60,
                millis % 1_000
            )
        }
        _ => value.to_string(),
    }
}

pub fn column_label(mut column: u32) -> String {
    let mut label = Vec::new();
    loop {
        label.push(b'A' + (column % 26) as u8);
        if column < 26 {
            break;
        }
        column = column / 26 - 1;
    }
    label.into_iter().rev().map(char::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workbook_formats_preserve_sheets_coordinates_values_and_empty_sheets() {
        for (extension, bytes) in test_support::spreadsheet_fixtures() {
            let workbook =
                read_workbook(&bytes).unwrap_or_else(|error| panic!("{extension}: {error}"));
            assert_eq!(workbook.sheets.len(), 3, "{extension}");
            let data = &workbook.sheets[0];
            assert_eq!(data.name, "Data");
            assert_eq!((data.start_row, data.start_column), (2, 2), "{extension}");
            assert_eq!((data.total_rows, data.total_columns), (3, 2));
            assert_eq!(data.columns, ["C", "D"]);
            assert_eq!(data.rows[0], ["Name", "Value"]);
            assert_eq!(data.rows[1], ["中文\tvalue\nnext", "42.5"], "{extension}");
            assert_eq!(data.rows[2], ["true", "2026-09-29"], "{extension}");
            assert!(!data.truncated);
            assert_eq!(workbook.sheets[1].rows[0], ["Second sheet"]);
            assert!(workbook.sheets[2].rows.is_empty());
        }
    }

    #[test]
    fn large_worksheet_bounds_projection_without_losing_dimensions() {
        let range = calamine::Range::new((5, 3), (10_005, 4));
        let sheet = worksheet_preview("Tall".into(), &range);
        assert_eq!(sheet.total_rows, 10_001);
        assert_eq!(sheet.rows.len(), MAX_ROWS);
        assert_eq!(sheet.start_row, 5);
        assert!(sheet.truncated);
        let range = calamine::Range::new((0, 0), (999, 299));
        let sheet = worksheet_preview("Wide".into(), &range);
        assert_eq!(sheet.total_columns, 300);
        assert_eq!(sheet.columns.len(), MAX_COLUMNS);
        assert!(sheet.rows.len() * sheet.columns.len() <= MAX_CELLS);
        assert!(sheet.truncated);
    }

    #[test]
    fn streaming_preserves_used_origin_cached_formulas_and_empty_formatting_cells() {
        let bytes = test_support::xlsx_with_sheet(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<dimension ref="A1:XFD1048576"/><sheetData>
<row r="1"><c r="A1" s="1"/></row>
<row r="3"><c r="E3"><v>7</v></c></row>
<row r="4"><c r="C4"><f>1+2</f><v>3</v></c></row>
<row r="5"><c r="D5" t="e"><v>#DIV/0!</v></c></row>
</sheetData></worksheet>"#,
        );
        let workbook = read_workbook(&bytes).unwrap();
        let sheet = &workbook.sheets[0];
        assert_eq!((sheet.start_row, sheet.start_column), (2, 2));
        assert_eq!((sheet.total_rows, sheet.total_columns), (3, 3));
        assert_eq!(sheet.columns, ["C", "D", "E"]);
        assert_eq!(sheet.rows[0], ["", "", "7"]);
        assert_eq!(sheet.rows[1], ["3", "", ""]);
        assert_eq!(sheet.rows[2], ["", "#DIV/0!", ""]);
        assert!(!sheet.truncated);
    }

    #[test]
    fn column_addresses_cross_letter_boundaries() {
        assert_eq!(column_label(0), "A");
        assert_eq!(column_label(25), "Z");
        assert_eq!(column_label(26), "AA");
        assert_eq!(column_label(16383), "XFD");
    }

    #[test]
    fn malformed_workbook_reports_decode_error() {
        let PreviewBody::Binary(info) = preview_body(b"invalid", 7) else {
            panic!("expected a decode error");
        };
        assert!(matches!(info.reason, BinaryReason::DecodeError(_)));
    }
}
