//! Code navigation engine. Tree-sitter is the always-on tier here:
//! it produces the user-facing answer to `gd` / Ctrl+click within ~5ms
//! per file and stays bundled into the binary (no runtime download, no
//! external language server) so SSH mode and offline use both work.
//!
//! Phase 2 will layer cross-file `stack-graphs` queries on top of the
//! same `FileParse` cache. Phase 3 will optionally spawn rust-analyzer
//! to *refine* tree-sitter answers silently into a cache — it never
//! moves the user's cursor itself; the next `gd` at the same position
//! consults the cache first. SSH mode bypasses the LSP tier entirely
//! (see `Backend::is_remote`).
//!
//! The plan file `/Users/pan/.claude/plans/1-2-prancy-whisper.md`
//! documents the full three-phase rollout.

use std::ops::Range;
use std::sync::Arc;

pub mod grammars;
pub mod intrafile;
pub mod lsp;
pub mod workspace;

pub use grammars::{LangProfile, LspProfile, NavLang};
pub use lsp::{LspBadge, LspClient, LspLocation};
pub use workspace::{SymbolLoc, WorkspaceIndex, build_workspace_index};

/// `(file_line, byte_column_in_line)` — same coordinate system as
/// `mouse_to_file_coord` returns and `preview_selection.active` uses.
/// We pass this directly to tree-sitter as a `Point` (tree-sitter's
/// columns are 0-based UTF-8 byte counts since line start, which is
/// the byte_offset_in_line we already have).
pub type Cursor = (usize, usize);

/// Parsed file plus the source bytes that produced it. Stored inside
/// `PreviewBody::Text.parsed` so its lifetime tracks the preview cache —
/// fs_watcher invalidates the preview, the parse goes with it. No
/// separate LRU.
///
/// Source bytes are held by `Arc` so the preview worker can share them
/// with the text-body line splitter without doubling memory.
#[derive(Debug)]
pub struct FileParse {
    pub language: NavLang,
    pub tree: tree_sitter::Tree,
    pub source: Arc<[u8]>,
}

impl FileParse {
    /// Borrow the source bytes — convenience for callers that need to
    /// slice into a node's range without cloning the Arc.
    pub fn source(&self) -> &[u8] {
        &self.source
    }
}

/// A jump target. `byte_range` is **per-line** — same convention as
/// `MatchHit` and `PreviewHighlight` so they share the highlight
/// pathway in `ui::preview`. Intra-file results omit `path`
/// (caller knows the current file); cross-file results (Phase 2)
/// populate it.
#[derive(Debug, Clone)]
pub struct Location {
    pub path: Option<std::path::PathBuf>,
    pub line: usize,
    /// Per-line byte range of the identifier — `(col_start, col_end)`
    /// in bytes since the line's start. Single-line by construction
    /// (the queries only match name identifiers, which never span
    /// newlines).
    pub byte_range: Range<usize>,
    pub snippet: String,
}

/// Parse `source` for `lang`. Returns `None` if the parser refused
/// the input (corrupt grammar, etc.). This is sub-10ms for files at
/// the preview cap (512KB / 5K lines) on the languages we ship.
///
/// Source ownership: caller passes an `Arc<[u8]>` so the bytes can be
/// shared with the syntect highlight path without duplication.
pub fn parse_file_if_supported(lang: NavLang, source: Arc<[u8]>) -> Option<FileParse> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang.language()).ok()?;
    let tree = parser.parse(&*source, None)?;
    Some(FileParse {
        language: lang,
        tree,
        source,
    })
}

