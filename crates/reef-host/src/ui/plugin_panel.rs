use crate::app::App;
use crate::mouse::ClickAction;
use crate::renderer::to_ratatui_line;
use ratatui::layout::Rect;
use ratatui::Frame;

/// Render a plugin panel identified by `panel_id` into `area`.
/// Requests a new render from the plugin if needed, and displays the cached result.
pub fn render(f: &mut Frame, app: &mut App, area: Rect, panel_id: &str, focused: bool) {
    // Request render from plugin if the panel needs it
    let needs = app.plugin_manager.panels.iter()
        .find(|p| p.decl.id == panel_id)
        .map(|p| p.needs_render)
        .unwrap_or(false);

    if needs {
        app.plugin_manager.request_render(panel_id, area.width, area.height, focused);
    }

    // Grab the cached lines (clone to release borrow on plugin_manager)
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

        let line = to_ratatui_line(styled_line);
        f.render_widget(line, Rect::new(area.x, y, area.width, 1));

        // Register click zone if this line has a command
        if let Some(ref cmd) = styled_line.click_command {
            let args = styled_line.click_args.clone().unwrap_or(serde_json::Value::Null);
            app.hit_registry.register_row(
                area.x,
                y,
                area.width,
                ClickAction::PluginCommand {
                    command: cmd.clone(),
                    args,
                },
            );
        }
    }
}
