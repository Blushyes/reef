//! Library crate for reef — exposes modules as public API so that
//! integration tests under `tests/` can exercise them. The `reef` binary
//! (src/main.rs) is a thin wrapper that consumes this library.

pub mod app;
pub mod editor;
pub mod file_tree;
pub mod fs_watcher;
pub mod git;
pub mod global_search;
pub mod i18n;
pub mod input;
pub mod input_edit;
pub mod place_mode;
pub mod prefs;
pub mod quick_open;
pub mod search;
pub mod tasks;
pub mod ui;
