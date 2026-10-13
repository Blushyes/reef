use reef_core::diff::DiffSide;
use reef_core::menu::{MenuEntry, menu_items, navigate_menu};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionContextTarget {
    Preview,
    Diff(DiffSide),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionContextMenuItem {
    Copy,
    SelectAll,
}

impl SelectionContextMenuItem {
    pub const ALL: [MenuEntry<Self>; 2] = [
        MenuEntry::Item(Self::Copy),
        MenuEntry::Item(Self::SelectAll),
    ];
}

#[derive(Debug, Default)]
pub struct SelectionContextMenuState {
    target: Option<SelectionContextTarget>,
    anchor: (u16, u16),
    selected: usize,
}

impl SelectionContextMenuState {
    pub fn open(&mut self, target: SelectionContextTarget, anchor: (u16, u16)) {
        self.target = Some(target);
        self.anchor = anchor;
        self.selected = 0;
    }

    pub fn close(&mut self) {
        self.target = None;
        self.selected = 0;
    }

    pub fn is_active(&self) -> bool {
        self.target.is_some()
    }

    pub fn target(&self) -> Option<SelectionContextTarget> {
        self.target
    }

    pub fn anchor(&self) -> (u16, u16) {
        self.anchor
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn navigate(&mut self, delta: i32) {
        self.selected = navigate_menu(&SelectionContextMenuItem::ALL, self.selected, delta);
    }

    pub fn current(&self) -> SelectionContextMenuItem {
        menu_items(&SelectionContextMenuItem::ALL)
            .nth(self.selected)
            .copied()
            .expect("selection identifies a menu item")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_records_target_anchor_and_resets_selection() {
        let mut state = SelectionContextMenuState {
            target: None,
            anchor: (0, 0),
            selected: 1,
        };

        state.open(SelectionContextTarget::Preview, (14, 8));

        assert_eq!(state.target(), Some(SelectionContextTarget::Preview));
        assert_eq!(state.anchor(), (14, 8));
        assert_eq!(state.selected(), 0);
    }

    #[test]
    fn close_clears_target() {
        let mut state = SelectionContextMenuState::default();
        state.open(SelectionContextTarget::Diff(DiffSide::SbsRight), (14, 8));

        state.close();

        assert!(!state.is_active());
        assert_eq!(state.target(), None);
    }

    #[test]
    fn navigate_wraps_in_both_directions() {
        let mut state = SelectionContextMenuState::default();

        state.navigate(-1);
        assert_eq!(state.current(), SelectionContextMenuItem::SelectAll);
        state.navigate(1);
        assert_eq!(state.current(), SelectionContextMenuItem::Copy);
    }
}
