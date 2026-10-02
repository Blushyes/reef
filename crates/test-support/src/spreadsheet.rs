//! Build tiny workbook inputs in memory so tests need no binary assets or local fixture checkout.

use std::io::{Cursor, Write};
use zip::{ZipWriter, write::SimpleFileOptions};

pub fn spreadsheet_fixtures() -> [(&'static str, Vec<u8>); 2] {
    [("xlsx", xlsx()), ("ods", ods())]
}

fn archive(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, xml) in entries {
        writer.start_file(*name, options).unwrap();
        writer.write_all(xml.as_bytes()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn xlsx() -> Vec<u8> {
    xlsx_with_sheet(
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
<row r="3"><c r="C3" t="inlineStr"><is><t>Name</t></is></c><c r="D3" t="inlineStr"><is><t>Value</t></is></c></row>
<row r="4"><c r="C4" t="inlineStr"><is><t>中文&#9;value&#10;next</t></is></c><c r="D4"><v>42.5</v></c></row>
<row r="5"><c r="C5" t="b"><v>1</v></c><c r="D5" s="1"><v>46294</v></c></row>
</sheetData></worksheet>"#,
    )
}

/// Build an XLSX with a caller-supplied first worksheet, entirely in memory.
pub fn xlsx_with_sheet(sheet: &str) -> Vec<u8> {
    archive(&[
        (
            "[Content_Types].xml",
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
</Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
        ),
        (
            "xl/workbook.xml",
            r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Data" sheetId="1" r:id="rId1"/><sheet name="Summary" sheetId="2" r:id="rId2"/><sheet name="Empty" sheetId="3" r:id="rId3"/></sheets>
</workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/>
<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet3.xml"/>
<Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#,
        ),
        (
            "xl/styles.xml",
            r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="14"/></cellXfs></styleSheet>"#,
        ),
        ("xl/worksheets/sheet1.xml", sheet),
        (
            "xl/worksheets/sheet2.xml",
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Second sheet</t></is></c></row></sheetData></worksheet>"#,
        ),
        (
            "xl/worksheets/sheet3.xml",
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData/></worksheet>"#,
        ),
    ])
}

fn ods() -> Vec<u8> {
    archive(&[
        ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
        (
            "META-INF/manifest.xml",
            r#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>"#,
        ),
        (
            "content.xml",
            r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" office:version="1.2">
<office:body><office:spreadsheet><table:table table:name="Data">
<table:table-row/><table:table-row/>
<table:table-row><table:table-cell/><table:table-cell/><table:table-cell office:value-type="string"><text:p>Name</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>Value</text:p></table:table-cell></table:table-row>
<table:table-row><table:table-cell/><table:table-cell/><table:table-cell office:value-type="string"><text:p>中文&#9;value&#10;next</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="42.5"/></table:table-row>
<table:table-row><table:table-cell/><table:table-cell/><table:table-cell office:value-type="boolean" office:boolean-value="true"/><table:table-cell office:value-type="date" office:date-value="2026-09-29"/></table:table-row>
</table:table><table:table table:name="Summary"><table:table-row><table:table-cell office:value-type="string"><text:p>Second sheet</text:p></table:table-cell></table:table-row></table:table>
<table:table table:name="Empty"/></office:spreadsheet></office:body></office:document-content>"#,
        ),
    ])
}
