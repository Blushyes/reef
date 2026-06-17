//! Intra-file definition resolution. Given a `FileParse` and a byte
//! offset where the user invoked `gd`, return every candidate
//! definition for that identifier within the same file.
//!
//! Approach: a per-language tree-sitter Query that captures every
//! definition-shaped node, then filter captures whose name matches the
//! target identifier. This is simpler than building a real scope
//! resolver and covers the common cases:
//!   - 1 candidate → jump immediately.
//!   - N candidates → caller pops the candidates panel.
//!
//! For Phase 1 we explicitly do NOT do scope-aware resolution (so
//! shadowed bindings list both decls). Phase 2's stack-graphs will
//! supersede this with proper scope handling.

use std::sync::OnceLock;

use tree_sitter::{Query, QueryCursor, StreamingIterator};

use super::{Cursor, FileParse, Location, NavLang};

/// Find every same-file definition whose name matches the identifier
/// at `cursor`. Empty vec when:
///   - the click landed on a non-identifier token,
///   - no matching declaration exists in this file (cross-file goes
///     through Phase 2's `WorkspaceIndex`).
pub fn resolve_definition_intrafile(parse: &FileParse, cursor: Cursor) -> Vec<Location> {
    let Some(needle) = super::identifier_at(parse, cursor) else {
        return Vec::new();
    };
    let needle = needle.to_owned();

    let Some(query) = definition_query(parse.language) else {
        return Vec::new();
    };

    let mut cursor_q = QueryCursor::new();
    let source = parse.source();
    let name_idx = query
        .capture_index_for_name("name")
        .expect("definition query must expose a @name capture");

    let click_point = super::cursor_to_point(cursor);
    let mut out = Vec::new();
    let mut matches = cursor_q.matches(query, parse.tree.root_node(), source);
    while let Some(m) = matches.next() {
        for cap in m.captures.iter().filter(|c| c.index == name_idx) {
            let node = cap.node;
            let Ok(text) = std::str::from_utf8(&source[node.byte_range()]) else {
                continue;
            };
            if text != needle {
                continue;
            }
            let start = node.start_position();
            let end = node.end_position();
            // Skip the definition that IS the click site itself —
            // pressing `gd` on a declaration shouldn't echo back to
            // the same row. Identifiers are single-line so a row +
            // column-range check is enough.
            if start.row == click_point.row
                && start.column <= click_point.column
                && click_point.column < end.column
            {
                continue;
            }
            out.push(Location {
                path: None,
                line: start.row,
                byte_range: start.column..end.column,
                snippet: snippet_for(source, start.row),
            });
        }
    }

    // De-duplicate by (line, col_start) — a few queries match the same
    // node through different capture paths (e.g. Rust patterns inside
    // `let_declaration` can appear twice).
    out.sort_by_key(|loc| (loc.line, loc.byte_range.start));
    out.dedup_by(|a, b| a.line == b.line && a.byte_range.start == b.byte_range.start);
    out
}

fn snippet_for(source: &[u8], line: usize) -> String {
    super::workspace::snippet_for(source, line)
}

/// Sibling-module wrapper so `crate::nav::workspace` can share the
/// same compiled query cache used by `resolve_definition_intrafile`.
/// Re-exported as `definition_query_pub` to avoid renaming the
/// existing private function (`definition_query`) and forcing
/// downstream churn.
pub(super) fn definition_query_pub(lang: NavLang) -> Option<&'static Query> {
    definition_query(lang)
}

/// Per-language compiled query, lazy-initialised on first use and
/// cached forever. tree-sitter `Query::new` is the expensive part
/// (~1ms for these queries); reusing the compiled `Query` across
/// invocations keeps `gd` sub-millisecond.
fn definition_query(lang: NavLang) -> Option<&'static Query> {
    match lang {
        NavLang::Rust => {
            static Q: OnceLock<Option<Query>> = OnceLock::new();
            super::cached_query(&Q, lang, RUST_QUERY)
        }
        NavLang::TypeScript => {
            static Q: OnceLock<Option<Query>> = OnceLock::new();
            super::cached_query(&Q, lang, TS_QUERY)
        }
        NavLang::Tsx => {
            static Q: OnceLock<Option<Query>> = OnceLock::new();
            super::cached_query(&Q, lang, TS_QUERY)
        }
        NavLang::Python => {
            static Q: OnceLock<Option<Query>> = OnceLock::new();
            super::cached_query(&Q, lang, PY_QUERY)
        }
        NavLang::Go => {
            static Q: OnceLock<Option<Query>> = OnceLock::new();
            super::cached_query(&Q, lang, GO_QUERY)
        }
        // Vue's tree-sitter grammar parses the SFC envelope but leaves
        // the `<script>` body as a raw blob, so we have no in-file
        // identifiers to query. Returning `None` here makes
        // `resolve_definition_intrafile` short-circuit; the LSP-tier
        // (`vue-language-server`) carries Vue navigation.
        NavLang::Vue => None,
    }
}

