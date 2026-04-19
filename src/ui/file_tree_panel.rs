use crate::app::App;
use crate::i18n::{Msg, t};
use crate::search::SearchTarget;
use crate::ui::mouse::ClickAction;
use crate::ui::text::overlay_match_highlight;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    if app.place_mode.active {
        render_place_mode(f, app, area);
    } else {
        render_normal(f, app, area);
    }
}

fn render_normal(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(th.border));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let padded = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(1),
        inner.height,
    );

    let entries = &app.file_tree.entries;
    if entries.is_empty() {
        let msg = Line::from(Span::styled(
            t(Msg::EmptyDir),
            Style::default().fg(th.fg_secondary),
        ));
        f.render_widget(msg, Rect::new(padded.x, padded.y, padded.width, 1));
        return;
    }

    // Clamp scroll to valid range
    let max_scroll = entries.len().saturating_sub(padded.height as usize);
    app.tree_scroll = app.tree_scroll.min(max_scroll);

    // Keep the selection visible, but only when the selection actually moved
    // since the last render. Running this every frame meant mouse-wheel scroll
    // (which only changes tree_scroll, not selected) got snapped back to the
    // opened file on the next tick — so the scroll appeared locked. See #10.
    let selection_changed = app.last_rendered_tree_selected != Some(app.file_tree.selected);
    if selection_changed {
        if app.file_tree.selected < app.tree_scroll {
            app.tree_scroll = app.file_tree.selected;
        } else if app.file_tree.selected >= app.tree_scroll + padded.height as usize {
            app.tree_scroll = app
                .file_tree
                .selected
                .saturating_sub(padded.height as usize - 1);
        }
        app.last_rendered_tree_selected = Some(app.file_tree.selected);
    }

    let scroll = app.tree_scroll;
    let max_y = padded.y + padded.height;

    for (i, entry) in entries.iter().skip(scroll).enumerate() {
        let y = padded.y + i as u16;
        if y >= max_y {
            break;
        }
        let global_idx = scroll + i;
        let is_selected = global_idx == app.file_tree.selected;
        let is_hovered = app.hover_row == Some(y)
            && app
                .hover_col
                .map(|c| c >= area.x && c < area.x + area.width)
                .unwrap_or(false);

        let indent = "  ".repeat(entry.depth);
        let icon = if entry.is_dir {
            if entry.is_expanded { "▾ " } else { "▸ " }
        } else {
            "  "
        };

        let name_style = if entry.is_dir {
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.fg_primary)
        };

        let bg = if is_selected {
            th.selection_bg
        } else if is_hovered {
            th.hover_bg
        } else {
            Color::Reset
        };

        let mut spans = vec![
            Span::styled(indent.clone(), Style::default().bg(bg)),
            Span::styled(icon, Style::default().fg(th.fg_secondary).bg(bg)),
        ];
        // Overlay search highlights onto the filename span. Collect_rows for
        // the FileTree target emits `entry.name` — byte ranges returned by
        // `ranges_on_row` are directly applicable here.
        let name_base_style = if is_selected || is_hovered {
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

        // Pad remainder
        let content_width: usize =
            indent.len() + icon.len() + entry.name.len() + entry.git_status.map(|_| 2).unwrap_or(0);
        let pad = (padded.width as usize).saturating_sub(content_width);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
        }

        let line = Line::from(spans);
        f.render_widget(line, Rect::new(padded.x, y, padded.width, 1));

        // Register click zone
        app.hit_registry
            .register_row(area.x, y, area.width, ClickAction::TreeClick(global_idx));
    }
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

fn render_place_mode(f: &mut Frame, app: &mut App, area: Rect) {
    use crate::place_mode::{HoverTarget, resolve_hover_target};
    let th = app.theme;

    // Double-line accent border — the visual cue for "this entire panel is
    // the root drop zone". Rendering the block consumes one cell on each
    // side; `inner` is what remains for content.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(th.accent));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Panel-wide root drop zone. Registered first so folder/nested-file
    // zones can shadow it. Covers the full `area` so there's no dead
    // strip where clicks vanish.
    for y in area.y..area.y + area.height {
        app.hit_registry
            .register_row(area.x, y, area.width, ClickAction::PlaceModeRoot);
    }

    // Banner line, inset 1 from the top of the inner area. The accent bg
    // + contrast fg make this a high-visibility "modal active" marker —
    // the previous `selection_bg` tint blended in with the rest of the UI
    // so users couldn't tell they were in place mode. `unicode_width`
    // (not `.chars().count()`) is load-bearing because the banner holds
    // a wide emoji; char count would under-pad the right edge by a cell.
    //
    // While a copy worker is actually running, swap the banner to a
    // "⋯ Copying…" indicator so large directory copies show evidence of
    // life. Without this, the UI looks identical to "nothing happened"
    // for the duration of a 10GB folder copy.
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

    // Tree content starts one row below the banner.
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

    // ── Compute the current hover target ──
    //
    // `hover_row` / `hover_col` are written by the last mouse Moved event.
    // Translate that back to an entry index (taking scroll into account),
    // then let `resolve_hover_target` figure out which block (if any) the
    // click would land in.
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
        None => HoverTarget::Root, // cursor elsewhere — no highlight applied below
    };

    // Drive the auto-expand tracker. Only folder targets count; root-target
    // hovers shouldn't keep a timer running for the most recent folder.
    let active_folder_idx = match &hover_target {
        HoverTarget::Folder { folder_idx, .. } if cursor_in_panel => Some(*folder_idx),
        _ => None,
    };
    app.place_mode.update_hover(active_folder_idx);

    // ── Root-hover whole-panel fill ──
    //
    // User asked for "hovering empty space highlights the entire left side
    // as a signal for root-drop". We paint a bright tint over the whole
    // interior when the hover target is Root AND the cursor is inside the
    // panel — otherwise the modal is quiet until the mouse enters. Using
    // `selection_bg` (a clearly-blue tint) rather than `hover_bg` (which
    // in dark theme is almost indistinguishable from the default bg) so
    // the "drop-at-root" state reads at a glance.
    let root_hover = cursor_in_panel && matches!(hover_target, HoverTarget::Root);
    if root_hover {
        f.render_widget(
            Block::default().style(Style::default().bg(th.selection_bg)),
            inner,
        );
        // Re-draw the banner on top so the fill doesn't cover it.
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

        // Row bg + fg are a package deal in place mode. We pick a mode
        // based on where this row sits relative to the current hover:
        //
        // - `in_block` (hovered folder block)   → accent bg + chrome-bg fg.
        //   Maximum contrast: whatever the theme's loud colour is becomes
        //   a solid bar behind the block, with dark-on-light (or
        //   light-on-dark) text. This is what the user reaches for.
        // - `root_hover` (whole panel drop-to-root)
        //                                       → selection_bg + normal fg.
        //   Softer than the block highlight so the two modes are
        //   visually distinct — the user can tell "I'm dropping into THIS
        //   folder" vs. "I'm dropping to the project root".
        // - everything else                     → no bg, dimmed fg for
        //   files, accent fg for folders. The row just sits there.
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
            RowMode::InBlock => th.chrome_bg, // high-contrast on the accent bg
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

        // Per-row click zone:
        // - Folder row → dropping lands in that folder.
        // - Nested file (depth > 0) → lands in its parent folder.
        // - Top-level file (depth 0) → no zone, falls through to the
        //   panel-wide `PlaceModeRoot` registered above.
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
