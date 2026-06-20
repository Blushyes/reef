//! Workspace-wide symbol index. Phase 2 deliverable: cross-file
//! `gd` and `gr` find-references.
//!
//! Strategy: name-based, not type-based. We tree-sitter-parse every
//! file with a supported grammar and extract identifier-position pairs
//! into a `HashMap<String, Vec<SymbolLoc>>`. Lookups are O(1) on the
//! identifier name, results are filtered by language at query time.
//! Less precise than stack-graphs (no scope resolution, false
//! positives across unrelated files with same-name symbols) but it
//! covers all four bundled languages — `tree-sitter-stack-graphs-rust`
//! and `-go` don't exist on crates.io, so a name-based path is the
//! only way to ship cross-file Rust/Go in Phase 2.
//!
//! Phase 3's lazy LSP refines these results into a precise cache when
//! the language server is up. So the v1 trade-off — "fast but with
//! false positives" — is fine: rust-analyzer / pyright fix the
//! precision later, transparently.
//!
//! Walks are gitignore-aware via the `ignore` crate (already in tree
//! for grep-searcher). Files beyond the per-file size cap are
//! skipped to keep index build sub-second on medium repos.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::WalkBuilder;
use std::sync::OnceLock;

use tree_sitter::{Query, QueryCursor, StreamingIterator};

use super::{NavLang, intrafile};

/// Cap per-file bytes for indexing. Matches the highlight cap so the
/// index covers exactly the same files the preview highlights.
const MAX_FILE_BYTES_INDEX: u64 = 512 * 1024;

/// Bounded snippet length for the candidate / refs UI.
pub(crate) const SNIPPET_MAX_W: usize = 80;

/// In-memory workspace symbol index. Keyed by identifier text; each
/// value is the list of definition sites across the workspace.
#[derive(Debug)]
pub struct WorkspaceIndex {
    pub root: PathBuf,
    pub defs_by_name: HashMap<String, Vec<SymbolLoc>>,
    pub refs_by_name: HashMap<String, Vec<SymbolLoc>>,
    /// File count for status-bar reporting (`indexed 412 files`).
    pub file_count: usize,
}

/// A definition or reference site.
#[derive(Debug, Clone)]
pub struct SymbolLoc {
    pub path: PathBuf,
    pub line: usize,
    pub byte_range: Range<usize>,
    pub snippet: String,
    pub lang: NavLang,
}

/// Build (or rebuild) the workspace symbol index. Runs in the nav
/// worker thread — callers dispatch via `TaskCoordinator::build_nav_workspace`.
///
/// Walks `root` honoring `.gitignore` (via `ignore::WalkBuilder`),
/// parses every file whose extension matches a supported `NavLang`,
/// and accumulates definition + reference sites.
pub fn build_workspace_index(root: PathBuf) -> WorkspaceIndex {
    let mut defs_by_name: HashMap<String, Vec<SymbolLoc>> = HashMap::new();
    let mut refs_by_name: HashMap<String, Vec<SymbolLoc>> = HashMap::new();
    let mut file_count = 0usize;

    let walker = WalkBuilder::new(&root)
        .standard_filters(true)
        .git_ignore(true)
        // Apply .gitignore even when the directory isn't a git repo
        // — matches reef's overall design ("works on any directory,
        // not just git checkouts"). Without this flag, the walker
        // would only honour `.gitignore` inside a `.git`-having tree.
        .require_git(false)
        .hidden(true)
        .build();

    for entry in walker.flatten() {
        if entry.file_type().is_none_or(|t| !t.is_file()) {
            continue;
        }
        let abs = entry.path();
        let Ok(rel) = abs.strip_prefix(&root) else {
            continue;
        };
        let Some(lang) = NavLang::from_path(abs) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() > MAX_FILE_BYTES_INDEX {
            continue;
        }
        let Ok(bytes) = std::fs::read(abs) else {
            continue;
        };
        // Skip apparent binaries: tree-sitter parsers happily accept
        // garbage and emit useless ERROR nodes; the index would gain
        // noise without value.
        if bytes.iter().take(8192).any(|b| *b == 0) {
            continue;
        }
        let source: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());
        let Some(parse) = super::parse_file_if_supported(lang, source) else {
            continue;
        };
        file_count += 1;
        let rel_path: PathBuf = rel.to_path_buf();
        extract_definitions(&parse, &rel_path, lang, &mut defs_by_name);
        extract_references(&parse, &rel_path, lang, &mut refs_by_name);
    }

    WorkspaceIndex {
        root,
        defs_by_name,
        refs_by_name,
        file_count,
    }
}

fn extract_definitions(
    parse: &super::FileParse,
    rel_path: &Path,
    lang: NavLang,
    out: &mut HashMap<String, Vec<SymbolLoc>>,
) {
    let Some(query) = intrafile::definition_query_pub(lang) else {
        return;
    };
    extract_symbols(parse, rel_path, lang, query, "name", out);
}

