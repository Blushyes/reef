//! Library crate for reef-git — exposes modules as public API so that
//! integration tests under `tests/` can exercise them. The `reef-git`
//! binary (src/main.rs) is a thin wrapper that consumes this library.

pub mod git;
pub mod graph;
pub mod prefs;
pub mod tree;
pub mod writer;

pub use writer::Writer;
