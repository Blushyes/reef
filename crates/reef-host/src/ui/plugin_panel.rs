use crate::app::App;
use crate::mouse::ClickAction;
use crate::renderer::to_ratatui_line;
use ratatui::layout::Rect;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

/// Render a plugin panel identified by `panel_id` into `area`.
pub fn render(f: &mut Frame, app: &mut App, area: Rect, panel_id: &str, focused: bool) {
    let is_diff_panel = app.plugin_manager.panels.iter()
        .find(|p| p.decl.id == panel_id)
        .map(|p| p.decl.slot == reef_protocol::PanelSlot::Editor)
        .unwrap_or(false);

    // Determine current scroll and clamp it using total_lines from the last render.
    let raw_scroll = if is_diff_panel { app.diff_scroll } else { app.file_scroll };
    let total_lines = app.plugin_manager.panels.iter()
        .find(|p| p.decl.id == panel_id)
        .and_then(|p| p.last_render.as_ref())
        .map(|r| r.total_lines)
        .unwrap_or(usize::MAX); // unknown → don't clamp yet
    let clamped = raw_scroll.min(total_lines.saturating_sub(area.height as usize));
    if is_diff_panel {
        app.diff_scroll = clamped;
    } else {
        app.file_scroll = clamped;
    }
    let scroll = clamped as u32;

    // Request render if content is stale OR the scroll offset changed.
    let needs = app.plugin_manager.panels.iter()
        .find(|p| p.decl.id == panel_id)
        .map(|p| p.needs_render || p.last_render_scroll != scroll)
        .unwrap_or(false);

    if needs {
        app.plugin_manager.request_render(panel_id, area.width, area.height, focused, scroll);
    }

    // Grab cached lines — plugin already applied the scroll offset, render directly.
    let lines: Vec<_> = app.plugin_manager.panels.iter()
        .find(|p| p.decl.id == panel_id)
        .and_then(|p| p.last_render.as_ref())
        .map(|r| r.lines.clone())
        .unwrap_or_default();

    let max_y = area.y + area.height;
    for (i, styled_line) in lines.iter().enumerate() {
        let y = area.y + i as u16;
        if y >= max_y {
            break;
        }

        let hover = app.hover_row == Some(y)
            && app.hover_col.map(|c| c >= area.x && c < area.x + area.width).unwrap_or(false);
        let line = to_ratatui_line(styled_line, hover);
        f.render_widget(line, Rect::new(area.x, y, area.width, 1));

        // Register click zones per-span; span-level click overrides line-level for that region
        let mut span_x = area.x;
        for span in &styled_line.spans {
            let span_w = UnicodeWidthStr::width(span.text.as_str()) as u16;
            if span_w > 0 {
                let action = if let Some(ref cmd) = span.click_command {
                    Some(ClickAction::PluginCommand {
                        command: cmd.clone(),
                        args: span.click_args.clone().unwrap_or(serde_json::Value::Null),
                    })
                } else if let Some(ref cmd) = styled_line.click_command {
                    Some(ClickAction::PluginCommand {
                        command: cmd.clone(),
                        args: styled_line.click_args.clone().unwrap_or(serde_json::Value::Null),
                    })
                } else {
                    None
                };
                if let Some(action) = action {
                    app.hit_registry.register_row(span_x, y, span_w, action);
                }
            }
            span_x = span_x.saturating_add(span_w);
        }
    }
}
