//! Single-line text-input editing primitives shared by the quick-open and
//! global-search palettes. Each function takes `(&mut String, &mut usize)`
//! so callers just pass their own query/cursor fields — no trait, no struct,
//! no coupling to either palette's state.
//!
//! Semantics match readline / VSCode input conventions: `backspace` deletes
//! one char, `delete_word_backward` sweeps trailing non-word chars then
//! swallows the word (so `"src/ui/|"` → `"src/"` in one press), `clear`
//! wipes the line. All operations respect UTF-8 char boundaries so the
//! cursor never lands mid-codepoint.
//!
//! `dispatch_key` rolls the whole key-to-op map (cursor motion, deletion,
//! readline aliases, plain-char insert) into one match so callers don't
//! re-implement the same 80-line table per input field. Returns an
//! `Outcome` so the caller can decide whether to fire edit-derived side
//! effects (re-run search, mark dirty) or fall through (unhandled key).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Result of [`dispatch_key`]. `Edited` and `CursorOnly` both mean the
/// key was consumed; `Unhandled` lets the caller try its own arms (for
/// keys that aren't part of the text-input vocabulary, e.g. Esc, Tab,
/// list navigation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Buffer content changed (insert, delete, clear, …).
    Edited,
    /// Recognized cursor-motion key; buffer untouched.
    CursorOnly,
    /// Not a text-input key. Caller should match it against its own
    /// per-field handlers.
    Unhandled,
}

/// Apply `key` to `text` / `cursor` using the readline / VSCode
/// conventions documented above. Centralises the ~50-line key table
/// previously inlined in `input::handle_key_search_find_input` and
/// `input::handle_key_search_replace_input` (90% identical). Caller
/// invokes any edit-derived side effect (e.g. `mark_query_edited`)
/// when the result is `Outcome::Edited`.
pub fn dispatch_key(key: &KeyEvent, text: &mut String, cursor: &mut usize) -> Outcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        // ── Cursor motion ──
        KeyCode::Left if alt || ctrl => {
            move_cursor_word_backward(text, cursor);
            Outcome::CursorOnly
        }
        KeyCode::Right if alt || ctrl => {
            move_cursor_word_forward(text, cursor);
            Outcome::CursorOnly
        }
        KeyCode::Left => {
            move_cursor(text, cursor, -1);
            Outcome::CursorOnly
        }
        KeyCode::Right => {
            move_cursor(text, cursor, 1);
            Outcome::CursorOnly
        }
        KeyCode::Home => {
            *cursor = 0;
            Outcome::CursorOnly
        }
        KeyCode::End => {
            *cursor = text.len();
            Outcome::CursorOnly
        }
        KeyCode::Char('a') if ctrl => {
            *cursor = 0;
            Outcome::CursorOnly
        }
        KeyCode::Char('e') if ctrl => {
            *cursor = text.len();
            Outcome::CursorOnly
        }
        KeyCode::Char('b') if alt => {
            move_cursor_word_backward(text, cursor);
            Outcome::CursorOnly
        }
        KeyCode::Char('f') if alt => {
            move_cursor_word_forward(text, cursor);
            Outcome::CursorOnly
        }

        // ── Edit ──
        KeyCode::Backspace if alt || ctrl => {
            delete_word_backward(text, cursor);
            Outcome::Edited
        }
        KeyCode::Char('w') if ctrl => {
            delete_word_backward(text, cursor);
            Outcome::Edited
        }
        KeyCode::Char('u') if ctrl => {
            clear(text, cursor);
            Outcome::Edited
        }
        KeyCode::Backspace => {
            backspace(text, cursor);
            Outcome::Edited
        }
        KeyCode::Delete if alt || ctrl => {
            delete_word_forward(text, cursor);
            Outcome::Edited
        }
        KeyCode::Delete => {
            delete_char_forward(text, cursor);
            Outcome::Edited
        }
        KeyCode::Char('d') if alt => {
            delete_word_forward(text, cursor);
            Outcome::Edited
        }
        KeyCode::Char(c) if !ctrl => {
            insert_char(text, cursor, c);
            Outcome::Edited
        }
        _ => Outcome::Unhandled,
    }
}

pub fn insert_char(text: &mut String, cursor: &mut usize, c: char) {
    text.insert(*cursor, c);
    *cursor += c.len_utf8();
}