/// Identifier (or identifier-shaped node — `field_identifier`,
/// `type_identifier`, etc.) at `cursor`. Used by `gd` / Ctrl+click to
/// know "what symbol did the user click on" before searching for its
/// definition.
///
/// Returns the identifier *text* as a borrowed slice into the source.
/// Lifetime is tied to `parse`, so callers `.to_owned()` if they need
/// to keep it across an async boundary.
pub fn identifier_at(parse: &FileParse, cursor: Cursor) -> Option<&str> {
    let point = cursor_to_point(cursor);
    let root = parse.tree.root_node();
    // `descendant_for_point_range` with a zero-width range targets the
    // smallest node containing the point. A click between two tokens
    // picks the next-anchored token, matching editor intuition.
    let node = root.descendant_for_point_range(point, point)?;
    if !is_identifier_kind(node.kind()) {
        // Allow one parent hop: clicking on the `.` in `foo.bar` lands
        // on a punctuation node whose parent is a `field_expression`
        // whose `name` field is the identifier. Walk up once.
        let parent = node.parent()?;
        let id = parent
            .child_by_field_name("name")
            .or_else(|| parent.child_by_field_name("field"))?;
        if !is_identifier_kind(id.kind()) {
            return None;
        }
        return std::str::from_utf8(&parse.source[id.byte_range()]).ok();
    }
    std::str::from_utf8(&parse.source[node.byte_range()]).ok()
}

/// Refine-cache / LSP-request key. Position-based — `(lang, path, line,
/// byte_col)` — so two distinct same-named symbols at different cursor
/// positions don't collide on a single cached answer (the prior
/// name-only key returned one definition for every occurrence of a
/// name). `path` is the workdir-relative preview path.
pub fn refine_key(path: &std::path::Path, cursor: Cursor) -> String {
    format!("{}:{}:{}", path.to_string_lossy(), cursor.0, cursor.1)
}

