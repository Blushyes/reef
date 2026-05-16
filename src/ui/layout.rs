//! Tiny geometry helpers shared by overlay panels.
//!
//! Multiple overlays (`confirm_modal`, `quick_open_panel`,
//! `global_search_panel`, `hosts_picker_panel`) center a popup over the
//! current screen using the same `(screen - popup) / 2` math. Keeping
//! the calculation in one place avoids drift if we later swap in
//! `Rect::y` clamping, off-center placement for dialogs near the cursor,
//! or anything more elaborate.

use ratatui::layout::Rect;

/// Center a `w × h` rectangle inside `screen`, saturating to the
/// top-left if `screen` is smaller than the popup in either dimension.
pub fn center_rect(screen: Rect, w: u16, h: u16) -> Rect {
    let x = screen.x + screen.width.saturating_sub(w) / 2;
    let y = screen.y + screen.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}
