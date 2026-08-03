use std::path::Path;
use std::sync::Arc;

use super::binary::{BinaryInfo, BinaryReason, decode_error};
use super::image::load_image_preview;
use super::{PreviewBody, PreviewDocument, TextPreview};

const PROBE_BYTES: usize = 8192;
const SQLITE_MIME: &str = "application/vnd.sqlite3";
pub const MAX_TEXT_PREVIEW_BYTES: u64 = 10 * 1024 * 1024;
const MAX_ENRICHMENT_BYTES: u64 = 512 * 1024;
const MAX_ENRICHMENT_LINES: usize = 5_000;
const MAX_TEXT_PREVIEW_LINES: usize = 10_000;

pub const INITIAL_DB_PAGE_ROWS: u32 = 50;

fn preview_document(
    path: &str,
    bytes_on_disk: u64,
    mime: Option<&str>,
    body: PreviewBody,
) -> PreviewDocument {
    PreviewDocument {
        path: path.to_string(),
        resolved_path: None,
        local_path: None,
        bytes_on_disk,
        mime: mime.map(str::to_string),
        body,
    }
}

pub fn load_preview(
    root: &Path,
    rel_path: &Path,
    wants_decoded_image: bool,
) -> Option<PreviewDocument> {
    load_preview_from_path(&root.join(rel_path), rel_path, wants_decoded_image)
}

pub fn load_preview_from_path(
    full: &Path,
    rel_path: &Path,
    wants_decoded_image: bool,
) -> Option<PreviewDocument> {
    use std::io::Read;

    let rel_str = rel_path.to_string_lossy().to_string();
    let mut file = std::fs::File::open(full).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let file_size = meta.len();

    if file_size == 0 {
        return Some(preview_document(
            &rel_str,
            file_size,
            None,
            PreviewBody::Binary(BinaryInfo::new(0, None, BinaryReason::Empty)),
        ));
    }

    let probe_len = (file_size as usize).min(PROBE_BYTES);
    let mut probe = vec![0u8; probe_len];
    let n = file.read(&mut probe).ok()?;
    probe.truncate(n);

    let mime: Option<&'static str> = infer::get(&probe).map(|kind| kind.mime_type());

    if reef_sqlite_preview::has_sqlite_extension(rel_path)
        && reef_sqlite_preview::has_sqlite_magic(&probe)
    {
        use reef_sqlite_preview::PreviewError as SqlitePreviewError;
        match reef_sqlite_preview::read_initial_v2(full, INITIAL_DB_PAGE_ROWS) {
            Ok(info) => {
                return Some(preview_document(
                    &rel_str,
                    file_size,
                    Some(SQLITE_MIME),
                    PreviewBody::Database(info),
                ));
            }
            Err(SqlitePreviewError::TooLarge { .. }) => {
                return Some(preview_document(
                    &rel_str,
                    file_size,
                    Some(SQLITE_MIME),
                    PreviewBody::Binary(BinaryInfo::with_head_bytes(
                        file_size,
                        Some(SQLITE_MIME),
                        BinaryReason::TooLarge,
                        &probe,
                    )),
                ));
            }
            Err(e) => {
                return Some(preview_document(
                    &rel_str,
                    file_size,
                    Some(SQLITE_MIME),
                    PreviewBody::Binary(BinaryInfo::with_head_bytes(
                        file_size,
                        Some(SQLITE_MIME),
                        decode_error(format!("sqlite: {e}")),
                        &probe,
                    )),
                ));
            }
        }
    }

    if let Some(mime) = mime
        && mime.starts_with("image/")
    {
        return Some(load_image_preview(
            full,
            &rel_str,
            file_size,
            mime,
            wants_decoded_image,
        ));
    }

    if let Some(mime) = mime
        && !mime.starts_with("text/")
    {
        return Some(preview_document(
            &rel_str,
            file_size,
            Some(mime),
            PreviewBody::Binary(BinaryInfo::with_head_bytes(
                file_size,
                Some(mime),
                BinaryReason::NonImage,
                &probe,
            )),
        ));
    }

    if file_size > MAX_TEXT_PREVIEW_BYTES {
        return Some(preview_document(
            &rel_str,
            file_size,
            mime,
            PreviewBody::Binary(BinaryInfo::with_head_bytes(
                file_size,
                mime,
                BinaryReason::TooLarge,
                &probe,
            )),
        ));
    }

    if probe.contains(&0) {
        return Some(preview_document(
            &rel_str,
            file_size,
            mime,
            PreviewBody::Binary(BinaryInfo::with_head_bytes(
                file_size,
                mime,
                BinaryReason::NullBytes,
                &probe,
            )),
        ));
    }

    let mut raw = probe;
    if file_size as usize > raw.len() {
        raw.reserve((file_size as usize).saturating_sub(raw.len()));
        file.read_to_end(&mut raw).ok()?;
    }

    Some(preview_document(
        &rel_str,
        file_size,
        mime,
        build_textual_preview_body(&rel_str, &String::from_utf8_lossy(&raw)),
    ))
}

