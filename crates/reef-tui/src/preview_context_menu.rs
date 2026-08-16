#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewContextMenuItem {
    Copy,
    SelectAll,
}

impl PreviewContextMenuItem {
    pub const ALL: [Self; 2] = [Self::Copy, Self::SelectAll];
}

#[derive(Debug, Default)]
pub struct PreviewContextMenuState {
    pub active: bool,
    pub anchor: (u16, u16),
    pub selected: usize,
}

impl PreviewContextMenuState {
    pub fn open(&mut self, anchor: (u16, u16)) {
        self.active = true;
        self.anchor = anchor;
        self.selected = 0;
    }

    pub fn close(&mut self) {
        self.active = false;
        self.selected = 0;
    }

    pub fn navigate(&mut self, delta: i32) {
        let len = PreviewContextMenuItem::ALL.len() as i32;
        self.selected = (self.selected as i32 + delta).rem_euclid(len) as usize;
    }

    pub fn current(&self) -> PreviewContextMenuItem {
        PreviewContextMenuItem::ALL[self.selected]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_resets_selection_and_records_anchor() {
        let mut state = PreviewContextMenuState {
            active: false,
            anchor: (0, 0),
            selected: 1,
        };

        state.open((14, 8));

        assert_eq!(
            (state.active, state.anchor, state.selected),
            (true, (14, 8), 0)
        );
    }

    #[test]
    fn navigate_wraps_in_both_directions() {
        let mut state = PreviewContextMenuState::default();

        state.navigate(-1);
        assert_eq!(state.current(), PreviewContextMenuItem::SelectAll);
        state.navigate(1);
        assert_eq!(state.current(), PreviewContextMenuItem::Copy);
    }
}
