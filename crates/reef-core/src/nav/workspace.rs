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
//! The caller owns workspace IO. This module only indexes relative
//! paths plus source bytes, keeping `reef-core` independent from local
//! filesystem walking.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::sync::OnceLock;

use tree_sitter::{Query, QueryCursor, StreamingIterator};

use super::{NavLang, intrafile};

/// Cap per-file bytes for indexing. Matches the highlight cap so the
/// index covers exactly the same files the preview highlights.
pub const MAX_FILE_BYTES_INDEX: u64 = 512 * 1024;

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

#[derive(Debug, Clone)]
pub struct WorkspaceIndexFile {
    pub path: PathBuf,
    pub source: Arc<[u8]>,
}

impl WorkspaceIndex {
    pub fn definitions_for(
        &self,
        name: &str,
        lang: NavLang,
        skip_site: Option<(&Path, Option<usize>)>,
    ) -> Vec<super::Location> {
        self.defs_by_name
            .get(name)
            .map(|defs| symbol_locs_to_locations(defs, lang, skip_site))
            .unwrap_or_default()
    }

    pub fn references_for(&self, name: &str, lang: NavLang) -> Vec<super::Location> {
        self.refs_by_name
            .get(name)
            .map(|refs| symbol_locs_to_locations(refs, lang, None))
            .unwrap_or_default()
    }
}

fn symbol_locs_to_locations(
    symbols: &[SymbolLoc],
    lang: NavLang,
    skip_site: Option<(&Path, Option<usize>)>,
) -> Vec<super::Location> {
    symbols
        .iter()
        .filter(|symbol| symbol.lang == lang)
        .filter(|symbol| {
            !skip_site.is_some_and(|(path, line)| {
                symbol.path == path && line.is_none_or(|line| symbol.line == line)
            })
        })
        .map(|symbol| super::Location {
            path: Some(symbol.path.clone()),
            line: symbol.line,
            byte_range: symbol.byte_range.clone(),
            snippet: symbol.snippet.clone(),
        })
        .collect()
}

/// Build (or rebuild) the workspace symbol index from already-loaded
/// workspace files. Callers are responsible for walking the workspace,
/// honoring ignore rules, applying path security, and reading bytes.
pub fn build_workspace_index(
    root: PathBuf,
    files: impl IntoIterator<Item = WorkspaceIndexFile>,
) -> WorkspaceIndex {
    let mut defs_by_name: HashMap<String, Vec<SymbolLoc>> = HashMap::new();
    let mut refs_by_name: HashMap<String, Vec<SymbolLoc>> = HashMap::new();
    let mut file_count = 0usize;

    for file in files {
        let rel_path = file.path;
        let Some(lang) = NavLang::from_path(&rel_path) else {
            continue;
        };
        if file.source.len() as u64 > MAX_FILE_BYTES_INDEX {
            continue;
        }
        // Skip apparent binaries: tree-sitter parsers happily accept
        // garbage and emit useless ERROR nodes; the index would gain
        // noise without value.
        if file.source.iter().take(8192).any(|b| *b == 0) {
            continue;
        }
        let Some(parse) = super::parse_file_if_supported(lang, file.source) else {
            continue;
        };
        file_count += 1;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(path: &str, line: usize, lang: NavLang) -> SymbolLoc {
        SymbolLoc {
            path: PathBuf::from(path),
            line,
            byte_range: 1..4,
            snippet: "foo".to_string(),
            lang,
        }
    }

    #[test]
    fn definitions_for_filters_lang_and_skip_site() {
        let mut defs_by_name = HashMap::new();
        defs_by_name.insert(
            "foo".to_string(),
            vec![
                symbol("src/a.rs", 1, NavLang::Rust),
                symbol("src/b.rs", 2, NavLang::Rust),
                symbol("src/a.py", 1, NavLang::Python),
            ],
        );
        let index = WorkspaceIndex {
            root: PathBuf::new(),
            defs_by_name,
            refs_by_name: HashMap::new(),
            file_count: 0,
        };

        let defs =
            index.definitions_for("foo", NavLang::Rust, Some((Path::new("src/a.rs"), Some(1))));

        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].path.as_deref(), Some(Path::new("src/b.rs")));
    }

    #[test]
    fn definitions_for_can_skip_whole_path() {
        let mut defs_by_name = HashMap::new();
        defs_by_name.insert(
            "foo".to_string(),
            vec![
                symbol("src/a.rs", 1, NavLang::Rust),
                symbol("src/a.rs", 2, NavLang::Rust),
                symbol("src/b.rs", 3, NavLang::Rust),
            ],
        );
        let index = WorkspaceIndex {
            root: PathBuf::new(),
            defs_by_name,
            refs_by_name: HashMap::new(),
            file_count: 0,
        };

        let defs = index.definitions_for("foo", NavLang::Rust, Some((Path::new("src/a.rs"), None)));

        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].path.as_deref(), Some(Path::new("src/b.rs")));
    }
}
