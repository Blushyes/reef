#[derive(Debug, Default)]
pub struct PickerState {
    pub active: bool,
    pub filter: String,
    pub cursor: usize,
    pub selected_idx: usize,
}

impl PickerState {
    pub fn open(&mut self) {
        self.filter.clear();
        self.cursor = 0;
        self.selected_idx = 0;
        self.active = true;
    }

    pub fn close(&mut self) {
        self.active = false;
        self.filter.clear();
        self.cursor = 0;
        self.selected_idx = 0;
    }

    pub fn move_selection(&mut self, visible_count: usize, delta: i32) {
        if visible_count == 0 {
            self.selected_idx = 0;
            return;
        }
        let last = visible_count as i32 - 1;
        let next = (self.selected_idx as i32 + delta).clamp(0, last);
        self.selected_idx = next as usize;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerInput {
    Cancel,
    Quit,
    Confirm,
    MoveSelection(i32),
    Edit(crate::TextEditOp),
    Unhandled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerInputOutcome {
    Cancel,
    Quit,
    Confirm,
    Edited,
    Rejected,
    SelectionMoved,
    CursorMoved,
    Unhandled,
}

pub fn apply_picker_input(
    state: &mut PickerState,
    input: PickerInput,
    visible_count: usize,
) -> PickerInputOutcome {
    match input {
        PickerInput::Cancel => PickerInputOutcome::Cancel,
        PickerInput::Quit => PickerInputOutcome::Quit,
        PickerInput::Confirm => PickerInputOutcome::Confirm,
        PickerInput::MoveSelection(delta) => {
            state.move_selection(visible_count, delta);
            PickerInputOutcome::SelectionMoved
        }
        PickerInput::Edit(op) => {
            match crate::text_input::apply_single_line_op(op, &mut state.filter, &mut state.cursor)
            {
                crate::TextEditOutcome::Edited => {
                    state.selected_idx = 0;
                    PickerInputOutcome::Edited
                }
                crate::TextEditOutcome::CursorOnly => PickerInputOutcome::CursorMoved,
                crate::TextEditOutcome::Rejected => PickerInputOutcome::Rejected,
                crate::TextEditOutcome::Unhandled => PickerInputOutcome::Unhandled,
            }
        }
        PickerInput::Unhandled => PickerInputOutcome::Unhandled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TextEditOp;

    #[test]
    fn edit_resets_selection() {
        let mut picker = PickerState {
            selected_idx: 4,
            ..PickerState::default()
        };
        let outcome = apply_picker_input(
            &mut picker,
            PickerInput::Edit(TextEditOp::InsertChar('a')),
            10,
        );
        assert_eq!(outcome, PickerInputOutcome::Edited);
        assert_eq!(picker.filter, "a");
        assert_eq!(picker.cursor, 1);
        assert_eq!(picker.selected_idx, 0);
    }

    #[test]
    fn cursor_motion_preserves_selection() {
        let mut picker = PickerState {
            active: true,
            filter: "hello".into(),
            cursor: 5,
            selected_idx: 3,
        };
        let outcome = apply_picker_input(&mut picker, PickerInput::Edit(TextEditOp::MoveLeft), 10);
        assert_eq!(outcome, PickerInputOutcome::CursorMoved);
        assert_eq!(picker.cursor, 4);
        assert_eq!(picker.selected_idx, 3);
    }
}
