use std::path::Path;
use std::sync::Arc;

use super::binary::{BinaryInfo, BinaryReason, decode_error};
use super::image::load_image_preview;
use super::{PreviewBody, PreviewDocument, TextPreview};

const PROBE_BYTES: usize = 8192;
const SQLITE_MIME: &str = "application/vnd.sqlite3";
const MAX_TEXT_PROBE_BYTES: u64 = 10 * 1024 * 1024;

pub const INITIAL_DB_PAGE_ROWS: u32 = 50;

pub fn load_preview(
    root: &Path,
    rel_path: &Path,
    dark: bool,
    wants_decoded_image: bool,
) -> Option<PreviewDocument> {
    use std::io::Read;

    let full = root.join(rel_path);
    let rel_str = rel_path.to_string_lossy().to_string();
    let mut file = std::fs::File::open(&full).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let file_size = meta.len();

    if file_size == 0 {
        return Some(PreviewDocument {
            path: rel_str,
            body: PreviewBody::Binary(BinaryInfo::new(0, None, BinaryReason::Empty)),
        });
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
        match reef_sqlite_preview::read_initial_v2(&full, INITIAL_DB_PAGE_ROWS) {
            Ok(info) => {
                return Some(PreviewDocument {
                    path: rel_str,
                    body: PreviewBody::Database(info),
                });
            }
            Err(SqlitePreviewError::TooLarge { .. }) => {
                return Some(PreviewDocument {
                    path: rel_str,
                    body: PreviewBody::Binary(BinaryInfo::new(
                        file_size,
                        Some(SQLITE_MIME),
                        BinaryReason::TooLarge,
                    )),
                });
            }
            Err(e) => {
                return Some(PreviewDocument {
                    path: rel_str,
                    body: PreviewBody::Binary(BinaryInfo::new(
                        file_size,
                        Some(SQLITE_MIME),
                        decode_error(format!("sqlite: {e}")),
                    )),
                });
            }
        }
    }

    if let Some(mime) = mime
        && mime.starts_with("image/")
    {
        return Some(load_image_preview(
            &full,
            &rel_str,
            file_size,
            mime,
            wants_decoded_image,
        ));
    }

    if let Some(mime) = mime
        && !mime.starts_with("text/")
    {
        return Some(PreviewDocument {
            path: rel_str,
            body: PreviewBody::Binary(BinaryInfo::new(
                file_size,
                Some(mime),
                BinaryReason::NonImage,
            )),
        });
    }

    if file_size > MAX_TEXT_PROBE_BYTES {
        return Some(PreviewDocument {
            path: rel_str,
            body: PreviewBody::Binary(BinaryInfo::new(file_size, None, BinaryReason::NullBytes)),
        });
    }

    if probe.contains(&0) {
        return Some(PreviewDocument {
            path: rel_str,
            body: PreviewBody::Binary(BinaryInfo::new(file_size, None, BinaryReason::NullBytes)),
        });
    }

    let mut raw = probe;
    if file_size as usize > raw.len() {
        raw.reserve((file_size as usize).saturating_sub(raw.len()));
        file.read_to_end(&mut raw).ok()?;
    }

    let content = String::from_utf8_lossy(&raw);
    let lines: Vec<String> = content.lines().map(str::to_string).collect();
    let lines = if lines.len() > 10_000 {
        lines[..10_000].to_vec()
    } else {
        lines
    };

    let within_cap = raw.len() <= 512 * 1024 && lines.len() <= 5_000;
    if within_cap
        && let Some(markdown) = crate::markdown::build_markdown_preview(&rel_str, &content, dark)
    {
        return Some(PreviewDocument {
            path: rel_str,
            body: PreviewBody::Markdown(markdown),
        });
    }

    let highlighted = if within_cap {
        crate::highlight::highlight_file(&rel_str, &lines, dark)
    } else {
        None
    };

    let parsed = if within_cap {
        let path_buf = std::path::PathBuf::from(&rel_str);
        crate::nav::NavLang::from_path(&path_buf).and_then(|lang| {
            let source: Arc<[u8]> = Arc::from(raw.clone().into_boxed_slice());
            crate::nav::parse_file_if_supported(lang, source).map(Arc::new)
        })
    } else {
        None
    };

    Some(PreviewDocument {
        path: rel_str,
        body: PreviewBody::Text(TextPreview {
            lines,
            highlighted,
            parsed,
        }),
    })
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

        let content = load_preview(tmp.path(), Path::new("red.png"), true, true).expect("preview");

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

        let content = load_preview(tmp.path(), Path::new("shot.jpg"), true, true).expect("preview");

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

        let content = load_preview(tmp.path(), Path::new("huge.png"), true, true).expect("preview");

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
    fn load_preview_text_attaches_highlight_and_parse() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "src.rs", b"fn main() {}\n");

        let content = load_preview(tmp.path(), Path::new("src.rs"), true, true).expect("preview");

        match content.body {
            PreviewBody::Text(text) => {
                assert_eq!(text.lines, vec!["fn main() {}".to_string()]);
                assert!(text.highlighted.is_some());
                assert!(text.parsed.is_some());
            }
            other => panic!("expected Text body, got {other:?}"),
        }
    }

    #[test]
    fn load_preview_markdown_uses_markdown_body() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(
            tmp.path(),
            "README.md",
            b"# Title\n\n| Name | Count |\n|:---|---:|\n| reef | 1 |\n",
        );

        let content =
            load_preview(tmp.path(), Path::new("README.md"), true, true).expect("preview");

        match content.body {
            PreviewBody::Markdown(markdown) => {
                assert_eq!(markdown.text_rows[0], "Title");
                let rows: Vec<String> = markdown
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
    fn load_preview_plain_text_stays_text() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "notes.txt", b"# not markdown here\n");

        let content =
            load_preview(tmp.path(), Path::new("notes.txt"), true, true).expect("preview");

        match content.body {
            PreviewBody::Text(text) => assert_eq!(text.lines, vec!["# not markdown here"]),
            other => panic!("expected Text body, got {other:?}"),
        }
    }

    #[test]
    fn load_preview_zero_byte_reports_empty() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "empty.bin", b"");

        let content =
            load_preview(tmp.path(), Path::new("empty.bin"), true, true).expect("preview");

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

        let content = load_preview(tmp.path(), Path::new("red.png"), true, false).expect("preview");

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

        let content = load_preview(tmp.path(), Path::new("doc.pdf"), true, true).expect("preview");

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

        let content =
            load_preview(tmp.path(), Path::new("General.vue"), true, true).expect("preview");

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

        let content =
            load_preview(tmp.path(), Path::new("weird.dat"), true, true).expect("preview");

        match content.body {
            PreviewBody::Binary(info) => assert!(matches!(info.reason, BinaryReason::NullBytes)),
            other => panic!("expected Binary(NullBytes), got {other:?}"),
        }
    }

    #[test]
    fn load_preview_huge_unknown_skips_full_read() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.dat");
        let big_size = MAX_TEXT_PROBE_BYTES + 1;
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

        let content = load_preview(tmp.path(), Path::new("big.dat"), true, true).expect("preview");

        match content.body {
            PreviewBody::Binary(info) => {
                assert!(matches!(info.reason, BinaryReason::NullBytes));
                assert_eq!(info.bytes_on_disk, big_size);
            }
            other => panic!("expected Binary(NullBytes), got {other:?}"),
        }
    }

    #[test]
    fn load_preview_sqlite_reads_initial_database_info() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fixture.db");
        seed_sqlite(&path);

        let content =
            load_preview(tmp.path(), Path::new("fixture.db"), true, true).expect("preview");

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
