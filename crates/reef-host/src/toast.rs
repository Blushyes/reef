//! Cross-panel toast/notification queue.
//!
//! Previously the plugin layer surfaced plugin-originated `reef/notify`
//! messages via `PluginManager::notifications`. Now that everything runs
//! in-process, any panel can push a toast here and the top-level `ui`
//! render reads the queue so errors (push failures, etc.) stay visible when
//! the user switches tabs.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ToastLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Toast {
    pub level: ToastLevel,
    pub message: String,
}

#[allow(dead_code)]
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
