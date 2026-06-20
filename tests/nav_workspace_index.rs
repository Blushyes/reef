//! Phase 2 end-to-end tests for the workspace symbol index:
//!   - cross-file `gd` finds defs in sibling files,
//!   - `gr` populates the candidates popup with references workspace-wide,
//!   - per-language filtering keeps `foo` in Python out of `foo` in Rust.

use reef_core::nav::{NavLang, build_workspace_index};
use std::fs;
use std::sync::Mutex;
use tempfile::TempDir;
use test_support::CwdGuard;

static CWD_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn index_finds_rust_function_across_files() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    fs::write(tmp.path().join("a.rs"), "pub fn helper() -> i32 { 42 }\n").unwrap();
    fs::write(
        tmp.path().join("b.rs"),
        "fn main() { let _ = crate::helper(); }\n",
    )
    .unwrap();

    let index = build_workspace_index(tmp.path().to_path_buf());

    // Defs side: `helper` only declared in a.rs.
    let defs = index.defs_by_name.get("helper").expect("helper indexed");
    assert!(
        defs.iter()
            .any(|d| d.path.ends_with("a.rs") && d.lang == NavLang::Rust),
        "expected helper def in a.rs, got {:?}",
        defs
    );

    // Refs side: `helper` appears in BOTH files (decl token also
    // counts as a reference by tree-sitter; we filter at query time
    // by clicking on the call site, not the decl).
    let refs = index.refs_by_name.get("helper").expect("helper referenced");
    assert!(refs.iter().any(|r| r.path.ends_with("b.rs")));
    assert!(refs.iter().any(|r| r.path.ends_with("a.rs")));
}

#[test]
fn index_filters_by_language() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    fs::write(tmp.path().join("a.rs"), "fn shared() {}\n").unwrap();
    fs::write(tmp.path().join("b.py"), "def shared():\n    pass\n").unwrap();

    let index = build_workspace_index(tmp.path().to_path_buf());

    let defs = index.defs_by_name.get("shared").expect("shared indexed");
    // Both should be present BUT with distinct lang tags so callers
    // can filter at query time.
    assert!(defs.iter().any(|d| d.lang == NavLang::Rust));
    assert!(defs.iter().any(|d| d.lang == NavLang::Python));
}

#[test]
fn index_respects_gitignore() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    // Mark `target/` ignored — same convention as Rust projects.
    fs::write(tmp.path().join(".gitignore"), "target/\n").unwrap();
    fs::create_dir_all(tmp.path().join("target/release/build")).unwrap();
    fs::write(tmp.path().join("a.rs"), "fn visible() {}\n").unwrap();
    fs::write(
        tmp.path().join("target/release/build/gen.rs"),
        "fn hidden() {}\n",
    )
    .unwrap();

    let index = build_workspace_index(tmp.path().to_path_buf());
    assert!(index.defs_by_name.contains_key("visible"));
    assert!(
        !index.defs_by_name.contains_key("hidden"),
        "gitignored file should not appear in the index"
    );
}

#[test]
fn vue_files_parse_without_panicking_even_with_no_queries() {
    // Vue's tree-sitter grammar is bundled mainly so the engine
    // recognises `.vue` files for the LSP-tier routing. Intra-file
    // queries are empty (the SFC's `<script>` body is a raw blob),
    // so the index won't surface Vue identifiers — but the build
    // must still complete cleanly when .vue files are present.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    fs::write(
        tmp.path().join("App.vue"),
        "<template>\n  <h1>{{ msg }}</h1>\n</template>\n\
         <script setup>\nconst msg = 'hi'\nfunction greet() {}\n</script>\n",
    )
    .unwrap();
    fs::write(tmp.path().join("plain.rs"), "fn rust_marker() {}\n").unwrap();

    let index = reef_core::nav::build_workspace_index(tmp.path().to_path_buf());
    // Rust file picked up normally — proves the walker isn't crashing
    // on the Vue file mid-walk.
    assert!(index.defs_by_name.contains_key("rust_marker"));
    // Vue scripts contribute zero defs/refs by design.
    assert!(!index.defs_by_name.contains_key("greet"));
    assert!(!index.defs_by_name.contains_key("msg"));
}

#[test]
fn empty_workspace_yields_empty_index() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let index = build_workspace_index(tmp.path().to_path_buf());
    assert_eq!(index.file_count, 0);
    assert!(index.defs_by_name.is_empty());
    assert!(index.refs_by_name.is_empty());
}
