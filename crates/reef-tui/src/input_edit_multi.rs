use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::input_edit;
#[cfg(test)]
use crate::input_edit::Outcome;
use reef_app::TextEditOp;

#[cfg(test)]
pub fn dispatch_key_multi(key: &KeyEvent, text: &mut String, cursor: &mut usize) -> Outcome {
    match op_for_key(key) {
        Some(op) => reef_app::apply_multi_line_op(op, text, cursor),
        None => Outcome::Unhandled,
    }
}

pub fn op_for_key(key: &KeyEvent) -> Option<TextEditOp> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter if !ctrl => Some(TextEditOp::InsertNewline),
        KeyCode::Up if !ctrl => Some(TextEditOp::MoveLineUp),
        KeyCode::Down if !ctrl => Some(TextEditOp::MoveLineDown),
        KeyCode::Home if !ctrl => Some(TextEditOp::MoveLineStart),
        KeyCode::End if !ctrl => Some(TextEditOp::MoveLineEnd),
        KeyCode::Char('\r') => Some(TextEditOp::Consume),
        _ => input_edit::op_for_key(key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn shift_enter_inserts_newline() {
        let mut t = "ab".to_string();
        let mut c = 1;
        let outcome = dispatch_key_multi(&k(KeyCode::Enter, KeyModifiers::SHIFT), &mut t, &mut c);
        assert_eq!(outcome, Outcome::Edited);
        assert_eq!(t, "a\nb");
        assert_eq!(c, 2);
    }

    #[test]
    fn ctrl_enter_unhandled_so_caller_can_submit() {
        let mut t = "msg".to_string();
        let mut c = 3;
        let outcome = dispatch_key_multi(&k(KeyCode::Enter, KeyModifiers::CONTROL), &mut t, &mut c);
        assert_eq!(outcome, Outcome::Unhandled);
        assert_eq!(t, "msg");
        assert_eq!(c, 3);
    }

    #[test]
    fn shift_up_moves_cursor_like_bare_up() {
        let mut t = "alpha\nbeta\ngamma".to_string();
        let mut c = "alpha\nbeta\nga".len();
        let outcome = dispatch_key_multi(&k(KeyCode::Up, KeyModifiers::SHIFT), &mut t, &mut c);
        assert_eq!(outcome, Outcome::CursorOnly);
        assert_eq!(c, 8);
    }

    #[test]
    fn single_line_ops_still_apply() {
        let mut t = "hello world".to_string();
        let mut c = 11;
        let outcome = dispatch_key_multi(
            &k(KeyCode::Char('w'), KeyModifiers::CONTROL),
            &mut t,
            &mut c,
        );
        assert_eq!(outcome, Outcome::Edited);
        assert_eq!(t, "hello ");
    }
}
