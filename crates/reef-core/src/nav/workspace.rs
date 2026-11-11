//! Workspace-wide symbol index. Phase 2 deliverable: cross-file
//! `gd` and `gr` find-references.
//!
//! Strategy: name-based, not type-based. We tree-sitter-parse every
//! file with a supported grammar and extract identifier-position pairs
//! into per-file identifier maps. Snapshots share unchanged file partitions;
//! lookups visit those maps and filter by language at query time.
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

use std::collections::{BTreeMap, HashMap};
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
const SNIPPET_LEADING_CONTEXT: usize = 16;

/// Immutable file partitions share unchanged symbols between workspace snapshots.
/// Each partition is keyed by identifier; lookups visit the indexed files.
#[derive(Debug, Clone)]
pub struct WorkspaceIndex {
    pub root: PathBuf,
    files: BTreeMap<PathBuf, Arc<FileSymbols>>,
}

#[derive(Debug, Default)]
struct FileSymbols {
    definitions: HashMap<String, Vec<SymbolLoc>>,
    references: HashMap<String, Vec<SymbolLoc>>,
}

/// A definition or reference site.
#[derive(Debug, Clone)]
pub struct SymbolLoc {
    pub path: PathBuf,
    pub line: usize,
    pub byte_range: Range<usize>,
    pub snippet: String,
    pub snippet_match_range: Range<usize>,
    pub lang: NavLang,
}

#[derive(Debug, Clone)]
pub struct WorkspaceIndexFile {
    pub path: PathBuf,
    pub source: Arc<[u8]>,
}

impl WorkspaceIndex {
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn definitions_for(
        &self,
        name: &str,
        lang: NavLang,
        skip_site: Option<(&Path, Option<usize>)>,
    ) -> Vec<super::Location> {
        self.files
            .values()
            .filter_map(|file| file.definitions.get(name))
            .flat_map(|symbols| symbol_locs_to_locations(symbols, lang, skip_site))
            .collect()
    }

    pub fn references_for(&self, name: &str, lang: NavLang) -> Vec<super::Location> {
        self.files
            .values()
            .filter_map(|file| file.references.get(name))
            .flat_map(|symbols| symbol_locs_to_locations(symbols, lang, None))
            .collect()
    }

    pub fn contains_path(&self, path: &Path) -> bool {
        self.files
            .range(path.to_path_buf()..)
            .next()
            .is_some_and(|(file, _)| file.starts_with(path))
    }

    /// Replace the affected files/subtrees, retaining all other file partitions.
    /// Missing, ignored, oversized and binary files are removed by omitting them
    /// from `files`. IO and ignore-rule evaluation belong to the caller.
    pub fn update_files(
        &mut self,
        changed_paths: &[PathBuf],
        files: impl IntoIterator<Item = WorkspaceIndexFile>,
    ) {
        for path in changed_paths {
            let removed: Vec<_> = self
                .files
                .range(path.clone()..)
                .take_while(|(file, _)| file.starts_with(path))
                .map(|(file, _)| file.clone())
                .collect();
            for file in removed {
                self.files.remove(&file);
            }
        }
        for file in files {
            let Some(lang) = NavLang::from_path(&file.path) else {
                continue;
            };
            if file.source.len() as u64 > MAX_FILE_BYTES_INDEX
                || file.source.iter().take(8192).any(|b| *b == 0)
            {
                continue;
            }
            let Some(parse) = super::parse_file_if_supported(lang, file.source) else {
                continue;
            };
            let mut symbols = FileSymbols::default();
            extract_definitions(&parse, &file.path, lang, &mut symbols.definitions);
            extract_references(&parse, &file.path, lang, &mut symbols.references);
            self.files.insert(file.path, Arc::new(symbols));
        }
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
            snippet_match_range: symbol.snippet_match_range.clone(),
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
    let mut index = WorkspaceIndex {
        root,
        files: BTreeMap::new(),
    };
    index.update_files(&[], files);
    index
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
            let snippet = snippet_for(parse, start.row, start.column..end.column);
            out.entry(text.to_owned()).or_default().push(SymbolLoc {
                path: rel_path.to_path_buf(),
                line: start.row,
                byte_range: start.column..end.column,
                snippet: snippet.text,
                snippet_match_range: snippet.match_range,
                lang,
            });
        }
    }
}

