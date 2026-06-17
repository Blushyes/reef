//! "纯预览" (FocusedPreview) — full-screen takeover that maximises the
//! active tab's preview/diff content. Triggered via `Space+V` or the
//! CLI quick-look path (`reef <file>`). The header row carries a
//! floating ☰ chip + invisible click zone covering the file path,
//! opening a no-border popup that lists changed files for one-click
//! file switching.
//!
//! Layout summary
//! - Whole frame minus 1 bottom hint row goes to the content panel.
//! - On Git tab + Graph 3-col, a ☰ chip + path-wide click zone is
//!   painted on top of the diff header. Graph 2-col uses a different
//!   header layout (commit_detail_panel) so the chip is deliberately
//!   skipped there — see [`render`] for the gate.
//! - When the picker is open, a 36×N popup hangs under the chip with
//!   the changed-files list; row clicks dispatch `PickFocusedPreviewFile`.

use crate::app::{App, Tab};
use crate::i18n::{Msg, t};
use crate::ui::mouse::ClickAction;
use crate::ui::text::truncate_to_width;
use crate::ui::{diff_panel, file_preview_panel, find_widget_panel, hover};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Width of the ☰ chip in the upper-left of the focused-preview body.
const FOCUSED_PREVIEW_CHIP: &str = " ☰ ";

/// Public entry point — mirrors per-tab routing in `ui::render`.
/// Files/Search show the file preview, Git the working-tree diff,
/// Graph the commit diff (3-col diff column or 2-col commit detail).
pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    if area.height == 0 {
        return;
    }
    app.last_total_width = area.width;

    let body_h = area.height.saturating_sub(1);
    let body = Rect::new(area.x, area.y, area.width, body_h);
    let hint_row = Rect::new(area.x, area.y + body_h, area.width, 1);

    match app.active_tab {
        Tab::Files | Tab::Search => {
            file_preview_panel::render(f, app, body, true);
        }
        Tab::Git => {
            if app.backend.has_repo() {
                diff_panel::render(f, app, body);
            } else {
                super::render_no_repo(f, app, body);
            }
        }
        Tab::Graph => {
            if app.graph_uses_three_col() {
                super::render_graph_diff_column(f, app, body);
            } else {
                super::render_graph_editor(f, app, body);
            }
        }
    }

    // Float the file-picker affordance on top of the diff for tabs
    // where switching files makes sense + the chip's width math
    // actually applies. Single source of truth for that predicate
    // lives on `App` so the `o` keyboard shortcut in
    // `handle_key_focused_preview` can gate identically — the original
    // bug was render saying "no" while input said "yes" on Graph 2-col.
    //
    // Order matters: popup renders its catch-all + per-row hit zones
    // first, then the chip is registered *last* so a click on the
    // chip wins over the popup's body-wide CloseFocusedPreviewFiles
    // fallthrough (hit_test scans in reverse — later-registered =
    // higher z-order).
    if app.focused_preview_chip_visible() {
        if app.focused_preview_files_open {
            render_picker(f, app, body);
        }
        render_chip(f, app, body);
    }

    // FocusedPreview takes over `ui::render` and returns early before the
    // normal `find_widget_panel::render` call, so the Space+F overlay
    // would never appear here without this hop. Draw after the body
    // renderers (which write `last_preview_rect` / `last_diff_rect`)
    // so the widget can anchor itself to the visible content column.
    // Skips itself when `find_widget.active == false`.
    find_widget_panel::render(f, app);

    // Same story for the nav candidates popup: `ui::render` draws it
    // only in the normal frame, *after* the FocusedPreview early
    // return. Without this hop, Ctrl+click / `gd` in focused mode
    // would set `nav_candidates` but the popup would never paint —
    // the user sees "no reaction". Render over the full takeover area.
    if app.nav_candidates.is_some() {
        super::nav_candidates_popup::render(f, app, area);
    }

    // FocusedPreview replaces the normal status bar entirely, so the search
    // prompt would never render here without this branch — `/` would silently
    // capture keystrokes against an invisible input. Mirror the same
    // priority order `render_status_bar` uses (active prompt → dormant
    // counter → hint) so the bottom row stays meaningful in every state.
    if app.search.active {
        super::render_search_prompt(f, app, hint_row);
    } else if !app.search.matches.is_empty() {
        super::render_search_dormant(f, app, hint_row);
    } else {
        render_hint(f, app, hint_row);
    }
}

