use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmRequest {
    TreeDelete(TreeDeleteConfirm),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeDeleteConfirm {
    pub path: PathBuf,
    pub display_name: String,
    pub is_dir: bool,
    pub hard: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmTone {
    Default,
    Danger,
}

impl ConfirmRequest {
    pub fn tone(&self) -> ConfirmTone {
        match self {
            Self::TreeDelete(_) => ConfirmTone::Danger,
        }
    }
}
