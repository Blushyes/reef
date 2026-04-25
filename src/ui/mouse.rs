use crate::app::Tab;
use ratatui::layout::Rect;
use serde_json;

#[derive(Debug, Clone)]
pub enum ClickAction {
    SelectFile {
        path: String,
        staged: bool,
    },
    StageFile(String),
    UnstageFile(String),
    ToggleStaged,
    ToggleUnstaged,
    StartDragSplit,
    /// Graph tab 3-col 布局下,中间 commit 列与右侧 diff 列之间的拖拽
    /// 分界。跟 `StartDragSplit` 互不干扰。
    StartDragGraphDiffSplit,
    /// 标签栏右侧的侧边栏可见性切换按钮。等价于 Ctrl+B,但鼠标可点。
    ToggleSidebar,
    SwitchTab(Tab),
    TreeClick(usize),
    /// Click on a row in the quick-open palette. The `usize` indexes
    /// into `QuickOpenState.matches`. Double-click semantics (select +
    /// accept) live in `input::handle_mouse` rather than here — the palette
    /// owns mouse when active, so we don't round-trip through
    /// `App::handle_action` for it.
    QuickOpenSelect(usize),
    /// Click on a row in the global-search (Space+F) palette. The `usize`
    /// indexes into `GlobalSearchState.results`. Like `QuickOpenSelect`,
    /// dispatched inside `global_search::handle_mouse` (not via
    /// `App::handle_action`) so the palette keeps exclusive mouse ownership.
    GlobalSearchSelect(usize),
    /// Click on a row in the hosts picker (Ctrl+O). The `usize` indexes
    /// into `HostsPickerState::visible_rows()`. Handled in
    /// `input::handle_mouse` while the picker owns mouse input.
    HostsPickerSelect(usize),
    /// Click on the input row of the Search tab's left panel while in
    /// list mode. Flips `tab_input_focused` so the user can mouse-drive
    /// the mode switch instead of hunting for `/` or `i`. Only registered
    /// when the input isn't already focused (overlay always is).
    GlobalSearchFocusInput,
    /// Invoke an inline Git panel command (`git.stage`, `git.selectCommit`, …).
    /// `dbl_command`/`dbl_args`, if present, are fired on double-click instead.
    GitCommand {
        command: String,
        args: serde_json::Value,
        dbl_command: Option<String>,
        dbl_args: Option<serde_json::Value>,
    },
    /// Place-mode destination: dropping onto a specific folder row.
    /// Index into `file_tree.entries`; must point at a directory entry.
    PlaceModeFolder(usize),
    /// Place-mode destination: dropping onto the root drop-zone (the dashed
    /// border surrounding the whole file tree, or any tree-panel spot that
    /// isn't a folder row).
    PlaceModeRoot,
    /// Click on one of the Files-tab tree toolbar buttons.
    FileTreeToolbarNewFile,
    FileTreeToolbarNewFolder,
    FileTreeToolbarRefresh,
    FileTreeToolbarCollapse,
    /// Pick from an open right-click context menu. Dispatched when
    /// the user left-clicks a menu row; keyboard picks go through
    /// `App::dispatch_context_menu_item` directly from `input`.
    TreeContextMenuItem(crate::tree_context_menu::ContextMenuItem),
    /// Registered panel-wide underneath an open context menu — any
    /// left-click that misses a menu row falls through to this and
    /// just closes the menu.
    TreeContextMenuClose,
    /// Left-click on the Files-tab tree panel that missed every
    /// entry row (i.e. clicked on the empty area below the last
    /// entry). Clears the selection so a subsequent toolbar New
    /// File / Folder creates at the project root — VSCode behaviour.
    TreeClearSelection,
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
        assert!(matches!(
            r.hit_test(5, 0),
            Some(ClickAction::ToggleUnstaged)
        ));
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
