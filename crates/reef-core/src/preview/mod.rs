pub mod binary;
pub mod image;
pub mod loader;

use std::borrow::Cow;
use std::sync::Arc;

pub use binary::{BinaryInfo, BinaryReason};
pub use image::ImagePreview;
pub use loader::{INITIAL_DB_PAGE_ROWS, load_preview};

#[derive(Debug, Clone)]
pub struct PreviewDocument {
    pub path: String,
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
    pub highlighted: Option<Vec<Vec<crate::text::StyledToken>>>,
    pub parsed: Option<Arc<crate::nav::FileParse>>,
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
                .text_rows
                .iter()
                .map(|line| Cow::Borrowed(line.as_str()))
                .collect(),
            _ => Vec::new(),
        }
    }
}