fn extract_references(
    parse: &super::FileParse,
    rel_path: &Path,
    lang: NavLang,
    out: &mut HashMap<String, Vec<SymbolLoc>>,
) {
    let Some(query) = reference_query_compiled(lang) else {
        return;
    };
    extract_symbols(parse, rel_path, lang, query, "ref", out);
}

fn extract_symbols(
    parse: &super::FileParse,
    rel_path: &Path,
    lang: NavLang,
    query: &Query,
    capture: &str,
    out: &mut HashMap<String, Vec<SymbolLoc>>,
) {
    let mut cursor = QueryCursor::new();
    let source = parse.source();
    let Some(name_idx) = query.capture_index_for_name(capture) else {
        return;
    };

    let mut matches = cursor.matches(query, parse.tree.root_node(), source);
    while let Some(m) = matches.next() {
        for cap in m.captures.iter().filter(|c| c.index == name_idx) {
            let node = cap.node;
            let Ok(text) = std::str::from_utf8(&source[node.byte_range()]) else {
                continue;
            };
            let start = node.start_position();
            let end = node.end_position();
            if start.row != end.row {
                continue;
            }
            out.entry(text.to_owned()).or_default().push(SymbolLoc {
                path: rel_path.to_path_buf(),
                line: start.row,
                byte_range: start.column..end.column,
                snippet: snippet_for(source, start.row),
                lang,
            });
        }
    }
}

pub(crate) fn snippet_for(source: &[u8], line: usize) -> String {
    snippet_from_line(super::line_bytes_at(source, line))
}

pub(crate) fn snippet_from_line(line: &[u8]) -> String {
    let s = String::from_utf8_lossy(line);
    let trimmed = s.trim_start();
    if trimmed.chars().count() > SNIPPET_MAX_W {
        trimmed.chars().take(SNIPPET_MAX_W).collect::<String>() + "…"
    } else {
        trimmed.to_string()
    }
}

/// Per-language reference query. Matches every identifier-shaped node
/// that can stand for a use (call site, type reference, field access,
/// etc.). Cast as wide as the definition query but uses the parser's
/// node-kind names directly rather than field paths — references are
/// less structurally-constrained than declarations.
fn reference_query(lang: NavLang) -> &'static str {
    match lang {
        NavLang::Rust => RUST_REF_QUERY,
        NavLang::TypeScript | NavLang::Tsx => TS_REF_QUERY,
        NavLang::Python => PY_REF_QUERY,
        NavLang::Go => GO_REF_QUERY,
        // Vue: identifiers live inside the unparsed `<script>` blob.
        // An empty query yields zero references — Phase 4 polish
        // could re-parse the script with tree-sitter-typescript,
        // but volar / vue-language-server already does this and
        // we'd just be duplicating it.
        NavLang::Vue => "",
    }
}

/// Per-language compiled reference query, lazily initialised and cached
/// forever — same pattern as `intrafile::definition_query`. The index
/// build calls this once per file; `Query::new` is the expensive part
/// (~1ms each), so recompiling it for every file in a 2000-file repo on
/// every (re)index was the bulk of the workspace-build cost. TypeScript
/// and Tsx share the same query *string* but compile against different
/// `Language`s, so they keep separate cache slots.
fn reference_query_compiled(lang: NavLang) -> Option<&'static Query> {
    // TypeScript and Tsx share the same query *string* but compile
    // against different `Language`s, so they keep separate cache slots.
    match lang {
        NavLang::Rust => {
            static Q: OnceLock<Option<Query>> = OnceLock::new();
            super::cached_query(&Q, lang, reference_query(lang))
        }
        NavLang::TypeScript => {
            static Q: OnceLock<Option<Query>> = OnceLock::new();
            super::cached_query(&Q, lang, reference_query(lang))
        }
        NavLang::Tsx => {
            static Q: OnceLock<Option<Query>> = OnceLock::new();
            super::cached_query(&Q, lang, reference_query(lang))
        }
        NavLang::Python => {
            static Q: OnceLock<Option<Query>> = OnceLock::new();
            super::cached_query(&Q, lang, reference_query(lang))
        }
        NavLang::Go => {
            static Q: OnceLock<Option<Query>> = OnceLock::new();
            super::cached_query(&Q, lang, reference_query(lang))
        }
        NavLang::Vue => None,
    }
}

const RUST_REF_QUERY: &str = r#"
((identifier) @ref)
((type_identifier) @ref)
((field_identifier) @ref)
"#;

const TS_REF_QUERY: &str = r#"
((identifier) @ref)
((type_identifier) @ref)
((property_identifier) @ref)
"#;

const PY_REF_QUERY: &str = r#"
((identifier) @ref)
"#;

const GO_REF_QUERY: &str = r#"
((identifier) @ref)
((type_identifier) @ref)
((field_identifier) @ref)
"#;
