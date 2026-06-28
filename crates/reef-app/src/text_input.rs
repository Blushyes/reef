#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEditOutcome {
    Edited,
    CursorOnly,
    Rejected,
    Unhandled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEditOp {
    MoveLeft,
    MoveRight,
    MoveWordLeft,
    MoveWordRight,
    MoveStart,
    MoveEnd,
    DeleteBackward,
    DeleteWordBackward,
    DeleteForward,
    DeleteWordForward,
    Clear,
    InsertChar(char),
    InsertNewline,
    MoveLineUp,
    MoveLineDown,
    MoveLineStart,
    MoveLineEnd,
    Consume,
}

pub fn apply_single_line_op(
    op: TextEditOp,
    text: &mut String,
    cursor: &mut usize,
) -> TextEditOutcome {
    apply_single_line_op_filtered(op, text, cursor, |_| true)
}

pub fn apply_single_line_op_filtered(
    op: TextEditOp,
    text: &mut String,
    cursor: &mut usize,
    accept_char: impl Fn(char) -> bool,
) -> TextEditOutcome {
    let pre_len = text.len();
    let outcome = match op {
        TextEditOp::MoveLeft => {
            move_cursor(text, cursor, -1);
            TextEditOutcome::CursorOnly
        }
        TextEditOp::MoveRight => {
            move_cursor(text, cursor, 1);
            TextEditOutcome::CursorOnly
        }
        TextEditOp::MoveWordLeft => {
            move_cursor_word_backward(text, cursor);
            TextEditOutcome::CursorOnly
        }
        TextEditOp::MoveWordRight => {
            move_cursor_word_forward(text, cursor);
            TextEditOutcome::CursorOnly
        }
        TextEditOp::MoveStart => {
            *cursor = 0;
            TextEditOutcome::CursorOnly
        }
        TextEditOp::MoveEnd => {
            *cursor = text.len();
            TextEditOutcome::CursorOnly
        }
        TextEditOp::DeleteBackward => {
            backspace(text, cursor);
            TextEditOutcome::Edited
        }
        TextEditOp::DeleteWordBackward => {
            delete_word_backward(text, cursor);
            TextEditOutcome::Edited
        }
        TextEditOp::DeleteForward => {
            delete_char_forward(text, cursor);
            TextEditOutcome::Edited
        }
        TextEditOp::DeleteWordForward => {
            delete_word_forward(text, cursor);
            TextEditOutcome::Edited
        }
        TextEditOp::Clear => {
            clear(text, cursor);
            TextEditOutcome::Edited
        }
        TextEditOp::InsertChar(c) => {
            if accept_char(c) {
                insert_char(text, cursor, c);
                TextEditOutcome::Edited
            } else {
                TextEditOutcome::Rejected
            }
        }
        TextEditOp::Consume => TextEditOutcome::CursorOnly,
        TextEditOp::InsertNewline
        | TextEditOp::MoveLineUp
        | TextEditOp::MoveLineDown
        | TextEditOp::MoveLineStart
        | TextEditOp::MoveLineEnd => TextEditOutcome::Unhandled,
    };

    if outcome == TextEditOutcome::Edited && text.len() == pre_len {
        TextEditOutcome::CursorOnly
    } else {
        outcome
    }
}

pub fn apply_multi_line_op(
    op: TextEditOp,
    text: &mut String,
    cursor: &mut usize,
) -> TextEditOutcome {
    match op {
        TextEditOp::InsertNewline => {
            insert_char(text, cursor, '\n');
            TextEditOutcome::Edited
        }
        TextEditOp::MoveLineUp => {
            move_line_vertical(text, cursor, -1);
            TextEditOutcome::CursorOnly
        }
        TextEditOp::MoveLineDown => {
            move_line_vertical(text, cursor, 1);
            TextEditOutcome::CursorOnly
        }
        TextEditOp::MoveLineStart => {
            *cursor = line_start_of(text, *cursor);
            TextEditOutcome::CursorOnly
        }
        TextEditOp::MoveLineEnd => {
            *cursor = line_end_of(text, *cursor);
            TextEditOutcome::CursorOnly
        }
        TextEditOp::Consume => TextEditOutcome::CursorOnly,
        op => apply_single_line_op(op, text, cursor),
    }
}

