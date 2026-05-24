//! Multi-line variant of [`crate::input_edit`].
//!
//! `dispatch_key_multi` extends the single-line editor vocabulary with
//! line-aware cursor motion (Up/Down across visual lines, Home/End by
//! current line) and treats a bare Enter as a newline insert instead of
//! "submit". Everything else — word-motion, word-delete, paste, char
//! insert with UTF-8 boundary safety — is delegated to the single-line
//! primitives so the two layers stay consistent.
//!
//! Hard-wrap / soft-wrap awareness is intentionally out of scope: the
//! buffer model is "flat string with embedded `\n`", which matches how
//! every multi-line textarea in reef (currently just the Git tab's
//! commit message) actually stores its content. Callers that want
//! visual column tracking on top of wrapped lines need to keep their
//! own "preferred column" state and feed it back here — not yet
//! exercised, so deliberately left to a future caller.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::input_edit::{self, Outcome};

/// Multi-line variant of [`input_edit::dispatch_key`]. Differences:
///
/// - **Bare `Enter`** inserts a literal `\n` (`Outcome::Edited`). Single-line
///   inputs treat Enter as "commit"; multi-line buffers need a way to
///   start a new paragraph. Callers that want a submit shortcut should
///   match `Ctrl+Enter` (or whatever they prefer) BEFORE calling this
///   dispatcher and `return` early.
/// - **`Up`/`Down`** move the cursor to the same byte-column on the
///   previous / next line, clamped to that line's length. Returned as
///   `Outcome::CursorOnly`.
/// - **`Home`/`End`** snap to the start / end of the current line
///   (i.e. between the surrounding `\n`s), not the whole buffer.
/// - **Everything else** flows into `input_edit::dispatch_key` —
///   word-motion, delete, Ctrl+A/E, paste, plain-char insert all
///   behave identically to the single-line path.
pub fn dispatch_key_multi(key: &KeyEvent, text: &mut String, cursor: &mut usize) -> Outcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // All multi-line specifics accept `!ctrl` (Shift / Alt are OK).
    // Without Shift-tolerance, Shift+Up / Shift+Down / Shift+Home /
    // Shift+End fall through to the caller as Unhandled, and the
    // outer Git handler's `Up | Char('k') if !ctrl` arm fires
    // `navigate_files` mid-message. Shift+arrow text-selection isn't
    // a thing in this textarea today, so accepting the modifier as
    // equivalent to bare-key motion is the right call.
    match key.code {
        KeyCode::Enter if !ctrl => {
            input_edit::insert_char(text, cursor, '\n');
            Outcome::Edited
        }
        KeyCode::Up if !ctrl => {
            move_line_vertical(text, cursor, -1);
            Outcome::CursorOnly
        }
        KeyCode::Down if !ctrl => {
            move_line_vertical(text, cursor, 1);
            Outcome::CursorOnly
        }
        KeyCode::Home if !ctrl => {
            *cursor = line_start_of(text, *cursor);
            Outcome::CursorOnly
        }
        KeyCode::End if !ctrl => {
            *cursor = line_end_of(text, *cursor);
            Outcome::CursorOnly
        }
        // Defensive `\r` filter. Bracketed-paste payloads go through
        // the paste handler which strips CR explicitly, but some
        // terminals deliver Char('\r') as a literal key event on
        // raw paste; without this guard the buffer ingests CR and
        // `git commit -F -` surfaces `^M` in `git log`.
        KeyCode::Char('\r') => Outcome::CursorOnly,
        _ => input_edit::dispatch_key(key, text, cursor),
    }
}

/// Byte offset of the `\n` that starts the line containing `cursor`,
/// or 0 if `cursor` is on the first line. The returned offset points
/// AT the first character of the current line (immediately after the
/// preceding `\n`, or position 0 for the first line).
pub fn line_start_of(text: &str, cursor: usize) -> usize {
    let clamped = cursor.min(text.len());
    text[..clamped].rfind('\n').map(|p| p + 1).unwrap_or(0)
}

