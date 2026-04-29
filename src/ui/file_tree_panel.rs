use crate::app::App;
use crate::i18n::{Msg, t};
use crate::search::SearchTarget;
use crate::tree_edit::TreeEditMode;
use crate::ui::mouse::ClickAction;
use crate::ui::text::overlay_match_highlight;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};

pub fn render(f: &mut Frame, app: &mut App, area: Rect, focused: bool) {
    if app.place_mode.active {
        render_place_mode(f, app, area, focused);
    } else {
        render_normal(f, app, area, focused);
    }
}

fn render_normal(f: &mut Frame, app: &mut App, area: Rect, focused: bool) {
    let th = app.theme;
    let border_color = if focused { th.accent } else { th.border };
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let padded = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(1),
        inner.height,
    );

    // Toolbar on the first row, tree content from the second row down.
    // The toolbar area extends one cell wider than `padded` so the
    // leftmost button can use the very-left column — otherwise the
    // toolbar would sit offset by the 1-cell padding and feel
    // misaligned with the panel's left edge.
    let toolbar_area = Rect::new(inner.x, inner.y, inner.width, 1.min(padded.height));
    render_toolbar(f, app, toolbar_area);

    let tree_area = Rect::new(
        padded.x,
        padded.y.saturating_add(1),
        padded.width,
        padded.height.saturating_sub(1),
    );

    let entries_len = app.file_tree.entries.len();
    if entries_len == 0 && !app.tree_edit.active {
        let msg = Line::from(Span::styled(
            t(Msg::EmptyDir),
            Style::default().fg(th.fg_secondary),
        ));
        f.render_widget(msg, Rect::new(tree_area.x, tree_area.y, tree_area.width, 1));
        return;
    }

    // Clamp scroll to valid range (entry-index space — the edit row
    // injection can briefly push the last entries off-screen, which
    // is fine).
    let max_scroll = entries_len.saturating_sub(tree_area.height as usize);
    app.tree_scroll = app.tree_scroll.min(max_scroll);

    // Keep the selection visible, but only when the selection actually moved
    // since the last render. Running this every frame meant mouse-wheel scroll
    // (which only changes tree_scroll, not selected) got snapped back to the
    // opened file on the next tick. Also skip when the selection has been
    // cleared (sentinel = entries.len()) — without this guard, the `>=`
    // branch below would slam scroll all the way to the end.
    let selection_changed = app.last_rendered_tree_selected != Some(app.file_tree.selected);
    if selection_changed && !app.file_tree.selected_cleared() {
        if app.file_tree.selected < app.tree_scroll {
            app.tree_scroll = app.file_tree.selected;
        } else if app.file_tree.selected >= app.tree_scroll + tree_area.height as usize {
            app.tree_scroll = app
                .file_tree
                .selected
                .saturating_sub(tree_area.height as usize - 1);
        }
    }
    app.last_rendered_tree_selected = Some(app.file_tree.selected);

    // Register a panel-wide "clear selection" click zone BEFORE rows,
    // so per-row `TreeClick` registrations shadow it via the late-wins
    // hit-test ordering. Left-click on empty tree space falls through
    // to this and drops the selection highlight.
    for ty in tree_area.y..tree_area.y + tree_area.height {
        app.hit_registry.register_row(
            tree_area.x,
            ty,
            tree_area.width,
            ClickAction::TreeClearSelection,
        );
    }

    let scroll = app.tree_scroll;
    let max_y = tree_area.y + tree_area.height;
    let mut visual_y = tree_area.y;

    // Resolve the intra-tree drag's hover target once per frame (cheap
    // — linear in tree size) so per-row lookups stay O(1). When the
    // drag is active but the cursor is in tree empty space (or has
    // strayed to the toolbar / off-panel), `hover_idx` is `None` —
    // collapse that case to `HoverTarget::Root` so the renderer
    // treats it identically to an explicit depth-0 hover. Per-row
    // styling pulls "is this a root hover?" off this same value, so
    // we don't pass it as a separate flag.
    let drag_hover_target = if app.tree_drag.active {
        Some(match app.tree_drag.hover_idx {
            Some(idx) => crate::place_mode::resolve_hover_target(&app.file_tree.entries, idx),
            None => crate::place_mode::HoverTarget::Root,
        })
    } else {
        None
    };

    // Paint the whole tree area with `selection_bg` while the drop
    // target is the workspace root. Mirrors `render_place_mode`'s
    // RootHover backdrop — the user sees at a glance that releasing
    // here lands files at the project root rather than guessing
    // which row will absorb the drop. Rendered BEFORE rows so per-
    // row bg styling layers on top.
    if matches!(
        drag_hover_target,
        Some(crate::place_mode::HoverTarget::Root)
    ) {
        f.render_widget(
            Block::default().style(Style::default().bg(th.selection_bg)),
            tree_area,
        );
    }

    // VSCode-style "create at root" injection: when the user clicks
    // the toolbar `+ File` with no folder selected, anchor_idx is None
    // and we render the editable row right at the top of the tree.
    let edit_active = app.tree_edit.active;
    let edit_mode = app.tree_edit.mode;
    let edit_anchor = app.tree_edit.anchor_idx;
    let is_create_mode = matches!(
        edit_mode,
        Some(TreeEditMode::NewFile) | Some(TreeEditMode::NewFolder)
    );

    if edit_active && is_create_mode && edit_anchor.is_none() && visual_y < max_y {
        render_edit_row(f, app, tree_area.x, visual_y, tree_area.width, 0);
        visual_y = visual_y.saturating_add(1);
        if app.tree_edit.has_error() && visual_y < max_y {
            render_edit_error(f, app, tree_area.x, visual_y, tree_area.width);
            visual_y = visual_y.saturating_add(1);
        }
    }

    // Iterate by index so we never hold a shared borrow on entries
    // across calls that need `&mut app` (render_entry_row,
    // render_edit_row). Clone the one entry we need per iteration;
    // `TreeEntry` is small and the clone is cheap.
    for visual_i in 0..entries_len.saturating_sub(scroll) {
        if visual_y >= max_y {
            break;
        }
        let global_idx = scroll + visual_i;
        let Some(entry) = app.file_tree.entries.get(global_idx).cloned() else {
            break;
        };

        let is_rename_target = edit_active
            && matches!(edit_mode, Some(TreeEditMode::Rename))
            && edit_anchor == Some(global_idx);

        if is_rename_target {
            let rename_depth = entry.depth;
            render_edit_row(f, app, tree_area.x, visual_y, tree_area.width, rename_depth);
            visual_y = visual_y.saturating_add(1);
            if app.tree_edit.has_error() && visual_y < max_y {
                render_edit_error(f, app, tree_area.x, visual_y, tree_area.width);
                visual_y = visual_y.saturating_add(1);
            }
        } else {
            render_entry_row(
                f,
                app,
                &entry,
                global_idx,
                tree_area.x,
                visual_y,
                tree_area.width,
                area,
                drag_hover_target.as_ref(),
            );
            visual_y = visual_y.saturating_add(1);
        }

        let inject_after =
            edit_active && is_create_mode && edit_anchor == Some(global_idx) && visual_y < max_y;
        if inject_after {
            let inject_depth = entry.depth + if entry.is_dir { 1 } else { 0 };
            render_edit_row(f, app, tree_area.x, visual_y, tree_area.width, inject_depth);
            visual_y = visual_y.saturating_add(1);
            if app.tree_edit.has_error() && visual_y < max_y {
                render_edit_error(f, app, tree_area.x, visual_y, tree_area.width);
                visual_y = visual_y.saturating_add(1);
            }
        }
    }
}

