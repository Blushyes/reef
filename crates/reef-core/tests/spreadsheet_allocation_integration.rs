//! Regression for sparse cells expanding into an unbounded dense worksheet.
use reef_core::preview::{PreviewBody, spreadsheet::preview_body};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct AllocationProbe;
static LARGEST: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for AllocationProbe {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        LARGEST.fetch_max(size, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: AllocationProbe = AllocationProbe;

#[test]
fn sparse_xlsx_allocates_only_the_preview_projection() {
    // A million-cell used rectangle containing only two values. Keep this small
    // enough that a regression fails the assertion without exhausting test RAM.
    let bytes = test_support::xlsx_with_sheet(
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
<row r="1"><c r="A1"><v>1</v></c></row>
<row r="1000"><c r="ALL1000"><v>2</v></c></row>
</sheetData></worksheet>"#,
    );
    LARGEST.store(0, Ordering::Relaxed);
    let body = preview_body(&bytes, bytes.len() as u64);
    let largest = LARGEST.load(Ordering::Relaxed);
    assert!(
        largest < 10 * 1024 * 1024,
        "unexpected dense worksheet allocation: {largest} bytes"
    );
    let PreviewBody::Spreadsheet(workbook) = body else {
        panic!("expected workbook")
    };
    let sheet = &workbook.sheets[0];
    assert_eq!((sheet.total_rows, sheet.total_columns), (1000, 1000));
    assert_eq!((sheet.rows.len(), sheet.columns.len()), (390, 256));
    assert_eq!(sheet.rows[0][0], "1");
    assert!(sheet.truncated);
}