/// Byte offset of the `\n` that ends the line containing `cursor`, or
/// `text.len()` if `cursor` is on the last (unterminated) line. Points
/// AT the trailing `\n`, not past it — same convention as
/// `line_start_of` returning the first char of the next line.
pub fn line_end_of(text: &str, cursor: usize) -> usize {
    let clamped = cursor.min(text.len());
    text[clamped..]
        .find('\n')
        .map(|p| clamped + p)
        .unwrap_or(text.len())
}

/// Move `cursor` `delta` visual lines (negative = up, positive =
/// down), preserving the byte-column within the line where possible.
/// Clamps when the destination line is shorter — Up at the first
/// line goes to column 0; Down at the last line goes to end of file.
///
/// The "byte-column" tracking here is intentionally coarse: a single
/// CJK / emoji code-point counts as one byte-column unit equal to its
/// UTF-8 length, not its visual width. That mirrors how every
/// single-line `input_edit` op already treats byte offsets, and
/// matches what the textarea renderer sees when laying out lines.
/// Width-aware column tracking would require a per-line wrap pass —
/// not worth the complexity until a caller actually needs it.
pub fn move_line_vertical(text: &str, cursor: &mut usize, delta: i32) {
    if delta == 0 || text.is_empty() {
        return;
    }
    let clamped = (*cursor).min(text.len());
    let line_start = line_start_of(text, clamped);
    let column = clamped - line_start;

    if delta < 0 {
        // Move to the previous line — `line_start - 1` lands on the
        // `\n` that ends the previous line; one more `rfind` gives
        // that line's start.
        if line_start == 0 {
            // Already on the first line: snap to start (mirrors how
            // most editors behave when Up has nowhere to go).
            *cursor = 0;
            return;
        }
        let prev_line_end = line_start - 1; // position of the '\n'
        let prev_line_start = line_start_of(text, prev_line_end);
        let prev_line_len = prev_line_end - prev_line_start;
        let target = prev_line_start + column.min(prev_line_len);
        *cursor = prev_safe_boundary(text, target);
    } else {
        // Move to the next line.
        let line_end = line_end_of(text, clamped);
        if line_end == text.len() {
            // Already on the last line: snap to EOF.
            *cursor = text.len();
            return;
        }
        let next_line_start = line_end + 1; // skip the '\n'
        let next_line_end = line_end_of(text, next_line_start);
        let next_line_len = next_line_end - next_line_start;
        let target = next_line_start + column.min(next_line_len);
        *cursor = prev_safe_boundary(text, target);
    }
}