/// Render the 4-button Files-tab toolbar. Icons sit at fixed cell
/// offsets from the panel's left edge; each button registers its
/// ClickAction with the hit registry so the main mouse pipeline
/// dispatches through the normal handle_action path.
///
/// Layout: `[+ File] [+ Folder]  [↻]  [⊟]` — icons with abbreviated
/// labels; the labels get dropped first when the panel is narrow so
/// only icons show on cramped widths. The right-side pair (refresh /
/// collapse) sit flush-left-of-border so the visual weight stays
/// balanced.
fn render_toolbar(f: &mut Frame, app: &mut App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let th = app.theme;

    // Paint the full-width background first so the buttons sit on an
    // even chrome band instead of whatever happens to be underneath.
    let bg = Line::from(Span::styled(
        " ".repeat(area.width as usize),
        Style::default().bg(th.chrome_bg),
    ));
    f.render_widget(bg, area);

    // Narrow panel → icon-only. We consider anything under 28 cols
    // narrow (roughly the default 30% split at 80 cols = 24 cells;
    // need headroom for the button separators).
    let compact = area.width < 28;
    let btn_new_file = if compact {
        " + ".to_string()
    } else {
        format!(" + {} ", crate::i18n::tree_toolbar_new_file())
    };
    let btn_new_folder = if compact {
        " 📁 ".to_string()
    } else {
        format!(" 📁 {} ", crate::i18n::tree_toolbar_new_folder())
    };
    let btn_refresh = " ↻ ".to_string();
    let btn_collapse = " ⊟ ".to_string();

    let style_btn = Style::default()
        .fg(th.chrome_fg)
        .bg(th.chrome_bg)
        .add_modifier(Modifier::BOLD);
    let style_sep = Style::default().fg(th.chrome_muted_fg).bg(th.chrome_bg);

    let buttons: [(String, ClickAction); 4] = [
        (btn_new_file, ClickAction::FileTreeToolbarNewFile),
        (btn_new_folder, ClickAction::FileTreeToolbarNewFolder),
        (btn_refresh, ClickAction::FileTreeToolbarRefresh),
        (btn_collapse, ClickAction::FileTreeToolbarCollapse),
    ];

    let mut x = area.x;
    let max_x = area.x + area.width;
    let mut spans: Vec<Span> = Vec::new();
    for (i, (label, action)) in buttons.iter().enumerate() {
        let w = unicode_width::UnicodeWidthStr::width(label.as_str()) as u16;
        if x + w > max_x {
            break;
        }
        // Hover highlight: bump the background on the button under the
        // cursor so the user can tell what's clickable.
        let is_hovered = !app.tree_context_menu.active
            && app.hover_row == Some(area.y)
            && app.hover_col.map(|c| c >= x && c < x + w).unwrap_or(false);
        let bg = if is_hovered {
            th.chrome_active_bg
        } else {
            th.chrome_bg
        };
        spans.push(Span::styled(label.clone(), style_btn.bg(bg)));
        app.hit_registry.register_row(x, area.y, w, action.clone());
        x += w;

        // Cheap gutter between buttons so they don't run together
        // visually; only the last button has no separator after it.
        if i < buttons.len() - 1 && x < max_x {
            spans.push(Span::styled(" ", style_sep));
            x += 1;
        }
    }
    // Tail-fill to the right edge so the chrome band covers the whole
    // toolbar width even if the buttons didn't use it all.
    if x < max_x {
        let rest = (max_x - x) as usize;
        spans.push(Span::styled(
            " ".repeat(rest),
            Style::default().bg(th.chrome_bg),
        ));
    }
    f.render_widget(Line::from(spans), area);
}

