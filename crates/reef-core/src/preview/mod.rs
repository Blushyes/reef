pub mod binary;
pub mod image;
pub mod loader;

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

pub use binary::{BinaryInfo, BinaryReason};
pub use image::ImagePreview;
pub use loader::{
    INITIAL_DB_PAGE_ROWS, MAX_TEXT_PREVIEW_BYTES, build_text_preview_enrichment,
    build_textual_preview_body, load_preview, load_preview_from_path,
    structured_data_source_required, text_preview_can_be_enriched,
};

#[derive(Debug, Clone)]
pub struct PreviewDocument {
    pub path: String,
    pub local_path: Option<PathBuf>,
    pub bytes_on_disk: u64,
    pub mime: Option<String>,
    pub body: PreviewBody,
}

#[derive(Debug, Clone)]
pub enum PreviewBody {
    Text(TextPreview),
    Markdown(crate::markdown::MarkdownPreview),
    Image(ImagePreview),
    Binary(BinaryInfo),
    Database(reef_sqlite_preview::DatabaseInfoV2),
}

#[derive(Debug, Clone)]
pub struct TextPreview {
    pub lines: Vec<String>,
    /// Complete source retained for structured-data consumers. `lines` remains the bounded
    /// display projection used by ordinary text previews.
    pub source: Option<Arc<str>>,
    pub highlighted: Option<Vec<Vec<crate::text::StyledToken>>>,
    pub parsed: Option<Arc<crate::nav::FileParse>>,
}

#[derive(Debug, Clone)]
pub struct TextPreviewEnrichment {
    pub highlighted: Option<Vec<Vec<crate::text::StyledToken>>>,
    pub parsed: Option<Arc<crate::nav::FileParse>>,
}

#[derive(Debug, Clone)]
pub enum PreviewEnrichment {
    Text(TextPreviewEnrichment),
    Markdown(crate::markdown::MarkdownPreview),
}

impl PreviewDocument {
    pub fn is_text(&self) -> bool {
        matches!(self.body, PreviewBody::Text(_) | PreviewBody::Markdown(_))
    }

    pub fn is_database(&self) -> bool {
        matches!(self.body, PreviewBody::Database(_))
    }
}

impl PreviewBody {
    pub fn display_text_rows(&self) -> Vec<Cow<'_, str>> {
        match self {
            PreviewBody::Text(text) => text
                .lines
                .iter()
                .map(|line| Cow::Borrowed(line.as_str()))
                .collect(),
            PreviewBody::Markdown(markdown) => markdown
                .text_rows()
                .map(|rows| {
                    rows.iter()
                        .map(|line| Cow::Borrowed(line.as_str()))
                        .collect()
                })
                .unwrap_or_else(|| markdown.source.lines().map(Cow::Borrowed).collect()),
            _ => Vec::new(),
        }
    }
}