/// Snap to the nearest valid UTF-8 char boundary at or BEFORE
/// `target`. Defends `move_line_vertical` against the case where the
/// byte-column from the source line lands mid-codepoint on the
/// destination line (e.g. source "abc" cursor at col 2 → target line
/// "你好" col 2 → byte 2 is mid-`你`).
///
/// The backward direction is deliberate: a byte-column of N means
/// "after N source bytes". Walking *forward* on a CJK destination
/// would overshoot — the cursor lands one codepoint to the right of
/// the visual column the user expects. Walking backward keeps the
/// caret at-or-before the intended visual column, matching how
/// most editors (vim, emacs) handle the equivalent navigation.
fn prev_safe_boundary(text: &str, target: usize) -> usize {
    let mut p = target.min(text.len());
    while p > 0 && !text.is_char_boundary(p) {
        p -= 1;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn enter_inserts_newline() {
        let mut t = "ab".to_string();
        let mut c = 1;
        let outcome = dispatch_key_multi(&k(KeyCode::Enter, KeyModifiers::NONE), &mut t, &mut c);
        assert_eq!(outcome, Outcome::Edited);
        assert_eq!(t, "a\nb");
        assert_eq!(c, 2);
    }

    #[test]
    fn shift_enter_inserts_newline() {
        // Regression: Shift+Enter is the universal "soft newline"
        // convention in IM / VSCode / GitHub. Before the gate was
        // widened from `no_mods` to `!ctrl`, this returned Unhandled
        // and the outer Git handler interpreted Enter as "open file
        // in $EDITOR" — a destructive surprise mid-typing.
        let mut t = "ab".to_string();
        let mut c = 1;
        let outcome = dispatch_key_multi(&k(KeyCode::Enter, KeyModifiers::SHIFT), &mut t, &mut c);
        assert_eq!(outcome, Outcome::Edited);
        assert_eq!(t, "a\nb");
        assert_eq!(c, 2);
    }

    #[test]
    fn alt_enter_inserts_newline() {
        // Same regression class as Shift+Enter: Alt+Enter pre-fix
        // fell through to the outer Git handler.
        let mut t = "ab".to_string();
        let mut c = 1;
        let outcome = dispatch_key_multi(&k(KeyCode::Enter, KeyModifiers::ALT), &mut t, &mut c);
        assert_eq!(outcome, Outcome::Edited);
        assert_eq!(t, "a\nb");
    }

    #[test]
    fn ctrl_enter_unhandled_so_caller_can_submit() {
        // The one Enter variant we deliberately don't insert: Ctrl+Enter
        // is the textarea-submit convention (handled by the caller).
        let mut t = "msg".to_string();
        let mut c = 3;
        let outcome = dispatch_key_multi(
            &k(KeyCode::Enter, KeyModifiers::CONTROL),
            &mut t,
            &mut c,
        );
        assert_eq!(outcome, Outcome::Unhandled);
        assert_eq!(t, "msg", "buffer untouched");
    }

    #[test]
    fn home_snaps_to_line_start_not_buffer_start() {
        let mut t = "first\nsecond".to_string();
        let mut c = "first\nsecond".len(); // end of second line
        dispatch_key_multi(&k(KeyCode::Home, KeyModifiers::NONE), &mut t, &mut c);
        assert_eq!(c, 6, "cursor on start of 'second'");
    }

    #[test]
    fn end_snaps_to_line_end_not_buffer_end() {
        let mut t = "first\nsecond".to_string();
        let mut c = 0; // start of first line
        dispatch_key_multi(&k(KeyCode::End, KeyModifiers::NONE), &mut t, &mut c);
        assert_eq!(c, 5, "cursor on the '\\n' that ends line 1");
    }

    #[test]
    fn shift_up_moves_cursor_like_bare_up() {
        // Regression: Shift+Up previously failed the `no_mods` gate,
        // fell through as Unhandled, and the outer Git handler's
        // `Up | Char('k') if !ctrl` arm navigated the files list
        // instead of the textarea cursor. With the widened `!ctrl`
        // gate, Shift+Up behaves identically to bare Up.
        let mut t = "alpha\nbeta\ngamma".to_string();
        let mut c = "alpha\nbeta\nga".len(); // column 2 on "gamma"
        let outcome = dispatch_key_multi(&k(KeyCode::Up, KeyModifiers::SHIFT), &mut t, &mut c);
        assert_eq!(outcome, Outcome::CursorOnly);
        assert_eq!(c, 8, "should land at column 2 on 'beta'");
    }

    #[test]
    fn up_moves_cursor_to_previous_line_same_column() {
        let mut t = "alpha\nbeta\ngamma".to_string();
        let mut c = "alpha\nbeta\nga".len(); // column 2 on line "gamma"
        dispatch_key_multi(&k(KeyCode::Up, KeyModifiers::NONE), &mut t, &mut c);
        // Should land at column 2 on "beta" — byte offset 6 + 2 = 8.
        assert_eq!(c, 8);
    }

    #[test]
    fn down_moves_cursor_to_next_line_same_column() {
        let mut t = "alpha\nbeta\ngamma".to_string();
        let mut c = 2; // column 2 on line "alpha"
        dispatch_key_multi(&k(KeyCode::Down, KeyModifiers::NONE), &mut t, &mut c);
        // Should land at column 2 on "beta" — byte offset 6 + 2 = 8.
        assert_eq!(c, 8);
    }

    #[test]
    fn up_clamps_to_shorter_previous_line() {
        let mut t = "x\nyyyyyy".to_string();
        let mut c = "x\nyyy".len(); // column 3 on line "yyyyyy"
        dispatch_key_multi(&k(KeyCode::Up, KeyModifiers::NONE), &mut t, &mut c);
        // "x" has only 1 char — column clamps to 1 (end of line).
        assert_eq!(c, 1);
    }

    #[test]
    fn up_on_first_line_goes_to_buffer_start() {
        let mut t = "alpha".to_string();
        let mut c = 3;
        dispatch_key_multi(&k(KeyCode::Up, KeyModifiers::NONE), &mut t, &mut c);
        assert_eq!(c, 0);
    }

    #[test]
    fn down_on_last_line_goes_to_buffer_end() {
        let mut t = "alpha".to_string();
        let mut c = 1;
        dispatch_key_multi(&k(KeyCode::Down, KeyModifiers::NONE), &mut t, &mut c);
        assert_eq!(c, 5);
    }

    #[test]
    fn up_lands_on_char_boundary_with_cjk() {
        let mut t = "abc\n你好".to_string();
        let mut c = "abc\n你".len(); // 3 + 1 + 3 = 7; column = 3 on line "你好"
        dispatch_key_multi(&k(KeyCode::Up, KeyModifiers::NONE), &mut t, &mut c);
        // Source column 3 → on "abc", that's the end of line.
        assert_eq!(c, 3);
        assert!(t.is_char_boundary(c));
    }

    #[test]
    fn down_lands_on_char_boundary_with_cjk() {
        let mut t = "abc\n你好".to_string();
        let mut c = 2; // column 2 on line "abc"
        dispatch_key_multi(&k(KeyCode::Down, KeyModifiers::NONE), &mut t, &mut c);
        // Column 2 in "你好" would be mid-codepoint of '你' (3-byte);
        // prev_safe_boundary snaps BACKWARD to byte 0 (before '你')
        // so the caret stays at-or-before the source's visual column
        // instead of overshooting to byte 3 (after '你').
        assert_eq!(c, "abc\n".len());
        assert!(t.is_char_boundary(c));
    }

    #[test]
    fn other_keys_fall_through_to_single_line_dispatch() {
        let mut t = "hello world".to_string();
        let mut c = 11;
        // Ctrl+W should still word-delete via the single-line dispatcher.
        let outcome = dispatch_key_multi(
            &k(KeyCode::Char('w'), KeyModifiers::CONTROL),
            &mut t,
            &mut c,
        );
        assert_eq!(outcome, Outcome::Edited);
        assert_eq!(t, "hello ");
    }

    #[test]
    fn ctrl_enter_falls_through_for_caller_to_handle() {
        // Superseded by `ctrl_enter_unhandled_so_caller_can_submit`
        // above — kept as a docs-style regression sentinel.
        let mut t = "msg".to_string();
        let mut c = 3;
        let outcome = dispatch_key_multi(
            &k(KeyCode::Enter, KeyModifiers::CONTROL),
            &mut t,
            &mut c,
        );
        assert_eq!(outcome, Outcome::Unhandled);
        assert_eq!(t, "msg");
        assert_eq!(c, 3);
    }

    #[test]
    fn line_start_of_first_line() {
        assert_eq!(line_start_of("foo\nbar", 2), 0);
    }

    #[test]
    fn line_start_of_second_line() {
        assert_eq!(line_start_of("foo\nbar", 5), 4); // 'b' position
    }

    #[test]
    fn line_end_of_last_line_unterminated() {
        assert_eq!(line_end_of("foo\nbar", 5), 7); // text.len()
    }

    #[test]
    fn line_end_of_middle_line() {
        assert_eq!(line_end_of("foo\nbar\nbaz", 5), 7); // the '\n' after "bar"
    }
}