/// Render a single file-tree entry row. Extracted from the main loop
/// so the edit-row injection path can live alongside without blowing
/// out the function length.
#[allow(clippy::too_many_arguments)]
fn render_entry_row(
    f: &mut Frame,
    app: &mut App,
    entry: &crate::file_tree::TreeEntry,
    global_idx: usize,
    x: u16,
    y: u16,
    width: u16,
    panel_area: Rect,
    drag_hover_target: Option<&crate::place_mode::HoverTarget>,
) {
    let th = app.theme;
    let is_selected = global_idx == app.file_tree.selected;
    let in_multi_selection = app.file_selection.contains(&entry.path);
    // Suppress hover on tree rows while a context menu overlay is open.
    // The menu sits ABOVE the tree but narrower, and `hover_row` is a
    // single global coord — without this guard the tree row underneath
    // the currently-hovered menu item ends up painting `hover_bg` on
    // the strips the popup doesn't cover.
    let is_hovered = !app.tree_context_menu.active
        && app.hover_row == Some(y)
        && app
            .hover_col
            .map(|c| c >= panel_area.x && c < panel_area.x + panel_area.width)
            .unwrap_or(false);
    // Highlight the drag-target folder block: every row inside the
    // hovered folder's expanded range gets the accent bg so the user
    // sees what the drop will land in. Mirrors `place_mode`'s
    // RowMode::InBlock visual but applied to the regular tree path.
    let in_drag_block = drag_hover_target
        .map(|t| t.contains_row(global_idx))
        .unwrap_or(false);
    // `drag_hover_target == Some(Root)` means the drop will land at
    // the workspace root; idle rows paint `selection_bg` so the
    // whole panel reads as a single armed surface.
    let drag_root_hover = matches!(
        drag_hover_target,
        Some(crate::place_mode::HoverTarget::Root)
    );

    let indent = "  ".repeat(entry.depth);
    let icon = if entry.is_dir {
        if entry.is_expanded { "▾ " } else { "▸ " }
    } else {
        "  "
    };

    let mut name_style = if entry.is_dir {
        Style::default().fg(th.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(th.fg_primary)
    };
    // Cut-clipboard rows render dimmed so the user sees at a glance
    // which sources will move on the next Paste. Copy mode does not
    // dim — the source stays in place. Computed directly off the
    // clipboard rather than stamped onto entries up front; for the
    // ~50 rows visible at a time × small clipboards this stays a
    // few hundred path comparisons per frame.
    if app.file_clipboard.is_cut() && app.file_clipboard.contains(&entry.path) {
        name_style = name_style.add_modifier(Modifier::DIM);
    }

    let bg = if in_drag_block {
        th.accent
    } else if is_selected || in_multi_selection {
        th.selection_bg
    } else if is_hovered {
        th.hover_bg
    } else if drag_root_hover {
        // Drop target is the workspace root — every idle row paints
        // the same `selection_bg` as the backdrop so the panel reads
        // as a single armed surface (matches `place_mode`'s RootHover
        // visual). Without this, idle rows would `Color::Reset` over
        // the backdrop and break the unified look.
        th.selection_bg
    } else {
        Color::Reset
    };

    let mut spans = vec![
        Span::styled(indent.clone(), Style::default().bg(bg)),
        Span::styled(icon, Style::default().fg(th.fg_secondary).bg(bg)),
    ];
    let name_base_style =
        if is_selected || is_hovered || in_multi_selection || in_drag_block || drag_root_hover {
            name_style.bg(bg)
        } else {
            name_style
        };
    let (ranges, cur) = app.search.ranges_on_row(SearchTarget::FileTree, global_idx);
    if ranges.is_empty() {
        spans.push(Span::styled(entry.name.clone(), name_base_style));
    } else {
        let name_tokens = vec![(name_base_style, entry.name.clone())];
        let overlaid = overlay_match_highlight(
            name_tokens,
            &ranges,
            cur,
            th.search_match,
            th.search_current,
        );
        for (style, text) in overlaid {
            spans.push(Span::styled(text, style));
        }
    }

    // Git status indicator
    if let Some(ch) = entry.git_status {
        let status_color = match ch {
            'M' => Color::Yellow,
            'A' => Color::Green,
            'D' => Color::Red,
            'U' | '?' => Color::Green,
            _ => th.fg_secondary,
        };
        spans.push(Span::styled(
            format!(" {}", ch),
            Style::default().fg(status_color).bg(bg),
        ));
    }

    let content_width: usize =
        indent.len() + icon.len() + entry.name.len() + entry.git_status.map(|_| 2).unwrap_or(0);
    let pad = (width as usize).saturating_sub(content_width);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    }

    let line = Line::from(spans);
    f.render_widget(line, Rect::new(x, y, width, 1));

    app.hit_registry.register_row(
        panel_area.x,
        y,
        panel_area.width,
        ClickAction::TreeClick(global_idx),
    );
}

