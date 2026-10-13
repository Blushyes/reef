//! Declarative menu entries. Selection indices count items, not separator rows.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuEntry<T> {
    Item(T),
    Separator,
}

impl<T> MenuEntry<T> {
    pub fn as_item(&self) -> Option<&T> {
        match self {
            Self::Item(item) => Some(item),
            Self::Separator => None,
        }
    }

    pub fn map<U>(&self, map: impl FnOnce(&T) -> U) -> MenuEntry<U> {
        match self {
            Self::Item(item) => MenuEntry::Item(map(item)),
            Self::Separator => MenuEntry::Separator,
        }
    }
}

pub fn menu_items<T>(entries: &[MenuEntry<T>]) -> impl Iterator<Item = &T> {
    entries.iter().filter_map(MenuEntry::as_item)
}

pub fn navigate_menu<T>(entries: &[MenuEntry<T>], selected: usize, delta: i32) -> usize {
    let count = menu_items(entries).count();
    if count == 0 {
        return 0;
    }
    (selected as i64 + i64::from(delta)).rem_euclid(count as i64) as usize
}

#[cfg(test)]
mod tests {
    use super::{MenuEntry::*, *};

    #[test]
    fn grouped_menu_navigation_counts_only_items_and_wraps() {
        let entries = [
            Separator,
            Item("one"),
            Separator,
            Separator,
            Item("two"),
            Separator,
        ];
        assert_eq!(
            menu_items(&entries).copied().collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert_eq!(navigate_menu(&entries, 0, 1), 1);
        assert_eq!(navigate_menu(&entries, 1, 1), 0);
        assert_eq!(navigate_menu(&entries, 0, -1), 1);
        assert_eq!(navigate_menu::<()>(&[Separator], 0, 1), 0);
    }
}