/// Convert a UTF-8 byte column to a UTF-16 code-unit column within a
/// single line. LSP positions default to the UTF-16 encoding, but
/// tree-sitter (and reef's `Cursor`) speak UTF-8 byte columns. Without
/// this conversion, a `gd` on a line containing non-ASCII text before
/// the identifier sends the wrong column and the server resolves the
/// wrong position. `line_bytes` is the line's bytes (no newline);
/// `byte_col` is clamped to the line length.
pub fn byte_col_to_utf16(line_bytes: &[u8], byte_col: usize) -> u32 {
    let mut end = byte_col.min(line_bytes.len());
    // Snap DOWN to a UTF-8 char boundary so the slice never cuts a
    // multi-byte sequence in half (which `from_utf8_lossy` would turn
    // into a U+FFFD, miscounting the UTF-16 offset). This is the
    // `&[u8]` analog of `input_edit_multi::prev_safe_boundary` (which
    // uses `str::is_char_boundary` on a `&str`) — we operate on raw
    // bytes here because the line may not be valid UTF-8. A
    // continuation byte is `0b10xxxxxx`; `end == len` is already a
    // boundary, so the `end < len` guard also keeps the index in range.
    while end > 0 && end < line_bytes.len() && (line_bytes[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    // `end` is now a char boundary. On valid UTF-8 (the common case)
    // `from_utf8_lossy` returns `Cow::Borrowed` — no allocation, no
    // U+FFFD — so the count equals the strict `from_utf8` path; on an
    // already-corrupt source it's the same best-effort fallback. One
    // expression covers both.
    String::from_utf8_lossy(&line_bytes[..end])
        .encode_utf16()
        .count() as u32
}

/// Inverse of `byte_col_to_utf16`: convert a UTF-16 code-unit column
/// (as an LSP position's `character`) back to a UTF-8 byte column within
/// `line_bytes`. Production code converts ranges via `utf16_range_to_byte`
/// (one pass for both endpoints), so this single-column form exists only
/// to back that function's tests as the named inverse of
/// `byte_col_to_utf16` — hence `#[cfg(test)]`. `utf16_col` past the
/// line's end clamps to the line length.
#[cfg(test)]
fn utf16_col_to_byte(line_bytes: &[u8], utf16_col: u32) -> usize {
    let text = String::from_utf8_lossy(line_bytes);
    utf16_col_to_byte_in_str(&text, line_bytes.len(), utf16_col)
}

/// Convert a `start..end` UTF-16 column range to a byte range in one
/// pass — does the `from_utf8_lossy` conversion + char walk once instead
/// of twice (callers resolving an LSP definition's range need both
/// endpoints on the same line). Equivalent to two `utf16_col_to_byte`
/// calls.
pub fn utf16_range_to_byte(line_bytes: &[u8], start: u32, end: u32) -> Range<usize> {
    let text = String::from_utf8_lossy(line_bytes);
    let len = line_bytes.len();
    let b_start = utf16_col_to_byte_in_str(&text, len, start);
    let b_end = utf16_col_to_byte_in_str(&text, len, end);
    // Clamp so a malformed `end < start` range can't produce an inverted
    // Range that panics when used to slice a line.
    b_start..b_end.max(b_start)
}

/// Shared core: walk `text`'s chars accumulating UTF-16 units until
/// `utf16_col`, returning the byte offset there. Operates on the lossy
/// `&str` so an invalid byte counts as one unit (U+FFFD) — symmetric
/// with `byte_col_to_utf16`'s lossy path. `byte_len` is the original
/// slice length, used to clamp.
fn utf16_col_to_byte_in_str(text: &str, byte_len: usize, utf16_col: u32) -> usize {
    let mut units = 0u32;
    for (byte_idx, ch) in text.char_indices() {
        if units >= utf16_col {
            return byte_idx.min(byte_len);
        }
        units += ch.len_utf16() as u32;
    }
    byte_len
}

/// Extract the bytes of `line` (0-based) from `source`, excluding the
/// trailing newline. Used to feed `byte_col_to_utf16` for the LSP
/// position conversion. Returns an empty slice when `line` is out of
/// range.
pub fn line_bytes_at(source: &[u8], line: usize) -> &[u8] {
    let mut start = 0usize;
    let mut current = 0usize;
    while current < line {
        match source[start..].iter().position(|b| *b == b'\n') {
            Some(p) => {
                start += p + 1;
                current += 1;
            }
            None => return &[],
        }
    }
    if start > source.len() {
        return &[];
    }
    let end = source[start..]
        .iter()
        .position(|b| *b == b'\n')
        .map(|p| start + p)
        .unwrap_or(source.len());
    &source[start..end]
}

pub(crate) fn cursor_to_point(cursor: Cursor) -> tree_sitter::Point {
    tree_sitter::Point {
        row: cursor.0,
        column: cursor.1,
    }
}

/// Like `identifier_at`, but also returns the identifier's per-line
/// byte range — used by the Ctrl+hover underline pathway to know what
/// region to mark as clickable. Returns `(line, byte_range)` in the
/// same coordinate system as `mouse_to_file_coord`.
pub fn identifier_range_at(parse: &FileParse, cursor: Cursor) -> Option<(usize, Range<usize>)> {
    let point = cursor_to_point(cursor);
    let root = parse.tree.root_node();
    let node = root.descendant_for_point_range(point, point)?;
    let node = if is_identifier_kind(node.kind()) {
        node
    } else {
        // Same "walk up one level" rule as `identifier_at` — clicking
        // on `.` in `foo.bar` lands on punctuation whose parent has
        // the `name`/`field` field with the actual identifier.
        let parent = node.parent()?;
        let id = parent
            .child_by_field_name("name")
            .or_else(|| parent.child_by_field_name("field"))?;
        if !is_identifier_kind(id.kind()) {
            return None;
        }
        id
    };
    let start = node.start_position();
    let end = node.end_position();
    if start.row != end.row {
        return None;
    }
    Some((start.row, start.column..end.column))
}

/// Shared per-language compiled-query cache. Both the intra-file
/// definition queries (`intrafile`) and the workspace reference queries
/// (`workspace`) keep one `OnceLock<Option<Query>>` per (language,
/// query-kind) and resolve it identically: compile lazily on first use,
/// cache forever, yield `None` for an empty source or a query that fails
/// to compile. Centralizing the get_or_init+compile+as_ref dance keeps
/// the two former copies from drifting (e.g. one panicking on a bad
/// query while the other silently returned `None`). A compile failure on
/// a bundled query surfaces as `None`, which the
/// `semantic_queries_flag_matches_definition_query` test asserts against.
pub(crate) fn cached_query(
    slot: &'static std::sync::OnceLock<Option<tree_sitter::Query>>,
    lang: NavLang,
    source: &str,
) -> Option<&'static tree_sitter::Query> {
    slot.get_or_init(|| {
        if source.is_empty() {
            return None;
        }
        tree_sitter::Query::new(&lang.language(), source).ok()
    })
    .as_ref()
}

fn is_identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "field_identifier"
            | "type_identifier"
            | "shorthand_field_identifier"
            | "property_identifier"
            | "shorthand_property_identifier"
            | "package_identifier"
            | "scoped_identifier"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_col_to_utf16_ascii_is_identity() {
        let line = b"let x = foo();";
        // ASCII: byte column == UTF-16 column.
        assert_eq!(byte_col_to_utf16(line, 8), 8);
    }

    #[test]
    fn byte_col_to_utf16_counts_multibyte_as_fewer_units() {
        // "café x" — 'é' is 2 UTF-8 bytes but 1 UTF-16 unit. The byte
        // column of 'x' is 6 ("caf" + 2-byte é + space = 6), but its
        // UTF-16 column is 5.
        let line = "café x".as_bytes();
        let byte_col = "café ".len(); // 6 bytes
        assert_eq!(byte_col, 6);
        assert_eq!(byte_col_to_utf16(line, byte_col), 5);
    }

    #[test]
    fn byte_col_to_utf16_clamps_past_end() {
        let line = b"ab";
        assert_eq!(byte_col_to_utf16(line, 99), 2);
    }

    #[test]
    fn line_bytes_at_returns_correct_line() {
        let src = b"alpha\nbeta\ngamma";
        assert_eq!(line_bytes_at(src, 0), b"alpha");
        assert_eq!(line_bytes_at(src, 1), b"beta");
        assert_eq!(line_bytes_at(src, 2), b"gamma");
    }

    #[test]
    fn line_bytes_at_out_of_range_is_empty() {
        let src = b"only\n";
        assert_eq!(line_bytes_at(src, 9), b"");
    }

    #[test]
    fn refine_key_is_position_distinct() {
        let p = std::path::Path::new("src/a.rs");
        assert_eq!(refine_key(p, (3, 7)), "src/a.rs:3:7");
        // Same name at a different position → different key (the bug
        // fix: name-only keys collided same-named symbols).
        assert_ne!(refine_key(p, (3, 7)), refine_key(p, (9, 2)));
    }

    #[test]
    fn utf16_col_to_byte_is_inverse_of_byte_col() {
        // ASCII round-trips identically.
        let ascii = b"let x = foo();";
        assert_eq!(utf16_col_to_byte(ascii, 8), 8);
        // "café x" — byte col 6 ↔ UTF-16 col 5 (é is 2 bytes, 1 unit).
        let line = "café x".as_bytes();
        assert_eq!(utf16_col_to_byte(line, 5), 6);
        assert_eq!(byte_col_to_utf16(line, 6), 5);
    }

    #[test]
    fn utf16_col_to_byte_clamps_past_end() {
        let line = b"ab";
        assert_eq!(utf16_col_to_byte(line, 99), 2);
    }

    #[test]
    fn utf16_col_to_byte_handles_astral_plane() {
        // "😀x" — the emoji is 4 UTF-8 bytes and 2 UTF-16 units (surrogate
        // pair). The 'x' is at UTF-16 col 2, byte col 4.
        let line = "😀x".as_bytes();
        assert_eq!(utf16_col_to_byte(line, 2), 4);
        // A column landing inside the surrogate pair resolves to a char
        // boundary (the next char's start), never mid-sequence.
        assert_eq!(utf16_col_to_byte(line, 1), 4);
    }

    #[test]
    fn byte_col_to_utf16_snaps_off_mid_char_boundary() {
        // "é" = 0xC3 0xA9 (2 bytes, 1 UTF-16 unit). A byte_col of 1
        // lands INSIDE the 'é' — must snap down to col 0, not miscount.
        let line = "éx".as_bytes();
        assert_eq!(byte_col_to_utf16(line, 1), 0);
        assert_eq!(byte_col_to_utf16(line, 2), 1); // after é
        assert_eq!(byte_col_to_utf16(line, 3), 2); // after x
    }
}