/// Render the inline editable row (either a fresh Create, rendered
/// INSERTED into the tree, or a Rename, rendered as a replacement of
/// the target entry). Uses an indented row prefix so the editor sits
/// at the correct depth, and paints the cursor as a reversed cell at
/// the buffer's current insertion point.
fn render_edit_row(f: &mut Frame, app: &mut App, x: u16, y: u16, width: u16, depth: usize) {
    let th = app.theme;
    let mode = app.tree_edit.mode.unwrap_or(TreeEditMode::Rename);
    let icon = match mode {
        TreeEditMode::NewFolder => "▸ ",
        // NewFile / Rename both use the plain-file icon. Rename keeps
        // whatever the target entry originally was — but we deliberately
        // simplify here because the icon is informational, not critical.
        _ => "  ",
    };
    let indent = "  ".repeat(depth);
    let buffer = &app.tree_edit.buffer;
    let cursor = app.tree_edit.cursor.min(buffer.len());
    let placeholder = crate::i18n::tree_edit_placeholder(mode);

    // Row background — the edit row stands out from the rest of the
    // tree using a soft selection tint, which also makes the cursor
    // contrast read.
    let row_bg = th.selection_bg;
    let base_style = Style::default().fg(th.fg_primary).bg(row_bg);
    let dim_style = Style::default().fg(th.fg_secondary).bg(row_bg);
    let cursor_style = base_style.add_modifier(Modifier::REVERSED);

    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::styled(indent.clone(), base_style));
    spans.push(Span::styled(icon, dim_style));

    let mut used = unicode_width::UnicodeWidthStr::width(indent.as_str())
        + unicode_width::UnicodeWidthStr::width(icon);

    if buffer.is_empty() {
        // Render the placeholder with the cursor painted on the
        // leading cell so the user sees where typing will go.
        spans.push(Span::styled(" ", cursor_style));
        spans.push(Span::styled(placeholder, dim_style));
        used += 1 + unicode_width::UnicodeWidthStr::width(placeholder);
    } else {
        // Split the buffer at the cursor so the cell under the cursor
        // gets the reversed style. If the cursor is at the end of the
        // buffer, paint a trailing reversed space so the caret is
        // still visible.
        let (before, rest) = buffer.split_at(cursor);
        if !before.is_empty() {
            spans.push(Span::styled(before.to_string(), base_style));
            used += unicode_width::UnicodeWidthStr::width(before);
        }
        if rest.is_empty() {
            spans.push(Span::styled(" ", cursor_style));
            used += 1;
        } else {
            // Grab the first char under the cursor. We know the buffer
            // is valid UTF-8 so `chars().next()` is safe.
            let mut chars = rest.chars();
            let cur_ch = chars.next().unwrap();
            let cur_str = cur_ch.to_string();
            let rest_after = chars.as_str();
            used += unicode_width::UnicodeWidthChar::width(cur_ch).unwrap_or(1);
            spans.push(Span::styled(cur_str, cursor_style));
            if !rest_after.is_empty() {
                spans.push(Span::styled(rest_after.to_string(), base_style));
                used += unicode_width::UnicodeWidthStr::width(rest_after);
            }
        }
    }

    // Fill to the right edge so the tint covers the full row width.
    if (width as usize) > used {
        let pad = width as usize - used;
        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(row_bg)));
    }

    f.render_widget(Line::from(spans), Rect::new(x, y, width, 1));
}

