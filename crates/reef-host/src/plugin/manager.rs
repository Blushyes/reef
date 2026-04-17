use super::process::PluginProcess;
use reef_protocol::{
    CommandParams, EventParams, InitializeParams, NotifyParams, PanelDecl, PanelSlot,
    PluginManifest, RenderParams, RenderResult, RpcMessage,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A plugin panel registered by a plugin, with cached render result.
pub struct ManagedPanel {
    pub decl: PanelDecl,
    pub plugin_name: String,
    pub last_render: Option<RenderResult>,
    pub needs_render: bool,
    /// Scroll offset used in the most recent render request.
    /// u32::MAX means "never rendered" — guarantees first render always fires.
    pub last_render_scroll: u32,
}

/// Pending plugin→host request that needs a response.
pub struct PendingRequest {
    pub plugin_name: String,
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
}

pub struct HelpEntry {
    pub key: String,
    pub description: String,
    pub plugin_name: String,
}

pub struct PluginManager {
    processes: HashMap<String, PluginProcess>,
    pub panels: Vec<ManagedPanel>, // ordered: sidebar panels in registration order
    /// Events raised by plugins for the host to act on this frame.
    pub pending_host_requests: Vec<PendingRequest>,
    pub notifications: Vec<NotifyParams>,
    /// Set when a plugin signals git status has changed (staging/unstaging/refresh).
    pub status_refresh_needed: bool,
    /// Help entries contributed by plugins via keybinding descriptions in their manifest.
    pub help_entries: Vec<HelpEntry>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self {
            processes: HashMap::new(),
            panels: Vec::new(),
            pending_host_requests: Vec::new(),
            notifications: Vec::new(),
            status_refresh_needed: false,
            help_entries: Vec::new(),
        }
    }

    /// Discover and load all plugins from a directory.
    /// Each subdirectory with a reef.json is considered a plugin.
    pub fn load_from_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let manifest_path = entry.path().join("reef.json");
            if manifest_path.exists() {
                if let Err(e) = self.load_plugin(&manifest_path) {
                    eprintln!("[reef] failed to load plugin at {:?}: {}", manifest_path, e);
                }
            }
        }
    }

    /// Load a single plugin from its manifest path.
    pub fn load_plugin(&mut self, manifest_path: &Path) -> std::io::Result<()> {
        let content = std::fs::read_to_string(manifest_path)?;
        let manifest: PluginManifest = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let plugin_dir = manifest_path.parent().unwrap();
        let exe = resolve_exe(&manifest.main, plugin_dir);

        let mut proc = PluginProcess::spawn(&manifest.name, &exe)?;

        // Send initialize
        let params = serde_json::to_value(InitializeParams {
            reef_version: env!("CARGO_PKG_VERSION").to_string(),
        })
        .unwrap();
        proc.send_request("reef/initialize", params)?;

        // Register panels declared in manifest
        for panel in &manifest.contributes.panels {
            self.panels.push(ManagedPanel {
                decl: panel.clone(),
                plugin_name: manifest.name.clone(),
                last_render: None,
                needs_render: true,
                last_render_scroll: u32::MAX,
            });
        }

        // Collect keybindings that have a description into the help registry
        for kb in &manifest.contributes.keybindings {
            if let Some(desc) = &kb.description {
                self.help_entries.push(HelpEntry {
                    key: kb.key.clone(),
                    description: desc.clone(),
                    plugin_name: manifest.name.clone(),
                });
            }
        }

        self.processes.insert(manifest.name.clone(), proc);
        Ok(())
    }

    /// Called each frame: drain all plugin messages and update state.
    pub fn tick(&mut self) {
        self.pending_host_requests.clear();
        self.notifications.clear();
        self.status_refresh_needed = false;

        let names: Vec<String> = self.processes.keys().cloned().collect();
        for name in names {
            let Some(proc) = self.processes.get_mut(&name) else {
                continue;
            };
            let messages = proc.drain_messages();
            for msg in messages {
                self.handle_message(&name.clone(), msg);
            }
        }
    }

    fn handle_message(&mut self, plugin_name: &str, msg: RpcMessage) {
        if msg.is_response() {
            if let Some(result_val) = &msg.result {
                // Try to deserialize as RenderResult
                if let Ok(render_result) =
                    serde_json::from_value::<RenderResult>(result_val.clone())
                {
                    for panel in &mut self.panels {
                        if panel.plugin_name == plugin_name
                            && panel.decl.id == render_result.panel_id
                        {
                            panel.last_render = Some(render_result);
                            panel.needs_render = false;
                            break;
                        }
                    }
                }
            }
            return;
        }

        // Plugin-initiated requests / notifications
        let method = msg.method.as_str();
        let params = msg.params.clone().unwrap_or(serde_json::Value::Null);

        match method {
            "reef/requestRender" => {
                if let Some(panel_id) = params.get("panel_id").and_then(|v| v.as_str()) {
                    for panel in &mut self.panels {
                        if panel.plugin_name == plugin_name && panel.decl.id == panel_id {
                            panel.needs_render = true;
                            break;
                        }
                    }
                }
            }
            // Plugin signals git state changed (after stage/unstage, or fs watcher).
            "reef/statusChanged" => {
                self.status_refresh_needed = true;
                // Invalidate the sender's panels so they re-render with fresh state.
                for panel in &mut self.panels {
                    if panel.plugin_name == plugin_name {
                        panel.needs_render = true;
                    }
                }
            }
            "reef/notify" => {
                if let Ok(n) = serde_json::from_value::<NotifyParams>(params) {
                    self.notifications.push(n);
                }
            }
            "reef/openFile" | "reef/executeCommand" => {
                if let Some(id) = msg.id {
                    self.pending_host_requests.push(PendingRequest {
                        plugin_name: plugin_name.to_string(),
                        id,
                        method: method.to_string(),
                        params: msg.params.unwrap_or_default(),
                    });
                }
            }
            _ => {}
        }
    }

    /// Request a render from the plugin owning `panel_id`.
    pub fn request_render(
        &mut self,
        panel_id: &str,
        width: u16,
        height: u16,
        focused: bool,
        scroll: u32,
    ) {
        let plugin_name = self
            .panels
            .iter()
            .find(|p| p.decl.id == panel_id)
            .map(|p| p.plugin_name.clone());

        if let Some(name) = plugin_name {
            if let Some(proc) = self.processes.get_mut(&name) {
                let params = serde_json::to_value(RenderParams {
                    panel_id: panel_id.to_string(),
                    width,
                    height,
                    focused,
                    scroll,
                })
                .unwrap();
                // Clear needs_render BEFORE sending so we don't flood the plugin
                // with duplicate render requests while waiting for the response.
                for panel in &mut self.panels {
                    if panel.plugin_name == name && panel.decl.id == panel_id {
                        panel.needs_render = false;
                        panel.last_render_scroll = scroll;
                        break;
                    }
                }
                let _ = proc.send_request("reef/render", params);
            }
        }
    }

    /// Forward a key event to the plugin owning `panel_id`.
    /// Returns true if the plugin consumed it.
    pub fn send_key_event(&mut self, panel_id: &str, key: &str, modifiers: Vec<String>) -> bool {
        let plugin_name = self
            .panels
            .iter()
            .find(|p| p.decl.id == panel_id)
            .map(|p| p.plugin_name.clone());

        if let Some(name) = plugin_name {
            if let Some(proc) = self.processes.get_mut(&name) {
                let params = serde_json::to_value(EventParams {
                    panel_id: panel_id.to_string(),
                    event: reef_protocol::InputEvent::Key {
                        key: key.to_string(),
                        modifiers,
                    },
                })
                .unwrap();
                let _ = proc.send_request("reef/event", params);
                // Optimistic: assume consumed; plugin can correct on next tick
                return true;
            }
        }
        false
    }

    /// Notify the plugin about file selection (for sidebar highlight).
    /// The plugin no longer handles diff rendering, so this is lightweight.
    pub fn queue_select_file(&mut self, path: &str, staged: bool) {
        self.execute_command(
            "git.selectFile",
            serde_json::json!({ "path": path, "staged": staged }),
        );
    }

    /// Execute a command on the plugin that registered it.
    pub fn execute_command(&mut self, command_id: &str, args: serde_json::Value) {
        // Find which plugin owns this command (by prefix matching plugin name)
        let plugin_name = command_id.split('.').next().map(|s| s.to_string());
        if let Some(name) = plugin_name {
            if let Some(proc) = self.processes.get_mut(&name) {
                let params = serde_json::to_value(CommandParams {
                    id: command_id.to_string(),
                    args,
                })
                .unwrap();
                let _ = proc.send_request("reef/command", params);
            }
        }
    }

    /// Respond to a pending plugin→host request.
    pub fn respond_to_plugin(&mut self, req: &PendingRequest, result: serde_json::Value) {
        if let Some(proc) = self.processes.get_mut(&req.plugin_name) {
            let _ = proc.send_response(req.id, result);
        }
    }

    /// Sidebar panels in order.
    pub fn sidebar_panels(&self) -> Vec<&ManagedPanel> {
        self.panels
            .iter()
            .filter(|p| p.decl.slot == PanelSlot::Sidebar)
            .collect()
    }

    /// Mark every panel as needing a re-render on the next frame.
    pub fn invalidate_panels(&mut self) {
        for panel in &mut self.panels {
            panel.needs_render = true;
        }
    }

    /// Send shutdown notification to all plugins.
    pub fn shutdown(&mut self) {
        for proc in self.processes.values_mut() {
            let _ = proc.send_notification("reef/shutdown", serde_json::Value::Null);
        }
    }
}