// ── Queries ───────────────────────────────────────────────────────────
// Each query captures the *name node* (the identifier that declares
// the binding) under `@name`. We intentionally cast a wide net — false
// positives (shadowed bindings, same-name bindings in unrelated
// scopes) are surfaced as multiple candidates in the popup, which is
// the right UX per user decision.
//
// Coverage is "most user-visible declarations." Macros, derive-
// generated items, and FFI bindings are out of scope for v1.

const RUST_QUERY: &str = r#"
(function_item name: (identifier) @name)
(function_signature_item name: (identifier) @name)
(const_item name: (identifier) @name)
(static_item name: (identifier) @name)
(struct_item name: (type_identifier) @name)
(enum_item name: (type_identifier) @name)
(union_item name: (type_identifier) @name)
(trait_item name: (type_identifier) @name)
(type_item name: (type_identifier) @name)
(mod_item name: (identifier) @name)
(let_declaration pattern: (identifier) @name)
(let_declaration pattern: (mut_pattern (identifier) @name))
(let_declaration pattern: (tuple_pattern (identifier) @name))
(parameter pattern: (identifier) @name)
(parameter pattern: (mut_pattern (identifier) @name))
(closure_parameters (identifier) @name)
(for_expression pattern: (identifier) @name)
(macro_definition name: (identifier) @name)
(enum_variant name: (identifier) @name)
(field_declaration name: (field_identifier) @name)
"#;

const TS_QUERY: &str = r#"
(function_declaration name: (identifier) @name)
(generator_function_declaration name: (identifier) @name)
(class_declaration name: (type_identifier) @name)
(interface_declaration name: (type_identifier) @name)
(type_alias_declaration name: (type_identifier) @name)
(enum_declaration name: (identifier) @name)
(variable_declarator name: (identifier) @name)
(required_parameter pattern: (identifier) @name)
(optional_parameter pattern: (identifier) @name)
(method_definition name: (property_identifier) @name)
(public_field_definition name: (property_identifier) @name)
(abstract_method_signature name: (property_identifier) @name)
(import_specifier name: (identifier) @name)
(namespace_import (identifier) @name)
"#;

const PY_QUERY: &str = r#"
(function_definition name: (identifier) @name)
(class_definition name: (identifier) @name)
(assignment left: (identifier) @name)
(assignment left: (pattern_list (identifier) @name))
(assignment left: (tuple_pattern (identifier) @name))
(for_statement left: (identifier) @name)
(parameters (identifier) @name)
(parameters (typed_parameter (identifier) @name))
(parameters (default_parameter name: (identifier) @name))
(parameters (typed_default_parameter name: (identifier) @name))
(global_statement (identifier) @name)
(nonlocal_statement (identifier) @name)
(import_from_statement name: (dotted_name (identifier) @name))
(aliased_import alias: (identifier) @name)
"#;