pub fn build_textual_preview_body(path: &str, content: &str) -> PreviewBody {
    if crate::markdown::is_markdown_path(path) {
        let line_count = content.lines().take(MAX_ENRICHMENT_LINES + 1).count();
        let markdown = if text_preview_can_be_enriched(content.len() as u64, line_count) {
            crate::markdown::build_markdown_preview(path, content)
                .expect("markdown path must produce a markdown preview")
        } else {
            crate::markdown::MarkdownPreview::source_only(content)
        };
        return PreviewBody::Markdown(markdown);
    }

    PreviewBody::Text(TextPreview {
        lines: content
            .lines()
            .take(MAX_TEXT_PREVIEW_LINES)
            .map(str::to_string)
            .collect(),
        source: structured_data_source_required(path).then(|| Arc::from(content)),
        highlighted: None,
        parsed: None,
    })
}

pub fn structured_data_source_required(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("json" | "jsonc" | "json5" | "jsonl" | "yaml" | "yml")
    )
}

pub fn build_text_preview_enrichment(
    path: &str,
    bytes_on_disk: u64,
    lines: &[String],
    source: Option<&str>,
    dark: bool,
) -> Option<super::TextPreviewEnrichment> {
    if !text_preview_can_be_enriched(bytes_on_disk, lines.len()) {
        return None;
    }
    let highlighted = crate::highlight::highlight_file(path, lines, dark);
    let parsed = crate::nav::NavLang::from_path(std::path::Path::new(path)).and_then(|lang| {
        let source: Arc<[u8]> = Arc::from(lines.join("\n").into_bytes().into_boxed_slice());
        crate::nav::parse_file_if_supported(lang, source).map(Arc::new)
    });
    let structured = source.and_then(|source| structured_document_for_path(path, source));
    if highlighted.is_none() && parsed.is_none() && structured.is_none() {
        return None;
    }
    Some(super::TextPreviewEnrichment {
        highlighted,
        parsed,
        structured,
    })
}

fn structured_document_for_path(
    path: &str,
    source: &str,
) -> Option<crate::structured_data::StructuredDataDocument> {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("json") => crate::structured_data::StructuredDataDocument::from_json(source).ok(),
        Some("jsonl") => {
            crate::structured_data::StructuredDataDocument::from_json_lines(source).ok()
        }
        _ => None,
    }
}

