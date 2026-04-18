//! Library crate for reef — exposes modules as public API so that
//! integration tests under `tests/` can exercise them. The `reef` binary
//! (src/main.rs) is a thin wrapper that consumes this library.

pub mod app;
pub mod file_tree;
pub mod fs_watcher;
pub mod git;
pub mod prefs;
pub mod ui;
