use ratatui::layout::Rect;

#[derive(Debug, Clone)]
pub enum ClickAction {
    SelectFile { path: String, staged: bool },
    StageFile(String),
    UnstageFile(String),
    ToggleStaged,
    ToggleUnstaged,
    StartDragSplit,
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
