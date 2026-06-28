use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reef_app::TextEditOp;
pub(crate) use reef_app::TextEditOutcome as Outcome;

pub fn op_for_key(key: &KeyEvent) -> Option<TextEditOp> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Left if alt || ctrl => Some(TextEditOp::MoveWordLeft),
        KeyCode::Right if alt || ctrl => Some(TextEditOp::MoveWordRight),
        KeyCode::Left => Some(TextEditOp::MoveLeft),
        KeyCode::Right => Some(TextEditOp::MoveRight),
        KeyCode::Home => Some(TextEditOp::MoveStart),
        KeyCode::End => Some(TextEditOp::MoveEnd),
        KeyCode::Char('a') if ctrl => Some(TextEditOp::MoveStart),
        KeyCode::Char('e') if ctrl => Some(TextEditOp::MoveEnd),
        KeyCode::Char('b') if alt => Some(TextEditOp::MoveWordLeft),
        KeyCode::Char('f') if alt => Some(TextEditOp::MoveWordRight),
        KeyCode::Backspace if alt || ctrl => Some(TextEditOp::DeleteWordBackward),
        KeyCode::Char('w') if ctrl => Some(TextEditOp::DeleteWordBackward),
        KeyCode::Char('u') if ctrl => Some(TextEditOp::Clear),
        KeyCode::Backspace => Some(TextEditOp::DeleteBackward),
        KeyCode::Delete if alt || ctrl => Some(TextEditOp::DeleteWordForward),
        KeyCode::Delete => Some(TextEditOp::DeleteForward),
        KeyCode::Char('d') if alt => Some(TextEditOp::DeleteWordForward),
        KeyCode::Char(c) if !ctrl => Some(TextEditOp::InsertChar(c)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn char_key_inserts_through_app_editor() {
        let mut text = String::new();
        let mut cursor = 0;
        let op = op_for_key(&k(KeyCode::Char('你'), KeyModifiers::NONE)).unwrap();
        let outcome = reef_app::apply_single_line_op(op, &mut text, &mut cursor);
        assert_eq!(outcome, Outcome::Edited);
        assert_eq!(text, "你");
        assert_eq!(cursor, 3);
    }

    #[test]
    fn ctrl_u_maps_to_clear() {
        let mut text = "abc".to_string();
        let mut cursor = 2;
        let op = op_for_key(&k(KeyCode::Char('u'), KeyModifiers::CONTROL)).unwrap();
        let outcome = reef_app::apply_single_line_op(op, &mut text, &mut cursor);
        assert_eq!(outcome, Outcome::Edited);
        assert!(text.is_empty());
        assert_eq!(cursor, 0);
    }

    #[test]
    fn filtered_char_returns_rejected() {
        let mut text = String::new();
        let mut cursor = 0;
        let op = op_for_key(&k(KeyCode::Char('/'), KeyModifiers::NONE)).unwrap();
        let outcome =
            reef_app::apply_single_line_op_filtered(op, &mut text, &mut cursor, |c| c != '/');
        assert_eq!(outcome, Outcome::Rejected);
        assert!(text.is_empty());
        assert_eq!(cursor, 0);
    }
}