/// Render the validation-error banner below the edit row (only shown
/// when `tree_edit.has_error()`). Uses a dim red background so the
/// user reads it as "something went wrong, fix and retry" without the
/// alarm level of an error toast.
fn render_edit_error(f: &mut Frame, app: &App, x: u16, y: u16, width: u16) {
    let Some(err) = app.tree_edit.error.clone() else {
        return;
    };
    let th = app.theme;
    let text = format!("  ✖ {}", crate::i18n::tree_edit_error(&err));
    let style = Style::default().fg(Color::White).bg(th.error_bg);
    let used = unicode_width::UnicodeWidthStr::width(text.as_str());
    let mut padded = text.clone();
    if (width as usize) > used {
        padded.push_str(&" ".repeat(width as usize - used));
    }
    f.render_widget(
        Line::from(Span::styled(padded, style)),
        Rect::new(x, y, width, 1),
    );
}

// ─── Place-mode rendering ────────────────────────────────────────────────────
//
// Renders the file tree as a VSCode-style drag-and-drop destination picker.
// Hard-swapped from `render_normal` when `app.place_mode.active`. Shares the
// same rect layout so the panel slot doesn't shift mid-interaction.
//
// Visual contract:
// - Full double-line accent border around the whole tree panel. This is
//   *the* root drop zone: any click inside the panel that doesn't hit a
//   folder row drops into the project root.
// - A top-inset banner line showing "Placing <name> → click folder, Esc to
//   cancel", styled with the accent background so it reads as modal.
// - Folder rows get an accent-colored bold label and highlight-on-hover
//   with `selection_bg`; they register `ClickAction::PlaceModeFolder(idx)`.
// - File rows are dimmed and register no click zone, so clicks fall
//   through to the underlying `PlaceModeRoot` zone (= drop to root).
//
// Hit-registry ordering matters: we register the panel-wide `PlaceModeRoot`
// zone FIRST, then per-folder zones on top. `HitTestRegistry::hit_test`
// returns the last-registered match, so folder rows shadow the root zone
// cleanly.