pub fn text_preview_can_be_enriched(bytes_on_disk: u64, line_count: usize) -> bool {
    bytes_on_disk <= MAX_ENRICHMENT_BYTES && line_count <= MAX_ENRICHMENT_LINES
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn tiny_png(width: u32, height: u32) -> Vec<u8> {
        use image::{ImageBuffer, ImageFormat, Rgb};
        use std::io::Cursor;

        let img = ImageBuffer::from_pixel(width, height, Rgb([255u8, 0, 0]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        buf
    }

    fn write_bytes(dir: &Path, name: &str, data: &[u8]) {
        std::fs::write(dir.join(name), data).unwrap();
    }

    fn seed_sqlite(path: &Path) {
        let conn = rusqlite::Connection::open(path).expect("open sqlite");
        conn.execute_batch(
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT); \
             INSERT INTO users(name) VALUES ('alice'), ('bob');",
        )
        .expect("seed sqlite");
    }

    #[test]
    fn load_preview_detects_png_by_magic_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "red.png", &tiny_png(4, 4));

        let content = load_preview(tmp.path(), Path::new("red.png"), true).expect("preview");

        match content.body {
            PreviewBody::Image(img) => {
                assert_eq!(img.width_px, 4);
                assert_eq!(img.height_px, 4);
                assert_eq!(img.format, image::ImageFormat::Png);
                assert!(img.image.is_some());
            }
            other => panic!("expected Image body, got {other:?}"),
        }
    }

    #[test]
    fn load_preview_png_with_wrong_extension_uses_magic_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "shot.jpg", &tiny_png(2, 2));

        let content = load_preview(tmp.path(), Path::new("shot.jpg"), true).expect("preview");

        match content.body {
            PreviewBody::Image(img) => assert_eq!(img.format, image::ImageFormat::Png),
            other => panic!("expected Image body, got {other:?}"),
        }
    }

    #[test]
    fn load_preview_refuses_huge_dimensions() {
        let mut png = Vec::<u8>::new();
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&40000u32.to_be_bytes());
        png.extend_from_slice(&40000u32.to_be_bytes());
        png.extend_from_slice(&[8, 2, 0, 0, 0]);
        png.extend_from_slice(&[0u8; 4]);

        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "huge.png", &png);

        let content = load_preview(tmp.path(), Path::new("huge.png"), true).expect("preview");

        match content.body {
            PreviewBody::Binary(info) => {
                assert!(
                    matches!(
                        info.reason,
                        BinaryReason::TooLarge | BinaryReason::DecodeError(_)
                    ),
                    "expected TooLarge or DecodeError, got {:?}",
                    info.reason
                );
                assert_eq!(info.mime, Some("image/png"));
            }
            other => panic!("expected Binary body, got {other:?}"),
        }
    }

    #[test]
    fn load_preview_text_returns_plain_content_before_enrichment() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "src.rs", b"fn main() {}\n");

        let content = load_preview(tmp.path(), Path::new("src.rs"), true).expect("preview");

        match content.body {
            PreviewBody::Text(text) => {
                assert_eq!(text.lines, vec!["fn main() {}".to_string()]);
                assert!(text.highlighted.is_none());
                assert!(text.parsed.is_none());

                let enrichment = build_text_preview_enrichment(
                    "src.rs",
                    content.bytes_on_disk,
                    &text.lines,
                    text.source.as_deref(),
                    true,
                )
                .expect("small rust preview should enrich");
                assert!(enrichment.highlighted.is_some());
                assert!(enrichment.parsed.is_some());
            }
            other => panic!("expected Text body, got {other:?}"),
        }
    }

    #[test]
    fn structured_data_preserves_complete_source_after_visible_rows_are_capped() {
        let source = format!(
            "{{\"items\":[]}}\n{}",
            "\n\n".repeat(MAX_TEXT_PREVIEW_LINES)
        );
        let PreviewBody::Text(text) = build_textual_preview_body("large.json", &source) else {
            panic!("expected text preview");
        };

        assert_eq!(text.lines.len(), MAX_TEXT_PREVIEW_LINES);
        assert_eq!(text.source.as_deref(), Some(source.as_str()));
    }

    #[test]
    fn json_lines_enrichment_builds_a_structured_outline() {
        let source = "{\"event\":\"open\"}\n{\"event\":\"close\"}\n";
        let PreviewBody::Text(text) = build_textual_preview_body("events.jsonl", source) else {
            panic!("expected text preview");
        };

        let enrichment = build_text_preview_enrichment(
            "events.jsonl",
            source.len() as u64,
            &text.lines,
            text.source.as_deref(),
            true,
        )
        .expect("jsonl preview should enrich");

        let document = enrichment.structured.expect("structured document");
        assert!(
            document
                .outline()
                .rows()
                .iter()
                .any(|row| row.id == "root/1/event.value")
        );
    }

    #[test]
    fn large_structured_preview_skips_enrichment() {
        let source = r#"{"event":"open","count":1}"#;
        let lines = vec![source.to_string()];

        assert!(
            build_text_preview_enrichment(
                "events.jsonl",
                MAX_ENRICHMENT_BYTES + 1,
                &lines,
                Some(source),
                true,
            )
            .is_none()
        );
    }

    #[test]
    fn load_preview_markdown_uses_markdown_body() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(
            tmp.path(),
            "README.md",
            b"# Title\n\n| Name | Count |\n|:---|---:|\n| reef | 1 |\n",
        );

        let content = load_preview(tmp.path(), Path::new("README.md"), true).expect("preview");

        match content.body {
            PreviewBody::Markdown(markdown) => {
                let model = markdown
                    .render_model
                    .as_ref()
                    .expect("small markdown render model");
                assert_eq!(model.text_rows[0], "Title");
                let rows: Vec<String> = model
                    .rows
                    .iter()
                    .map(|r| r.iter().map(|s| s.text.as_str()).collect())
                    .collect();
                assert!(rows.contains(&"┃ reef ┃     1 ┃".to_string()));
            }
            other => panic!("expected Markdown body, got {other:?}"),
        }
    }

    #[test]
    fn load_preview_large_markdown_keeps_markdown_body() {
        let tmp = tempfile::tempdir().unwrap();
        let source = format!("# Title\n\n{}", "large markdown paragraph ".repeat(24_000));
        assert!(source.len() > MAX_ENRICHMENT_BYTES as usize);
        write_bytes(tmp.path(), "README.md", source.as_bytes());

        let content = load_preview(tmp.path(), Path::new("README.md"), true).expect("preview");

        let PreviewBody::Markdown(markdown) = content.body else {
            panic!("large markdown must not be downgraded to text");
        };
        assert_eq!(markdown.source, source);
        assert!(markdown.render_model.is_none());
    }

    #[test]
    fn load_preview_many_line_markdown_keeps_markdown_body() {
        let tmp = tempfile::tempdir().unwrap();
        let source = (0..=MAX_ENRICHMENT_LINES)
            .map(|line| format!("paragraph {line}\n\n"))
            .collect::<String>();
        write_bytes(tmp.path(), "notes.markdown", source.as_bytes());

        let content = load_preview(tmp.path(), Path::new("notes.markdown"), true).expect("preview");

        assert!(matches!(content.body, PreviewBody::Markdown(_)));
    }

    #[test]
    fn load_preview_plain_text_stays_text() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "notes.txt", b"# not markdown here\n");

        let content = load_preview(tmp.path(), Path::new("notes.txt"), true).expect("preview");

        match content.body {
            PreviewBody::Text(text) => assert_eq!(text.lines, vec!["# not markdown here"]),
            other => panic!("expected Text body, got {other:?}"),
        }
    }

    #[test]
    fn load_preview_zero_byte_reports_empty() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "empty.bin", b"");

        let content = load_preview(tmp.path(), Path::new("empty.bin"), true).expect("preview");

        match content.body {
            PreviewBody::Binary(info) => {
                assert!(matches!(info.reason, BinaryReason::Empty));
                assert_eq!(info.bytes_on_disk, 0);
            }
            other => panic!("expected Binary(Empty), got {other:?}"),
        }
    }

    #[test]
    fn load_preview_without_decode_skips_pixels_keeps_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "red.png", &tiny_png(8, 8));

        let content = load_preview(tmp.path(), Path::new("red.png"), false).expect("preview");

        match content.body {
            PreviewBody::Image(img) => {
                assert_eq!(img.width_px, 8);
                assert_eq!(img.height_px, 8);
                assert_eq!(img.format, image::ImageFormat::Png);
                assert!(img.image.is_none());
            }
            other => panic!("expected Image body, got {other:?}"),
        }
    }

    #[test]
    fn load_preview_pdf_is_non_image_binary() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "doc.pdf", b"%PDF-1.4\n%bogus content\n");

        let content = load_preview(tmp.path(), Path::new("doc.pdf"), true).expect("preview");

        match content.body {
            PreviewBody::Binary(info) => {
                assert!(matches!(info.reason, BinaryReason::NonImage));
                assert_eq!(info.mime, Some("application/pdf"));
            }
            other => panic!("expected Binary(NonImage), got {other:?}"),
        }
    }

    #[test]
    fn load_preview_vue_sfc_renders_as_text() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(
            tmp.path(),
            "General.vue",
            b"<template>\n  <div>hello</div>\n</template>\n",
        );

        let content = load_preview(tmp.path(), Path::new("General.vue"), true).expect("preview");

        match content.body {
            PreviewBody::Text(text) => {
                assert_eq!(text.lines[0], "<template>");
                assert_eq!(text.lines[1], "  <div>hello</div>");
                assert_eq!(text.lines[2], "</template>");
            }
            other => panic!("expected Text body for .vue, got {other:?}"),
        }
    }

    #[test]
    fn load_preview_unknown_binary_falls_back_to_null_byte_heuristic() {
        let tmp = tempfile::tempdir().unwrap();
        let mut data = vec![b'A'; 1024];
        data[512] = 0;
        write_bytes(tmp.path(), "weird.dat", &data);

        let content = load_preview(tmp.path(), Path::new("weird.dat"), true).expect("preview");

        match content.body {
            PreviewBody::Binary(info) => assert!(matches!(info.reason, BinaryReason::NullBytes)),
            other => panic!("expected Binary(NullBytes), got {other:?}"),
        }
    }

    #[test]
    fn load_preview_huge_unknown_skips_full_read() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.dat");
        let big_size = MAX_TEXT_PREVIEW_BYTES + 1;
        {
            use std::io::Write;

            let mut file = std::fs::File::create(&path).unwrap();
            let chunk = vec![b'A'; 1024 * 1024];
            let mut remaining = big_size;
            while remaining > 0 {
                let n = (remaining as usize).min(chunk.len());
                file.write_all(&chunk[..n]).unwrap();
                remaining -= n as u64;
            }
        }

        let content = load_preview(tmp.path(), Path::new("big.dat"), true).expect("preview");

        match content.body {
            PreviewBody::Binary(info) => {
                assert!(matches!(info.reason, BinaryReason::TooLarge));
                assert_eq!(info.bytes_on_disk, big_size);
            }
            other => panic!("expected Binary(TooLarge), got {other:?}"),
        }
    }

    #[test]
    fn load_preview_sqlite_reads_initial_database_info() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fixture.db");
        seed_sqlite(&path);

        let content = load_preview(tmp.path(), Path::new("fixture.db"), true).expect("preview");

        match content.body {
            PreviewBody::Database(info) => {
                assert_eq!(info.default_schema, "main");
                assert_eq!(
                    info.default_object
                        .as_ref()
                        .map(|object| object.name.as_str()),
                    Some("users")
                );
                assert_eq!(info.initial_page.rows.len(), 2);
            }
            other => panic!("expected Database body, got {other:?}"),
        }
    }
}
