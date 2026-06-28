//! Library crate for reef — exposes modules as public API so that
//! integration tests under `tests/` can exercise them. The `reef` binary
//! (`src/main.rs`) is a thin wrapper that consumes this library.

#[path = "app/mod.rs"]
mod tui_app;

pub use tui_app::TuiApp;
pub mod clipboard;
pub mod editor;
pub mod find_widget;
pub mod global_search;
pub(crate) mod hosts_picker;
pub mod i18n;
pub mod images;
pub mod input;
pub(crate) mod input_edit;
pub(crate) mod input_edit_multi;
pub mod keymap;
pub(crate) mod picker_core;
pub mod prefs;
pub mod quick_open;
pub mod reveal;
pub mod search;
pub mod settings;
pub mod shell_integration;
pub mod ui;