pub fn insert_char(text: &mut String, cursor: &mut usize, c: char) {
    text.insert(*cursor, c);
    *cursor += c.len_utf8();
}

pub fn paste_single_line(s: &str, text: &mut String, cursor: &mut usize) -> bool {
    let filtered: String = s.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    if filtered.is_empty() {
        return false;
    }
    text.insert_str(*cursor, &filtered);
    *cursor += filtered.len();
    true
}

pub fn paste_single_line_filtered<F: Fn(char) -> bool>(
    s: &str,
    text: &mut String,
    cursor: &mut usize,
    accept: F,
) -> bool {
    let filtered: String = s
        .chars()
        .filter(|c| *c != '\n' && *c != '\r' && accept(*c))
        .collect();
    if filtered.is_empty() {
        return false;
    }
    text.insert_str(*cursor, &filtered);
    *cursor += filtered.len();
    true
}

pub fn paste_multi_line_strip_cr(s: &str, text: &mut String, cursor: &mut usize) -> bool {
    let filtered: String = s.chars().filter(|c| *c != '\r').collect();
    if filtered.is_empty() {
        return false;
    }
    text.insert_str(*cursor, &filtered);
    *cursor += filtered.len();
    true
}

pub fn paste_ascii_digits_capped(
    s: &str,
    text: &mut String,
    cursor: &mut usize,
    max_len: usize,
) -> bool {
    let remaining = max_len.saturating_sub(text.len());
    if remaining == 0 {
        return false;
    }
    let mut to_insert = String::with_capacity(remaining.min(s.len()));
    for c in s.chars() {
        if to_insert.len() >= remaining {
            break;
        }
        if c.is_ascii_digit() {
            to_insert.push(c);
        }
    }
    if to_insert.is_empty() {
        return false;
    }
    text.insert_str(*cursor, &to_insert);
    *cursor += to_insert.len();
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

pub fn delete_word_backward(text: &mut String, cursor: &mut usize) {
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

    let start = chars.get(i).map(|&(b, _)| b).unwrap_or(0);
    text.replace_range(start..*cursor, "");
    *cursor = start;
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

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

pub fn delete_char_forward(text: &mut String, cursor: &mut usize) {
    if *cursor >= text.len() {
        return;
    }
    let next = next_char_boundary(text, *cursor);
    text.replace_range(*cursor..next, "");
}

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

pub fn line_start_of(text: &str, cursor: usize) -> usize {
    let clamped = cursor.min(text.len());
    text[..clamped].rfind('\n').map(|p| p + 1).unwrap_or(0)
}

pub fn line_end_of(text: &str, cursor: usize) -> usize {
    let clamped = cursor.min(text.len());
    text[clamped..]
        .find('\n')
        .map(|p| clamped + p)
        .unwrap_or(text.len())
}

pub fn move_line_vertical(text: &str, cursor: &mut usize, delta: i32) {
    if delta == 0 || text.is_empty() {
        return;
    }
    let clamped = (*cursor).min(text.len());
    let line_start = line_start_of(text, clamped);
    let column = clamped - line_start;

    if delta < 0 {
        if line_start == 0 {
            *cursor = 0;
            return;
        }
        let prev_line_end = line_start - 1;
        let prev_line_start = line_start_of(text, prev_line_end);
        let prev_line_len = prev_line_end - prev_line_start;
        let target = prev_line_start + column.min(prev_line_len);
        *cursor = prev_safe_boundary(text, target);
    } else {
        let line_end = line_end_of(text, clamped);
        if line_end == text.len() {
            *cursor = text.len();
            return;
        }
        let next_line_start = line_end + 1;
        let next_line_end = line_end_of(text, next_line_start);
        let next_line_len = next_line_end - next_line_start;
        let target = next_line_start + column.min(next_line_len);
        *cursor = prev_safe_boundary(text, target);
    }
}

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

    fn state(q: &str, c: usize) -> (String, usize) {
        (q.to_string(), c)
    }

    #[test]
    fn single_line_insert_and_backspace_roundtrip() {
        let (mut q, mut c) = state("", 0);
        assert_eq!(
            apply_single_line_op(TextEditOp::InsertChar('h'), &mut q, &mut c),
            TextEditOutcome::Edited
        );
        assert_eq!(
            apply_single_line_op(TextEditOp::InsertChar('i'), &mut q, &mut c),
            TextEditOutcome::Edited
        );
        assert_eq!(q, "hi");
        assert_eq!(c, 2);
        assert_eq!(
            apply_single_line_op(TextEditOp::DeleteBackward, &mut q, &mut c),
            TextEditOutcome::Edited
        );
        assert_eq!(q, "h");
        assert_eq!(c, 1);
    }

    #[test]
    fn no_op_delete_is_cursor_only() {
        let (mut q, mut c) = state("", 0);
        assert_eq!(
            apply_single_line_op(TextEditOp::DeleteBackward, &mut q, &mut c),
            TextEditOutcome::CursorOnly
        );
        assert_eq!(q, "");
        assert_eq!(c, 0);
    }

    #[test]
    fn cursor_moves_respect_char_boundaries() {
        let (q, mut c) = state("a你b", "a你b".len());
        move_cursor(&q, &mut c, -1);
        assert_eq!(c, 4);
        move_cursor(&q, &mut c, -1);
        assert_eq!(c, 1);
        move_cursor(&q, &mut c, 1);
        assert_eq!(c, 4);
    }

    #[test]
    fn delete_word_backward_sweeps_trailing_separators() {
        let (mut q, mut c) = state("src/ui/", "src/ui/".len());
        delete_word_backward(&mut q, &mut c);
        assert_eq!(q, "src/");
        assert_eq!(c, 4);
    }

    #[test]
    fn filtered_insert_rejects_without_edit() {
        let (mut q, mut c) = state("", 0);
        assert_eq!(
            apply_single_line_op_filtered(TextEditOp::InsertChar('/'), &mut q, &mut c, |ch| ch
                != '/'),
            TextEditOutcome::Rejected
        );
        assert!(q.is_empty());
        assert_eq!(c, 0);
    }

    #[test]
    fn paste_single_line_drops_crlf() {
        let mut text = "ab".to_string();
        let mut cursor = 1;
        assert!(paste_single_line("X\r\nY", &mut text, &mut cursor));
        assert_eq!(text, "aXYb");
        assert_eq!(cursor, 3);
    }

    #[test]
    fn paste_single_line_filtered_keeps_only_matching_chars() {
        let mut text = String::new();
        let mut cursor = 0;
        assert!(paste_single_line_filtered(
            "1a2",
            &mut text,
            &mut cursor,
            |c| c.is_ascii_digit()
        ));
        assert_eq!(text, "12");
        assert_eq!(cursor, 2);
    }

    #[test]
    fn paste_ascii_digits_capped_enforces_max_len() {
        let mut text = String::new();
        let mut cursor = 0;
        assert!(paste_ascii_digits_capped(
            "1234567890123456789012345",
            &mut text,
            &mut cursor,
            18
        ));
        assert_eq!(text, "123456789012345678");
        assert_eq!(cursor, 18);
    }

    #[test]
    fn paste_ascii_digits_capped_inserts_at_cursor() {
        let mut text = String::from("19");
        let mut cursor = 1;
        assert!(paste_ascii_digits_capped("23", &mut text, &mut cursor, 18));
        assert_eq!(text, "1239");
        assert_eq!(cursor, 3);
    }

    #[test]
    fn multi_line_enter_and_vertical_motion() {
        let mut text = "alpha\nbeta\ngamma".to_string();
        let mut cursor = "alpha\nbeta\nga".len();
        assert_eq!(
            apply_multi_line_op(TextEditOp::MoveLineUp, &mut text, &mut cursor),
            TextEditOutcome::CursorOnly
        );
        assert_eq!(cursor, 8);
        assert_eq!(
            apply_multi_line_op(TextEditOp::InsertNewline, &mut text, &mut cursor),
            TextEditOutcome::Edited
        );
        assert_eq!(&text[8..9], "\n");
    }

    #[test]
    fn line_helpers_find_current_line_bounds() {
        assert_eq!(line_start_of("foo\nbar", 5), 4);
        assert_eq!(line_end_of("foo\nbar\nbaz", 5), 7);
    }

    #[test]
    fn multi_line_motion_lands_on_char_boundary() {
        let text = "abc\n你好".to_string();
        let mut cursor = 2;
        move_line_vertical(&text, &mut cursor, 1);
        assert_eq!(cursor, "abc\n".len());
        assert!(text.is_char_boundary(cursor));
    }
}