fn resolve_exe(main: &str, plugin_dir: &Path) -> String {
    let p = if main.starts_with("./") || main.starts_with("../") {
        plugin_dir.join(main)
    } else {
        PathBuf::from(main)
    };
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reef_protocol::{PanelDecl, PanelSlot, RpcMessage};

    fn sidebar_panel(id: &str, plugin: &str) -> ManagedPanel {
        ManagedPanel {
            decl: PanelDecl {
                id: id.into(),
                title: id.into(),
                slot: PanelSlot::Sidebar,
                icon: None,
            },
            plugin_name: plugin.into(),
            last_render: None,
            needs_render: false,
            last_render_scroll: u32::MAX,
        }
    }

    fn editor_panel(id: &str, plugin: &str) -> ManagedPanel {
        ManagedPanel {
            decl: PanelDecl {
                id: id.into(),
                title: id.into(),
                slot: PanelSlot::Editor,
                icon: None,
            },
            plugin_name: plugin.into(),
            last_render: None,
            needs_render: false,
            last_render_scroll: u32::MAX,
        }
    }

    // ── new ──────────────────────────────────────────────────────────────────

    #[test]
    fn new_is_empty() {
        let m = PluginManager::new();
        assert!(m.panels.is_empty());
        assert!(m.pending_host_requests.is_empty());
        assert!(m.notifications.is_empty());
        assert!(!m.status_refresh_needed);
        assert!(m.help_entries.is_empty());
    }

    // ── sidebar_panels ───────────────────────────────────────────────────────

    #[test]
    fn sidebar_panels_filters_out_editor_panels() {
        let mut m = PluginManager::new();
        m.panels.push(sidebar_panel("git.status", "git"));
        m.panels.push(editor_panel("git.diff", "git"));
        m.panels.push(sidebar_panel("git.graph", "git"));
        let sidebar = m.sidebar_panels();
        assert_eq!(sidebar.len(), 2);
        assert!(sidebar.iter().all(|p| p.decl.slot == PanelSlot::Sidebar));
    }

    #[test]
    fn sidebar_panels_empty_when_no_panels() {
        let m = PluginManager::new();
        assert!(m.sidebar_panels().is_empty());
    }

    // ── invalidate_panels ────────────────────────────────────────────────────

    #[test]
    fn invalidate_panels_marks_all_needs_render() {
        let mut m = PluginManager::new();
        m.panels.push(sidebar_panel("p1", "plugin"));
        m.panels.push(sidebar_panel("p2", "plugin"));
        m.panels[0].needs_render = false;
        m.panels[1].needs_render = false;
        m.invalidate_panels();
        assert!(m.panels.iter().all(|p| p.needs_render));
    }

    // ── resolve_exe ──────────────────────────────────────────────────────────

    #[test]
    fn resolve_exe_relative_joined_with_dir() {
        let dir = Path::new("/plugins/myplugin");
        let result = resolve_exe("./plugin", dir);
        assert!(
            result.contains("myplugin"),
            "should be joined with plugin_dir"
        );
        assert!(result.contains("plugin"));
    }

    #[test]
    fn resolve_exe_parent_relative() {
        let dir = Path::new("/plugins/myplugin");
        let result = resolve_exe("../bin/plugin", dir);
        assert!(
            result.contains("plugins"),
            "parent-relative path joined with dir"
        );
    }

    #[test]
    fn resolve_exe_absolute_unchanged() {
        let dir = Path::new("/some/dir");
        let result = resolve_exe("/usr/bin/my-plugin", dir);
        assert_eq!(result, "/usr/bin/my-plugin");
    }

    // ── handle_message: reef/requestRender ───────────────────────────────────

    #[test]
    fn handle_message_request_render_marks_panel_needs_render() {
        let mut m = PluginManager::new();
        m.panels.push(sidebar_panel("git.status", "git"));
        m.panels[0].needs_render = false;

        let msg = RpcMessage {
            jsonrpc: "2.0".into(),
            id: None,
            method: "reef/requestRender".into(),
            params: Some(serde_json::json!({"panel_id": "git.status"})),
            result: None,
            error: None,
        };
        m.handle_message("git", msg);
        assert!(m.panels[0].needs_render);
    }

    #[test]
    fn handle_message_request_render_wrong_plugin_no_effect() {
        let mut m = PluginManager::new();
        m.panels.push(sidebar_panel("git.status", "git"));
        m.panels[0].needs_render = false;

        let msg = RpcMessage {
            jsonrpc: "2.0".into(),
            id: None,
            method: "reef/requestRender".into(),
            params: Some(serde_json::json!({"panel_id": "git.status"})),
            result: None,
            error: None,
        };
        // Message arrives from "other" plugin, not "git"
        m.handle_message("other", msg);
        assert!(
            !m.panels[0].needs_render,
            "wrong plugin should not mark panel"
        );
    }

    #[test]
    fn handle_message_status_changed_sets_flag_and_invalidates() {
        let mut m = PluginManager::new();
        m.panels.push(sidebar_panel("git.status", "git"));
        m.panels[0].needs_render = false;

        let msg = RpcMessage {
            jsonrpc: "2.0".into(),
            id: None,
            method: "reef/statusChanged".into(),
            params: Some(serde_json::json!({})),
            result: None,
            error: None,
        };
        m.handle_message("git", msg);
        assert!(m.status_refresh_needed);
        assert!(m.panels[0].needs_render);
    }
}
