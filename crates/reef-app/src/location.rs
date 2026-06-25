use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorPosition {
    pub line: usize,
    pub byte_col: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollPosition {
    pub vertical: usize,
    pub horizontal: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocationSurface {
    FilePreview,
    GitDiff {
        file_path: String,
        is_staged: bool,
    },
    GraphDiff {
        commit_oid: String,
        file_path: String,
    },
    SearchPreview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationSnapshot {
    pub surface: LocationSurface,
    pub path: PathBuf,
    pub cursor: CursorPosition,
    pub scroll: ScrollPosition,
}