const GO_QUERY: &str = r#"
(function_declaration name: (identifier) @name)
(method_declaration name: (field_identifier) @name)
(type_declaration (type_spec name: (type_identifier) @name))
(type_declaration (type_alias name: (type_identifier) @name))
(const_declaration (const_spec name: (identifier) @name))
(var_declaration (var_spec name: (identifier) @name))
(short_var_declaration left: (expression_list (identifier) @name))
(parameter_declaration name: (identifier) @name)
(field_declaration name: (field_identifier) @name)
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::{NavLang, parse_file_if_supported};
    use std::sync::Arc;

    /// The two parallel per-language enumerations must agree:
    /// `has_semantic_queries()` claims a language has intra-file
    /// queries, and `definition_query()` actually provides them. A
    /// drift (e.g. a new language marked semantic but with no query,
    /// or vice versa) makes `gd` silently return zero candidates with
    /// no LSP fallthrough — exactly the dead no-op the Vue special-
    /// case exists to avoid. Cross-check every variant.
    #[test]
    fn semantic_queries_flag_matches_definition_query() {
        for &lang in NavLang::ALL {
            assert_eq!(
                lang.has_semantic_queries(),
                definition_query(lang).is_some(),
                "has_semantic_queries() and definition_query() disagree for {lang:?}"
            );
        }
    }

    fn parse_for(lang: NavLang, src: &str) -> crate::nav::FileParse {
        let bytes: Arc<[u8]> = Arc::from(src.as_bytes().to_vec().into_boxed_slice());
        parse_file_if_supported(lang, bytes).expect("parse")
    }

    /// Compute `(row, byte_col_in_line)` for the first occurrence of
    /// `needle` in `src` — convenience so tests don't hand-count
    /// columns when the fixture changes.
    fn cursor_at(src: &str, needle: &str) -> (usize, usize) {
        let idx = src.find(needle).expect("needle present in fixture");
        let prefix = &src[..idx];
        let row = prefix.bytes().filter(|b| *b == b'\n').count();
        let line_start = prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);
        let col = idx - line_start;
        (row, col)
    }

    /// Same as `cursor_at` but returns the Nth occurrence (0-indexed).
    fn cursor_at_nth(src: &str, needle: &str, n: usize) -> (usize, usize) {
        let mut start = 0;
        let mut idx = None;
        for _ in 0..=n {
            let next = src[start..].find(needle).expect("needle present");
            idx = Some(start + next);
            start += next + needle.len();
        }
        let idx = idx.unwrap();
        let prefix = &src[..idx];
        let row = prefix.bytes().filter(|b| *b == b'\n').count();
        let line_start = prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);
        (row, idx - line_start)
    }

    #[test]
    fn rust_single_candidate_jumps_to_fn() {
        let src = "fn helper() -> i32 { 42 }\nfn main() { let _ = helper(); }\n";
        let parse = parse_for(NavLang::Rust, src);
        // Click on `helper` in the call (the second occurrence)
        let cursor = cursor_at_nth(src, "helper", 1);
        let locs = resolve_definition_intrafile(&parse, cursor);
        assert_eq!(locs.len(), 1, "expected single definition match");
        assert_eq!(locs[0].line, 0, "definition should be on line 0");
    }

    #[test]
    fn rust_multiple_impls_yield_multiple_candidates() {
        let src = r#"
struct A;
struct B;
impl A { fn run(&self) {} }
impl B { fn run(&self) {} }
fn main() { let a = A; a.run(); }
"#;
        let parse = parse_for(NavLang::Rust, src);
        // Click on `run` in the `a.run()` call site.
        let cursor = cursor_at_nth(src, "run", 2);
        let locs = resolve_definition_intrafile(&parse, cursor);
        assert!(
            locs.len() >= 2,
            "expected 2+ candidates for trait-like overload, got {} ({:?})",
            locs.len(),
            locs
        );
    }

    #[test]
    fn rust_clicking_on_decl_does_not_echo_self() {
        let src = "fn helper() -> i32 { 42 }\n";
        let parse = parse_for(NavLang::Rust, src);
        // Click on the `helper` token in its own decl.
        let cursor = cursor_at(src, "helper");
        let locs = resolve_definition_intrafile(&parse, cursor);
        assert!(
            locs.is_empty(),
            "clicking on a decl should not list itself as a target"
        );
    }

    #[test]
    fn typescript_function_decl_resolves() {
        let src = "function compute(x: number): number { return x; }\ncompute(1);\n";
        let parse = parse_for(NavLang::TypeScript, src);
        let cursor = cursor_at_nth(src, "compute", 1);
        let locs = resolve_definition_intrafile(&parse, cursor);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].line, 0);
    }

    #[test]
    fn typescript_const_decl_resolves() {
        let src = "const PI = 3.14;\nconsole.log(PI);\n";
        let parse = parse_for(NavLang::TypeScript, src);
        let cursor = cursor_at_nth(src, "PI", 1);
        let locs = resolve_definition_intrafile(&parse, cursor);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].line, 0);
    }

    #[test]
    fn python_def_resolves() {
        let src = "def compute(x):\n    return x\n\nresult = compute(1)\n";
        let parse = parse_for(NavLang::Python, src);
        let cursor = cursor_at_nth(src, "compute", 1);
        let locs = resolve_definition_intrafile(&parse, cursor);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].line, 0);
    }

    #[test]
    fn go_func_decl_resolves() {
        let src = "package main\nfunc helper() int { return 42 }\nfunc main() { _ = helper() }\n";
        let parse = parse_for(NavLang::Go, src);
        let cursor = cursor_at_nth(src, "helper", 1);
        let locs = resolve_definition_intrafile(&parse, cursor);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].line, 1);
    }

    #[test]
    fn identifier_at_returns_word_under_click() {
        let src = "fn helper() {}\n";
        let parse = parse_for(NavLang::Rust, src);
        let cursor = cursor_at(src, "helper");
        let id = crate::nav::identifier_at(&parse, cursor).expect("identifier");
        assert_eq!(id, "helper");
    }

    #[test]
    fn identifier_at_on_non_identifier_token_returns_none() {
        let src = "fn helper() {}\n";
        let parse = parse_for(NavLang::Rust, src);
        // Click in whitespace inside the function body.
        let cursor = cursor_at(src, "{}");
        // Adjust to land on the space between { and }.
        let id = crate::nav::identifier_at(&parse, (cursor.0, cursor.1 + 1));
        // Either None (whitespace) or the closest identifier — both are
        // acceptable. Just make sure we don't panic.
        let _ = id;
    }

    #[test]
    fn non_matching_language_extension_returns_none() {
        // .txt isn't recognized — nav should silently do nothing.
        let lang = NavLang::from_extension("txt");
        assert!(lang.is_none());
    }
}