#[derive(Default)]
pub(super) struct NavigationSnippet {
    pub text: String,
    pub match_range: Range<usize>,
}

/// Decode and trim each source line once, shared by every symbol on that line.
#[derive(Debug)]
pub(super) struct SnippetLine {
    text: String,
    leading_bytes: usize,
}

impl SnippetLine {
    pub(super) fn new(line: &[u8]) -> Self {
        let text = String::from_utf8_lossy(line);
        let trimmed = text.trim_start();
        Self {
            leading_bytes: text.len() - trimmed.len(),
            text: trimmed.to_owned(),
        }
    }

    fn snippet(&self, target: Range<usize>) -> NavigationSnippet {
        let Some(target) = self.target_range(target) else {
            return NavigationSnippet {
                text: self.text.chars().take(SNIPPET_MAX_W).collect::<String>() + "…",
                match_range: 0..0,
            };
        };
        // Inspect at most one display window to distinguish a short line.
        if self.text.chars().nth(SNIPPET_MAX_W).is_none() {
            return NavigationSnippet {
                text: self.text.clone(),
                match_range: target,
            };
        }
        // Walk backwards from the target, not forwards from the line's start.
        let start = self.text[..target.start]
            .char_indices()
            .rev()
            .nth(SNIPPET_LEADING_CONTEXT - 1)
            .map_or(0, |(offset, _)| offset);
        let end = self.text[start..]
            .char_indices()
            .nth(SNIPPET_MAX_W)
            .map_or(self.text.len(), |(offset, _)| start + offset)
            .max(target.end);
        let mut text = String::new();
        if start > 0 {
            text.push('…');
        }
        let prefix = text.len();
        text.push_str(&self.text[start..end]);
        if end < self.text.len() {
            text.push('…');
        }
        NavigationSnippet {
            text,
            match_range: prefix + target.start - start..prefix + target.end - start,
        }
    }

    fn target_range(&self, target: Range<usize>) -> Option<Range<usize>> {
        let start = target.start.checked_sub(self.leading_bytes)?;
        let end = target.end.checked_sub(self.leading_bytes)?;
        self.text.get(start..end)?;
        Some(start..end)
    }
}

