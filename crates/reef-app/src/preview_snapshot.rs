use std::path::Path;

use reef_core::markdown::{MarkdownPreview, MarkdownRole, MarkdownSpan, MarkdownStyle};
use reef_core::preview::{BinaryInfo, BinaryReason, PreviewBody, PreviewDocument, TextPreview};
use reef_core::text::{Rgb, TextStyle};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PreviewDocumentSnapshot {
    pub revision: u64,
    pub source: PreviewSourceSnapshot,
    pub body: PreviewBodySnapshot,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PreviewSourceSnapshot {
    pub path: String,
    pub file_name: String,
    pub extension: Option<String>,
    pub mime: Option<String>,
    pub bytes_on_disk: u64,
    pub detected_kind: PreviewDetectedKindSnapshot,
    pub local_file_available: bool,
    pub local_path: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PreviewDetectedKindSnapshot {
    Text,
    Code,
    Markdown,
    Image,
    Video,
    Database,
    StructuredData,
    Diff,
    Mermaid,
    ApiSchema,
    Log,
    Binary,
    Report,
    Custom,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PreviewBodySnapshot {
    Text {
        text: String,
        style_spans: Vec<TextStyleSpanSnapshot>,
    },
    Code {
        language: Option<String>,
        text: String,
        style_spans: Vec<TextStyleSpanSnapshot>,
    },
    Markdown {
        source: String,
        rows: Vec<Vec<MarkdownSpanSnapshot>>,
        text_rows: Vec<String>,
    },
    Image {
        width_px: u32,
        height_px: u32,
        format: String,
        bytes_on_disk: u64,
        animated: bool,
        meta_line: String,
    },
    Video {
        bytes_on_disk: u64,
        mime: Option<String>,
        meta_line: String,
    },
    Database {
        schemas: Vec<DatabaseSchemaSnapshot>,
        default_schema: String,
        default_object: Option<DatabaseObjectKeySnapshot>,
        initial_page: DatabasePageSnapshot,
        bytes_on_disk: u64,
    },
    StructuredData {
        format: StructuredDataFormatSnapshot,
    },
    Diff {
        lines: Vec<String>,
    },
    Mermaid {
        source: String,
    },
    Log {
        lines: Vec<String>,
    },
    Binary {
        bytes_on_disk: u64,
        mime: Option<String>,
        reason: String,
        meta_line: String,
        head_hex: Vec<String>,
    },
    Custom {
        payload_kind: String,
        payload_json: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StructuredDataFormatSnapshot {
    Json,
    Yaml,
    OpenApi,
    JsonSchema,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TextStyleSpanSnapshot {
    pub utf16_start: u32,
    pub utf16_length: u32,
    pub style: TextStyleSnapshot,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TextStyleSnapshot {
    pub fg: Option<RgbSnapshot>,
    pub bold: bool,
    pub italic: bool,
    pub underlined: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RgbSnapshot {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownSpanSnapshot {
    pub text: String,
    pub style: MarkdownStyleSnapshot,
    pub link: Option<String>,
    pub syntax: Option<TextStyleSnapshot>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownStyleSnapshot {
    pub role: MarkdownRoleSnapshot,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MarkdownRoleSnapshot {
    Normal,
    Heading,
    Quote,
    Code,
    CodeBlockHeader,
    CodeBlockText,
    Link,
    TableHeader,
    Border,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseSchemaSnapshot {
    pub name: String,
    pub kind: String,
    pub file: Option<String>,
    pub objects: Vec<DatabaseObjectSnapshot>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseObjectSnapshot {
    pub schema: String,
    pub name: String,
    pub kind: String,
    pub table_name: Option<String>,
    pub row_count: Option<u64>,
    pub columns: Vec<DatabaseColumnSnapshot>,
    pub is_virtual: bool,
    pub is_without_rowid: bool,
    pub is_strict: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseObjectKeySnapshot {
    pub schema: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseColumnSnapshot {
    pub name: String,
    pub decl_type: String,
    pub notnull: bool,
    pub pk: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DatabasePageSnapshot {
    pub rows: Vec<Vec<String>>,
}

impl PreviewDocumentSnapshot {
    pub fn from_document(document: &PreviewDocument, revision: u64) -> Self {
        let detected_kind = detected_kind(document);
        Self {
            revision,
            source: PreviewSourceSnapshot::from_document(document, detected_kind),
            body: PreviewBodySnapshot::from_document(document, detected_kind),
        }
    }
}

impl PreviewSourceSnapshot {
    fn from_document(
        document: &PreviewDocument,
        detected_kind: PreviewDetectedKindSnapshot,
    ) -> Self {
        let rel_path = Path::new(&document.path);
        let file_name = rel_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&document.path)
            .to_string();
        let extension = rel_path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase());
        let local_path = document
            .local_path
            .as_ref()
            .map(|path| path.display().to_string());
        Self {
            path: document.path.clone(),
            file_name,
            extension,
            mime: document.mime.clone(),
            bytes_on_disk: document.bytes_on_disk,
            detected_kind,
            local_file_available: local_path.is_some(),
            local_path,
        }
    }
}

impl PreviewBodySnapshot {
    fn from_document(
        document: &PreviewDocument,
        detected_kind: PreviewDetectedKindSnapshot,
    ) -> Self {
        match &document.body {
            PreviewBody::Text(text) => text_body_snapshot(&document.path, text, detected_kind),
            PreviewBody::Markdown(markdown) => {
                if matches!(detected_kind, PreviewDetectedKindSnapshot::Mermaid) {
                    Self::Mermaid {
                        source: markdown.source.clone(),
                    }
                } else {
                    Self::Markdown {
                        source: markdown.source.clone(),
                        rows: markdown_rows(markdown),
                        text_rows: markdown.text_rows.clone(),
                    }
                }
            }
            PreviewBody::Image(image) => Self::Image {
                width_px: image.width_px,
                height_px: image.height_px,
                format: format!("{:?}", image.format).to_ascii_lowercase(),
                bytes_on_disk: image.bytes_on_disk,
                animated: image.animated,
                meta_line: image.meta_line.clone(),
            },
            PreviewBody::Binary(info)
                if matches!(detected_kind, PreviewDetectedKindSnapshot::Video) =>
            {
                Self::Video {
                    bytes_on_disk: info.bytes_on_disk,
                    mime: info
                        .mime
                        .map(str::to_string)
                        .or_else(|| document.mime.clone()),
                    meta_line: info.meta_line.clone(),
                }
            }
            PreviewBody::Binary(info) => binary_body_snapshot(info),
            PreviewBody::Database(database) => database_body_snapshot(database),
        }
    }
}

fn text_body_snapshot(
    path: &str,
    text: &TextPreview,
    detected_kind: PreviewDetectedKindSnapshot,
) -> PreviewBodySnapshot {
    match detected_kind {
        PreviewDetectedKindSnapshot::Code => {
            let (source, style_spans) = text_and_style_spans(text);
            PreviewBodySnapshot::Code {
                language: language_for_path(path),
                text: source,
                style_spans,
            }
        }
        PreviewDetectedKindSnapshot::StructuredData | PreviewDetectedKindSnapshot::ApiSchema => {
            PreviewBodySnapshot::StructuredData {
                format: structured_format_for_path(path),
            }
        }
        PreviewDetectedKindSnapshot::Diff => PreviewBodySnapshot::Diff {
            lines: text.lines.clone(),
        },
        PreviewDetectedKindSnapshot::Mermaid => PreviewBodySnapshot::Mermaid {
            source: text.lines.join("\n"),
        },
        PreviewDetectedKindSnapshot::Log => PreviewBodySnapshot::Log {
            lines: text.lines.clone(),
        },
        _ => {
            let (source, style_spans) = text_and_style_spans(text);
            PreviewBodySnapshot::Text {
                text: source,
                style_spans,
            }
        }
    }
}

fn text_and_style_spans(text: &TextPreview) -> (String, Vec<TextStyleSpanSnapshot>) {
    let source = text.lines.join("\n");
    let Some(highlighted) = text.highlighted.as_ref() else {
        return (source, Vec::new());
    };
    if highlighted.len() != text.lines.len() {
        return (source, Vec::new());
    }

    let mut spans: Vec<TextStyleSpanSnapshot> = Vec::new();
    let mut utf16_offset = 0_u32;
    for (line_index, (line, tokens)) in text.lines.iter().zip(highlighted).enumerate() {
        let mut byte_offset: usize = 0;
        for token in tokens {
            let token_end = byte_offset.saturating_add(token.text.len());
            if line.as_bytes().get(byte_offset..token_end) != Some(token.text.as_bytes()) {
                return (source, Vec::new());
            }
            let Ok(utf16_length) = u32::try_from(token.text.encode_utf16().count()) else {
                return (source, Vec::new());
            };
            if utf16_length > 0 {
                push_style_span(
                    &mut spans,
                    TextStyleSpanSnapshot {
                        utf16_start: utf16_offset,
                        utf16_length,
                        style: TextStyleSnapshot::from(token.style),
                    },
                );
                let Some(next_offset) = utf16_offset.checked_add(utf16_length) else {
                    return (source, Vec::new());
                };
                utf16_offset = next_offset;
            }
            byte_offset = token_end;
        }
        if byte_offset != line.len() {
            return (source, Vec::new());
        }
        if line_index + 1 < text.lines.len() {
            let Some(next_offset) = utf16_offset.checked_add(1) else {
                return (source, Vec::new());
            };
            utf16_offset = next_offset;
        }
    }
    (source, spans)
}

fn push_style_span(spans: &mut Vec<TextStyleSpanSnapshot>, span: TextStyleSpanSnapshot) {
    if let Some(previous) = spans.last_mut()
        && previous.style == span.style
        && previous.utf16_start.checked_add(previous.utf16_length) == Some(span.utf16_start)
    {
        previous.utf16_length = previous.utf16_length.saturating_add(span.utf16_length);
        return;
    }
    spans.push(span);
}

fn binary_body_snapshot(info: &BinaryInfo) -> PreviewBodySnapshot {
    PreviewBodySnapshot::Binary {
        bytes_on_disk: info.bytes_on_disk,
        mime: info.mime.map(str::to_string),
        reason: binary_reason_label(&info.reason).to_string(),
        meta_line: info.meta_line.clone(),
        head_hex: info.head_hex.clone(),
    }
}

fn database_body_snapshot(info: &reef_sqlite_preview::DatabaseInfoV2) -> PreviewBodySnapshot {
    PreviewBodySnapshot::Database {
        schemas: info
            .schemas
            .iter()
            .map(|schema| DatabaseSchemaSnapshot {
                name: schema.name.clone(),
                kind: format!("{:?}", schema.kind).to_ascii_lowercase(),
                file: schema.file.clone(),
                objects: schema
                    .objects
                    .iter()
                    .map(|object| DatabaseObjectSnapshot {
                        schema: object.schema.clone(),
                        name: object.name.clone(),
                        kind: object.kind.as_master_type().to_string(),
                        table_name: object.tbl_name.clone(),
                        row_count: object.row_count,
                        columns: object
                            .columns
                            .iter()
                            .map(DatabaseColumnSnapshot::from)
                            .collect(),
                        is_virtual: object.is_virtual,
                        is_without_rowid: object.is_without_rowid,
                        is_strict: object.is_strict,
                    })
                    .collect(),
                truncated: schema.truncated,
            })
            .collect(),
        default_schema: info.default_schema.clone(),
        default_object: info
            .default_object
            .as_ref()
            .map(DatabaseObjectKeySnapshot::from),
        initial_page: DatabasePageSnapshot {
            rows: info
                .initial_page
                .rows
                .iter()
                .map(|row| row.iter().map(ToString::to_string).collect())
                .collect(),
        },
        bytes_on_disk: info.bytes_on_disk,
    }
}

fn detected_kind(document: &PreviewDocument) -> PreviewDetectedKindSnapshot {
    let path = document.path.to_ascii_lowercase();
    let ext = Path::new(&path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    match &document.body {
        PreviewBody::Image(_) => PreviewDetectedKindSnapshot::Image,
        PreviewBody::Database(_) => PreviewDetectedKindSnapshot::Database,
        PreviewBody::Markdown(_) if is_report_path(&path) => PreviewDetectedKindSnapshot::Report,
        PreviewBody::Markdown(_) if is_mermaid_path(&path) => PreviewDetectedKindSnapshot::Mermaid,
        PreviewBody::Markdown(_) => PreviewDetectedKindSnapshot::Markdown,
        PreviewBody::Binary(_) if is_video_source(ext, document.mime.as_deref()) => {
            PreviewDetectedKindSnapshot::Video
        }
        PreviewBody::Binary(_) => PreviewDetectedKindSnapshot::Binary,
        PreviewBody::Text(_) if is_api_schema_path(&path) => PreviewDetectedKindSnapshot::ApiSchema,
        PreviewBody::Text(_) if matches!(ext, "json" | "jsonc" | "json5" | "yaml" | "yml") => {
            PreviewDetectedKindSnapshot::StructuredData
        }
        PreviewBody::Text(_) if matches!(ext, "diff" | "patch") => {
            PreviewDetectedKindSnapshot::Diff
        }
        PreviewBody::Text(_) if is_mermaid_path(&path) => PreviewDetectedKindSnapshot::Mermaid,
        PreviewBody::Text(text) if ext == "log" || looks_like_log(text) => {
            PreviewDetectedKindSnapshot::Log
        }
        PreviewBody::Text(_) if language_for_path(&path).is_some() => {
            PreviewDetectedKindSnapshot::Code
        }
        PreviewBody::Text(_) => PreviewDetectedKindSnapshot::Text,
    }
}

fn language_for_path(path: &str) -> Option<String> {
    reef_core::nav::NavLang::from_path(Path::new(path)).map(|lang| format!("{lang:?}"))
}

fn structured_format_for_path(path: &str) -> StructuredDataFormatSnapshot {
    let lower = path.to_ascii_lowercase();
    if is_api_schema_path(&lower) {
        if lower.ends_with("schema.json") || lower.ends_with(".schema.json") {
            StructuredDataFormatSnapshot::JsonSchema
        } else {
            StructuredDataFormatSnapshot::OpenApi
        }
    } else if lower.ends_with(".yaml") || lower.ends_with(".yml") {
        StructuredDataFormatSnapshot::Yaml
    } else {
        StructuredDataFormatSnapshot::Json
    }
}

fn is_api_schema_path(path: &str) -> bool {
    path.ends_with("openapi.json")
        || path.ends_with("openapi.yaml")
        || path.ends_with("openapi.yml")
        || path.ends_with("swagger.json")
        || path.ends_with("swagger.yaml")
        || path.ends_with("swagger.yml")
        || path.ends_with(".schema.json")
}

fn is_report_path(path: &str) -> bool {
    path.ends_with(".analysis.md") || path.ends_with(".report.md")
}

fn is_mermaid_path(path: &str) -> bool {
    path.ends_with(".mmd") || path.ends_with(".mermaid")
}

fn is_video_source(ext: &str, mime: Option<&str>) -> bool {
    mime.is_some_and(|mime| mime.starts_with("video/"))
        || matches!(ext, "mp4" | "m4v" | "mov" | "webm" | "mkv" | "avi")
}

fn looks_like_log(text: &TextPreview) -> bool {
    text.lines.iter().take(8).any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains(" error ")
            || lower.contains(" warn ")
            || lower.starts_with("error ")
            || lower.starts_with("warn ")
            || line.starts_with('[')
                && line.len() > 12
                && line.as_bytes().get(5) == Some(&b'-')
                && line.as_bytes().get(8) == Some(&b'-')
    })
}

fn binary_reason_label(reason: &BinaryReason) -> &'static str {
    match reason {
        BinaryReason::NonImage => "nonImage",
        BinaryReason::UnsupportedImage => "unsupportedImage",
        BinaryReason::TooLarge => "tooLarge",
        BinaryReason::DecodeError(_) => "decodeError",
        BinaryReason::NullBytes => "nullBytes",
        BinaryReason::Empty => "empty",
    }
}

fn markdown_rows(markdown: &MarkdownPreview) -> Vec<Vec<MarkdownSpanSnapshot>> {
    markdown
        .rows
        .iter()
        .map(|row| row.iter().map(MarkdownSpanSnapshot::from).collect())
        .collect()
}

impl From<TextStyle> for TextStyleSnapshot {
    fn from(style: TextStyle) -> Self {
        Self {
            fg: style.fg.map(RgbSnapshot::from),
            bold: style.bold,
            italic: style.italic,
            underlined: style.underlined,
        }
    }
}

impl From<Rgb> for RgbSnapshot {
    fn from(rgb: Rgb) -> Self {
        Self {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        }
    }
}

impl From<&MarkdownSpan> for MarkdownSpanSnapshot {
    fn from(span: &MarkdownSpan) -> Self {
        Self {
            text: span.text.clone(),
            style: MarkdownStyleSnapshot::from(span.style),
            link: span.link.clone(),
            syntax: span.syntax.map(TextStyleSnapshot::from),
        }
    }
}

impl From<MarkdownStyle> for MarkdownStyleSnapshot {
    fn from(style: MarkdownStyle) -> Self {
        Self {
            role: MarkdownRoleSnapshot::from(style.role),
            bold: style.bold,
            italic: style.italic,
        }
    }
}

impl From<MarkdownRole> for MarkdownRoleSnapshot {
    fn from(role: MarkdownRole) -> Self {
        match role {
            MarkdownRole::Normal => Self::Normal,
            MarkdownRole::Heading => Self::Heading,
            MarkdownRole::Quote => Self::Quote,
            MarkdownRole::Code => Self::Code,
            MarkdownRole::CodeBlockHeader => Self::CodeBlockHeader,
            MarkdownRole::CodeBlockText => Self::CodeBlockText,
            MarkdownRole::Link => Self::Link,
            MarkdownRole::TableHeader => Self::TableHeader,
            MarkdownRole::Border => Self::Border,
        }
    }
}

impl From<&reef_sqlite_preview::DbObjectKey> for DatabaseObjectKeySnapshot {
    fn from(key: &reef_sqlite_preview::DbObjectKey) -> Self {
        Self {
            schema: key.schema.clone(),
            name: key.name.clone(),
            kind: key.kind.as_master_type().to_string(),
        }
    }
}

impl From<&reef_sqlite_preview::ColumnInfo> for DatabaseColumnSnapshot {
    fn from(column: &reef_sqlite_preview::ColumnInfo) -> Self {
        Self {
            name: column.name.clone(),
            decl_type: column.decl_type.clone(),
            notnull: column.notnull,
            pk: column.pk,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reef_core::text::StyledToken;
    use std::path::PathBuf;

    fn text_doc(path: &str, lines: &[&str]) -> PreviewDocument {
        PreviewDocument {
            path: path.to_string(),
            local_path: None,
            bytes_on_disk: lines.iter().map(|line| line.len() as u64).sum(),
            mime: Some("text/plain".into()),
            body: PreviewBody::Text(TextPreview {
                lines: lines.iter().map(|line| line.to_string()).collect(),
                highlighted: None,
                parsed: None,
            }),
        }
    }

    #[test]
    fn json_preview_becomes_structured_data() {
        let mut doc = text_doc("schema.json", &["{\"type\":\"object\"}"]);
        doc.local_path = Some(PathBuf::from("/tmp/ws/schema.json"));
        let snapshot = PreviewDocumentSnapshot::from_document(&doc, 1);

        assert_eq!(
            snapshot.source.detected_kind,
            PreviewDetectedKindSnapshot::StructuredData
        );
        assert!(matches!(
            snapshot.body,
            PreviewBodySnapshot::StructuredData {
                format: StructuredDataFormatSnapshot::Json,
            }
        ));
        assert_eq!(
            snapshot.source.local_path.as_deref(),
            Some("/tmp/ws/schema.json")
        );
    }

    #[test]
    fn source_snapshot_does_not_synthesize_local_path() {
        let doc = text_doc("../secret.json", &["{}"]);
        let snapshot = PreviewDocumentSnapshot::from_document(&doc, 1);

        assert!(!snapshot.source.local_file_available);
        assert_eq!(snapshot.source.local_path, None);
    }

    #[test]
    fn source_snapshot_uses_backend_supplied_local_path() {
        let mut doc = text_doc("safe.json", &["{}"]);
        doc.local_path = Some(PathBuf::from("/tmp/ws/safe.json"));
        let snapshot = PreviewDocumentSnapshot::from_document(&doc, 1);

        assert!(snapshot.source.local_file_available);
        assert_eq!(
            snapshot.source.local_path.as_deref(),
            Some("/tmp/ws/safe.json")
        );
    }

    #[test]
    fn structured_data_snapshot_omits_text_payload() {
        let doc = PreviewDocument {
            path: "package.json".to_string(),
            local_path: None,
            bytes_on_disk: 15,
            mime: Some("application/json".into()),
            body: PreviewBody::Text(TextPreview {
                lines: vec!["{\"name\":\"reef\"}".to_string()],
                highlighted: Some(vec![vec![StyledToken::new(
                    TextStyle {
                        fg: Some(Rgb {
                            r: 150,
                            g: 40,
                            b: 150,
                        }),
                        bold: true,
                        italic: false,
                        underlined: false,
                    },
                    "\"name\"",
                )]]),
                parsed: None,
            }),
        };

        let snapshot = PreviewDocumentSnapshot::from_document(&doc, 1);

        assert!(matches!(
            snapshot.body,
            PreviewBodySnapshot::StructuredData {
                format: StructuredDataFormatSnapshot::Json
            }
        ));
    }

    #[test]
    fn code_snapshot_uses_one_source_and_utf16_style_spans() {
        let style = TextStyle {
            fg: Some(Rgb {
                r: 120,
                g: 80,
                b: 200,
            }),
            bold: true,
            italic: false,
            underlined: false,
        };
        let doc = PreviewDocument {
            path: "src/main.rs".to_string(),
            local_path: None,
            bytes_on_disk: 16,
            mime: Some("text/plain".into()),
            body: PreviewBody::Text(TextPreview {
                lines: vec!["let icon = \"🪸\";".to_string()],
                highlighted: Some(vec![vec![
                    StyledToken::new(style, "let icon = \""),
                    StyledToken::new(style, "🪸"),
                    StyledToken::new(style, "\";"),
                ]]),
                parsed: None,
            }),
        };

        let snapshot = PreviewDocumentSnapshot::from_document(&doc, 7);

        let PreviewBodySnapshot::Code {
            text, style_spans, ..
        } = snapshot.body
        else {
            panic!("expected code snapshot");
        };
        assert_eq!(text, "let icon = \"🪸\";");
        assert_eq!(style_spans.len(), 1, "adjacent equal styles should merge");
        assert_eq!(style_spans[0].utf16_start, 0);
        assert_eq!(style_spans[0].utf16_length, 16);
    }

    #[test]
    fn markdown_snapshot_exposes_raw_source_for_native_renderers() {
        let source = "# Title\n\n| A | B |\n| --- | --- |\n| 1 | 2 |\n";
        let markdown = reef_core::markdown::build_markdown_preview("README.md", source)
            .expect("markdown preview");
        let doc = PreviewDocument {
            path: "README.md".to_string(),
            local_path: None,
            bytes_on_disk: source.len() as u64,
            mime: Some("text/markdown".into()),
            body: PreviewBody::Markdown(markdown),
        };

        let snapshot = PreviewDocumentSnapshot::from_document(&doc, 1);

        assert!(matches!(
            snapshot.body,
            PreviewBodySnapshot::Markdown { ref source, .. } if source == "# Title\n\n| A | B |\n| --- | --- |\n| 1 | 2 |\n"
        ));
    }

    #[test]
    fn video_binary_keeps_video_kind() {
        let doc = PreviewDocument {
            path: "clip.mp4".to_string(),
            local_path: None,
            bytes_on_disk: 42,
            mime: Some("video/mp4".into()),
            body: PreviewBody::Binary(BinaryInfo::new(
                42,
                Some("video/mp4"),
                BinaryReason::NonImage,
            )),
        };
        let snapshot = PreviewDocumentSnapshot::from_document(&doc, 1);

        assert_eq!(
            snapshot.source.detected_kind,
            PreviewDetectedKindSnapshot::Video
        );
        assert!(matches!(snapshot.body, PreviewBodySnapshot::Video { .. }));
    }

    #[test]
    fn binary_snapshot_carries_head_hex() {
        let doc = PreviewDocument {
            path: "archive.bin".to_string(),
            local_path: None,
            bytes_on_disk: 4,
            mime: None,
            body: PreviewBody::Binary(BinaryInfo::with_head_bytes(
                4,
                None,
                BinaryReason::NonImage,
                &[0xde, 0xad, 0xbe, 0xef],
            )),
        };

        let snapshot = PreviewDocumentSnapshot::from_document(&doc, 1);

        assert!(matches!(
            snapshot.body,
            PreviewBodySnapshot::Binary { ref head_hex, .. } if head_hex == &vec!["de ad be ef".to_string()]
        ));
    }
}
