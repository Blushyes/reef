#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub level: ToastLevel,
    pub message: String,
}

impl Toast {
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            level: ToastLevel::Info,
            message: message.into(),
        }
    }

    pub fn warn(message: impl Into<String>) -> Self {
        Self {
            level: ToastLevel::Warn,
            message: message.into(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            level: ToastLevel::Error,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum AppEffect {
    Quit,
    CopyToClipboard {
        text: String,
        success: Option<Toast>,
        failure: Toast,
    },
    OpenUrl(String),
    OpenInEditor(std::path::PathBuf),
    Toast(Toast),
    SwitchSession(crate::features::hosts_picker::SshTarget),
}