pub(super) fn snippet_for(
    parse: &super::FileParse,
    line: usize,
    target_byte_range: Range<usize>,
) -> NavigationSnippet {
    parse
        .snippet_line(line)
        .map(|line| line.snippet(target_byte_range))
        .unwrap_or_default()
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

    #[test]
    fn large_workspace_file_preserves_symbol_locations_and_snippets() {
        let line_count = 4_000;
        let source_line = "    fn café() { café(); }\r\n";
        let source = source_line.repeat(line_count);
        assert!(source.len() as u64 <= MAX_FILE_BYTES_INDEX);
        let index = build_workspace_index(
            PathBuf::new(),
            [WorkspaceIndexFile {
                path: PathBuf::from("large.rs"),
                source: Arc::from(source.into_bytes()),
            }],
        );

        assert_eq!(index.file_count(), 1);
        let definitions = index.definitions_for("café", NavLang::Rust, None);
        let references = index.references_for("café", NavLang::Rust);
        assert_eq!(definitions.len(), line_count);
        assert_eq!(references.len(), line_count * 2);
        for (line, definition) in definitions.iter().enumerate() {
            assert_eq!(definition.line, line);
            assert_eq!(definition.byte_range, 7..12);
        }
        for (offset, reference) in references.iter().enumerate() {
            assert_eq!(reference.line, offset / 2);
            assert_eq!(
                reference.byte_range,
                if offset % 2 == 0 { 7..12 } else { 17..22 }
            );
        }
        for symbol in definitions.iter().chain(references.iter()) {
            assert_eq!(symbol.path.as_deref(), Some(Path::new("large.rs")));
            assert_eq!(symbol.snippet, "fn café() { café(); }\r");
            assert_eq!(&symbol.snippet[symbol.snippet_match_range.clone()], "café");
        }
    }

    #[test]
    fn definitions_for_filters_lang_and_skip_site() {
        let index = build_workspace_index(
            PathBuf::new(),
            [
                file("src/a.rs", "fn foo() {}\nfn foo() {}"),
                file("src/b.rs", "fn foo() {}"),
                file("src/a.py", "def foo(): pass"),
            ],
        );
        let defs =
            index.definitions_for("foo", NavLang::Rust, Some((Path::new("src/a.rs"), Some(0))));
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].line, 1);
        let defs = index.definitions_for("foo", NavLang::Rust, Some((Path::new("src/a.rs"), None)));
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].path.as_deref(), Some(Path::new("src/b.rs")));
    }

    fn file(path: &str, source: &str) -> WorkspaceIndexFile {
        WorkspaceIndexFile {
            path: path.into(),
            source: Arc::from(source.as_bytes()),
        }
    }

    fn snippet_from_line(line: &[u8], target: Range<usize>) -> NavigationSnippet {
        SnippetLine::new(line).snippet(target)
    }

    #[test]
    fn file_updates_share_unchanged_symbols_and_preserve_old_snapshot() {
        let original = build_workspace_index(
            PathBuf::new(),
            [
                file("a.rs", "fn before() {}"),
                file("nested/b.rs", "fn untouched() {}"),
            ],
        );
        let mut updated = original.clone();
        updated.update_files(&["a.rs".into()], [file("a.rs", "fn after() {}")]);
        assert!(Arc::ptr_eq(
            &original.files[Path::new("nested/b.rs")],
            &updated.files[Path::new("nested/b.rs")]
        ));
        assert_eq!(
            original
                .definitions_for("before", NavLang::Rust, None)
                .len(),
            1
        );
        assert!(
            updated
                .definitions_for("before", NavLang::Rust, None)
                .is_empty()
        );
        assert_eq!(
            updated.definitions_for("after", NavLang::Rust, None).len(),
            1
        );
        updated.update_files(&["nested".into()], []);
        assert_eq!(updated.file_count(), 1);
        assert!(
            updated
                .references_for("untouched", NavLang::Rust)
                .is_empty()
        );
        assert_eq!(original.file_count(), 2);
    }

    #[test]
    fn long_single_line_indexes_every_unicode_symbol_with_exact_snippets() {
        let count = 4_000;
        let source = "fn café() { café(); } ".repeat(count);
        let index = build_workspace_index(PathBuf::new(), [file("long.rs", &source)]);
        let refs = index.references_for("café", NavLang::Rust);
        assert_eq!(refs.len(), count * 2);
        for symbol in refs {
            assert_eq!(symbol.line, 0);
            assert_eq!(&source[symbol.byte_range], "café");
            assert_eq!(&symbol.snippet[symbol.snippet_match_range], "café");
            assert!(symbol.snippet.chars().count() <= SNIPPET_MAX_W + 2);
        }
    }

    #[test]
    fn snippets_keep_utf8_boundaries_at_start_middle_and_end() {
        let line = "  ".to_owned() + &"汉🙂café ".repeat(100);
        for (start, name) in line.match_indices("café") {
            let snippet = snippet_from_line(line.as_bytes(), start..start + name.len());
            assert_eq!(&snippet.text[snippet.match_range], name);
            assert!(snippet.text.chars().count() <= SNIPPET_MAX_W + 2);
        }
    }

    #[test]
    fn long_navigation_snippet_keeps_target_near_the_front() {
        let line = concat!(
            "from pipeline.shared.script_v4 import CharacterImageRequest, ",
            "GeneratedCharacterImage, parse_script_v4, TargetSpec"
        );
        let target_start = line.find("parse_script_v4").unwrap();

        let snippet = snippet_from_line(
            line.as_bytes(),
            target_start..target_start + "parse_script_v4".len(),
        );

        assert!(snippet.text.starts_with('…'));
        assert_eq!(
            &snippet.text[snippet.match_range.clone()],
            "parse_script_v4"
        );
        assert!(
            snippet.text[..snippet.match_range.start].chars().count()
                <= SNIPPET_LEADING_CONTEXT + 1
        );
        assert!(snippet.text.chars().count() <= SNIPPET_MAX_W + 2);
    }

    #[test]
    fn short_navigation_snippet_preserves_trimmed_line() {
        let line = "    let value = parse_script_v4();";
        let target_start = line.find("parse_script_v4").unwrap();

        let snippet = snippet_from_line(
            line.as_bytes(),
            target_start..target_start + "parse_script_v4".len(),
        );

        assert_eq!(snippet.text, "let value = parse_script_v4();");
        assert_eq!(&snippet.text[snippet.match_range], "parse_script_v4");
    }
}