/// Insert a bracketed-paste payload into a single-line `(text, cursor)`
/// buffer, dropping CR/LF so a multi-line clipboard can't leave
/// invisible characters in the prompt. Returns `true` when at least one
/// char actually landed.
///
/// Filters into a temporary `String` then does a single `insert_str`
/// instead of per-char `String::insert` — an N-char paste at position P
/// becomes O(N+L) bytes moved rather than O(N·(L−P)) per-char memmoves.
/// Bracketed-paste payloads can be MB-scale (large clipboards), so this
/// matters even though the typical case is small.
pub fn paste_single_line(s: &str, text: &mut String, cursor: &mut usize) -> bool {
    let filtered: String = s.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    if filtered.is_empty() {
        return false;
    }
    text.insert_str(*cursor, &filtered);
    *cursor += filtered.len();
    true
}

pub fn backspace(text: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let prev = prev_char_boundary(text, *cursor);
    text.replace_range(prev..*cursor, "");
    *cursor = prev;
}

/// Delete the word immediately before the cursor. A "word" is a run of
/// alphanumeric + `_` chars; any trailing non-word chars (whitespace, `/`,
/// `.`, `-`) are swept first so deleting `"src/ui/|"` once lands on
/// `"src/"`. No-op at cursor 0.
pub fn delete_word_backward(text: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    // Char-by-char not byte-by-byte — query may contain CJK/emoji.
    let chars: Vec<(usize, char)> = text[..*cursor].char_indices().collect();
    let mut i = chars.len();

    // Phase 1: sweep trailing non-word chars.
    while i > 0 && !is_word_char(chars[i - 1].1) {
        i -= 1;
    }
    // Phase 2: swallow the word.
    while i > 0 && is_word_char(chars[i - 1].1) {
        i -= 1;
    }

    let start = chars.get(i).map(|&(b, _)| b).unwrap_or(0);
    text.replace_range(start..*cursor, "");
    *cursor = start;
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Wipe the whole query and reset the cursor. Bound to Ctrl+U — readline's
/// "kill to beginning" collapses to "clear everything" in a single-line
/// input, which is the more useful operation for a palette.
pub fn clear(text: &mut String, cursor: &mut usize) {
    text.clear();
    *cursor = 0;
}

pub fn move_cursor(text: &str, cursor: &mut usize, delta: i32) {
    if delta < 0 {
        if *cursor == 0 {
            return;
        }
        *cursor = prev_char_boundary(text, *cursor);
    } else {
        if *cursor >= text.len() {
            return;
        }
        *cursor = next_char_boundary(text, *cursor);
    }
}

/// Move cursor to the start of the previous word. Mirror of
/// `delete_word_backward` but non-destructive: sweeps trailing non-word
/// chars then skips over the word. Bound to Alt+Left / Ctrl+Left.
pub fn move_cursor_word_backward(text: &str, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let chars: Vec<(usize, char)> = text[..*cursor].char_indices().collect();
    let mut i = chars.len();
    while i > 0 && !is_word_char(chars[i - 1].1) {
        i -= 1;
    }
    while i > 0 && is_word_char(chars[i - 1].1) {
        i -= 1;
    }
    *cursor = chars.get(i).map(|&(b, _)| b).unwrap_or(0);
}

/// Move cursor to the end of the next word. Symmetric with
/// `move_cursor_word_backward`: skips leading non-word chars then swallows
/// the word. Bound to Alt+Right / Ctrl+Right.
pub fn move_cursor_word_forward(text: &str, cursor: &mut usize) {
    if *cursor >= text.len() {
        return;
    }
    let chars: Vec<(usize, char)> = text[*cursor..].char_indices().collect();
    let mut i = 0;
    while i < chars.len() && !is_word_char(chars[i].1) {
        i += 1;
    }
    while i < chars.len() && is_word_char(chars[i].1) {
        i += 1;
    }
    *cursor = if i < chars.len() {
        *cursor + chars[i].0
    } else {
        text.len()
    };
}

/// Delete the character at `cursor` (forward delete). Bound to the Delete
/// key. No-op at end of string.
pub fn delete_char_forward(text: &mut String, cursor: &mut usize) {
    if *cursor >= text.len() {
        return;
    }
    let next = next_char_boundary(text, *cursor);
    text.replace_range(*cursor..next, "");
    // Cursor stays put — the char to the right slid over to sit under it.
}

/// Delete the word to the right of the cursor. Mirror of
/// `delete_word_backward`: skip leading non-word chars, swallow the word.
/// Bound to Alt+Delete / Ctrl+Delete.
pub fn delete_word_forward(text: &mut String, cursor: &mut usize) {
    if *cursor >= text.len() {
        return;
    }
    let chars: Vec<(usize, char)> = text[*cursor..].char_indices().collect();
    let mut i = 0;
    while i < chars.len() && !is_word_char(chars[i].1) {
        i += 1;
    }
    while i < chars.len() && is_word_char(chars[i].1) {
        i += 1;
    }
    let end = if i < chars.len() {
        *cursor + chars[i].0
    } else {
        text.len()
    };
    text.replace_range(*cursor..end, "");
    // Cursor stays; the tail slid left.
}

pub fn prev_char_boundary(s: &str, offset: usize) -> usize {
    s[..offset]
        .char_indices()
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

pub fn next_char_boundary(s: &str, offset: usize) -> usize {
    s[offset..]
        .chars()
        .next()
        .map(|c| offset + c.len_utf8())
        .unwrap_or(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(q: &str, c: usize) -> (String, usize) {
        (q.to_string(), c)
    }

    #[test]
    fn insert_and_backspace_roundtrip() {
        let (mut q, mut c) = state("", 0);
        insert_char(&mut q, &mut c, 'h');
        insert_char(&mut q, &mut c, 'i');
        assert_eq!(q, "hi");
        assert_eq!(c, 2);
        backspace(&mut q, &mut c);
        assert_eq!(q, "h");
        assert_eq!(c, 1);
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let (mut q, mut c) = state("", 0);
        backspace(&mut q, &mut c);
        assert_eq!(q, "");
        assert_eq!(c, 0);
    }

    #[test]
    fn cursor_moves_respect_char_boundaries() {
        let (mut q, mut c) = state("a你b", "a你b".len());
        move_cursor(&q, &mut c, -1);
        assert_eq!(c, 4); // back over 'b'
        move_cursor(&q, &mut c, -1);
        assert_eq!(c, 1); // back over '你' (3 bytes)
        move_cursor(&q, &mut c, 1);
        assert_eq!(c, 4);
        let _ = &mut q;
    }

    #[test]
    fn delete_word_backward_at_start_is_noop() {
        let (mut q, mut c) = state("", 0);
        delete_word_backward(&mut q, &mut c);
        assert_eq!(q, "");
        assert_eq!(c, 0);
    }

    #[test]
    fn delete_word_backward_consumes_one_word() {
        let (mut q, mut c) = state("hello world", "hello world".len());
        delete_word_backward(&mut q, &mut c);
        assert_eq!(q, "hello ");
        assert_eq!(c, 6);
    }

    #[test]
    fn delete_word_backward_sweeps_trailing_separators() {
        let (mut q, mut c) = state("src/ui/", "src/ui/".len());
        delete_word_backward(&mut q, &mut c);
        assert_eq!(q, "src/");
        assert_eq!(c, 4);
    }

    #[test]
    fn delete_word_backward_handles_cjk() {
        let (mut q, mut c) = state("测试 文件", "测试 文件".len());
        delete_word_backward(&mut q, &mut c);
        assert_eq!(q, "测试 ");
        assert_eq!(c, "测试 ".len());
    }

    #[test]
    fn delete_word_backward_respects_midquery_cursor() {
        let (mut q, mut c) = state("foo bar baz", 7);
        delete_word_backward(&mut q, &mut c);
        assert_eq!(q, "foo  baz");
        assert_eq!(c, 4);
    }

    #[test]
    fn clear_wipes_all() {
        let (mut q, mut c) = state("anything", 4);
        clear(&mut q, &mut c);
        assert_eq!(q, "");
        assert_eq!(c, 0);
    }

    #[test]
    fn move_cursor_word_backward_jumps_over_word() {
        let (q, mut c) = state("foo bar baz", "foo bar baz".len());
        move_cursor_word_backward(&q, &mut c);
        assert_eq!(c, 8); // start of "baz"
        move_cursor_word_backward(&q, &mut c);
        assert_eq!(c, 4); // start of "bar"
        move_cursor_word_backward(&q, &mut c);
        assert_eq!(c, 0); // start of "foo"
        move_cursor_word_backward(&q, &mut c);
        assert_eq!(c, 0); // no-op at start
    }

    #[test]
    fn move_cursor_word_backward_sweeps_trailing_separators() {
        let (q, mut c) = state("src/ui/panel", "src/ui/panel".len());
        move_cursor_word_backward(&q, &mut c);
        assert_eq!(c, 7); // start of "panel"
        move_cursor_word_backward(&q, &mut c);
        assert_eq!(c, 4); // start of "ui"
    }

    #[test]
    fn move_cursor_word_forward_jumps_over_word() {
        let (q, mut c) = state("foo bar baz", 0);
        move_cursor_word_forward(&q, &mut c);
        assert_eq!(c, 3); // end of "foo"
        move_cursor_word_forward(&q, &mut c);
        assert_eq!(c, 7); // end of "bar"
        move_cursor_word_forward(&q, &mut c);
        assert_eq!(c, 11); // end of "baz" / text end
        move_cursor_word_forward(&q, &mut c);
        assert_eq!(c, 11); // no-op at end
    }

    #[test]
    fn move_cursor_word_forward_skips_leading_separators() {
        let (q, mut c) = state("  foo bar", 0);
        move_cursor_word_forward(&q, &mut c);
        assert_eq!(c, 5); // end of "foo"
    }

    #[test]
    fn delete_char_forward_removes_next_char() {
        let (mut q, mut c) = state("abc", 1);
        delete_char_forward(&mut q, &mut c);
        assert_eq!(q, "ac");
        assert_eq!(c, 1); // cursor stays
    }

    #[test]
    fn delete_char_forward_at_end_is_noop() {
        let (mut q, mut c) = state("abc", 3);
        delete_char_forward(&mut q, &mut c);
        assert_eq!(q, "abc");
        assert_eq!(c, 3);
    }

    #[test]
    fn delete_char_forward_handles_cjk() {
        let (mut q, mut c) = state("a你b", 1); // cursor between 'a' and '你'
        delete_char_forward(&mut q, &mut c);
        assert_eq!(q, "ab");
        assert_eq!(c, 1);
    }

    #[test]
    fn delete_word_forward_consumes_one_word() {
        let (mut q, mut c) = state("foo bar baz", 0);
        delete_word_forward(&mut q, &mut c);
        assert_eq!(q, " bar baz");
        assert_eq!(c, 0);
    }

    #[test]
    fn delete_word_forward_sweeps_leading_separators() {
        // Cursor on the space between foo and bar; one press kills the space
        // AND "bar".
        let (mut q, mut c) = state("foo bar baz", 3);
        delete_word_forward(&mut q, &mut c);
        assert_eq!(q, "foo baz");
        assert_eq!(c, 3);
    }

    #[test]
    fn delete_word_forward_at_end_is_noop() {
        let (mut q, mut c) = state("foo", 3);
        delete_word_forward(&mut q, &mut c);
        assert_eq!(q, "foo");
        assert_eq!(c, 3);
    }

    #[test]
    fn paste_single_line_inserts_at_cursor_and_advances() {
        let (mut t, mut c) = state("ab", 1);
        assert!(paste_single_line("XY", &mut t, &mut c));
        assert_eq!(t, "aXYb");
        assert_eq!(c, 3);
    }

    #[test]
    fn paste_single_line_drops_crlf() {
        // Multi-line payload (Windows clipboard) flattens into a single
        // run with no embedded line terminators.
        let (mut t, mut c) = state("", 0);
        assert!(paste_single_line("foo\r\nbar\nbaz", &mut t, &mut c));
        assert_eq!(t, "foobarbaz");
        assert_eq!(c, 9);
    }

    #[test]
    fn paste_single_line_returns_false_for_empty_or_pure_newlines() {
        let (mut t, mut c) = state("keep", 4);
        assert!(!paste_single_line("", &mut t, &mut c));
        assert!(!paste_single_line("\n\r\n", &mut t, &mut c));
        assert_eq!(t, "keep");
        assert_eq!(c, 4);
    }

    #[test]
    fn paste_single_line_preserves_utf8_at_cursor() {
        // Insert into a string with multi-byte chars on either side of
        // the cursor — verifies the byte-offset cursor lands on a
        // codepoint boundary after the insert.
        let s = "你好";
        let mid = s.char_indices().nth(1).map(|(i, _)| i).unwrap(); // between '你' and '好'
        let (mut t, mut c) = state(s, mid);
        assert!(paste_single_line("X", &mut t, &mut c));
        assert_eq!(t, "你X好");
        assert_eq!(c, mid + 1);
    }
}
