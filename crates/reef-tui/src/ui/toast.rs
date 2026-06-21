//! Cross-panel toast/notification queue.
//!
//! Any panel can push a toast here and the top-level `ui::render_status_bar`
//! shows the most recent one. Used for things that must stay visible when
//! the user switches tabs (push success/failure being the current primary
//! consumer).

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
