use crate::app::Tab;
use ratatui::layout::Rect;
use serde_json;

#[derive(Debug, Clone)]
pub enum ClickAction {
    SelectFile { path: String, staged: bool },
    StageFile(String),
    UnstageFile(String),
    ToggleStaged,
    ToggleUnstaged,
    StartDragSplit,
    SwitchTab(Tab),
    TreeClick(usize),
    /// Invoke a plugin command (from a StyledLine click_command).
    /// `dbl_command`/`dbl_args`, if present, are fired on double-click instead.
    PluginCommand {
        command: String,
        args: serde_json::Value,
        dbl_command: Option<String>,
        dbl_args: Option<serde_json::Value>,
    },
}

#[derive(Debug, Clone)]
struct Region {
    rect: Rect,
    action: ClickAction,
}

pub struct HitTestRegistry {
    regions: Vec<Region>,
}

impl HitTestRegistry {
    pub fn new() -> Self {
        Self {
            regions: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.regions.clear();
    }

    pub fn register(&mut self, rect: Rect, action: ClickAction) {
        self.regions.push(Region { rect, action });
    }

    /// Register a single-row region at (x, y) with given width
    pub fn register_row(&mut self, x: u16, y: u16, width: u16, action: ClickAction) {
        self.register(
            Rect {
                x,
                y,
                width,
                height: 1,
            },
            action,
        );
    }

    pub fn hit_test(&self, col: u16, row: u16) -> Option<ClickAction> {
        // Search in reverse order so later (on-top) elements take priority
        for region in self.regions.iter().rev() {
            let r = &region.rect;
            if col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height {
                return Some(region.action.clone());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_has_no_regions() {
        let r = HitTestRegistry::new();
        assert!(r.hit_test(0, 0).is_none());
    }

    #[test]
    fn hit_test_returns_registered_action() {
        let mut r = HitTestRegistry::new();
        r.register_row(10, 5, 20, ClickAction::ToggleStaged);
        assert!(matches!(r.hit_test(15, 5), Some(ClickAction::ToggleStaged)));
    }

    #[test]
    fn hit_test_misses_outside_rect() {
        let mut r = HitTestRegistry::new();
        r.register_row(10, 5, 20, ClickAction::ToggleStaged);
        assert!(r.hit_test(9, 5).is_none(), "col just to the left");
        assert!(r.hit_test(30, 5).is_none(), "col at right edge (exclusive)");
        assert!(r.hit_test(15, 4).is_none(), "row above");
        assert!(r.hit_test(15, 6).is_none(), "row below (single-row region)");
    }

    #[test]
    fn hit_test_later_region_takes_priority() {
        let mut r = HitTestRegistry::new();
        r.register_row(0, 0, 10, ClickAction::ToggleStaged);
        r.register_row(0, 0, 10, ClickAction::ToggleUnstaged);
        // Later registration should win on overlap
        assert!(matches!(r.hit_test(5, 0), Some(ClickAction::ToggleUnstaged)));
    }

    #[test]
    fn clear_removes_all_regions() {
        let mut r = HitTestRegistry::new();
        r.register_row(0, 0, 10, ClickAction::ToggleStaged);
        r.clear();
        assert!(r.hit_test(5, 0).is_none());
    }

    #[test]
    fn hit_test_inclusive_left_exclusive_right() {
        let mut r = HitTestRegistry::new();
        r.register_row(10, 0, 5, ClickAction::ToggleStaged); // cols 10..15
        assert!(r.hit_test(10, 0).is_some(), "left edge is inclusive");
        assert!(r.hit_test(14, 0).is_some(), "last valid col");
        assert!(r.hit_test(15, 0).is_none(), "right edge is exclusive");
    }
}