fn render_place_mode(f: &mut Frame, app: &mut App, area: Rect, _focused: bool) {
    use crate::place_mode::{HoverTarget, resolve_hover_target};
    let th = app.theme;

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(th.accent));
    let inner = block.inner(area);
    f.render_widget(block, area);

    for y in area.y..area.y + area.height {
        app.hit_registry
            .register_row(area.x, y, area.width, ClickAction::PlaceModeRoot);
    }

    let banner_text = if app.file_copy_load.loading {
        crate::i18n::place_mode_copying_banner()
    } else {
        crate::i18n::place_mode_banner(&app.place_mode.primary_name(), app.place_mode.count())
    };
    let banner_style = Style::default()
        .fg(th.chrome_bg)
        .bg(th.accent)
        .add_modifier(Modifier::BOLD);
    let banner_width = inner.width as usize;
    let used = unicode_width::UnicodeWidthStr::width(banner_text.as_str());
    let mut banner_line = banner_text.clone();
    if used < banner_width {
        banner_line.push_str(&" ".repeat(banner_width - used));
    }
    f.render_widget(
        Line::from(Span::styled(banner_line.clone(), banner_style)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let tree_area = Rect::new(
        inner.x,
        inner.y.saturating_add(1),
        inner.width,
        inner.height.saturating_sub(1),
    );
    let padded = Rect::new(
        tree_area.x + 1,
        tree_area.y,
        tree_area.width.saturating_sub(1),
        tree_area.height,
    );

    let cursor_in_panel = app
        .hover_col
        .map(|c| c >= area.x && c < area.x + area.width)
        .zip(
            app.hover_row
                .map(|r| r >= area.y && r < area.y + area.height),
        )
        .map(|(a, b)| a && b)
        .unwrap_or(false);

    let scroll = {
        let max_scroll = app
            .file_tree
            .entries
            .len()
            .saturating_sub(padded.height as usize);
        let s = app.tree_scroll.min(max_scroll);
        app.tree_scroll = s;
        s
    };
    let max_y = padded.y + padded.height;

    let hovered_entry_idx = if cursor_in_panel {
        app.hover_row.and_then(|r| {
            if r >= padded.y && r < max_y {
                let visual = (r - padded.y) as usize;
                let idx = scroll + visual;
                if idx < app.file_tree.entries.len() {
                    Some(idx)
                } else {
                    None
                }
            } else {
                None
            }
        })
    } else {
        None
    };

    let hover_target = match hovered_entry_idx {
        Some(idx) => resolve_hover_target(&app.file_tree.entries, idx),
        None if cursor_in_panel => HoverTarget::Root,
        None => HoverTarget::Root,
    };

    let active_folder_idx = match &hover_target {
        HoverTarget::Folder { folder_idx, .. } if cursor_in_panel => Some(*folder_idx),
        _ => None,
    };
    app.place_mode.update_hover(active_folder_idx);

    let root_hover = cursor_in_panel && matches!(hover_target, HoverTarget::Root);
    if root_hover {
        f.render_widget(
            Block::default().style(Style::default().bg(th.selection_bg)),
            inner,
        );
        f.render_widget(
            Line::from(Span::styled(banner_line.clone(), banner_style)),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
    }

    let entries = &app.file_tree.entries;
    if entries.is_empty() {
        let msg = Line::from(Span::styled(
            t(Msg::EmptyDir),
            Style::default().fg(th.fg_secondary),
        ));
        f.render_widget(msg, Rect::new(padded.x, padded.y, padded.width, 1));
        return;
    }

    for (i, entry) in entries.iter().skip(scroll).enumerate() {
        let y = padded.y + i as u16;
        if y >= max_y {
            break;
        }
        let global_idx = scroll + i;

        let in_block = hover_target.contains_row(global_idx);

        let indent = "  ".repeat(entry.depth);
        let icon = if entry.is_dir {
            if entry.is_expanded { "▾ " } else { "▸ " }
        } else {
            "  "
        };

        enum RowMode {
            InBlock,
            RootHover,
            Idle,
        }
        let mode = if in_block {
            RowMode::InBlock
        } else if root_hover {
            RowMode::RootHover
        } else {
            RowMode::Idle
        };

        let row_bg = match mode {
            RowMode::InBlock => Some(th.accent),
            RowMode::RootHover => Some(th.selection_bg),
            RowMode::Idle => None,
        };
        let bg_style = match row_bg {
            Some(c) => Style::default().bg(c),
            None => Style::default(),
        };

        let name_fg = match mode {
            RowMode::InBlock => th.chrome_bg,
            _ => {
                if entry.is_dir {
                    th.accent
                } else {
                    th.fg_secondary
                }
            }
        };
        let icon_fg = match mode {
            RowMode::InBlock => th.chrome_bg,
            _ => th.fg_secondary,
        };
        let mut name_style = Style::default().fg(name_fg);
        if entry.is_dir {
            name_style = name_style.add_modifier(Modifier::BOLD);
        }

        let mut spans = vec![
            Span::styled(indent.clone(), bg_style),
            Span::styled(icon, Style::default().fg(icon_fg).patch(bg_style)),
        ];
        spans.push(Span::styled(entry.name.clone(), name_style.patch(bg_style)));

        let content_width: usize = indent.len() + icon.len() + entry.name.len();
        let pad = (padded.width as usize).saturating_sub(content_width);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), bg_style));
        }

        let line = Line::from(spans);
        f.render_widget(line, Rect::new(padded.x, y, padded.width, 1));

        let action = match resolve_hover_target(entries, global_idx) {
            HoverTarget::Folder { folder_idx, .. } => {
                Some(ClickAction::PlaceModeFolder(folder_idx))
            }
            HoverTarget::Root => None,
        };
        if let Some(a) = action {
            app.hit_registry.register_row(area.x, y, area.width, a);
        }
    }
}
