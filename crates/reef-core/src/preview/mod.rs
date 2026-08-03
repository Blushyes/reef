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
    /// Canonical target expressed relative to the workspace root when the
    /// backend can resolve it. This keeps watcher invalidation correct for a
    /// preview opened through an in-workspace symlink.
    pub resolved_path: Option<PathBuf>,
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
    pub structured: Option<crate::structured_data::StructuredDataDocument>,
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

    /// Workspace-relative files whose changes can alter this document.
    ///
    /// Most previews depend only on their source path. SQLite also reads
    /// committed data from its rollback journal or WAL sidecars, so watcher
    /// invalidation must observe those files without teaching app state about
    /// a specific preview format.
    pub fn dependency_paths(&self) -> Vec<PathBuf> {
        let logical_path = std::path::Path::new(&self.path);
        let mut paths = self.dependency_paths_from(logical_path);
        if let Some(resolved_path) = self.resolved_path.as_deref()
            && resolved_path != logical_path
        {
            paths.extend(self.dependency_paths_from(resolved_path));
        }
        paths
    }

    /// Host-local files whose metadata determines whether a cached preview is
    /// still current. Remote documents do not expose host-local paths.
    pub fn local_dependency_paths(&self) -> Vec<PathBuf> {
        self.local_path
            .as_deref()
            .map(|path| self.dependency_paths_from(path))
            .unwrap_or_default()
    }

    fn dependency_paths_from(&self, source: &std::path::Path) -> Vec<PathBuf> {
        let mut paths = vec![source.to_path_buf()];
        if self.is_database() {
            paths.extend(["-wal", "-shm", "-journal"].into_iter().map(|suffix| {
                let mut sidecar = source.as_os_str().to_os_string();
                sidecar.push(suffix);
                PathBuf::from(sidecar)
            }));
        }
        paths
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