fn render_chip(f: &mut Frame, app: &mut App, body: Rect) {
    if body.height == 0 || body.width < 3 {
        return;
    }
    let th = app.theme;
    let chip_w = UnicodeWidthStr::width(FOCUSED_PREVIEW_CHIP) as u16;
    let chip_rect = Rect::new(body.x, body.y, chip_w.min(body.width), 1);
    // The diff header is laid out as `path_display + tag_str`, where
    // `tag_str = "  [unified][compact]  m/f toggle"`. We want the hover
    // wash + clickable zone to stop where the path ends — the right-
    // hand metadata cluster is just visual chrome, not part of the
    // "open file picker" affordance.
    let interactive_w = interactive_width(app, body.width);
    let row_rect = Rect::new(body.x, body.y, interactive_w, 1);

    let is_hovered = hover::is_hover(app, row_rect, row_rect.y);

    // Hover wash on the chip + file-path span only. diff_panel already
    // wrote the path text into the buffer, so we patch its background
    // via `buffer_mut().set_style` (keeps glyphs + foreground intact)
    // rather than re-rendering a Line over it.
    if is_hovered && !app.focused_preview_files_open {
        let wash = Style::default().bg(th.hover_bg);
        f.buffer_mut().set_style(row_rect, wash);
    } else if app.focused_preview_files_open {
        // Picker-open state: mirror the chip's inverted look across
        // the file-path span so the "active" surface is visually
        // continuous with the chip itself.
        let wash = Style::default().bg(th.accent).fg(th.chrome_bg);
        f.buffer_mut().set_style(row_rect, wash);
    }

    let style = if app.focused_preview_files_open {
        Style::default()
            .fg(th.chrome_bg)
            .bg(th.accent)
            .add_modifier(Modifier::BOLD)
    } else if is_hovered {
        Style::default()
            .fg(th.accent)
            .bg(th.hover_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(th.accent)
            .bg(th.chrome_bg)
            .add_modifier(Modifier::BOLD)
    };
    f.render_widget(
        Line::from(Span::styled(FOCUSED_PREVIEW_CHIP, style)),
        chip_rect,
    );
    // Register the interactive zone last (chip+path, not the tag tail)
    // so hit_test's reverse scan picks it up across the whole washed
    // region. Clicking on `[unified][compact] m/f toggle` does NOT
    // toggle the picker — those reserve their visual role as chrome.
    app.hit_registry
        .register(row_rect, ClickAction::ToggleFocusedPreviewFiles);
}

/// Width of the "interactive" portion of the focused-preview header
/// row — chip + file path, stopping before the `[unified][compact]
/// m/f toggle` tag tail that diff_panel paints on the right. Falls
/// back to chip-only when we can't read the current diff's path.
///
/// Both Git tab `diff_panel::render` and Graph 3-col `render_graph_diff_column`
/// wrap their content in `Block::padding(1, 1, 0, 0)`, so the path
/// text is rendered starting at column `body.x + 1`, not `body.x`.
/// The wash starts at `body.x` (so the cell *under* the chip glyph
/// also gets the wash background) but its right edge must align with
/// the path's actual rightmost cell — hence the `+1` offset below.
/// Without it the wash falls short by exactly one cell and the trailing
/// character of the filename reads as un-highlighted.
fn interactive_width(app: &App, body_w: u16) -> u16 {
    let chip_w = UnicodeWidthStr::width(FOCUSED_PREVIEW_CHIP) as u16;
    let path: Option<&str> = match app.active_tab {
        Tab::Git => app.diff_content.as_ref().map(|d| d.diff.file_path.as_str()),
        Tab::Graph => app
            .commit_detail
            .file_diff
            .as_ref()
            .map(|d| d.path.as_str()),
        _ => None,
    };
    let Some(path) = path else {
        return chip_w.min(body_w);
    };
    // Mirror diff_panel's header tag exactly: Git tab uses the top-level
    // `app.diff_layout/diff_mode`, Graph tab uses `commit_detail`'s
    // independent pair. Mismatched math leaves the wash a few cells
    // short of (or past) the path's actual end.
    let (layout, mode) = match app.active_tab {
        Tab::Graph => (app.commit_detail.diff_layout, app.commit_detail.diff_mode),
        _ => (app.diff_layout, app.diff_mode),
    };
    let layout_label = match layout {
        crate::app::DiffLayout::Unified => t(Msg::LayoutUnified),
        crate::app::DiffLayout::SideBySide => t(Msg::LayoutSideBySide),
    };
    let mode_label = match mode {
        crate::app::DiffMode::Compact => t(Msg::ModeCompact),
        crate::app::DiffMode::FullFile => t(Msg::ModeFullFile),
    };
    let tag = crate::i18n::diff_mode_hint(layout_label, mode_label);
    let tag_w = UnicodeWidthStr::width(tag.as_str()) as u16;
    // `path_max` matches diff_panel's truncation budget *inside* the
    // padded inner rect — width is body_w-2 (left+right pad) - tag_w.
    let inner_w = body_w.saturating_sub(2);
    let path_max = inner_w.saturating_sub(tag_w) as usize;
    let path_display = truncate_to_width(path, path_max);
    let path_w = UnicodeWidthStr::width(path_display) as u16;
    // +1 for the diff_panel's left padding column that sits between
    // body.x=0 and the path's first character. The wash starts at
    // body.x to cover the chip glyph too, so its width is
    // (left_pad=1) + path_w, clamped so we never read past chip_w on
    // empty paths or past body_w on narrow terminals.
    chip_w.max(1 + path_w).min(body_w)
}

/// No-border popup listing diff-changed files. Anchored under the
/// chip; solid `chrome_bg` background so it reads as a distinct
/// surface even without a border. Rows below the visible window
/// scroll-clip when the file count exceeds the popup height.
fn render_picker(f: &mut Frame, app: &mut App, body: Rect) {
    let entries = app.focused_preview_file_entries();
    if entries.is_empty() {
        return;
    }
    let th = app.theme;

    let popup_w: u16 = 36;
    let popup_max_h: u16 = body.height.saturating_sub(1).min(20);
    let popup_h: u16 = (entries.len() as u16 + 1).min(popup_max_h);
    if popup_h < 2 {
        return;
    }
    let anchor_y = body.y + 1;
    if anchor_y + popup_h > body.y + body.height {
        return;
    }
    let popup = Rect::new(body.x, anchor_y, popup_w.min(body.width), popup_h);

    // Catch-all hit zone for click-outside-to-close. Registered first
    // so per-row PickFocusedPreviewFile zones below override it for
    // hits that land on actual rows.
    app.hit_registry
        .register(body, ClickAction::CloseFocusedPreviewFiles);

    let bg = th.chrome_bg;
    let fg = th.fg_primary;
    for dy in 0..popup.height {
        f.render_widget(
            Line::from(Span::styled(
                " ".repeat(popup.width as usize),
                Style::default().bg(bg),
            )),
            Rect::new(popup.x, popup.y + dy, popup.width, 1),
        );
    }

    let header_msg = match app.active_tab {
        Tab::Git => Msg::FocusedPreviewPickerHeaderGit,
        _ => Msg::FocusedPreviewPickerHeaderCommit,
    };
    f.render_widget(
        Line::from(Span::styled(
            t(header_msg),
            Style::default()
                .fg(th.fg_secondary)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        )),
        Rect::new(popup.x, popup.y, popup.width, 1),
    );

    let rows_avail = popup.height.saturating_sub(1) as usize;
    let sel = app
        .focused_preview_files_selected
        .min(entries.len().saturating_sub(1));
    let scroll = if sel >= rows_avail {
        sel + 1 - rows_avail
    } else {
        0
    };
    for (visible_idx, abs_idx) in (scroll..entries.len()).take(rows_avail).enumerate() {
        let row = &entries[abs_idx];
        let y = popup.y + 1 + visible_idx as u16;
        let is_sel = abs_idx == sel;
        let row_rect = Rect::new(popup.x, y, popup.width, 1);
        // Selection wins over hover so the cursor row stays distinct even
        // when the mouse drifts across it; otherwise show a subtle hover
        // wash so unselected rows still react to the pointer.
        let is_hovered = !is_sel && hover::is_hover(app, row_rect, y);
        let (fg_row, bg_row) = if is_sel {
            (th.chrome_bg, th.accent)
        } else if is_hovered {
            (fg, th.hover_bg)
        } else {
            (fg, bg)
        };
        let depth = row.path.matches('/').count();
        let indent = "  ".repeat(depth);
        let name = std::path::Path::new(&row.path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&row.path);
        let line_text = format!("{}{} {}", indent, row.status, name);
        let inner_w = popup.width.saturating_sub(2) as usize;
        let clipped = truncate_to_width(&line_text, inner_w).to_string();
        let used = UnicodeWidthStr::width(clipped.as_str());
        // Use saturating_sub defensively — inner_w is intentionally
        // <= popup.width-2 so today (popup.width-1) - used >= 1, but
        // a future tweak that lets `used` reach popup.width-1 would
        // underflow a plain `usize` subtraction and the subsequent
        // `\" \".repeat(pad_w)` would OOM. Doesn't change the safe-case
        // value.
        let pad_w = (popup.width.saturating_sub(1) as usize).saturating_sub(used);
        let style = Style::default()
            .fg(fg_row)
            .bg(bg_row)
            .add_modifier(if is_sel {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        f.render_widget(
            Line::from(vec![
                Span::styled(" ", Style::default().fg(fg_row).bg(bg_row)),
                Span::styled(clipped, style),
                Span::styled(" ".repeat(pad_w), Style::default().fg(fg_row).bg(bg_row)),
            ]),
            row_rect,
        );
        app.hit_registry
            .register(row_rect, ClickAction::PickFocusedPreviewFile(abs_idx));
    }
}

fn render_hint(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;
    let fill = Line::from(Span::styled(
        " ".repeat(area.width as usize),
        Style::default().bg(th.chrome_bg),
    ));
    f.render_widget(fill, area);
    let hint = t(Msg::FocusedPreviewHint);
    let hint_w = UnicodeWidthStr::width(hint) as u16;
    if hint_w == 0 || hint_w > area.width {
        return;
    }
    let x = area.x + (area.width.saturating_sub(hint_w)) / 2;
    f.render_widget(
        Line::from(Span::styled(
            hint,
            Style::default().fg(th.chrome_muted_fg).bg(th.chrome_bg),
        )),
        Rect::new(x, area.y, hint_w, 1),
    );
}
